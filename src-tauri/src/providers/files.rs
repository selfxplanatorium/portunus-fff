//! File-name search backed by fff (`fff-search`).
//!
//! Every configured root gets its own fff `FilePicker`: a parallel
//! gitignore-aware walk into a compact arena, an optional per-directory inotify
//! watcher that keeps it current, and a SIMD typo-tolerant fuzzy matcher that
//! scores a million paths in tens of milliseconds. This module only maps the
//! configured roots onto pickers and fff hits onto `SearchResult`s.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use fff_search::file_picker::FilePicker;
use fff_search::{
    FFFMode, FilePickerOptions, FuzzySearchOptions, GitRecencyConfig, MixedItemRef,
    PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};

use super::{ranking, Provider, SearchResult};
use crate::config::{FilesConfig, SharedConfig};
use crate::util;

/// Folders sink below files within the file band (they carry less signal than
/// a name-matched file but still beat the dict-fill band below).
const FOLDER_OFFSET: f32 = -700_000.0;

/// How many top-scored hits `search` materializes into `SearchResult`s. The
/// registry only ever shows `max_results` of them, but it re-ranks with
/// frecency and pins afterwards, so the provider hands over enough headroom for
/// that reordering to matter.
const CANDIDATE_CAP: usize = 128;

/// Hits scoring below this fraction of the best hit are dropped. fff is
/// typo-tolerant, so a short or unrelated query (a calc expression, a command
/// name) still "matches" thousands of paths; without a floor those would pad
/// every sparse result list with noise.
const RELATIVE_FLOOR: f32 = 0.25;

/// Top-level directories skipped when `/` is configured: kernel and runtime
/// pseudo-filesystems (millions of volatile entries, nothing a user opens by
/// name) and scratch space.
const PSEUDO_ROOT_DIRS: &[&str] = &["proc", "sys", "dev", "run", "tmp", "lost+found"];

/// Upper bound on how long the ready-notifier waits for an initial scan.
const SCAN_WAIT: Duration = Duration::from_secs(30 * 60);

// ── Index ─────────────────────────────────────────────────────────────────────

/// One fff picker over one root directory.
struct Root {
    dir: PathBuf,
    watch: bool,
    picker: SharedFilePicker,
    /// More specific configured roots inside this one. Their paths belong to
    /// them, so the same file is never listed twice and an unwatched `/` can't
    /// resurface a stale copy of a file the watched `~` already saw deleted.
    nested: Vec<PathBuf>,
}

impl Root {
    fn spawn(dir: PathBuf, watch: bool) -> Option<Self> {
        let picker = SharedFilePicker::default();
        let opts = FilePickerOptions {
            base_path: dir.to_string_lossy().into_owned(),
            mode: FFFMode::Neovim,
            watch,
            // Roots are chosen explicitly in the config, so `~` is fair game;
            // `/` itself never reaches fff (see `resolve_roots`).
            enable_home_dir_scanning: true,
            // Name search only: no content mmap warmup, no bigram content index,
            // no git-log ranking. Launch history is portunus' own frecency.
            enable_mmap_cache: false,
            enable_content_indexing: false,
            git_recency: GitRecencyConfig { enabled: false, ..Default::default() },
            ..Default::default()
        };
        match FilePicker::new_with_shared_state(picker.clone(), SharedFrecency::default(), opts) {
            Ok(()) => Some(Root { dir, watch, picker, nested: vec![] }),
            Err(e) => {
                eprintln!("[files] cannot index {}: {e}", dir.display());
                None
            }
        }
    }

    fn shutdown(&self) {
        self.picker.cancel();
        self.picker.shutdown_watches();
        // Dropping the picker stops its scan job, git worker and watcher.
        if let Ok(mut guard) = self.picker.write() {
            guard.take();
        }
    }
}

/// A hit from one root, before it becomes a `SearchResult`.
struct Hit {
    path: String,
    name: String,
    parent: String,
    is_dir: bool,
    hidden: bool,
    size: Option<u64>,
    modified: Option<u64>,
    score: i32,
}

