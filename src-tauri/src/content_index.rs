use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rusqlite::{params, Connection};

use crate::config::ContentConfig;
use crate::util;

use std::cell::RefCell;

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// One OCR'd word and its normalized bounding box `[x, y, w, h]` (0..1, top-left
/// origin) within the source image. `text` keeps Tesseract's original case —
/// selection copy must yield "London", not "london"; matching folds at compare
/// time via `content_match::match_key`. `line` is the 0-based line ordinal
/// within the image (block/paragraph/line changes in the TSV), so the frontend
/// can rebuild selection text. Produced by `parse_tsv`, stored in
/// `ocr_word_box` when `ocr_highlight_cache` is on, and read back by the
/// preview's `image_match_rects` / `image_text_layer`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OcrWord {
    pub text: String,
    pub rect: [f32; 4],
    pub line: u32,
}

/// Common English stopwords stripped from a content query when meaningful terms
/// remain. A stopword matches nearly every document, and FTS5 bm25 has no top-k
/// early exit - `ORDER BY rank` must score *every* matching row before `LIMIT`
/// cuts - so ranking by a stopword forces a whole-corpus scan. Dropping it bounds
/// the query to the rare, selective terms that actually drive iteration. An
/// all-stopword query keeps them but skips ranking (`ContentQuery::ranked`).
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into",
    "is", "it", "its", "no", "not", "of", "on", "or", "such", "that", "the", "their",
    "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

/// A user content query parsed into FTS terms. `tokens` are cleaned, deduped
/// (case-insensitive), and stripped of stopwords when meaningful terms remain.
/// `ranked` is false only for all-stopword queries, signalling the search to skip
/// the corpus-wide bm25 sort and just stream the first `LIMIT` matches.
pub struct ContentQuery {
    pub tokens: Vec<String>,
    pub ranked: bool,
}

