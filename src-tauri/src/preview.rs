use crate::config::SharedConfig;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::Manager;

/// Reply for a render job: (jpeg bytes, total page count). A oneshot the command
/// `await`s directly, so no blocking-pool thread is parked for the render's duration.
type RenderReply = tokio::sync::oneshot::Sender<Result<(Vec<u8>, u32), String>>;
/// Reply for a rects job: normalized [x, y, w, h] boxes (top-left origin, 0..1).
type RectsReply = tokio::sync::oneshot::Sender<Result<Vec<[f32; 4]>, String>>;
/// Reply for a text-layer job: the page's selectable text geometry.
type TextLayerReply = tokio::sync::oneshot::Sender<Result<PdfTextLayer, String>>;

/// One selectable word of a PDF page, `rect` normalized 0..1 top-left (same
/// space as the highlight boxes, so the frontend overlays it at any zoom).
#[derive(serde::Serialize)]
pub struct PdfTextWord {
    pub text: String,
    pub rect: [f32; 4],
}

/// One line of a PDF page text layer: its bounding `rect` plus its words.
#[derive(serde::Serialize)]
pub struct PdfTextLine {
    pub rect: [f32; 4],
    pub words: Vec<PdfTextWord>,
}

/// A PDF page's text layer, for the frontend's invisible selectable overlay.
/// `page_w`/`page_h` are PDF points (aspect/font sizing); rects are normalized.
/// `truncated` marks a page whose extraction hit the char/word caps.
#[derive(serde::Serialize)]
pub struct PdfTextLayer {
    pub page_w: f32,
    pub page_h: f32,
    pub truncated: bool,
    pub lines: Vec<PdfTextLine>,
}

/// Work for the single pdfium-bound worker thread. pdfium is not `Send` and is
/// bound once per thread, so both rasterizing and text-layer queries share this
/// one queue rather than each spinning up a `Pdfium` (see `pdfium_available`).
enum PdfJob {
    /// (path, 0-based page index, target pixel width, reply).
    Render(String, u32, u32, RenderReply),
    /// (path, 0-based page index, search terms, reply).
    Rects(String, u32, Vec<String>, RectsReply),
    /// (path, 0-based page index, reply) — full selectable text layer.
    TextLayer(String, u32, TextLayerReply),
}

pub struct PdfWorkerHandle {
    tx: std::sync::mpsc::SyncSender<PdfJob>,
}

/// Default render width for the side preview. Quicklook requests larger widths
/// (high-DPI / zoom) so A4 pages stay sharp and readable.
const PDF_RENDER_WIDTH: u32 = 800;
/// Hard ceiling so a runaway zoom can't allocate a multi-hundred-MB bitmap.
const PDF_MAX_RENDER_WIDTH: u32 = 4000;

/// Binds to pdfium, preferring the bundled library (AppImage) and falling back
/// to the system library for a source build.
fn bind_pdfium() -> Result<pdfium_render::prelude::Pdfium, String> {
    use pdfium_render::prelude::*;
    let bindings = match crate::runtime_assets::pdfium_library() {
        Some(path) => Pdfium::bind_to_library(&path)
            .or_else(|_| Pdfium::bind_to_system_library()),
        None => Pdfium::bind_to_system_library(),
    }
    .map_err(|e| e.to_string())?;
    Ok(Pdfium::new(bindings))
}

/// Whether pdfium can be loaded. Used by `check_dependencies` to report
/// PDF-preview availability in Settings without waiting for the user to
/// actually preview a PDF (where the failure would otherwise first surface).
///
/// This only *binds* the library (loads its symbols); it must NOT construct a
/// `Pdfium`. `Pdfium::new` calls the global `FPDF_InitLibrary` and its `Drop`
/// calls `FPDF_DestroyLibrary`, which would tear down the library state out
/// from under the long-lived preview worker and deadlock pdfium.
pub fn pdfium_available() -> bool {
    use pdfium_render::prelude::*;
    if let Some(path) = crate::runtime_assets::pdfium_library() {
        if Pdfium::bind_to_library(&path).is_ok() {
            return true;
        }
    }
    Pdfium::bind_to_system_library().is_ok()
}