/// The live set of fff pickers. Shared between the provider, the config reload
/// path and `--reindex`; the pickers keep themselves current via their own
/// watchers.
#[derive(Default)]
pub struct FileIndex {
    roots: RwLock<Vec<Root>>,
}

impl FileIndex {
    /// Reconcile the running pickers with `cfg`: roots whose path and watch
    /// flag are unchanged keep their index, the rest are torn down or spawned.
    /// `on_ready` fires once every newly spawned root finished its first scan.
    pub fn configure(&self, cfg: &FilesConfig, on_ready: Option<Arc<dyn Fn() + Send + Sync>>) {
        let wanted = resolve_roots(cfg);
        let mut roots = util::write(&self.roots);

        let mut kept: Vec<Root> = Vec::with_capacity(wanted.len());
        let mut old: Vec<Root> = std::mem::take(&mut *roots);
        let mut fresh: Vec<SharedFilePicker> = vec![];
        for (dir, watch) in wanted {
            if let Some(i) = old.iter().position(|r| r.dir == dir && r.watch == watch) {
                kept.push(old.swap_remove(i));
            } else if let Some(root) = Root::spawn(dir, watch) {
                fresh.push(root.picker.clone());
                kept.push(root);
            }
        }
        let dirs: Vec<PathBuf> = kept.iter().map(|r| r.dir.clone()).collect();
        for r in &mut kept {
            r.nested = dirs
                .iter()
                .filter(|d| **d != r.dir && d.starts_with(&r.dir))
                .cloned()
                .collect();
        }
        *roots = kept;
        drop(roots);
        // Outside the lock: dropping a picker waits for its write lock, which a
        // scan publishing its results may hold for a moment.
        for r in &old {
            r.shutdown();
        }

        if fresh.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            for p in &fresh {
                p.wait_for_scan(SCAN_WAIT);
            }
            let files: usize = fresh
                .iter()
                .filter_map(|p| p.read().ok()?.as_ref().map(|p| p.live_file_count()))
                .sum();
            eprintln!(
                "[files] indexed {files} files in {} new root(s) in {:.1?}",
                fresh.len(),
                started.elapsed()
            );
            if let Some(cb) = on_ready {
                cb();
            }
        });
    }

    /// Stop and drop every picker.
    pub fn clear(&self) {
        let roots = std::mem::take(&mut *util::write(&self.roots));
        for r in &roots {
            r.shutdown();
        }
    }

    /// Re-walk every root in the background. Unwatched roots (e.g. `/`) only
    /// pick up changes this way, on startup, or on a config change.
    pub fn rescan(&self) {
        for r in util::read(&self.roots).iter() {
            if let Err(e) = r.picker.trigger_full_rescan_async(&SharedFrecency::default()) {
                eprintln!("[files] rescan of {} failed: {e}", r.dir.display());
            }
        }
    }

    /// Indexed file count across all roots (directories excluded).
    #[cfg(test)]
    pub fn file_count(&self) -> usize {
        util::read(&self.roots)
            .iter()
            .filter_map(|r| r.picker.read().ok()?.as_ref().map(|p| p.live_file_count()))
            .sum()
    }

    /// Block until every root finished its current scan.
    #[cfg(test)]
    pub fn wait_for_scan(&self, timeout: Duration) -> bool {
        util::read(&self.roots).iter().all(|r| r.picker.wait_for_scan(timeout))
    }

    fn search(&self, query: &str, ignore: &[String]) -> Vec<Hit> {
        let parser = QueryParser::default();
        let parsed = parser.parse(query);
        let opts = FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit: CANDIDATE_CAP },
            ..Default::default()
        };

        let mut hits: Vec<Hit> = vec![];
        for root in util::read(&self.roots).iter() {
            let Ok(guard) = root.picker.read() else { continue };
            let Some(picker) = guard.as_ref() else { continue };
            let res = picker.fuzzy_search_mixed(&parsed, None, opts);
            for (item, score) in res.items.into_iter().zip(res.scores) {
                let (rel, abs, is_dir, size, modified) = match item {
                    MixedItemRef::File(f) => (
                        f.relative_path(picker),
                        f.absolute_path(picker, &root.dir),
                        false,
                        Some(f.size),
                        (f.modified > 0).then_some(f.modified),
                    ),
                    MixedItemRef::Dir(d) => (
                        d.relative_path(picker),
                        d.absolute_path(picker, &root.dir),
                        true,
                        None,
                        None,
                    ),
                };
                let rel = Path::new(&rel);
                if root.nested.iter().any(|n| abs.starts_with(n)) || is_ignored(rel, ignore) {
                    continue;
                }
                if let Some(hit) = make_hit(&abs, rel, is_dir, size, modified, score.total) {
                    hits.push(hit);
                }
            }
        }

        hits.sort_unstable_by(|a, b| b.score.cmp(&a.score));
        hits.truncate(CANDIDATE_CAP);
        hits
    }
}