impl ContentQuery {
    /// Builds the FTS5 `MATCH` string. Terms are ANDed (space-separated); the
    /// final term is a prefix term (`tok*`). The last token is the word currently
    /// being typed, so prefix-matching it lets partial words surface results
    /// mid-keystroke instead of returning nothing until the word completes and its
    /// porter stem lands. Earlier tokens are finished words, matched stemmed-exact.
    ///
    /// Note: FTS5 still runs the porter tokenizer over the `tok` part of `tok*`, so
    /// once the typed prefix runs past the stem into the word's suffix it can miss
    /// (e.g. `documenta*` vs indexed `document`); the frontend smooths that residual
    /// gap by not flashing "no results" until the query settles.
    pub fn fts_match(&self) -> String {
        let last = self.tokens.len().saturating_sub(1);
        self.tokens
            .iter()
            .enumerate()
            .map(|(i, t)| if i == last { format!("{t}*") } else { t.clone() })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Parses a raw user query into a [`ContentQuery`]. Non-alphanumeric chars (except
/// apostrophe) become token separators. Returns `None` when nothing usable remains.
///
/// Dedup collapses `the the the` → `the` (AND of a term with itself is the term),
/// and stopword stripping turns `the report` → `report`; both kill the
/// pathological common-term cost. When the query is *only* stopwords we keep them
/// but mark it unranked so the search avoids the whole-corpus bm25 scan.
pub fn parse_content_query(raw: &str) -> Option<ContentQuery> {
    // Tokenize + fold exactly like the index (`unicode61`): maximal alphanumeric
    // runs, apostrophe is a separator, diacritics folded. Do NOT stem here - the
    // FTS `MATCH` applies the porter wrapper itself; stemming twice risks drift.
    // Drop sub-2-char tokens (matches the frontend `deriveContentTerms` filter) so
    // possessives/contractions like `user's` don't inject a junk `s`/`t` term that
    // ANDs into the query and shrinks results. Dedup on the normalized form.
    let mut seen = HashSet::new();
    let mut tokens: Vec<String> = Vec::new();
    for (_, tok) in crate::content_match::tokenize(raw) {
        let norm = crate::content_match::normalize(tok);
        if norm.chars().count() >= 2 && seen.insert(norm.clone()) {
            tokens.push(norm);
        }
    }
    if tokens.is_empty() {
        return None;
    }

    let meaningful: Vec<String> = tokens
        .iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .cloned()
        .collect();

    if meaningful.is_empty() {
        // All stopwords - keep them but skip ranking (would scan the whole corpus).
        Some(ContentQuery { tokens, ranked: false })
    } else {
        Some(ContentQuery { tokens: meaningful, ranked: true })
    }
}

/// Set while a full-scan reindex (full or incremental) is running. Used to
/// coalesce overlapping reindex triggers so only one drives the progress bar.
static REINDEX_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard for the single-reindex-at-a-time invariant. `acquire` returns
/// `None` if a reindex is already running; the flag clears on drop, so it is
/// released even on early return or panic.
pub struct ReindexGuard(());

impl ReindexGuard {
    pub fn acquire() -> Option<ReindexGuard> {
        REINDEX_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ReindexGuard(()))
    }
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        REINDEX_ACTIVE.store(false, Ordering::Release);
    }
}

// ── database ──────────────────────────────────────────────────────────────────

struct StoredMeta {
    mtime: i64,
    size: u64,
}

struct FileUpdate {
    path: String,
    text: String,
    /// Per-section text: a PDF's pages, an office document's sheets or slides.
    /// `None` for a format with one section (plain text, an image). Populated into
    /// `section_fts` so a preview can open on the section that matched.
    sections: Option<Vec<String>>,
    /// OCR word boxes for an image, captured only when `ocr_highlight_cache` is on.
    /// Empty otherwise. Written to `ocr_word_box` so previews skip per-open OCR.
    boxes: Vec<OcrWord>,
    mtime: i64,
    size: u64,
}

pub struct ContentIndex {
    db: Arc<Mutex<Connection>>,
}

impl ContentIndex {
    pub fn open() -> rusqlite::Result<Self> {
        let dir = crate::paths::data_dir();
        std::fs::create_dir_all(&dir).ok();
        let db_path = dir.join("content_index.db");

        let conn = crate::util::open_sqlite_resilient(&db_path)?;

        // Schema version, bumped whenever the on-disk layout changes. A mismatch drops
        // every table and recreates them, which triggers a one-time full reindex.
        //   v2: added `pdf_page_fts` (per-page PDF text, for match-page preview).
        //   v3: `ocr_word_box` gained `line`; `word` now keeps original case.
        //   v4: `pdf_page_fts` → `section_fts` (`page` → `section`), now also
        //       holding an office document's per-sheet / per-slide text.
        const SCHEMA_VERSION: i64 = 4;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS file_meta;
                 DROP TABLE IF EXISTS content_fts;
                 DROP TABLE IF EXISTS pdf_page_fts;
                 DROP TABLE IF EXISTS section_fts;
                 DROP TABLE IF EXISTS ocr_word_box;",
            )?;
        }

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS file_meta (
                 path       TEXT    PRIMARY KEY,
                 mtime      INTEGER NOT NULL DEFAULT 0,
                 size       INTEGER NOT NULL DEFAULT 0,
                 indexed_at INTEGER NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
                 path UNINDEXED,
                 text,
                 tokenize='porter unicode61'
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS section_fts USING fts5(
                 path UNINDEXED,
                 section UNINDEXED,
                 text,
                 tokenize='porter unicode61'
             );
             CREATE TABLE IF NOT EXISTS ocr_word_box (
                 path TEXT NOT NULL,
                 word TEXT NOT NULL,
                 x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
                 line INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_ocr_word_box_path ON ocr_word_box(path);",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }

    fn all_meta(&self) -> rusqlite::Result<HashMap<String, StoredMeta>> {
        let db = util::lock(&self.db);
        let mut stmt = db.prepare("SELECT path, mtime, size FROM file_meta")?;
        let map = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    StoredMeta {
                        mtime: row.get::<_, i64>(1)?,
                        size: row.get::<_, u64>(2)?,
                    },
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(map)
    }

    fn delete_path(conn: &rusqlite::Connection, path: &str) -> rusqlite::Result<usize> {
        let mut n = 0;
        for tbl in ["content_fts", "section_fts", "ocr_word_box", "file_meta"] {
            n += conn.execute(&format!("DELETE FROM {tbl} WHERE path = ?"), [path])?;
        }
        Ok(n)
    }

    fn delete_prefix(conn: &rusqlite::Connection, pattern: &str) -> rusqlite::Result<usize> {
        // content_fts deletions are the reported count (the others mirror it).
        let removed = conn.execute("DELETE FROM content_fts WHERE path LIKE ?", [pattern])?;
        for tbl in ["section_fts", "ocr_word_box", "file_meta"] {
            conn.execute(&format!("DELETE FROM {tbl} WHERE path LIKE ?"), [pattern])?;
        }
        Ok(removed)
    }

    fn upsert_batch(&self, updates: &[FileUpdate]) -> rusqlite::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut db = util::lock(&self.db);
        let tx = db.transaction()?;
        for u in updates {
            Self::delete_path(&tx, &u.path)?;
            // Empty `text` is a negative-cache tombstone: a file that extracted to
            // nothing (e.g. an OCR'd screenshot with no detectable text) or errored.
            // We still write its file_meta below so the mtime+size skip catches it
            // next run instead of re-extracting it every startup - but we skip the
            // content_fts/section_fts inserts so it never surfaces in search.
            if !u.text.trim().is_empty() {
                tx.execute(
                    "INSERT INTO content_fts(path, text) VALUES (?, ?)",
                    params![u.path, u.text],
                )?;
                if let Some(sections) = &u.sections {
                    for (i, sec) in sections.iter().enumerate() {
                        // An empty section still holds its index: `i` is the
                        // renderer's section number, so a blank slide is skipped
                        // here without shifting the ones after it.
                        if sec.trim().is_empty() {
                            continue;
                        }
                        tx.execute(
                            "INSERT INTO section_fts(path, section, text) VALUES (?, ?, ?)",
                            params![u.path, i as i64, sec],
                        )?;
                    }
                }
                // Word boxes (only present when ocr_highlight_cache is on, for images).
                for b in &u.boxes {
                    let [x, y, w, h] = b.rect;
                    tx.execute(
                        "INSERT INTO ocr_word_box(path, word, x, y, w, h, line) VALUES (?, ?, ?, ?, ?, ?, ?)",
                        params![u.path, b.text, x, y, w, h, b.line],
                    )?;
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO file_meta(path, mtime, size, indexed_at) VALUES (?, ?, ?, ?)",
                params![u.path, u.mtime, u.size, now],
            )?;
        }
        tx.commit()
    }

    pub fn remove_stale(&self, live_paths: &HashSet<String>) -> rusqlite::Result<usize> {
        let mut db = util::lock(&self.db);
        let stale: Vec<String> = {
            let mut stmt = db.prepare("SELECT path FROM file_meta")?;
            let stale = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .filter(|p| !live_paths.contains(p))
                .collect();
            stale
        };
        let count = stale.len();
        let tx = db.transaction()?;
        for path in &stale {
            Self::delete_path(&tx, path)?;
        }
        tx.commit()?;
        Ok(count)
    }

    /// FTS search for `fts_query`, capped at `limit` rows. When `ranked`, results
    /// are bm25-ordered (`ORDER BY rank`) - correct but corpus-wide for common
    /// terms. When not (all-stopword queries), the `ORDER BY rank` is dropped so
    /// FTS5 streams the first `limit` matches in docid order and stops, turning a
    /// multi-second whole-corpus scan into a bounded one. `rank` is still selected
    /// (bm25 is then computed only for the returned rows) so the score wiring is
    /// unchanged; the values are just arbitrary for the unranked case.
    pub fn search(&self, fts_query: &str, limit: usize, ranked: bool) -> rusqlite::Result<Vec<(String, f64, String, i64, u64)>> {
        let profile = util::profile_search();
        let t_lock = profile.then(std::time::Instant::now);
        let db = util::lock(&self.db);
        // Time spent here = mutex contention (a reindex's upsert transaction holds
        // this lock); time after = actual FTS MATCH + snippet() generation.
        let t_query = t_lock.map(|t| {
            let waited = t.elapsed();
            (waited, std::time::Instant::now())
        });
        // \x02 = STX, \x03 = ETX - used as highlight start/end markers in the snippet.
        let order = if ranked { "ORDER BY rank " } else { "" };
        let sql = format!(
            "SELECT content_fts.path, rank, snippet(content_fts, 1, '\x02', '\x03', '…', 20), \
             COALESCE(m.mtime, 0), COALESCE(m.size, 0) \
             FROM content_fts LEFT JOIN file_meta m ON m.path = content_fts.path \
             WHERE content_fts.text MATCH ? {order}LIMIT ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let results = stmt
            .query_map(params![fts_query, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        if let Some((waited, t_query)) = t_query {
            eprintln!(
                "[profile] content_index::search lock_wait={:.2}ms query+collect={:.2}ms ranked={ranked} rows={} q={:?}",
                waited.as_secs_f64() * 1000.0,
                t_query.elapsed().as_secs_f64() * 1000.0,
                results.len(),
                fts_query,
            );
        }
        Ok(results)
    }

    pub fn remove_path(&self, path: &str) -> rusqlite::Result<()> {
        let db = util::lock(&self.db);
        Self::delete_path(&db, path)?;
        Ok(())
    }

    /// Section index (0-based) of the best-matching section for `fts_query`, or
    /// `None` if the file has no indexed sections or none matched. A section is a
    /// PDF page, a worksheet or a slide - whatever the preview can open on.
    ///
    /// Ranks sections by how many *distinct* query terms they cover (so a multi-part
    /// query lands on the section holding the most of them), and breaks ties toward
    /// the earliest one - rather than FTS BM25, which favours shorter sections and
    /// would e.g. pick a repeated header on the last page over the first.
    pub fn best_section(&self, path: &str, fts_query: &str) -> Option<u32> {
        let t_start = util::profile_search().then(std::time::Instant::now);
        let terms: Vec<String> = fts_query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            return None;
        }
        // Single OR query so this stays one FTS lookup per file (same cost as before):
        // it returns every section carrying *any* term; coverage is counted in Rust.
        // Phrase-quote each term so FTS treats it literally (apostrophes etc.); doubled
        // quotes escape an embedded quote.
        let or_query = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        let db = util::lock(&self.db);
        let mut stmt = db
            .prepare(
                "SELECT section, text FROM section_fts WHERE path = ? AND text MATCH ? \
                 ORDER BY section",
            )
            .ok()?;
        let rows = stmt
            .query_map(params![path, or_query], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .ok()?;

        // Key the query terms exactly as the index did (`porter unicode61`), so
        // coverage counts the same matches FTS ranked on - not a prefix guess.
        let query_keys = crate::content_match::query_keys(terms.iter().cloned());

        // Sections arrive in ascending order; keep the first that strictly beats the
        // best coverage so far - max distinct terms, earliest section on ties.
        let mut best: Option<(u32, u32)> = None; // (coverage, section)
        for (section, text) in rows.flatten() {
            // Key every word of the section once, then count how many distinct query
            // keys it covers - O(words) stems per section (best_section runs once per
            // preview open, not per keystroke).
            let keys: HashSet<String> = crate::content_match::tokenize(&text)
                .iter()
                .map(|(_, w)| crate::content_match::match_key(w))
                .collect();
            let cov = query_keys.iter().filter(|k| keys.contains(*k)).count() as u32;
            if best.is_none_or(|(bc, _)| cov > bc) {
                best = Some((cov, section as u32));
            }
        }
        if let Some(t) = t_start {
            eprintln!(
                "[profile] content_index::best_section took={:.2}ms path={path:?}",
                t.elapsed().as_secs_f64() * 1000.0,
            );
        }
        best.map(|(_, section)| section)
    }

    /// Removes all entries whose path starts with `dir_path/`. Used when a directory is
    /// deleted or moved out of the watched tree and no individual file events are fired.
    pub fn remove_prefix(&self, dir_path: &str) -> rusqlite::Result<usize> {
        let pattern = format!("{dir_path}/%");
        let mut db = util::lock(&self.db);
        let tx = db.transaction()?;
        let removed = Self::delete_prefix(&tx, &pattern)?;
        tx.commit()?;
        Ok(removed)
    }

    fn get_meta(&self, path: &str) -> rusqlite::Result<Option<StoredMeta>> {
        let db = util::lock(&self.db);
        match db.query_row(
            "SELECT mtime, size FROM file_meta WHERE path = ?",
            [path],
            |r| Ok(StoredMeta { mtime: r.get(0)?, size: r.get(1)? }),
        ) {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn clear(&self) -> rusqlite::Result<()> {
        let db = util::lock(&self.db);
        db.execute_batch(
            "DELETE FROM content_fts;
             DELETE FROM section_fts;
             DELETE FROM ocr_word_box;
             DELETE FROM file_meta;",
        )
    }

    /// Cached OCR word boxes for an image path, for the preview's `image_match_rects`
    /// / `image_text_layer` fast paths. `None` means the path is not indexed (so the
    /// caller may fall back to on-demand OCR); `Some(vec)` — possibly empty — means it
    /// is indexed and these are all its boxes (empty = OCR found no text, so no
    /// fallback is warranted). `ORDER BY rowid` preserves insertion (reading) order,
    /// which selection-text reconstruction depends on.
    pub fn cached_word_boxes(&self, path: &str) -> Option<Vec<OcrWord>> {
        let db = util::lock(&self.db);
        let indexed = db
            .query_row("SELECT 1 FROM file_meta WHERE path = ? LIMIT 1", [path], |_| Ok(()))
            .is_ok();
        if !indexed {
            return None;
        }
        let mut stmt = db
            .prepare("SELECT word, x, y, w, h, line FROM ocr_word_box WHERE path = ? ORDER BY rowid")
            .ok()?;
        let rows = stmt
            .query_map([path], |r| {
                Ok(OcrWord {
                    text: r.get::<_, String>(0)?,
                    rect: [
                        r.get::<_, f64>(1)? as f32,
                        r.get::<_, f64>(2)? as f32,
                        r.get::<_, f64>(3)? as f32,
                        r.get::<_, f64>(4)? as f32,
                    ],
                    line: r.get::<_, i64>(5)? as u32,
                })
            })
            .ok()?;
        Some(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn is_empty(&self) -> bool {
        let db = util::lock(&self.db);
        db.query_row("SELECT COUNT(*) FROM file_meta", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) == 0
    }
}

// ── text extraction ───────────────────────────────────────────────────────────

fn pdftotext(path: &str) -> Result<String, String> {
    let out = crate::runtime_assets::poppler_command("pdftotext")
        .args([path, "-"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

thread_local! {
    // Caches the instance together with the language it was built for, so a
    // changed `ocr_language` rebinds the worker instead of being ignored.
    static LEPTESS: RefCell<Option<(String, leptess::LepTess)>> = RefCell::new(None);
}

/// Runs `f` with the thread-local Tesseract instance, lazily initialising it on
/// first use (preferring the bundled tessdata dir, else the system install).
/// Rebuilds the instance whenever `lang` differs from the cached one.
fn with_leptess<R>(
    lang: &str,
    f: impl FnOnce(&mut leptess::LepTess) -> Result<R, String>,
) -> Result<R, String> {
    LEPTESS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|(l, _)| l.as_str()) != Some(lang) {
            let datapath = crate::runtime_assets::tessdata_path(lang);
            *slot = leptess::LepTess::new(datapath.as_deref(), lang)
                .ok()
                .map(|api| (lang.to_string(), api));
        }
        let (_, api) = slot.as_mut().ok_or_else(|| "tesseract unavailable".to_string())?;
        f(api)
    })
}

fn ocr_file(path: &str, lang: &str) -> Result<String, String> {
    ocr_file_text_and_boxes(path, lang).map(|(text, _)| text)
}

/// OCR an image, returning both its confidence-filtered text and its word boxes
/// in one pass (both from the TSV layout). Used during indexing when
/// `ocr_highlight_cache` is on (so the boxes land in `ocr_word_box`) and by the
/// preview's on-demand OCR worker, which inits that thread's own `LEPTESS`.
pub(crate) fn ocr_file_text_and_boxes(path: &str, lang: &str) -> Result<(String, Vec<OcrWord>), String> {
    with_leptess(lang, |api| {
        api.set_image(path).map_err(|e| format!("{e:?}"))?;
        let tsv = api.get_tsv_text(0).map_err(|e| format!("{e:?}"))?;
        let (w, h) = api.get_image_dimensions().unwrap_or((0, 0));
        Ok(parse_tsv(&tsv, w, h))
    })
}

// Tesseract word confidence is 0..100. Below this, OCR output is mostly noise
// (texture/edges read as short junk like "iy"); drop it from both the search
// index and the highlight boxes.
const MIN_WORD_CONFIDENCE: f32 = 50.0;

/// Single pass over Tesseract TSV → (reconstructed text, normalized word boxes),
/// both filtered to word-level rows (`level`==5) at or above `MIN_WORD_CONFIDENCE`
/// with non-empty text. Text joins kept words with a space, breaking to a newline
/// when the block/paragraph/line index changes; boxes carry the same break as a
/// 0-based `line` ordinal. Box text keeps original case (matching folds at compare
/// time). Boxes map the pixel `left/top/width/height` (top-left origin) into 0..1
/// of the source image; they need valid image dims, the text does not.
fn parse_tsv(tsv: &str, img_w: u32, img_h: u32) -> (String, Vec<OcrWord>) {
    // Defensive cap: a pathological image can't blow up memory / the DB row count.
    const MAX_BOXES: usize = 2000;
    let dims_ok = img_w != 0 && img_h != 0;
    let (fw, fh) = (img_w as f32, img_h as f32);
    let mut text = String::new();
    let mut boxes = Vec::new();
    let mut cur_line: Option<(&str, &str, &str)> = None;
    let mut line_ord: u32 = 0;
    for line in tsv.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        // Columns: level page block par line word left top width height conf text.
        if cols.len() < 12 || cols[0] != "5" {
            continue;
        }
        if cols[10].parse::<f32>().unwrap_or(-1.0) < MIN_WORD_CONFIDENCE {
            continue;
        }
        let word = cols[11].trim();
        if word.is_empty() {
            continue;
        }

        // Text: newline when block/paragraph/line changes, else a space. The box
        // line ordinal advances with the same breaks so text and boxes agree.
        let key = (cols[2], cols[3], cols[4]);
        if !text.is_empty() {
            if cur_line == Some(key) {
                text.push(' ');
            } else {
                text.push('\n');
                line_ord += 1;
            }
        }
        cur_line = Some(key);
        text.push_str(word);

        // Boxes: only with known dims and below the cap; text keeps accumulating.
        if dims_ok && boxes.len() < MAX_BOXES {
            if let (Ok(left), Ok(top), Ok(w), Ok(h)) = (
                cols[6].parse::<f32>(),
                cols[7].parse::<f32>(),
                cols[8].parse::<f32>(),
                cols[9].parse::<f32>(),
            ) {
                if w > 0.0 && h > 0.0 {
                    boxes.push(OcrWord {
                        text: word.to_string(),
                        rect: [
                            (left / fw).clamp(0.0, 1.0),
                            (top / fh).clamp(0.0, 1.0),
                            (w / fw).clamp(0.0, 1.0),
                            (h / fh).clamp(0.0, 1.0),
                        ],
                        line: line_ord,
                    });
                }
            }
        }
    }
    (text, boxes)
}

/// OCR raw image bytes (e.g. a clipboard image), returning text + word boxes in
/// one TSV pass. leptess reads bytes directly (`set_image_from_mem`), so no temp
/// file; leptonica sniffs the format from content.
pub(crate) fn ocr_bytes_text_and_boxes(bytes: &[u8], lang: &str) -> Result<(String, Vec<OcrWord>), String> {
    with_leptess(lang, |api| {
        api.set_image_from_mem(bytes).map_err(|e| format!("{e:?}"))?;
        let tsv = api.get_tsv_text(0).map_err(|e| format!("{e:?}"))?;
        let (w, h) = api.get_image_dimensions().unwrap_or((0, 0));
        Ok(parse_tsv(&tsv, w, h))
    })
}

fn extract_pdf(path: &str, ocr_fallback: bool, lang: &str) -> Result<String, String> {
    let text = pdftotext(path)?;
    if text.trim().len() >= 50 || !ocr_fallback {
        return Ok(text);
    }

    // No meaningful text layer - render pages to images via pdftoppm and OCR each
    let tmp_dir = std::env::temp_dir().join(format!(
        "portunus_pdftoppm_{}_{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let prefix = tmp_dir.join("page");

    let result = crate::runtime_assets::poppler_command("pdftoppm")
        .args(["-tiff", "-r", "150", path, prefix.to_str().unwrap_or("page")])
        .output();

    let ocr_text = match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::remove_dir_all(&tmp_dir).ok();
            return Ok(text);
        }
        Err(e) => {
            std::fs::remove_dir_all(&tmp_dir).ok();
            return Err(e.to_string());
        }
        Ok(out) if !out.status.success() => {
            std::fs::remove_dir_all(&tmp_dir).ok();
            return Ok(text);
        }
        Ok(_) => {
            let mut combined = String::new();
            let mut page_files: Vec<_> = std::fs::read_dir(&tmp_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tif"))
                .collect();
            page_files.sort_by_key(|e| e.file_name());
            for entry in page_files {
                if let Some(p) = entry.path().to_str() {
                    match ocr_file(p, lang) {
                        // Separate pages with a form-feed so they split the same
                        // way pdftotext output does (used for per-page indexing).
                        Ok(t) => {
                            combined.push_str(&t);
                            combined.push('\u{000C}');
                        }
                        Err(e) => eprintln!("[content] ocr page failed {p}: {e}"),
                    }
                }
            }
            combined
        }
    };

    std::fs::remove_dir_all(&tmp_dir).ok();
    Ok(ocr_text)
}

// Internal dispatch: these define HOW text is extracted, not which extensions are indexed.
// The user-facing extension list lives in ContentConfig::extensions.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "rst", "csv", "log", "toml", "yaml", "yml", "json", "xml", "sh", "bash",
    "zsh", "py", "rs", "js", "ts", "go", "c", "cpp", "h", "hpp", "rb", "java", "kt",
];

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "tiff", "tif"];

/// Extracted text plus, for images when `ocr_highlight_cache` is on, the OCR word
/// boxes (empty for every other path). Boxes ride alongside text so an image is
/// OCR'd once at index time, not again for highlight caching.
fn extract_text(path: &str, cfg: &ContentConfig) -> Result<(String, Vec<OcrWord>), String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext == "pdf" {
        extract_pdf(path, cfg.ocr_pdf_fallback, &cfg.ocr_language).map(|t| (t, Vec::new()))
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        if cfg.ocr_images {
            if cfg.ocr_highlight_cache {
                return ocr_file_text_and_boxes(path, &cfg.ocr_language);
            }
            return ocr_file(path, &cfg.ocr_language).map(|t| (t, Vec::new()));
        }
        Err("image OCR disabled".to_string())
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        std::fs::read_to_string(path)
            .map(|t| (t, Vec::new()))
            .map_err(|e| e.to_string())
    } else if crate::office::is_office_ext(&ext) {
        crate::office::extract_office_text(path).map(|t| (t, Vec::new()))
    } else {
        Err(format!("unsupported extension: {ext}"))
    }
}

/// Splits extracted text into per-section strings, or `None` for a format whose
/// preview has a single section (plain text, an image) and therefore nothing to
/// open on.
///
/// One separator serves both paged formats: `pdftotext` and the OCR fallback emit
/// a form feed between pages, and `office::extract_office_text` joins its sections
/// with the same character (`office::SECTION_SEP`) — which is why splitting here
/// costs nothing and cannot disagree with the extractor. Section *n* of the split
/// is the section `office::render` draws for `section = n`.
fn doc_sections(path: &str, text: &str) -> Option<Vec<String>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let paged = ext == "pdf" || crate::office::is_office_ext(&ext);
    paged.then(|| {
        text.split(crate::office::SECTION_SEP)
            .map(|s| s.to_string())
            .collect()
    })
}

// ── background indexer ────────────────────────────────────────────────────────

fn collect_eligible(cfg: &ContentConfig) -> Vec<(std::path::PathBuf, u64, i64)> {
    let mut eligible = Vec::new();
    for dir_entry in &cfg.dirs {
        let dir = crate::config::Config::expand_path(&dir_entry.path);
        if !dir.is_dir() {
            continue;
        }
        let effective_exts = dir_entry.extensions.as_ref().unwrap_or(&cfg.extensions);
        for entry in walkdir::WalkDir::new(&dir)
            .max_depth(dir_entry.depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !effective_exts.contains(&ext) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();
            if size > cfg.max_file_bytes {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            eligible.push((entry.path().to_path_buf(), size, mtime));
        }
    }
    eligible
}

/// A pre-index time/size estimate for a single directory. Counts are exact (a
/// cheap metadata-only walk); the seconds range is a coarse heuristic - see
/// `estimate_dir`. Serialized to the frontend for the per-directory estimate row.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DirEstimate {
    pub total_files: usize,
    pub pdf_files: usize,
    pub image_files: usize,
    pub est_secs_min: u64,
    pub est_secs_max: u64,
}

/// Per-file fixed overhead + per-megabyte rate, in seconds, for one work bucket.
/// Cost is modelled as `count * fixed + total_mb * per_mb` because the dominant
/// term differs by type: a subprocess spawn (PDF) or OCR init is per-file, while
/// read/tokenise/extract scales with bytes. Calibrated coarsely against real
/// runs - see `estimate_dir`.
struct Cost {
    fixed: f64,
    per_mb: f64,
}

impl Cost {
    fn secs(&self, count: usize, mb: f64) -> f64 {
        count as f64 * self.fixed + mb * self.per_mb
    }
}

/// Walks a single directory (respecting `depth`, `extensions`, and the size cap)
/// and produces file counts plus a min/max indexing-time estimate. No text is
/// extracted, so this is fast even on large trees.
///
/// The seconds are a size-weighted heuristic - they set expectations, not promises.
/// Each bucket's cost is `files * fixed_overhead + megabytes * per_mb_rate`, so a
/// folder of small screenshots and one of large scans estimate very differently
/// even at the same file count. PDFs dominate the uncertainty: with a text layer a
/// PDF is a quick `pdftotext`; a scanned one routed through OCR is far slower and
/// scales with page imagery. The range brackets those: the minimum assumes every
/// PDF has a text layer; the maximum assumes every PDF is OCR'd (only when
/// `ocr_pdf_fallback` is on, otherwise a no-text PDF just fails fast).
///
/// `extensions` overrides the global `cfg.extensions` when `Some` (per-dir list).
pub fn estimate_dir(
    path: &str,
    depth: usize,
    extensions: Option<&Vec<String>>,
    cfg: &ContentConfig,
) -> DirEstimate {
    let dir = crate::config::Config::expand_path(path);
    if !dir.is_dir() {
        return DirEstimate::default();
    }
    let effective_exts = extensions.unwrap_or(&cfg.extensions);

    // (count, total_bytes) per bucket.
    let (mut fast, mut office, mut pdf, mut image, mut other) =
        ((0usize, 0u64), (0usize, 0u64), (0usize, 0u64), (0usize, 0u64), (0usize, 0u64));
    for entry in walkdir::WalkDir::new(&dir)
        .max_depth(depth)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !effective_exts.contains(&ext) {
            continue;
        }
        let size = match entry.metadata() {
            Ok(m) if m.len() <= cfg.max_file_bytes => m.len(),
            _ => continue,
        };
        let bucket = if ext == "pdf" {
            &mut pdf
        } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            &mut image
        } else if crate::office::is_office_ext(&ext) {
            &mut office
        } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
            &mut fast
        } else {
            &mut other
        };
        bucket.0 += 1;
        bucket.1 += size;
    }

    let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);

    // Coarse, size-weighted per-bucket costs (seconds), recalibrated against real
    // indexing runs. Text reads are disk-bound and cheap; `pdftotext` pays a
    // subprocess spawn plus byte cost; OCR (image, or scanned-PDF fallback) is the
    // expensive outlier and scales with image data.
    const FAST: Cost = Cost { fixed: 0.002, per_mb: 0.01 };       // read + tokenise + FTS insert
    // An office document is a zip of XML: inflate every sheet or slide part, parse
    // each, walk it. No subprocess and no OCR, so it sits an order of magnitude
    // under `pdftotext` and an order over a flat text read - which is why it is its
    // own bucket rather than priced as plain text.
    const OFFICE: Cost = Cost { fixed: 0.004, per_mb: 0.12 };
    const PDF_TEXT: Cost = Cost { fixed: 0.04, per_mb: 0.03 };    // pdftotext subprocess
    const PDF_OCR: Cost = Cost { fixed: 0.20, per_mb: 1.50 };     // pdftoppm + tesseract per page
    const PDF_SKIP: Cost = Cost { fixed: 0.05, per_mb: 0.0 };     // no text layer, OCR off → give up
    const IMG_OCR_MIN: Cost = Cost { fixed: 0.20, per_mb: 0.30 };
    const IMG_OCR_MAX: Cost = Cost { fixed: 0.40, per_mb: 0.80 };
    const OTHER: Cost = Cost { fixed: 0.002, per_mb: 0.0 };       // listed but unsupported → quick fail

    let img_cost_min = if cfg.ocr_images { IMG_OCR_MIN.secs(image.0, mb(image.1)) } else { 0.0 };
    let img_cost_max = if cfg.ocr_images { IMG_OCR_MAX.secs(image.0, mb(image.1)) } else { 0.0 };
    let pdf_cost_max = if cfg.ocr_pdf_fallback {
        PDF_OCR.secs(pdf.0, mb(pdf.1))
    } else {
        PDF_SKIP.secs(pdf.0, mb(pdf.1))
    };

    let parallelism = if cfg.threads == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2)
    } else {
        cfg.threads
    }
    .max(1) as f64;

    let base = FAST.secs(fast.0, mb(fast.1))
        + OFFICE.secs(office.0, mb(office.1))
        + OTHER.secs(other.0, mb(other.1));
    let total_min = base + PDF_TEXT.secs(pdf.0, mb(pdf.1)) + img_cost_min;
    let total_max = base + pdf_cost_max + img_cost_max;

    DirEstimate {
        total_files: fast.0 + office.0 + pdf.0 + image.0 + other.0,
        pdf_files: pdf.0,
        image_files: image.0,
        est_secs_min: (total_min / parallelism).ceil() as u64,
        est_secs_max: (total_max / parallelism).ceil() as u64,
    }
}