fn start_pdf_worker(shared: SharedConfig) -> PdfWorkerHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<PdfJob>(4);
    std::thread::spawn(move || {
        use image::codecs::jpeg::JpegEncoder;
        use pdfium_render::prelude::*;
        // Read the flag fresh each message so the Debug toggle takes effect live.
        let log_pdf = || shared.read().unwrap().log_pdf;
        let pdfium = bind_pdfium().map_err(|e| {
            eprintln!("[pdf] failed to bind pdfium: {e}");
            e
        });
        // Prime pdfium's font subsystem (fontconfig enumeration on Linux) by
        // rendering a tiny embedded PDF once. That work otherwise lands on the
        // user's first real preview, making it noticeably slower than the rest.
        // Failures are non-fatal: a missed warmup just restores the old behavior.
        if let Ok(pdfium) = &pdfium {
            const WARMUP_PDF: &[u8] = include_bytes!("warmup.pdf");
            let warmed = (|| -> Result<(), PdfiumError> {
                let doc = pdfium.load_pdf_from_byte_slice(WARMUP_PDF, None)?;
                doc.pages()
                    .get(0)?
                    .render_with_config(&PdfRenderConfig::new().set_target_width(800))?;
                Ok(())
            })();
            if log_pdf() {
                match warmed {
                    Ok(()) => eprintln!("[pdf] warmup done"),
                    Err(e) => eprintln!("[pdf] warmup failed: {e}"),
                }
            }
        }
        while let Ok(job) = rx.recv() {
            let log = log_pdf();
            match job {
                PdfJob::Render(path, page_idx, width, reply) => {
                    let result = match &pdfium {
                        Err(msg) => Err(msg.clone()),
                        Ok(pdfium) => (|| {
                            if log {
                                eprintln!("[pdf] rendering: {path} (page {page_idx}, width {width})");
                            }
                            let doc = pdfium.load_pdf_from_file(&path, None).map_err(|e| {
                                let msg = e.to_string();
                                if log {
                                    eprintln!("[pdf] load_pdf_from_file failed: {msg}");
                                }
                                msg
                            })?;
                            let page_count = doc.pages().len();
                            if log {
                                eprintln!("[pdf] loaded, {page_count} page(s)");
                            }
                            // Clamp the requested page into range (pdfium uses u16 indices).
                            let idx = page_idx.min(page_count.saturating_sub(1) as u32) as u16;
                            let page = doc.pages().get(idx).map_err(|e| {
                                let msg = e.to_string();
                                if log {
                                    eprintln!("[pdf] get page {idx} failed: {msg}");
                                }
                                msg
                            })?;
                            let total = page_count as u32;
                            let bitmap = page
                                .render_with_config(
                                    &PdfRenderConfig::new().set_target_width(width as i32),
                                )
                                .map_err(|e| {
                                    let msg = e.to_string();
                                    if log {
                                        eprintln!("[pdf] render failed: {msg}");
                                    }
                                    msg
                                })?;
                            // Quality 90 (vs the encoder default of 75): text edges stay
                            // crisp when the page is enlarged for reading in Quicklook.
                            // Drop to 82 for big zoomed renders: at that resolution the
                            // quality loss is invisible but it markedly cuts encode time
                            // and the bytes shipped across IPC, so zoom-in feels faster.
                            let quality = if width >= 2000 { 82 } else { 90 };
                            let mut bytes = Vec::new();
                            bitmap
                                .as_image()
                                .into_rgb8()
                                .write_with_encoder(JpegEncoder::new_with_quality(
                                    std::io::Cursor::new(&mut bytes),
                                    quality,
                                ))
                                .map_err(|e| {
                                    let msg = e.to_string();
                                    if log {
                                        eprintln!("[pdf] jpeg encode failed: {msg}");
                                    }
                                    msg
                                })?;
                            if log {
                                eprintln!("[pdf] done, {} bytes", bytes.len());
                            }
                            Ok((bytes, total))
                        })(),
                    };
                    let _ = reply.send(result);
                }
                PdfJob::Rects(path, page_idx, terms, reply) => {
                    let result = match &pdfium {
                        Err(msg) => Err(msg.clone()),
                        Ok(pdfium) => page_match_rects(pdfium, &path, page_idx, &terms, log),
                    };
                    let _ = reply.send(result);
                }
                PdfJob::TextLayer(path, page_idx, reply) => {
                    let result = match &pdfium {
                        Err(msg) => Err(msg.clone()),
                        Ok(pdfium) => page_text_layer(pdfium, &path, page_idx, log),
                    };
                    let _ = reply.send(result);
                }
            }
        }
    });
    PdfWorkerHandle { tx }
}

pub fn setup(app: &tauri::AppHandle, shared: SharedConfig) {
    app.manage(start_pdf_worker(shared));
    app.manage(start_image_ocr_worker());
}

// ── image OCR worker ──────────────────────────────────────────────────────────

use crate::content_index::OcrWord;

/// What an OCR job should produce. One serialized worker/queue/generation
/// counter serves highlight boxes (search), file text layers, and clipboard
/// text layers (Live Text), so they never OCR concurrently.
enum ImageOcrOp {
    /// Match `needles` against the image's OCR words → highlight rects.
    MatchRects { path: String, needles: Vec<String> },
    /// Full selectable text layer of an image file.
    TextLayerFile { path: String },
    /// Full selectable text layer of decoded image bytes (clipboard entry).
    TextLayerBytes { bytes: Vec<u8> },
}

enum ImageOcrOut {
    Rects(Vec<[f32; 4]>),
    Words(Vec<OcrWord>),
}

impl ImageOcrOp {
    /// Coalescing lane: highlight boxes and Live Text are distinct consumers
    /// that may fire for the SAME image at once, so they must not supersede each
    /// other. Each lane has its own generation counter.
    fn lane(&self) -> usize {
        match self {
            ImageOcrOp::MatchRects { .. } => 0,
            ImageOcrOp::TextLayerFile { .. } | ImageOcrOp::TextLayerBytes { .. } => 1,
        }
    }
}

struct ImageOcrJob {
    op: ImageOcrOp,
    lang: String,
    /// Coalescing lane (see `ImageOcrOp::lane`) and this request's generation
    /// within it; the worker skips a job superseded by a newer one in its lane.
    lane: usize,
    generation: u64,
    reply: tokio::sync::oneshot::Sender<Result<ImageOcrOut, String>>,
}

/// Single-thread OCR worker for on-demand image OCR. Tesseract is far heavier
/// than the PDF text-layer path, so it gets its own thread (off tokio's blocking
/// pool and off the indexer) and coalesces requests per lane: only the newest
/// pending generation in a lane actually OCRs, so arrow-keying through image
/// results doesn't queue an OCR per selection (paired with the frontend's
/// debounce). Highlight and Live Text use separate lanes so a preview needing
/// both doesn't cancel one with the other.
pub struct ImageOcrHandle {
    tx: std::sync::mpsc::SyncSender<ImageOcrJob>,
    generations: [Arc<AtomicU64>; 2],
}

fn start_image_ocr_worker() -> ImageOcrHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<ImageOcrJob>(8);
    let generations = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let worker_gens = generations.clone();
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            // Superseded by a newer request in the same lane: reply empty (the
            // awaiting frontend has already moved on) without paying for the OCR.
            if job.generation < worker_gens[job.lane].load(Ordering::Acquire) {
                let empty = match job.op {
                    ImageOcrOp::MatchRects { .. } => ImageOcrOut::Rects(Vec::new()),
                    _ => ImageOcrOut::Words(Vec::new()),
                };
                let _ = job.reply.send(Ok(empty));
                continue;
            }
            let result = match job.op {
                ImageOcrOp::MatchRects { path, needles } => {
                    crate::content_index::ocr_file_text_and_boxes(&path, &job.lang)
                        .map(|(_, words)| ImageOcrOut::Rects(match_word_boxes(&words, &needles)))
                }
                ImageOcrOp::TextLayerFile { path } => {
                    crate::content_index::ocr_file_text_and_boxes(&path, &job.lang)
                        .map(|(_, words)| ImageOcrOut::Words(words))
                }
                ImageOcrOp::TextLayerBytes { bytes } => {
                    crate::content_index::ocr_bytes_text_and_boxes(&bytes, &job.lang)
                        .map(|(_, words)| ImageOcrOut::Words(words))
                }
            };
            let _ = job.reply.send(result);
        }
    });
    ImageOcrHandle { tx, generations }
}