pub type SharedFileIndex = Arc<FileIndex>;

/// Configured dirs → fff base paths, `~` expanded, missing dirs and duplicates
/// dropped. `/` fans out into its top-level directories (minus pseudo
/// filesystems and symlinks such as a merged-usr `/bin`), which also keeps fff
/// from refusing it as a filesystem root.
fn resolve_roots(cfg: &FilesConfig) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = vec![];
    let mut push = |dir: PathBuf, watch: bool| {
        if !out.iter().any(|(d, _)| *d == dir) {
            out.push((dir, watch));
        }
    };
    for entry in &cfg.dirs {
        let dir = crate::config::Config::expand_path(&entry.path);
        if dir.parent().is_none() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut children: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| {
                    e.file_name().to_str().is_some_and(|n| !PSEUDO_ROOT_DIRS.contains(&n))
                })
                .map(|e| e.path())
                .collect();
            children.sort();
            for c in children {
                push(c, entry.watch);
            }
        } else if dir.is_dir() {
            push(dir, entry.watch);
        }
    }
    out
}

/// True when any component of the root-relative path is a configured ignore
/// name. fff already prunes gitignored trees and common build/cache dirs during
/// its walk; this catches the user's extra names.
fn is_ignored(rel: &Path, ignore: &[String]) -> bool {
    !ignore.is_empty()
        && rel.components().any(|c| {
            matches!(c, Component::Normal(s) if s.to_str().is_some_and(|n| ignore.iter().any(|i| i == n)))
        })
}

/// Dot-prefixed component below the root. The root itself never counts: a
/// configured `~/.config/hypr` is an explicit choice.
fn has_hidden_component(rel: &Path) -> bool {
    rel.components()
        .any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with('.')))
}

fn make_hit(
    abs: &Path,
    rel: &Path,
    is_dir: bool,
    size: Option<u64>,
    modified: Option<u64>,
    score: i32,
) -> Option<Hit> {
    let name = abs.file_name()?.to_str()?.to_owned();
    let parent = abs.parent().and_then(|p| p.to_str()).unwrap_or("").to_owned();
    Some(Hit {
        path: abs.to_str()?.to_owned(),
        name,
        parent,
        is_dir,
        hidden: has_hidden_component(rel),
        size,
        modified,
        score,
    })
}

/// Extensions with a real preview renderer. MUST stay in sync with
/// `isFilePreviewable` / the ext maps in src/utils.ts.
fn is_previewable_ext(name: &str) -> bool {
    // extensionless "Dockerfile"/"Makefile" match via lowercased whole name
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(),
        "pdf"
        | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "tif"
        | "svg"
        | "csv" | "tsv"
        | "docx" | "pptx" | "odt" | "odp"
        | "xlsx" | "ods"
        | "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go"
        | "sh" | "bash" | "zsh" | "json" | "toml" | "ini" | "conf" | "cfg"
        | "env" | "yaml" | "yml" | "md" | "css" | "scss" | "less"
        | "html" | "htm" | "xml" | "vue" | "c" | "h" | "cpp" | "cc" | "cxx"
        | "hh" | "hpp" | "java" | "rb" | "kt" | "kts" | "sql" | "php" | "lua"
        | "swift" | "dockerfile" | "makefile" | "rst" | "log" | "txt"
    )
}