pub fn run_content_indexer(
    index: Arc<ContentIndex>,
    cfg: &ContentConfig,
    on_progress: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) {
    let eligible = collect_eligible(cfg);
    let total = eligible.len();

    if total > 0 {
        if let Some(ref cb) = on_progress {
            cb(0, total);
        }
    }

    let live_paths: HashSet<String> = eligible
        .iter()
        .map(|(p, _, _)| p.to_string_lossy().into_owned())
        .collect();

    let stored = match index.all_meta() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[content] failed to load metadata: {e}");
            HashMap::new()
        }
    };

    let processed = Arc::new(AtomicUsize::new(0));
    let cfg_ref = cfg;

    let pool_result = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads)
        .build();

    let updates: Vec<FileUpdate> = match pool_result {
        Ok(pool) => pool.install(|| {
            collect_updates(&eligible, &stored, cfg_ref, &processed, total, &on_progress)
        }),
        Err(_) => collect_updates(&eligible, &stored, cfg_ref, &processed, total, &on_progress),
    };

    let db_indexed = updates.len();
    match index.upsert_batch(&updates) {
        Ok(()) => {}
        Err(e) => eprintln!("[content] upsert_batch failed: {e}"),
    }

    match index.remove_stale(&live_paths) {
        Ok(removed) => eprintln!(
            "[content] done: total={total} indexed={db_indexed} removed={removed}"
        ),
        Err(e) => eprintln!("[content] remove_stale error: {e}"),
    }

    // Signal 100% only after the DB writes are committed, so the frontend
    // progress bar doesn't disappear while upsert_batch is still running.
    if total > 0 {
        if let Some(ref cb) = on_progress {
            cb(total, total);
        }
    }
}