/// Enqueue an OCR job and await its reply, bumping the op's lane generation so
/// any still-queued older job in that lane is skipped. Highlight and Live Text
/// jobs use separate lanes, so they coalesce independently.
async fn run_ocr_job(ocr: &ImageOcrHandle, op: ImageOcrOp, lang: String) -> Result<ImageOcrOut, String> {
    let lane = op.lane();
    let generation = ocr.generations[lane].fetch_add(1, Ordering::AcqRel) + 1;
    let (reply, reply_rx) = tokio::sync::oneshot::channel::<Result<ImageOcrOut, String>>();
    let tx = ocr.tx.clone();
    let job = ImageOcrJob { op, lang, lane, generation, reply };
    tauri::async_runtime::spawn_blocking(move || tx.send(job).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())??;
    reply_rx.await.map_err(|e| e.to_string())?
}

/// Boxes of words whose content key matches a query key — the same
/// `porter unicode61` keying the index used (see `content_match`). Shared by the
/// cached and on-demand paths so they highlight identically. `keys` are query keys
/// from `normalize_terms`.
fn match_word_boxes(words: &[OcrWord], keys: &[String]) -> Vec<[f32; 4]> {
    // Stored OCR words are verbatim Tesseract tokens and can carry attached
    // punctuation ("report.", "(note)"), which would never stem - so tokenize each
    // and match any contained token. Set membership avoids an O(words*keys) scan.
    let set: std::collections::HashSet<&str> = keys.iter().map(String::as_str).collect();
    words
        .iter()
        .filter(|w| {
            crate::content_match::tokenize(&w.text)
                .iter()
                .any(|(_, t)| set.contains(crate::content_match::match_key(t).as_str()))
        })
        .map(|w| w.rect)
        .collect()
}

/// Normalized highlight rectangles for `terms` over an OCR'd image preview. Returns
/// empty unless `content.ocr_highlight` is on. When `content.ocr_highlight_cache` is
/// on and the image is indexed, boxes come straight from the DB (no OCR); otherwise
/// the image is OCR'd on demand via the serialized worker.
#[tauri::command]
pub async fn image_match_rects(
    path: String,
    terms: Vec<String>,
    config: tauri::State<'_, crate::ConfigState>,
    content: tauri::State<'_, crate::ContentState>,
    ocr: tauri::State<'_, ImageOcrHandle>,
) -> Result<Vec<[f32; 4]>, String> {
    let (enabled, cache, lang) = {
        let cfg = crate::util::lock(&config);
        (
            cfg.content.ocr_highlight,
            cfg.content.ocr_highlight_cache,
            cfg.content.ocr_language.clone(),
        )
    };
    if !enabled {
        return Ok(Vec::new());
    }
    let needles = normalize_terms(terms);
    if needles.is_empty() {
        return Ok(Vec::new());
    }

    // Cached fast path: boxes captured at index time, no per-preview OCR.
    if cache {
        // Clone the Arc out and drop the lock before any await.
        let idx = content
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .map(Arc::clone);
        if let Some(idx) = idx {
            let p = path.clone();
            let cached =
                tauri::async_runtime::spawn_blocking(move || idx.cached_word_boxes(&p))
                    .await
                    .map_err(|e| e.to_string())?;
            // Some(boxes) => indexed; trust the cache (even if empty). None => not
            // indexed yet, so fall through to on-demand OCR below.
            if let Some(words) = cached {
                return Ok(match_word_boxes(&words, &needles));
            }
        }
    }

    // On-demand OCR via the serialized worker (cache off, or a cache miss).
    match run_ocr_job(&ocr, ImageOcrOp::MatchRects { path, needles }, lang).await? {
        ImageOcrOut::Rects(rects) => Ok(rects),
        ImageOcrOut::Words(_) => Ok(Vec::new()),
    }
}

/// Largest image (megapixels) OCR'd for a Live Text layer. Tesseract time and
/// memory scale with pixel count; a huge scan would stall the single OCR worker.
const MAX_OCR_MEGAPIXELS: u64 = 40;

/// Selectable OCR text layer (Live Text) for an image preview: original-case
/// words with normalized boxes and line ordinals, from the DB cache when the
/// image is indexed with `ocr_highlight_cache` on, else on-demand OCR via the
/// serialized worker. Empty (not an error) when Tesseract finds no text; `Err`
/// only when OCR itself is unavailable.
#[tauri::command]
pub async fn image_text_layer(
    path: String,
    config: tauri::State<'_, crate::ConfigState>,
    content: tauri::State<'_, crate::ContentState>,
    ocr: tauri::State<'_, ImageOcrHandle>,
) -> Result<Vec<OcrWord>, String> {
    let (cache, lang) = {
        let cfg = crate::util::lock(&config);
        (cfg.content.ocr_highlight_cache, cfg.content.ocr_language.clone())
    };

    // Cached fast path: boxes captured at index time carry case + line ordinals
    // (schema v3), so they serve Live Text directly with no per-preview OCR.
    if cache {
        let idx = content
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .map(Arc::clone);
        if let Some(idx) = idx {
            let p = path.clone();
            let cached =
                tauri::async_runtime::spawn_blocking(move || idx.cached_word_boxes(&p))
                    .await
                    .map_err(|e| e.to_string())?;
            if let Some(words) = cached {
                return Ok(words);
            }
        }
    }

    // Reject oversized scans before handing the worker a job that would stall it.
    let p = path.clone();
    if let Ok(Some((w, h))) =
        tauri::async_runtime::spawn_blocking(move || image::image_dimensions(&p).ok()).await
    {
        if (w as u64) * (h as u64) > MAX_OCR_MEGAPIXELS * 1_000_000 {
            return Err("image too large to OCR".into());
        }
    }

    match run_ocr_job(&ocr, ImageOcrOp::TextLayerFile { path }, lang).await? {
        ImageOcrOut::Words(words) => Ok(words),
        ImageOcrOut::Rects(_) => Ok(Vec::new()),
    }
}