// ── Search provider ───────────────────────────────────────────────────────────

pub struct FileProvider {
    index: SharedFileIndex,
    shared: SharedConfig,
}

impl FileProvider {
    pub fn new(index: SharedFileIndex, shared: SharedConfig) -> Self {
        Self { index, shared }
    }
}

impl Provider for FileProvider {
    fn id(&self) -> &str {
        "files"
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let q = query.trim();
        if q.is_empty() {
            return vec![];
        }

        let cfg = util::read(&self.shared);
        let show_dotfiles = cfg.show_dotfiles;
        let log_scores = cfg.log_scores;
        let ignore = cfg.files_ignore.clone();
        drop(cfg);

        let mut hits = self.index.search(q, &ignore);
        if !show_dotfiles {
            hits.retain(|h| !h.hidden);
        }
        let Some(top) = hits.first().map(|h| h.score.max(1)) else {
            return vec![];
        };
        let floor = (top as f32 * RELATIVE_FLOOR) as i32;

        hits.into_iter()
            .filter(|h| h.score >= floor)
            .map(|h| {
                // fff owns relevance; the registry's fuzzy bonus is fed its
                // score relative to the best hit, on the nucleo scale the
                // ranking formula expects.
                let fuzzy = (h.score.max(0) as f32 / top as f32 * super::FUZZY_REFERENCE) as u32;
                if log_scores {
                    eprintln!("[files] {query:?} → {:?}  fff={} fuzzy={fuzzy}", h.path, h.score);
                }
                let mut intra = 0.0;
                // Folders sink below files within the band but always render a
                // listing → no preview penalty for dirs.
                if h.is_dir {
                    intra += FOLDER_OFFSET;
                } else if !is_previewable_ext(&h.name) {
                    intra -= super::PENALTY_NO_PREVIEW;
                }
                if h.hidden {
                    intra -= super::PENALTY_HIDDEN;
                }
                let mut parts = ranking::ScoreParts::new(
                    ranking::Category::File,
                    ranking::detect_tier(&h.name, q),
                    fuzzy,
                );
                parts.intra = intra;
                let escaped = h.path.replace('"', "\\\"");
                SearchResult {
                    id: format!("file:{}", h.path),
                    title: h.name,
                    subtitle: Some(h.parent),
                    kind: if h.is_dir { "folder" } else { "file" }.to_string(),
                    exec: Some(format!("xdg-open \"{}\"", escaped)),
                    file_size: h.size,
                    modified: h.modified,
                    parts: Some(parts),
                    ..Default::default()
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DirEntry, SharedSearchConfig};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portunus-files-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_for(roots: &[&Path], ignore: &[&str]) -> FilesConfig {
        FilesConfig {
            dirs: roots
                .iter()
                .map(|r| DirEntry { path: r.to_string_lossy().into_owned(), watch: false })
                .collect(),
            show_dotfiles: true,
            colored_icons: true,
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn provider(files_cfg: &FilesConfig) -> (FileProvider, SharedFileIndex) {
        let mut cfg = Config::default();
        cfg.files = files_cfg.clone();
        let shared: SharedConfig = Arc::new(RwLock::new(SharedSearchConfig::from_config(&cfg)));
        let index: SharedFileIndex = Arc::default();
        index.configure(files_cfg, None);
        assert!(index.wait_for_scan(Duration::from_secs(30)));
        (FileProvider::new(Arc::clone(&index), shared), index)
    }

    fn titles(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn finds_files_and_folders_with_metadata() {
        let root = tmpdir("basic");
        std::fs::create_dir_all(root.join("projects/quarterly")).unwrap();
        std::fs::write(root.join("projects/quarterly/report.pdf"), "x").unwrap();
        std::fs::write(root.join("café.md"), "abc").unwrap();

        let (p, index) = provider(&cfg_for(&[&root], &[]));
        assert_eq!(index.file_count(), 2);

        let r = p.search("report");
        assert_eq!(r[0].title, "report.pdf");
        assert_eq!(r[0].kind, "file");
        assert_eq!(
            r[0].subtitle.as_deref(),
            Some(root.join("projects/quarterly").to_str().unwrap())
        );
        assert_eq!(r[0].file_size, Some(1));

        let r = p.search("quarterly");
        assert!(r.iter().any(|r| r.title == "quarterly" && r.kind == "folder"), "{:?}", titles(&r));

        let r = p.search("café");
        assert_eq!(r[0].file_size, Some(3));
    }

    /// Typo resistance is the point of fff: a transposition still finds the file.
    #[test]
    fn tolerates_typos() {
        let root = tmpdir("typo");
        std::fs::write(root.join("invoice.pdf"), "x").unwrap();
        std::fs::write(root.join("unrelated.txt"), "x").unwrap();
        let (p, _) = provider(&cfg_for(&[&root], &[]));
        assert_eq!(p.search("invocie")[0].title, "invoice.pdf");
    }

    #[test]
    fn ignore_names_are_filtered() {
        let root = tmpdir("ignore");
        std::fs::create_dir_all(root.join("build-out/deep")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("build-out/deep/notes.md"), "x").unwrap();
        std::fs::write(root.join("src/notes.md"), "x").unwrap();

        let (p, _) = provider(&cfg_for(&[&root], &["build-out"]));
        let r = p.search("notes");
        assert!(r.iter().all(|r| !r.id.contains("build-out")), "ignored tree leaked: {:?}", r);
        assert!(r.iter().any(|r| r.id.ends_with("src/notes.md")));
    }

    /// Overlapping roots never list a path twice: the most specific root owns it.
    #[test]
    fn nested_roots_do_not_duplicate() {
        let root = tmpdir("nested");
        std::fs::create_dir_all(root.join("inner")).unwrap();
        std::fs::write(root.join("inner/unique-name.txt"), "x").unwrap();

        let inner = root.join("inner");
        let (p, _) = provider(&cfg_for(&[&root, &inner], &[]));
        let r = p.search("unique-name");
        assert_eq!(r.iter().filter(|r| r.title == "unique-name.txt").count(), 1);
    }

    /// Reconfiguring keeps unchanged roots and drops removed ones.
    #[test]
    fn configure_reconciles_roots() {
        let a = tmpdir("reconcile-a");
        let b = tmpdir("reconcile-b");
        std::fs::write(a.join("alpha.txt"), "x").unwrap();
        std::fs::write(b.join("bravo.txt"), "x").unwrap();

        let (p, index) = provider(&cfg_for(&[&a], &[]));
        assert!(p.search("bravo").iter().all(|r| r.title != "bravo.txt"));

        index.configure(&cfg_for(&[&b], &[]), None);
        assert!(index.wait_for_scan(Duration::from_secs(30)));
        assert!(p.search("bravo").iter().any(|r| r.title == "bravo.txt"));
        assert!(p.search("alpha").iter().all(|r| r.title != "alpha.txt"));

        index.clear();
        assert!(p.search("bravo").is_empty());
    }

    #[test]
    fn filesystem_root_fans_out_without_pseudo_filesystems() {
        let cfg = FilesConfig {
            dirs: vec![DirEntry { path: "/".into(), watch: false }],
            ..FilesConfig::default()
        };
        let roots = resolve_roots(&cfg);
        assert!(!roots.is_empty());
        assert!(roots.iter().all(|(d, _)| d.parent() == Some(Path::new("/"))));
        assert!(!roots.iter().any(|(d, _)| d == Path::new("/proc") || d == Path::new("/sys")));
    }
}