fn collect_updates(
    eligible: &[(std::path::PathBuf, u64, i64)],
    stored: &HashMap<String, StoredMeta>,
    cfg: &ContentConfig,
    processed: &Arc<AtomicUsize>,
    total: usize,
    on_progress: &Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
) -> Vec<FileUpdate> {
    eligible
        .par_iter()
        .filter_map(|(path_buf, size, mtime)| {
            let path_str = path_buf.to_string_lossy().into_owned();

            // Fast-path: mtime+size unchanged → skip with no file read
            if let Some(m) = stored.get(&path_str) {
                if m.mtime == *mtime && m.size == *size {
                    let n = processed.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 10 == 0 && n < total {
                        if let Some(ref cb) = on_progress {
                            cb(n, total);
                        }
                    }
                    return None;
                }
            }

            // Empty text on a successful extract (e.g. an image that OCR'd to
            // nothing) or an extract error both yield an empty-text FileUpdate -
            // a negative-cache tombstone. upsert_batch writes its file_meta but no
            // FTS row, so the mtime+size fast-path skips it next run instead of
            // re-running the (expensive) extraction every startup.
            let (text, boxes) = match extract_text(&path_str, cfg) {
                Ok((t, b)) if !t.trim().is_empty() => (t, b),
                Ok(_) => (String::new(), Vec::new()),
                Err(e) => {
                    eprintln!("[content] extract failed {path_str}: {e}");
                    (String::new(), Vec::new())
                }
            };

            let n = processed.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 10 == 0 && n < total {
                if let Some(ref cb) = on_progress {
                    cb(n, total);
                }
            }

            let sections = doc_sections(&path_str, &text);

            Some(FileUpdate {
                path: path_str,
                text,
                sections,
                boxes,
                mtime: *mtime,
                size: *size,
            })
        })
        .collect()
}