/// Run OCR over already-decoded image bytes (a clipboard entry) for its Live
/// Text layer, via the same serialized worker. Callers cap the byte size.
pub async fn ocr_bytes_text_layer(
    ocr: &ImageOcrHandle,
    bytes: Vec<u8>,
    lang: String,
) -> Result<Vec<OcrWord>, String> {
    match run_ocr_job(ocr, ImageOcrOp::TextLayerBytes { bytes }, lang).await? {
        ImageOcrOut::Words(words) => Ok(words),
        ImageOcrOut::Rects(_) => Ok(Vec::new()),
    }
}

#[tauri::command]
pub async fn render_pdf_page(
    path: String,
    page: Option<u32>,
    width: Option<u32>,
    worker: tauri::State<'_, PdfWorkerHandle>,
) -> Result<tauri::ipc::Response, String> {
    let width = width
        .unwrap_or(PDF_RENDER_WIDTH)
        .clamp(PDF_RENDER_WIDTH, PDF_MAX_RENDER_WIDTH);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<(Vec<u8>, u32), String>>();
    let tx = worker.tx.clone();
    // Enqueue on a blocking thread, then `await` the oneshot reply below. Blocking `send`
    // (not `try_send`): when the single pdfium worker is busy - a slow high-zoom render
    // plus the adjacent-page prefetches - the bounded queue fills, and `try_send` would
    // drop the job, leaving a stale page on screen after the counter advanced. Waiting for
    // a slot keeps them in sync. The enqueue is brief; the slow render is awaited via the
    // oneshot, so no blocking-pool thread is held for its duration (unlike `recv`).
    tauri::async_runtime::spawn_blocking(move || {
        tx.send(PdfJob::Render(path, page.unwrap_or(0), width, reply_tx))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let (bytes, count) = reply_rx.await.map_err(|e| e.to_string())??;
    // Raw bytes across IPC: a JSON number[] would ~5x the JPEG payload and dominate
    // render time at high zoom. Prepend the page count as a u32 LE header that the
    // frontend slices back off (see getPdfUrl).
    let mut buf = Vec::with_capacity(4 + bytes.len());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&bytes);
    Ok(tauri::ipc::Response::new(buf))
}

/// Returns normalized highlight rectangles for `terms` on one page of a PDF that
/// has a real text layer. Each rect is `[x, y, w, h]` in 0..1, top-left origin,
/// so the frontend can place boxes over the rendered page at any width/zoom.
/// Empty terms, no text layer, or no matches all yield an empty list.
#[tauri::command]
pub async fn pdf_match_rects(
    path: String,
    page: u32,
    terms: Vec<String>,
    worker: tauri::State<'_, PdfWorkerHandle>,
) -> Result<Vec<[f32; 4]>, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<Vec<[f32; 4]>, String>>();
    let tx = worker.tx.clone();
    // Blocking `send` for the same reason as render_pdf_page: don't drop the job when the
    // worker queue is momentarily full; wait for a slot. Reply awaited via the oneshot.
    tauri::async_runtime::spawn_blocking(move || {
        tx.send(PdfJob::Rects(path, page, terms, reply_tx))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    reply_rx.await.map_err(|e| e.to_string())?
}

/// Selectable text layer for one PDF page: word-grouped lines with normalized
/// boxes, for the frontend's invisible selection overlay. A page without a text
/// layer yields an empty layer (frontend renders nothing); a load error is `Err`.
#[tauri::command]
pub async fn pdf_text_layer(
    path: String,
    page: u32,
    worker: tauri::State<'_, PdfWorkerHandle>,
) -> Result<PdfTextLayer, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Result<PdfTextLayer, String>>();
    let tx = worker.tx.clone();
    // Blocking `send` for the same reason as render_pdf_page; reply awaited via oneshot.
    tauri::async_runtime::spawn_blocking(move || {
        tx.send(PdfJob::TextLayer(path, page, reply_tx))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    reply_rx.await.map_err(|e| e.to_string())?
}

/// Lowercases search terms, drops 1-char noise, and deduplicates, mirroring the
/// frontend's `deriveContentTerms` and the content provider's tokenization, so
/// preview matching lands on the same terms the index matched. Dedup matters for
/// repeated-word queries (e.g. "the on the the on the"): without it each duplicate
/// needle re-scans the page and stamps the same boxes again.
fn normalize_terms<I: IntoIterator<Item = String>>(terms: I) -> Vec<String> {
    // Drop 1-char noise, then key each term the way the content index tokenized it
    // (`porter unicode61`) so highlight / box / section matching agrees with FTS.
    // `query_keys` folds, stems, dedups, and drops empties.
    crate::content_match::query_keys(terms.into_iter().filter(|t| t.chars().count() >= 2))
}

/// Largest vertical gap (in points) between consecutive chars of one match before
/// it is split into separate boxes - so a line-wrapped match yields one box per
/// line instead of a single bar spanning the gap.
const LINE_BREAK_POINTS: f32 = 2.0;

/// Locates `terms` in the page's text layer (word-prefix, case-insensitive, to
/// mirror the frontend's `buildTermRegex`) and returns normalized boxes. Runs on
/// the pdfium worker thread; `pdfium` is the thread's bound instance.
fn page_match_rects(
    pdfium: &pdfium_render::prelude::Pdfium,
    path: &str,
    page_idx: u32,
    terms: &[String],
    log: bool,
) -> Result<Vec<[f32; 4]>, String> {
    use pdfium_render::prelude::*;

    let needles = normalize_terms(terms.iter().cloned());
    if needles.is_empty() {
        return Ok(Vec::new());
    }

    let doc = pdfium.load_pdf_from_file(path, None).map_err(|e| {
        let msg = e.to_string();
        if log {
            eprintln!("[pdf] rects: load failed: {msg}");
        }
        msg
    })?;
    let page_count = doc.pages().len();
    let idx = page_idx.min(page_count.saturating_sub(1) as u32) as u16;
    let page = doc.pages().get(idx).map_err(|e| e.to_string())?;
    let page_w = page.width().value;
    let page_h = page.height().value;
    if page_w <= 0.0 || page_h <= 0.0 {
        return Ok(Vec::new());
    }

    let text = match page.text() {
        Ok(t) => t,
        Err(e) => {
            if log {
                eprintln!("[pdf] rects: no text layer: {e}");
            }
            return Ok(Vec::new());
        }
    };

    // Char string + parallel bounds. Skip chars without a value or bounds so the
    // two arrays stay aligned with `lower`.
    let mut lower = String::new();
    let mut bounds: Vec<PdfRect> = Vec::new();
    for ch in text.chars().iter() {
        let (Some(c), Ok(b)) = (ch.unicode_char(), ch.loose_bounds()) else {
            continue;
        };
        for lc in c.to_lowercase() {
            lower.push(lc);
            bounds.push(b);
        }
    }
    if lower.is_empty() {
        return Ok(Vec::new());
    }
    let chars: Vec<char> = lower.chars().collect();

    // Hard cap on boxes per page: a stopword query ("the") matches nearly every
    // word, and rendering hundreds of overlay divs per keystroke is the dominant
    // highlight cost. Beyond this many the highlight is visual noise anyway, so we
    // stop scanning - and log it rather than silently truncating.
    const MAX_RECTS: usize = 400;

    // Walk whole words; a word is highlighted when its content key matches a query
    // key - the same `porter unicode61` keying the index used. `chars`/`bounds` are
    // index-aligned, so a word's char span [start, end) maps straight to its bounds.
    // Diacritic chars continue the word so pdfium's split accents (`caf´e` =
    // ...U+00B4, e) aren't broken at the accent; match_key strips them when keying.
    let is_word = |c: char| c.is_alphanumeric() || crate::content_match::is_diacritic(c);
    // Membership set so each word is one hash lookup, not an O(needles) scan.
    let key_set: std::collections::HashSet<&str> = needles.iter().map(String::as_str).collect();
    let mut rects: Vec<[f32; 4]> = Vec::new();
    let mut capped = false;
    let mut i = 0;
    while i < chars.len() {
        if !is_word(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if key_set.contains(crate::content_match::match_key(&word).as_str()) {
            if rects.len() >= MAX_RECTS {
                capped = true;
                break;
            }
            push_rects(&bounds[start..i], page_w, page_h, &mut rects);
        }
    }
    if log {
        eprintln!(
            "[pdf] rects: {} box(es) on page {idx}{}",
            rects.len(),
            if capped { " (capped)" } else { "" }
        );
    }
    Ok(rects)
}

/// Unions the char bounds of one match into boxes, splitting on vertical line
/// jumps, and pushes each as a normalized `[x, y, w, h]` (top-left origin).
fn push_rects(
    chars: &[pdfium_render::prelude::PdfRect],
    page_w: f32,
    page_h: f32,
    out: &mut Vec<[f32; 4]>,
) {
    let mut run_start = 0;
    for i in 0..chars.len() {
        let split = i > run_start
            && (chars[i].top().value - chars[run_start].top().value).abs() > LINE_BREAK_POINTS;
        if split {
            out.push(union_rect(&chars[run_start..i], page_w, page_h));
            run_start = i;
        }
    }
    if run_start < chars.len() {
        out.push(union_rect(&chars[run_start..], page_w, page_h));
    }
}

/// Bounding box of `chars` (pdfium points, bottom-left origin) as a normalized
/// top-left-origin `[x, y, w, h]`.
fn union_rect(
    chars: &[pdfium_render::prelude::PdfRect],
    page_w: f32,
    page_h: f32,
) -> [f32; 4] {
    let mut left = f32::MAX;
    let mut right = f32::MIN;
    let mut top = f32::MIN;
    let mut bottom = f32::MAX;
    for r in chars {
        left = left.min(r.left().value);
        right = right.max(r.right().value);
        top = top.max(r.top().value);
        bottom = bottom.min(r.bottom().value);
    }
    union_rect_points(left, right, top, bottom, page_w, page_h)
}

/// Normalize a point-space box (bottom-left origin, `top` > `bottom`) to a
/// top-left-origin `[x, y, w, h]` in 0..1. The pure core shared by `union_rect`
/// and the text-layer grouper (which has no pdfium types, so it can be tested).
fn union_rect_points(left: f32, right: f32, top: f32, bottom: f32, page_w: f32, page_h: f32) -> [f32; 4] {
    [
        (left / page_w).clamp(0.0, 1.0),
        ((page_h - top) / page_h).clamp(0.0, 1.0),
        ((right - left) / page_w).clamp(0.0, 1.0),
        ((top - bottom) / page_h).clamp(0.0, 1.0),
    ]
}

/// A single glyph's box in PDF point space (bottom-left origin, so `top` >
/// `bottom`). The pdfium-free intermediate the text-layer grouper works on.
struct CharBox {
    ch: char,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

/// Scan caps so a pathological page can't blow up the payload / grouping cost.
const MAX_TEXT_LAYER_CHARS: usize = 20_000;
const MAX_TEXT_LAYER_WORDS: usize = 4_000;

/// Groups a page's glyph boxes into word-grouped lines for the selectable text
/// layer. Pure (no pdfium types) so it is unit-tested directly. A new line
/// starts on a synthesized CR/LF or when a glyph's vertical center departs the
/// current line's center by more than ~0.6× the line height (so super/subscripts
/// and mixed font sizes stay on their line, but the next row breaks); within a
/// line, words break on whitespace or a horizontal gap wider than 0.25× the line
/// height. Split diacritics (pdfium emits `caf´e` as `...´, e`) stay attached.
fn group_text_layer(chars: &[CharBox], page_w: f32, page_h: f32, mut truncated: bool) -> PdfTextLayer {
    let mut lines: Vec<PdfTextLine> = Vec::new();
    let mut word_count = 0usize;

    // Accumulators for the current word and line.
    let mut cur_word: Vec<&CharBox> = Vec::new();
    let mut cur_line_words: Vec<PdfTextWord> = Vec::new();
    // Vertical center of the current line's first glyph — a stable reference the
    // line is measured against (updating it per glyph would let a line drift).
    let mut line_center: Option<f32> = None;
    let mut prev_right: Option<f32> = None; // right edge of the previous glyph
    let mut prev_line_height = 0.0f32;

    let flush_word = |cur_word: &mut Vec<&CharBox>,
                      cur_line_words: &mut Vec<PdfTextWord>,
                      word_count: &mut usize,
                      truncated: &mut bool| {
        if cur_word.is_empty() {
            return;
        }
        if *word_count >= MAX_TEXT_LAYER_WORDS {
            *truncated = true;
            cur_word.clear();
            return;
        }
        let text: String = cur_word.iter().map(|c| c.ch).collect();
        let mut left = f32::MAX;
        let mut right = f32::MIN;
        let mut top = f32::MIN;
        let mut bottom = f32::MAX;
        for c in cur_word.iter() {
            left = left.min(c.left);
            right = right.max(c.right);
            top = top.max(c.top);
            bottom = bottom.min(c.bottom);
        }
        cur_word.clear();
        if right <= left || top <= bottom {
            return;
        }
        cur_line_words.push(PdfTextWord {
            text,
            rect: union_rect_points(left, right, top, bottom, page_w, page_h),
        });
        *word_count += 1;
    };

    let flush_line = |cur_line_words: &mut Vec<PdfTextLine>, words: &mut Vec<PdfTextWord>| {
        if words.is_empty() {
            return;
        }
        let taken = std::mem::take(words);
        let mut left = f32::MAX;
        let mut top = f32::MAX;
        let mut right = f32::MIN;
        let mut bottom = f32::MIN;
        for w in &taken {
            left = left.min(w.rect[0]);
            top = top.min(w.rect[1]);
            right = right.max(w.rect[0] + w.rect[2]);
            bottom = bottom.max(w.rect[1] + w.rect[3]);
        }
        cur_line_words.push(PdfTextLine {
            rect: [left, top, right - left, bottom - top],
            words: taken,
        });
    };

    for c in chars {
        // Synthesized line breaks (pdfium emits CR/LF at line ends): end the line.
        if c.ch == '\n' || c.ch == '\r' {
            flush_word(&mut cur_word, &mut cur_line_words, &mut word_count, &mut truncated);
            flush_line(&mut lines, &mut cur_line_words);
            line_center = None;
            prev_right = None;
            continue;
        }
        let char_height = (c.top - c.bottom).max(0.0);
        let center = (c.top + c.bottom) * 0.5;
        // A vertical center departing the line's reference center by more than
        // ~0.6× the line height starts a new line. The threshold is a fraction of
        // the line height (not a fixed 2pt) so a superscript/subscript or a
        // larger inline glyph stays on its line while the next row still breaks.
        if let Some(ref_center) = line_center {
            let line_h = if prev_line_height > 0.0 { prev_line_height } else { char_height.max(1.0) };
            if (center - ref_center).abs() > 0.6 * line_h {
                flush_word(&mut cur_word, &mut cur_line_words, &mut word_count, &mut truncated);
                flush_line(&mut lines, &mut cur_line_words);
                line_center = None;
                prev_right = None;
            }
        }

        if c.ch.is_whitespace() {
            flush_word(&mut cur_word, &mut cur_line_words, &mut word_count, &mut truncated);
            prev_right = Some(c.right);
            continue;
        }

        // Horizontal gap without a space char also splits a word, unless this is
        // a combining diacritic continuing the previous glyph.
        let is_diacritic = crate::content_match::is_diacritic(c.ch);
        if !is_diacritic {
            if let Some(pr) = prev_right {
                let line_h = if prev_line_height > 0.0 { prev_line_height } else { char_height };
                if c.left - pr > 0.25 * line_h {
                    flush_word(&mut cur_word, &mut cur_line_words, &mut word_count, &mut truncated);
                }
            }
        }

        cur_word.push(c);
        if line_center.is_none() {
            line_center = Some(center);
        }
        prev_right = Some(c.right);
        if char_height > 0.0 {
            prev_line_height = char_height;
        }
    }
    flush_word(&mut cur_word, &mut cur_line_words, &mut word_count, &mut truncated);
    flush_line(&mut lines, &mut cur_line_words);

    PdfTextLayer { page_w, page_h, truncated, lines }
}

/// Extracts a page's selectable text layer. Runs on the pdfium worker thread;
/// `pdfium` is that thread's bound instance.
fn page_text_layer(
    pdfium: &pdfium_render::prelude::Pdfium,
    path: &str,
    page_idx: u32,
    log: bool,
) -> Result<PdfTextLayer, String> {
    let doc = pdfium.load_pdf_from_file(path, None).map_err(|e| {
        let msg = e.to_string();
        if log {
            eprintln!("[pdf] text-layer: load failed: {msg}");
        }
        msg
    })?;
    let page_count = doc.pages().len();
    let idx = page_idx.min(page_count.saturating_sub(1) as u32) as u16;
    let page = doc.pages().get(idx).map_err(|e| e.to_string())?;
    let page_w = page.width().value;
    let page_h = page.height().value;
    if page_w <= 0.0 || page_h <= 0.0 {
        return Ok(PdfTextLayer { page_w: 0.0, page_h: 0.0, truncated: false, lines: Vec::new() });
    }

    // No text layer (scanned page) → empty layer, not an error: the frontend
    // simply renders no selection overlay (Live Text handles scanned pages).
    let text = match page.text() {
        Ok(t) => t,
        Err(_) => return Ok(PdfTextLayer { page_w, page_h, truncated: false, lines: Vec::new() }),
    };

    let mut chars: Vec<CharBox> = Vec::new();
    let mut truncated = false;
    for ch in text.chars().iter() {
        if chars.len() >= MAX_TEXT_LAYER_CHARS {
            truncated = true;
            break;
        }
        let (Some(c), Ok(b)) = (ch.unicode_char(), ch.loose_bounds()) else {
            continue;
        };
        chars.push(CharBox {
            ch: c,
            left: b.left().value,
            right: b.right().value,
            top: b.top().value,
            bottom: b.bottom().value,
        });
    }
    let layer = group_text_layer(&chars, page_w, page_h, truncated);
    if log {
        eprintln!("[pdf] text-layer: {} line(s) on page {idx}", layer.lines.len());
    }
    Ok(layer)
}

/// One rendered section of an office document, as HTML for the preview iframe.
///
/// Blocking work off the runtime: the renderer inflates zip members, decodes and
/// rescales embedded images, and can walk a several-megabyte sheet part.
#[tauri::command]
pub async fn render_office_doc(
    path: String,
    section: Option<u32>,
    terms: Option<Vec<String>>,
) -> Result<crate::office::OfficeDoc, String> {
    // Raw terms, deliberately: unlike the text and image paths, the office
    // renderer keys them itself (`office::highlight::Terms::new`, which mirrors
    // `normalize_terms` including dropping 1-char noise). Keying here as well
    // stemmed every term *twice*, and Porter is not idempotent — "University"
    // keyed to "univers" and then to "univer", which matches no word in any
    // document, so the one term a reader most wants marked silently was not.
    let terms = terms.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || crate::office::render(&path, section, &terms))
        .await
        .map_err(|e| e.to_string())?
}

const PREVIEW_MAX_LINES: usize = 300;
const PREVIEW_MAX_BYTES: usize = 32 * 2048;

/// Bit `i` is set when a word on the line keys to query key `i` (`key_idx` maps
/// query key -> bit, capped at 64). Keying mirrors the index (`porter unicode61`),
/// so `running` matches a `run` query but `category` does not match `cat`. `memo`
/// caches word -> bit across lines so repeated words aren't re-stemmed.
fn line_term_mask(
    line: &str,
    key_idx: &std::collections::HashMap<&str, usize>,
    memo: &mut std::collections::HashMap<String, Option<usize>>,
) -> u64 {
    let mut mask = 0u64;
    for (_, w) in crate::content_match::tokenize(line) {
        let bit = match memo.get(w) {
            Some(&cached) => cached,
            None => {
                let key = crate::content_match::match_key(w);
                let found = key_idx.get(key.as_str()).copied();
                memo.insert(w.to_string(), found);
                found
            }
        };
        if let Some(i) = bit {
            mask |= 1 << i;
        }
    }
    mask
}

/// Joins `lines[start..]` into a preview, bounded by line and byte caps.
fn clip_lines(lines: &[String], start: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for line in lines.iter().skip(start).take(PREVIEW_MAX_LINES) {
        total += line.len() + 1;
        if total > PREVIEW_MAX_BYTES {
            break;
        }
        out.push(line.as_str());
    }
    out.join("\n")
}

#[tauri::command]
pub fn read_text_preview(path: String, terms: Option<Vec<String>>) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    // Upper bound on how far we scan for a match before giving up (content-indexed
    // files are size-capped by config, so this is a safety net for pathological files).
    const SCAN_LINES: usize = 50_000;

    let terms = normalize_terms(terms.unwrap_or_default());

    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let reader = BufReader::new(file);

    // No terms: keep the cheap streaming path - first window from the top.
    if terms.is_empty() {
        let mut lines: Vec<String> = Vec::new();
        let mut total = 0usize;
        for line in reader.lines().take(PREVIEW_MAX_LINES) {
            let line = line.map_err(|e| e.to_string())?;
            total += line.len() + 1;
            if total > PREVIEW_MAX_BYTES {
                break;
            }
            lines.push(line);
        }
        return Ok(lines.join("\n"));
    }

    // Terms present: read up to SCAN_LINES, then center the window on the earliest
    // section that covers the most *distinct* query terms - so a multi-part query
    // lands on the section holding all of them, not the first stray single match.
    const CLUSTER_LINES: usize = 10;
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines().take(SCAN_LINES) {
        lines.push(line.map_err(|e| e.to_string())?);
    }

    // Map each query key to its bit (cap 64), then mask each line; `memo` avoids
    // re-stemming repeated words across the scan.
    let key_idx: std::collections::HashMap<&str, usize> = terms
        .iter()
        .take(64)
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let mut memo: std::collections::HashMap<String, Option<usize>> =
        std::collections::HashMap::new();
    let masks: Vec<u64> = lines
        .iter()
        .map(|l| line_term_mask(l, &key_idx, &mut memo))
        .collect();

    // Slide a CLUSTER_LINES window; track distinct terms covered. `counts[i]` is how
    // many lines in the window carry term `i`; `distinct` is how many terms have a
    // nonzero count. Strict `>` keeps the earliest window on ties.
    let mut counts = [0u16; 64];
    let mut distinct = 0i32;
    let mut best_distinct = 0i32;
    let mut best_start = 0usize;
    for end in 0..masks.len() {
        let mut m = masks[end];
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            if counts[i] == 0 {
                distinct += 1;
            }
            counts[i] += 1;
            m &= m - 1;
        }
        if end >= CLUSTER_LINES {
            let mut out = masks[end - CLUSTER_LINES];
            while out != 0 {
                let i = out.trailing_zeros() as usize;
                counts[i] -= 1;
                if counts[i] == 0 {
                    distinct -= 1;
                }
                out &= out - 1;
            }
        }
        if distinct > best_distinct {
            best_distinct = distinct;
            best_start = end.saturating_sub(CLUSTER_LINES - 1);
        }
    }

    // No term found anywhere in the scan: fall back to the top of the file.
    let start = if best_distinct == 0 {
        0
    } else {
        (best_start + CLUSTER_LINES / 2).saturating_sub(PREVIEW_MAX_LINES / 2)
    };
    Ok(clip_lines(&lines, start))
}

#[tauri::command]
pub async fn render_image_preview(
    path: String,
    width: Option<u32>,
) -> Result<tauri::ipc::Response, String> {
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        use image::ImageFormat;
        // Default 800 for the side preview; Quicklook requests a larger width so an
        // enlarged image stays crisp. Clamped to keep memory bounded.
        let max_width = width.unwrap_or(800).clamp(200, 2400);
        let img = image::open(&path).map_err(|e| e.to_string())?;
        let img = if img.width() > max_width {
            img.thumbnail(max_width, u32::MAX)
        } else {
            img
        };
        let mut bytes = Vec::new();
        img.into_rgb8()
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?;
        Ok::<Vec<u8>, String>(bytes)
    })
    .await
    .map_err(|e| e.to_string())??;
    // Raw bytes across IPC, not a JSON number[] (see render_pdf_page).
    Ok(tauri::ipc::Response::new(bytes))
}