// ── per-event helpers ─────────────────────────────────────────────────────────

/// Called for every filesystem event path. Returns true if the index was modified.
pub fn process_event_path(index: &Arc<ContentIndex>, path: &Path, cfg: &ContentConfig, log: bool) -> bool {
    let path_str = path.to_string_lossy().into_owned();

    if !path.exists() {
        if log { eprintln!("[content] event: path gone, removing - {path_str}"); }
        // Remove the path itself plus any children (handles directory deletions/moves-out
        // where individual file events are not fired for the contents).
        let removed_children = index.remove_prefix(&path_str).unwrap_or(0) > 0;
        let removed_self = index.remove_path(&path_str).is_ok();
        if log { eprintln!("[content] event: removed_children={removed_children} removed_self={removed_self}"); }
        return removed_children || removed_self;
    }

    if path.is_dir() {
        if log { eprintln!("[content] event: directory appeared, walking - {path_str}"); }
        // Directory appeared (created or moved in): walk it and index all eligible files.
        // Individual file Create events are not fired for files inside a moved-in directory.
        // Honour the per-dir depth limit from config, just like the initial index does.
        let walk_depth = cfg.dirs.iter()
            .filter_map(|d| {
                let base = crate::config::Config::expand_path(&d.path);
                let rel = path.strip_prefix(&base).ok()?;
                Some(d.depth.saturating_sub(rel.components().count()))
            })
            .max()
            .unwrap_or(usize::MAX);
        let mut changed = false;
        for entry in walkdir::WalkDir::new(path)
            .max_depth(walk_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            if process_event_path(index, entry.path(), cfg, log) {
                changed = true;
            }
        }
        return changed;
    }

    if !is_event_path_eligible(path, cfg) {
        if log { eprintln!("[content] event: not eligible - {path_str}"); }
        return index.remove_path(&path_str).is_ok();
    }

    let Ok(meta) = std::fs::metadata(path) else {
        if log { eprintln!("[content] event: metadata failed - {path_str}"); }
        return false;
    };
    let size = meta.len();
    if size > cfg.max_file_bytes {
        if log { eprintln!("[content] event: file too large ({size}) - {path_str}"); }
        return index.remove_path(&path_str).is_ok();
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if let Ok(Some(m)) = index.get_meta(&path_str) {
        if m.mtime == mtime && m.size == size {
            if log { eprintln!("[content] event: mtime+size unchanged, skipping - {path_str}"); }
            return false;
        }
    }

    // Empty text (extract found nothing) or an extract error both write an
    // empty-text tombstone: upsert_batch skips the content_fts/section_fts inserts
    // for empty text but still records file_meta, so the mtime+size fast-path skips
    // this file next time instead of re-extracting it, and it stays out of search
    // just as remove_path would leave it.
    let (text, boxes) = match extract_text(&path_str, cfg) {
        Ok((t, b)) if !t.trim().is_empty() => (t, b),
        Ok(_) => (String::new(), Vec::new()),
        Err(e) => {
            eprintln!("[content] event extract failed {path_str}: {e}");
            (String::new(), Vec::new())
        }
    };
    let sections = doc_sections(&path_str, &text);
    match index.upsert_batch(&[FileUpdate { path: path_str.clone(), text, sections, boxes, mtime, size }]) {
        Ok(()) => { eprintln!("[content] indexed {path_str}"); true }
        Err(e) => { eprintln!("[content] event upsert failed {path_str}: {e}"); false }
    }
}

fn is_event_path_eligible(path: &Path, cfg: &ContentConfig) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    for dir_entry in &cfg.dirs {
        let dir = crate::config::Config::expand_path(&dir_entry.path);
        if let Ok(rel) = path.strip_prefix(&dir) {
            let effective_exts = dir_entry.extensions.as_ref().unwrap_or(&cfg.extensions);
            if !effective_exts.contains(&ext) {
                return false;
            }
            return rel.components().count() <= dir_entry.depth;
        }
    }
    false
}