#[derive(serde::Serialize)]
pub struct FolderEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

#[tauri::command]
pub fn list_folder(path: String) -> Vec<FolderEntry> {
    const MAX: usize = 200;
    let mut entries: Vec<FolderEntry> = std::fs::read_dir(&path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .take(MAX)
        .map(|e| {
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            FolderEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir,
                size: meta.and_then(|m| if m.is_file() { Some(m.len()) } else { None }),
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

#[cfg(test)]
mod text_layer_tests {
    use super::*;

    // One glyph at [left,right] on a text baseline of height 10 (top-left in
    // point space is top>bottom). Page is 100×100 points for easy normalizing.
    fn ch(c: char, left: f32, right: f32, top: f32) -> CharBox {
        CharBox { ch: c, left, right, top, bottom: top - 10.0 }
    }

    fn text_of(layer: &PdfTextLayer) -> Vec<Vec<String>> {
        layer
            .lines
            .iter()
            .map(|l| l.words.iter().map(|w| w.text.clone()).collect())
            .collect()
    }

    #[test]
    fn splits_words_on_space_char() {
        let chars = vec![
            ch('h', 0.0, 10.0, 90.0),
            ch('i', 10.0, 20.0, 90.0),
            ch(' ', 20.0, 25.0, 90.0),
            ch('y', 25.0, 35.0, 90.0),
            ch('o', 35.0, 45.0, 90.0),
        ];
        let layer = group_text_layer(&chars, 100.0, 100.0, false);
        assert_eq!(text_of(&layer), vec![vec!["hi".to_string(), "yo".to_string()]]);
    }

    #[test]
    fn splits_words_on_horizontal_gap_without_space() {
        // No space glyph, but a wide gap (> 0.25 * line height=10 => 2.5pt).
        let chars = vec![
            ch('a', 0.0, 10.0, 90.0),
            ch('b', 10.0, 20.0, 90.0),
            ch('c', 60.0, 70.0, 90.0), // 40pt gap
        ];
        let layer = group_text_layer(&chars, 100.0, 100.0, false);
        assert_eq!(text_of(&layer), vec![vec!["ab".to_string(), "c".to_string()]]);
    }

    #[test]
    fn splits_lines_on_vertical_jump() {
        let chars = vec![
            ch('a', 0.0, 10.0, 90.0),
            ch('b', 0.0, 10.0, 50.0), // dropped a line (> LINE_BREAK_POINTS)
        ];
        let layer = group_text_layer(&chars, 100.0, 100.0, false);
        assert_eq!(text_of(&layer), vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }

    #[test]
    fn splits_lines_on_synthesized_newline() {
        let chars = vec![
            ch('a', 0.0, 10.0, 90.0),
            ch('\n', 10.0, 10.0, 90.0),
            ch('b', 0.0, 10.0, 90.0),
        ];
        let layer = group_text_layer(&chars, 100.0, 100.0, false);
        assert_eq!(text_of(&layer), vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }

    #[test]
    fn diacritic_continues_word() {
        // pdfium emits "café" as c a f <combining acute> e; the accent must not
        // split the word even though it may sit at a small horizontal gap.
        let chars = vec![
            ch('c', 0.0, 10.0, 90.0),
            ch('a', 10.0, 20.0, 90.0),
            ch('f', 20.0, 30.0, 90.0),
            ch('\u{0301}', 30.0, 30.0, 90.0),
            ch('e', 30.0, 40.0, 90.0),
        ];
        let layer = group_text_layer(&chars, 100.0, 100.0, false);
        assert_eq!(text_of(&layer), vec![vec!["caf\u{0301}e".to_string()]]);
    }

    #[test]
    fn empty_input_yields_empty_layer() {
        let layer = group_text_layer(&[], 100.0, 100.0, false);
        assert!(layer.lines.is_empty());
        assert!(!layer.truncated);
    }

    #[test]
    fn word_cap_marks_truncated() {
        let mut chars = Vec::new();
        for i in 0..(MAX_TEXT_LAYER_WORDS + 50) {
            let x = (i % 10) as f32 * 5.0;
            let top = 90.0 - (i / 10) as f32 * 20.0;
            chars.push(ch('w', x, x + 3.0, top));
            chars.push(ch(' ', x + 3.0, x + 5.0, top));
        }
        let layer = group_text_layer(&chars, 1000.0, 10000.0, false);
        assert!(layer.truncated);
    }
}