#[cfg(test)]
mod ocr_tests {
    use super::*;

    // A Tesseract TSV row at word level (level 5). Columns:
    // level page block par line word left top width height conf text.
    fn word_row(block: u32, par: u32, line: u32, left: u32, top: u32, conf: f32, text: &str) -> String {
        format!("5\t1\t{block}\t{par}\t{line}\t1\t{left}\t{top}\t20\t10\t{conf}\t{text}")
    }

    #[test]
    fn keeps_original_case_and_line_ordinals() {
        let tsv = [
            word_row(1, 1, 1, 0, 0, 90.0, "London"),
            word_row(1, 1, 1, 30, 0, 90.0, "Bridge"),
            word_row(1, 1, 2, 0, 20, 90.0, "Falls"),
        ]
        .join("\n");
        let (text, boxes) = parse_tsv(&tsv, 100, 100);
        assert_eq!(text, "London Bridge\nFalls");
        assert_eq!(boxes.len(), 3);
        // Original case preserved (selection copy must not lowercase).
        assert_eq!(boxes[0].text, "London");
        // Line ordinal advances only on a block/par/line change.
        assert_eq!(boxes[0].line, 0);
        assert_eq!(boxes[1].line, 0);
        assert_eq!(boxes[2].line, 1);
    }

    #[test]
    fn drops_low_confidence_words() {
        let tsv = [
            word_row(1, 1, 1, 0, 0, 95.0, "café"),
            word_row(1, 1, 1, 30, 0, 10.0, "iy"), // below MIN_WORD_CONFIDENCE
        ]
        .join("\n");
        let (text, boxes) = parse_tsv(&tsv, 100, 100);
        assert_eq!(text, "café");
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].text, "café");
    }

    #[test]
    fn no_dimensions_yields_text_but_no_boxes() {
        let tsv = word_row(1, 1, 1, 0, 0, 90.0, "naïve");
        let (text, boxes) = parse_tsv(&tsv, 0, 0);
        assert_eq!(text, "naïve");
        assert!(boxes.is_empty());
    }

    #[test]
    fn doc_sections_splits_every_paged_format_and_nothing_else() {
        let text = format!("one{sep}two{sep}three", sep = crate::office::SECTION_SEP);
        // A PDF's pages and an office document's sheets or slides are the same
        // thing to the index, and both arrive separated by the one character
        // `office::extract_office_text` joins with.
        for path in ["/x/report.pdf", "/x/report.PDF", "/x/book.xlsx", "/x/deck.odp"] {
            let got = doc_sections(path, &text).unwrap_or_else(|| panic!("{path} is paged"));
            assert_eq!(got, ["one", "two", "three"], "{path}");
        }
        // A single-section format has no section to open on, so it writes no
        // `section_fts` rows at all rather than one row per accidental form feed.
        for path in ["/x/notes.txt", "/x/photo.png", "/x/main.rs"] {
            assert!(doc_sections(path, &text).is_none(), "{path}");
        }
    }
}
