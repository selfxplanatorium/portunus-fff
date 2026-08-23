mod app_update;
mod cli;
mod cli_ext;
mod clipboard_ocr;
mod config;
mod content_index;
mod de_setup;
mod extensions;
mod focus;
mod frecency;
mod ipc;
mod keybinds;
mod layer_shell;
mod native_host;
mod paths;
mod preview;
mod provider_reload;
mod providers;
mod runtime_assets;
mod util;
mod watcher;

// Both live in their own crates now; the aliases keep every `crate::office::…` and
// `crate::content_match::…` call site unchanged.
pub(crate) use portunus_office as office;
pub(crate) use portunus_text as content_match;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tauri::{Emitter, Manager};

static APPS_READY: AtomicBool = AtomicBool::new(false);

pub(crate) type Registry = Arc<RwLock<providers::PluginRegistry>>;
pub(crate) type FrecencyState = Option<Arc<frecency::FrecencyStore>>;
pub(crate) type ContentWatcherTx =
    Arc<Mutex<Option<std::sync::mpsc::Sender<config::ContentConfig>>>>;
pub(crate) type FileWatcherTx =
    Arc<Mutex<Option<std::sync::mpsc::Sender<config::FilesConfig>>>>;
pub(crate) type SharedFileEntries = Arc<RwLock<Vec<providers::files::FileEntry>>>;
pub(crate) type ConfigState = Arc<Mutex<config::Config>>;
pub(crate) type ContentState = Arc<Mutex<Option<Arc<content_index::ContentIndex>>>>;
pub(crate) type ClipboardOcrState = Option<Arc<clipboard_ocr::ClipboardOcrStore>>;

/// The `bool` is `full`: `true` clears the index before rebuilding (needed only
/// when OCR settings change, which invalidates already-cached text); `false`
/// runs a cache-preserving incremental pass (dir/extension/depth/size edits).
pub struct TriggerFullReindexFn(pub Arc<dyn Fn(bool) + Send + Sync>);

/// Clone of the config watcher's diff baseline (`last_cfg`). The Apply path
/// advances its `content` so the file-watcher, which fires on the same save,
/// sees no content diff and skips a redundant second reindex. See
/// `trigger_full_reindex`.
pub struct WatcherBaseline(pub Arc<Mutex<config::Config>>);

#[derive(serde::Serialize, Clone)]
struct ContentIndexProgress {
    indexed: usize,
    total: usize,
}

#[derive(serde::Serialize)]
struct SearchResponse {
    query_id: u64,
    /// Sync-tier results (built-ins + extension `search` exports).
    results: Vec<providers::SearchResult>,
    /// Extensions whose async `query` export was started for this keystroke;
    /// their batches arrive later as `search-stream` events.
    pending: Vec<extensions::query::PendingExt>,
}

#[tauri::command]
fn search(
    query: String,
    query_id: u64,
    scope: Option<String>,
    scope_data: Option<String>,
    registry: tauri::State<'_, Registry>,
) -> SearchResponse {
    let t = util::profile_search().then(std::time::Instant::now);
    let reg = registry.read().unwrap_or_else(|e| e.into_inner());
    let (results, pending) = match scope.as_deref() {
        // A Scope command is active: route the whole query to its owner. No
        // root fan-out, no command entries. Extension scopes also dispatch
        // the async `query` tier (streaming), scoped to that one extension.
        // `scope_data` is the active scope frame's opaque blob (from a
        // PushScope), forwarded to both tiers.
        Some(scope_id) => {
            let pending = scope_id
                .strip_prefix("ext:")
                .and_then(|rest| rest.split_once(":cmd:"))
                .and_then(|(ext_name, cmd)| {
                    extensions::query::manager().map(|qm| {
                        qm.dispatch_scoped(query_id, ext_name, cmd, &query, scope_data.clone(), &reg)
                    })
                })
                .unwrap_or_default();
            (reg.search_scope(scope_id, &query, scope_data), pending)
        }
        // Root search. Async tier first: cancels stale workers and spawns new
        // ones, so their wall-clock overlaps the sync fan-out.
        None => {
            let pending = extensions::query::manager()
                .map(|qm| qm.dispatch(query_id, &query, &reg))
                .unwrap_or_default();
            (reg.search(&query), pending)
        }
    };
    if let Some(t) = t {
        eprintln!(
            "[profile] search cmd took={:.2}ms results={} pending={} scope={scope:?} q={query:?}",
            t.elapsed().as_secs_f64() * 1000.0,
            results.len(),
            pending.len(),
        );
    }
    SearchResponse { query_id, results, pending }
}

/// The full command catalog, for the frontend's command cache (mode entry via
/// Tab/flags needs descriptors without a search round-trip).
#[tauri::command]
fn list_commands(registry: tauri::State<'_, Registry>) -> Vec<providers::CommandDescriptor> {
    registry.read().unwrap_or_else(|e| e.into_inner()).commands()
}

/// Installed icon-theme names, for the Appearance section's theme picker.
#[tauri::command]
fn list_icon_themes() -> Vec<String> {
    providers::icon_theme::installed_themes()
}

/// Records frecency for an invoked command entry so frequently-used commands
/// rank higher in root search. Fired by the frontend on enter/run.
#[tauri::command]
fn command_used(id: String, frecency: tauri::State<'_, FrecencyState>) {
    if let Some(store) = frecency.as_ref() {
        store.record_launch(&id, "command");
    }
}

/// Staged config the ranking playground scores against - the Settings window
/// autosaves on a debounce, so disk config lags the knobs; explain must use
/// what the user sees, not what has landed.
#[derive(serde::Deserialize)]
struct ExplainOverrides {
    ranking: config::RankingConfig,
    frecency_enabled: bool,
}

#[derive(serde::Serialize)]
struct ExplainResponse {
    results: Vec<providers::SearchResult>,
    /// Extensions that would have run on this query; the playground never
    /// executes wasm, so their results are absent by design.
    skipped_extensions: Vec<String>,
}

/// Playground search: full sync pipeline with per-result score breakdowns,
/// no extension execution, no `search-stream`, no frecency recording.
#[tauri::command]
fn search_explain(
    query: String,
    overrides: Option<ExplainOverrides>,
    registry: tauri::State<'_, Registry>,
) -> ExplainResponse {
    let reg = registry.read().unwrap_or_else(|e| e.into_inner());
    let weights = match overrides {
        Some(o) => providers::ranking::RankingWeights::from_config(&o.ranking, o.frecency_enabled),
        None => reg.ranking_weights(),
    };
    let outcome = reg.search_inner(&query, &weights, true, true);
    ExplainResponse {
        results: outcome.results,
        skipped_extensions: outcome.skipped_extensions,
    }
}

/// Result snapshot stored with a pin so the Settings list can render pins
/// whose result no longer exists.
#[derive(serde::Deserialize)]
struct PinSnapshot {
    id: String,
    kind: String,
    title: String,
    subtitle: Option<String>,
}

#[tauri::command]
fn pin_result(
    query: String,
    result: PinSnapshot,
    frecency: tauri::State<'_, FrecencyState>,
) -> Result<(), String> {
    let store = frecency.inner().as_ref().ok_or("pins unavailable (frecency DB failed to open)")?;
    store
        .pin(&query, &result.id, &result.kind, &result.title, result.subtitle.as_deref())
        .map_err(|e| e.to_string())
}

/// Unpins `result_id`. `exact` (Settings list) removes only the pin stored
/// for exactly `query`; otherwise (launcher toggle) every pin currently
/// boosting the result for the typed query is removed.
#[tauri::command]
fn unpin_result(
    query: String,
    result_id: String,
    exact: Option<bool>,
    frecency: tauri::State<'_, FrecencyState>,
) -> Result<(), String> {
    let store = frecency.inner().as_ref().ok_or("pins unavailable (frecency DB failed to open)")?;
    if exact.unwrap_or(false) {
        store.unpin(&query, &result_id).map_err(|e| e.to_string())
    } else {
        store.unpin_matching(&query, &result_id).map_err(|e| e.to_string())
    }
}

#[derive(serde::Serialize)]
struct PinEntry {
    #[serde(flatten)]
    row: frecency::PinRow,
    /// Best-effort: the pinned result no longer resolves (file deleted,
    /// extension uninstalled, command gone). Apps can't be probed cheaply and
    /// report false.
    stale: bool,
}

#[tauri::command]
fn list_pins(
    registry: tauri::State<'_, Registry>,
    frecency: tauri::State<'_, FrecencyState>,
) -> Vec<PinEntry> {
    let Some(store) = frecency.as_ref() else { return Vec::new() };
    let reg = registry.read().unwrap_or_else(|e| e.into_inner());
    let commands = reg.commands();
    store
        .list_pins()
        .into_iter()
        .map(|row| {
            let stale = if let Some(path) = row.result_id.strip_prefix("file:") {
                !std::path::Path::new(path).exists()
            } else if let Some(rest) = row.result_id.strip_prefix("ext:") {
                match rest.split_once(':') {
                    Some((name, _)) => reg.extension(name).is_none(),
                    None => false,
                }
            } else if row.result_id.starts_with("cmd:") {
                !commands.iter().any(|c| c.id == row.result_id)
            } else {
                false
            };
            PinEntry { row, stale }
        })
        .collect()
}

/// Cancels all in-flight async extension queries (query cleared, window hidden).
#[tauri::command]
fn cancel_search() {
    if let Some(qm) = extensions::query::manager() {
        qm.cancel_all();
    }
}

/// Best-matching 0-based section for `query` - a PDF page, or an office
/// document's sheet or slide - computed on demand for the single file being
/// previewed. Split out of `search_content` so the per-file rescan runs once for
/// the previewed file rather than for every result on each keystroke (the old
/// behaviour dominated common-word content-search latency). Returns None when the
/// index is unavailable/contended or nothing matched - the preview then just opens
/// at section 0. The blocking FTS work runs off the main thread.
#[tauri::command]
async fn content_match_section(
    path: String,
    query: String,
    state: tauri::State<'_, ContentState>,
) -> Result<Option<u32>, ()> {
    // try_lock so a long-running reindex (which holds this lock) never stalls the
    // preview; fall back to a fresh read-only open of the on-disk DB.
    let idx = state.try_lock().ok().and_then(|g| g.as_ref().map(Arc::clone));
    let idx = match idx {
        Some(i) => i,
        None => match content_index::ContentIndex::open() {
            Ok(i) => Arc::new(i),
            Err(_) => return Ok(None),
        },
    };
    // Reuse ContentProvider's query parsing (dedup + stopword stripping) so the
    // matched section is chosen from the same selective terms the result list
    // ranked on - not skewed toward a stopword that appears in every section.
    let Some(parsed) = content_index::parse_content_query(&query) else {
        return Ok(None);
    };
    let fts_query = parsed.fts_match();
    Ok(tauri::async_runtime::spawn_blocking(move || idx.best_section(&path, &fts_query))
        .await
        .ok()
        .flatten())
}

/// Keys each word the way the content index tokenized it (`porter unicode61`):
/// `normalize` + Porter stem. The frontend highlighter calls this so its
/// matching agrees exactly with the index - no Porter re-implementation in JS.
/// Index-preserving: `out[i]` is the key for `words[i]`.
#[tauri::command]
fn content_match_keys(words: Vec<String>) -> Vec<String> {
    words.iter().map(|w| content_match::match_key(w)).collect()
}

/// Evaluate one calculator expression for the selection popover's math chip.
/// Scoped (no provider fan-out, no query-id plumbing); `None` when the text
/// isn't calculable or the calc provider is disabled. Sync is fine: fend
/// carries a 50 ms interrupt and `passes_gate` rejects prose cheaply.
#[tauri::command]
fn calc_eval(expr: String, config: tauri::State<ConfigState>) -> Option<String> {
    let (calc_cfg, enabled) = {
        let cfg = util::lock(&config);
        (cfg.calc.clone(), cfg.providers.calc)
    };
    if !enabled {
        return None;
    }
    providers::calc::eval_expression(&expr, &providers::calc::currency::shared(), calc_cfg.currency)
}

#[derive(serde::Serialize, Clone)]
struct DepStatus {
    /// Stable identifier the frontend can match against (e.g. "dict").
    id: &'static str,
    /// The underlying tool name shown to the user.
    label: &'static str,
    /// The feature that stops working when this dependency is missing.
    feature: &'static str,
    available: bool,
    /// Package the user should install to get this dependency.
    install_hint: &'static str,
}

/// Reports the runtime availability of optional system dependencies so the
/// Settings UI can warn the user instead of silently degrading. Each probe is
/// cheap (PATH lookup), except pdfium which attempts a library bind.
#[tauri::command]
fn check_dependencies() -> Vec<DepStatus> {
    use util::binary_in_path;
    vec![
        DepStatus {
            id: "cliphist",
            label: "cliphist",
            feature: "Clipboard history",
            available: binary_in_path("cliphist"),
            install_hint: "cliphist",
        },
        DepStatus {
            id: "wl-copy",
            label: "wl-copy",
            feature: "Clipboard paste",
            available: binary_in_path("wl-copy"),
            install_hint: "wl-clipboard",
        },
        DepStatus {
            id: "wtype",
            label: "wtype",
            feature: "Smart paste (auto Ctrl+V)",
            available: binary_in_path("wtype"),
            install_hint: "wtype",
        },
        DepStatus {
            id: "dict",
            label: "dict",
            feature: "Dictionary lookups",
            available: binary_in_path("dict"),
            install_hint: "dictd",
        },
        DepStatus {
            id: "poppler",
            label: "pdftotext",
            feature: "PDF content indexing",
            available: runtime_assets::poppler_bundled() || binary_in_path("pdftotext"),
            install_hint: "poppler",
        },
        DepStatus {
            id: "tesseract",
            label: "tesseract",
            feature: "OCR (images & scanned PDFs)",
            // OCR is linked in via leptess; it needs language data, which is
            // bundled in packaged builds or provided by a system tesseract.
            available: runtime_assets::tessdata_path("eng").is_some() || binary_in_path("tesseract"),
            install_hint: "tesseract + tesseract-data-eng",
        },
        DepStatus {
            id: "pdfium",
            label: "libpdfium",
            feature: "PDF preview",
            available: preview::pdfium_available(),
            install_hint: "pdfium (e.g. pdfium-bin)",
        },
    ]
}

fn split_exec(exec: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in exec.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Runs a result's launch action. App results carry `desktop_file` and go
/// through GIO, which implements the Desktop Entry spec properly (field codes,
/// `Terminal=`, D-Bus activation) - see [`util::launch_desktop_entry`]. Every
/// other caller (file opens, clipboard URLs, preview links) passes an `exec`
/// command line, which is tokenized here and spawned directly; that path is
/// also the fallback when GIO refuses an entry.
#[tauri::command]
fn launch_app(
    app: tauri::AppHandle,
    exec: String,
    id: Option<String>,
    kind: Option<String>,
    desktop_file: Option<String>,
    frecency: tauri::State<'_, FrecencyState>,
) {
    if let (Some(id), Some(kind), Some(store)) = (&id, &kind, frecency.as_ref()) {
        store.record_launch(id, kind);
    }

    let launched = match desktop_file.as_deref() {
        Some(path) => match util::launch_desktop_entry(path) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[launch] gio launch failed for {path}: {e} - falling back to Exec");
                false
            }
        },
        None => false,
    };

    if !launched {
        // Field codes only reach here on the fallback path; GIO expands them
        // itself. Anything else (`xdg-open "…"`) has none to strip.
        let args: Vec<String> = split_exec(&exec)
            .into_iter()
            .filter(|s| !(s.len() == 2 && s.starts_with('%')))
            .collect();

        if let Some((program, rest)) = args.split_first() {
            let _ = util::spawn_detached(program, rest);
        }
    }

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

/// Reveal a file in the system file manager with the file itself selected/focused.
/// Uses the freedesktop `org.freedesktop.FileManager1.ShowItems` D-Bus method, which
/// is honored by Nautilus, Dolphin, Nemo, Thunar, etc. Falls back to opening the
/// parent directory with `xdg-open` if the D-Bus call cannot be made.
#[tauri::command]
fn reveal_file(app: tauri::AppHandle, path: String) {
    let uri = format!("file://{}", encode_path_uri(&path));

    let dbus = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let ok = matches!(dbus, Ok(status) if status.success());

    if !ok {
        // Fallback: open the parent directory (no file selection).
        let parent = std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        let _ = util::spawn_detached("xdg-open", &[parent]);
    }

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

/// Percent-encode a filesystem path for use in a `file://` URI. Encodes everything
/// outside the RFC 3986 unreserved set plus `/`, which stays as the path separator.
fn encode_path_uri(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tauri::command]
fn is_apps_ready() -> bool {
    APPS_READY.load(Ordering::Acquire)
}

/// Hide the launcher plus the cleanup that goes with it. Shared by the frontend
/// `hide_window` command and the `--close`/`--toggle` socket commands so the
/// teardown can't drift between the two entry points.
pub fn hide_launcher(app: &tauri::AppHandle) {
    // A hidden launcher has no use for in-flight async extension queries.
    if let Some(qm) = extensions::query::manager() {
        qm.cancel_all();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn hide_window(app: tauri::AppHandle) {
    hide_launcher(&app);
}

/// Surface the launcher from the frontend. Used on first launch: the window is
/// hidden at startup, so the onboarding wizard reveals it itself once it has
/// rendered (showing it from `setup()` would flash an unpainted window).
#[tauri::command]
fn show_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        focus::show_and_focus(&window);
    }
}

#[tauri::command]
fn get_config(state: tauri::State<ConfigState>) -> config::Config {
    util::lock(&state).clone()
}

/// Read the external matugen theme CSS (`<config_dir>/matugen.css`) for the
/// `matugen` theme. Returns `None` if the file is absent/unreadable, in which
/// case the frontend leaves the `[data-theme="matugen"]` selector unmatched and
/// vars fall back to the App.css `:root` defaults.
#[tauri::command]
fn get_custom_theme_css() -> Option<String> {
    std::fs::read_to_string(config::config_dir().join("matugen.css")).ok()
}

#[tauri::command]
fn save_config(config: config::Config, state: tauri::State<ConfigState>) -> Result<(), String> {
    let result = config.save();
    if result.is_ok() {
        *util::lock(&state) = config;
    }
    result
}

#[tauri::command]
fn open_settings_window(app: tauri::AppHandle, section: Option<String>) {
    // Hide the launcher so it doesn't sit on top of (or steal layer-shell
    // keyboard focus from) the settings window.
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    if let Some(win) = app.get_webview_window("settings") {
        focus::show_and_focus(&win);
        if let Some(s) = section {
            let _ = win.emit("navigate-to-section", s);
        }
    }
}

#[tauri::command]
fn trigger_full_reindex(
    full: bool,
    reindex: tauri::State<'_, TriggerFullReindexFn>,
    config: tauri::State<'_, ConfigState>,
    baseline: tauri::State<'_, WatcherBaseline>,
) {
    // The caller (Settings "Apply") just persisted the config via save_config, which
    // also wakes the config-file watcher → provider_reload. Advance the watcher's
    // diff baseline to the freshly-saved content *now* (synchronously, before the
    // 500ms-debounced watcher event can fire) so it sees no content diff and skips
    // a redundant second reindex. This explicit trigger is then the sole content
    // indexer. Only `.content` is touched, so files/apps diffs still apply.
    {
        let current = util::lock(&config).content.clone();
        util::lock(&baseline.0).content = current;
    }
    let f = Arc::clone(&reindex.0);
    std::thread::spawn(move || f(full));
}

/// Pre-index estimate for a single directory: exact file counts plus a coarse
/// min/max time range. The OCR/size/threads flags are passed in from the live
/// (possibly still-staged) settings UI rather than read from on-disk config, so
/// the estimate reflects unapplied toggles too.
#[tauri::command]
fn estimate_dir_index(
    path: String,
    depth: usize,
    extensions: Option<Vec<String>>,
    max_file_bytes: u64,
    ocr_images: bool,
    ocr_pdf_fallback: bool,
    threads: usize,
) -> content_index::DirEstimate {
    let cfg = config::ContentConfig {
        max_file_bytes,
        ocr_images,
        ocr_pdf_fallback,
        threads,
        ..Default::default()
    };
    content_index::estimate_dir(&path, depth, extensions.as_ref(), &cfg)
}

/// Reports whether the content index currently holds no documents.
/// Used by the settings UI to decide whether enabling content search is a
/// (potentially slow) first-time build that needs confirmation, or a cheap
/// incremental run over an already-populated index.
#[tauri::command]
fn is_content_index_empty(state: tauri::State<'_, ContentState>) -> bool {
    // try_lock so we never block the UI behind an in-progress reindex (which
    // holds this lock for its full duration). A live index is only contended
    // while content is enabled - exactly the case where the answer is moot.
    if let Ok(guard) = state.try_lock() {
        if let Some(idx) = guard.as_ref() {
            return idx.is_empty();
        }
    }
    // No open index (content disabled) - count rows in the on-disk DB directly.
    // `open` is resilient: a corrupt DB is recreated empty rather than surfacing an
    // error, so a `true` here means genuinely empty (the first-build prompt is then
    // accurate). A residual Err is a hard I/O/permission failure with no rebuild
    // recovery, so we log it loudly instead of silently pretending the index is empty.
    match content_index::ContentIndex::open() {
        Ok(idx) => idx.is_empty(),
        Err(e) => {
            eprintln!("[content] is_content_index_empty: failed to open index: {e}");
            true
        }
    }
}

/// Drains the last config parse error (set by `Config::load`) so the settings UI
/// can show a one-time banner. Returns None when the config loaded cleanly.
#[tauri::command]
fn take_config_error() -> Option<String> {
    config::LAST_LOAD_ERROR
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// Rebuilds the content index using the current on-disk config. Shared by the
/// `--reindex` socket command and the settings "Apply & Reindex" trigger. Loads
/// the config fresh so it always reflects the latest saved state, opens the index
/// if it isn't open yet (first-time enable), and re-registers the provider so
/// results are searchable. Holds the content lock for the whole run so concurrent
/// config-triggered rebuilds serialise rather than racing the DB.
///
/// `full` controls cache reuse: `true` clears the index first (a hard rebuild -
/// the `--reindex` path and any OCR-settings change, which invalidate cached
/// text). `false` keeps existing rows so `run_content_indexer`'s mtime-skip and
/// `remove_stale` apply only the delta - the cache-preserving path for dir /
/// extension / depth / size edits.
fn run_full_reindex(
    content_state: &ContentState,
    registry: &Registry,
    progress_cb: &Arc<dyn Fn(usize, usize) + Send + Sync>,
    notify_cb: &Arc<dyn Fn() + Send + Sync>,
    content_watcher_tx: &ContentWatcherTx,
    full: bool,
) {
    let cfg = config::Config::load();
    if !cfg.content.enabled {
        eprintln!("[content] reindex requested but content indexing is disabled");
        return;
    }
    // Refresh the fs watcher's watch set with the freshly-loaded dirs. The Apply
    // path (trigger_full_reindex) advances the config-watcher's baseline to skip a
    // redundant second reindex, which also suppresses provider_reload's own
    // content_watcher_tx send - so without this, files created in newly-added dirs
    // would never be hot-indexed until restart. Idempotent: run_dir_watcher dedups
    // already-watched dirs. Done before ReindexGuard so it survives reindex coalescing.
    if let Some(tx) = content_watcher_tx.lock().unwrap().as_ref() {
        let _ = tx.send(cfg.content.clone());
    }
    // Coalesce overlapping reindex triggers so only one drives the progress bar.
    let _reindex_guard = match content_index::ReindexGuard::acquire() {
        Some(g) => g,
        None => {
            eprintln!("[content] reindex already in progress; skipping");
            return;
        }
    };
    let mut guard = util::lock(content_state);
    let idx = match guard.as_ref() {
        Some(idx) => Arc::clone(idx),
        None => match content_index::ContentIndex::open() {
            Ok(idx) => {
                let arc = Arc::new(idx);
                *guard = Some(Arc::clone(&arc));
                arc
            }
            Err(e) => {
                eprintln!("[content] reindex: failed to open index: {e}");
                return;
            }
        },
    };
    registry.write().unwrap().replace(
        "content",
        Some(Box::new(providers::content::ContentProvider::new(
            Arc::clone(&idx),
            cfg.general.max_results,
        ))),
    );
    if full {
        eprintln!("[content] full reindex started (clearing index)");
        idx.clear().ok();
    } else {
        eprintln!("[content] incremental reindex started (cache preserved)");
    }
    content_index::run_content_indexer(idx, &cfg.content, Some(Arc::clone(progress_cb)));
    eprintln!("[content] full reindex complete");
    notify_cb();
}

fn make_progress_cb(handle: tauri::AppHandle) -> Arc<dyn Fn(usize, usize) + Send + Sync> {
    Arc::new(move |indexed, total| {
        let _ = handle.emit("content-index-progress", ContentIndexProgress { indexed, total });
    })
}

// ── entry point ───────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if cli::handle_cli_args() {
        return;
    }

    // The AppImage's linuxdeploy GTK hook hard-codes `GDK_BACKEND=x11`, which
    // drops the window onto XWayland and loses the compositor's fractional/HiDPI
    // scaling (the window renders tiny). Portunus is a Wayland app, so when we
    // are running from the AppImage inside a Wayland session prefer the Wayland
    // backend. Must happen before any GTK init. `PORTUNUS_FORCE_X11` opts out.
    if std::env::var_os("APPDIR").is_some()
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("PORTUNUS_FORCE_X11").is_none()
    {
        std::env::set_var("GDK_BACKEND", "wayland");
    }

    let cfg = config::Config::load();

    let shared_config: config::SharedConfig = Arc::new(RwLock::new(
        config::SharedSearchConfig::from_config(&cfg),
    ));

    let frecency_cfg = cfg.frecency.clone();
    let files_cfg = cfg.files.clone();
    let providers_cfg = cfg.providers.clone();
    let icon_theme_cfg = cfg.general.icon_theme.clone();
    let dict_cfg = cfg.dict.clone();
    let calc_cfg = cfg.calc.clone();
    let marketplace_cfg = cfg.marketplace.clone();
    let max_results = cfg.general.max_results;
    let layer_shell_enabled = cfg.general.layer_shell;
    let content_cfg = cfg.content.clone();

    // last_cfg is used by the config watcher and reload_fn as the diff baseline.
    // Must be a separate Arc from config_state so that save_config (which updates
    // config_state) doesn't also advance last_cfg, which would cause the watcher
    // to see no diff and skip provider rebuilds.
    let last_cfg: Arc<Mutex<config::Config>> = Arc::new(Mutex::new(cfg.clone()));
    // config_state is the frontend-visible config (read by get_config, written by save_config).
    let config_state: ConfigState = Arc::new(Mutex::new(cfg.clone()));
    // The disk-reload paths (config watcher, --reload-config) refresh it too,
    // so a manual file edit isn't clobbered by the next Settings autosave.
    let watcher_config_state = Arc::clone(&config_state);
    // The release checker polls it for the enable toggle and the interval, which
    // is why those settings need no reload hook of their own.
    let update_config_state = Arc::clone(&config_state);

    // Populated once the content watcher thread starts; None until then.
    let content_watcher_tx: ContentWatcherTx = Arc::new(Mutex::new(None));
    // Populated once the file watcher thread starts; None until then.
    let file_watcher_tx: FileWatcherTx = Arc::new(Mutex::new(None));
    // Shared between FileProvider and the file watcher; populated in the startup thread.
    let file_entries: SharedFileEntries = Arc::new(RwLock::new(vec![]));

    // Open content index early (fast - just opens/creates the SQLite file).
    // Wrapped in Arc<Mutex<Option<...>>> so rebuild_providers can open/replace it at runtime.
    let content_state: Arc<Mutex<Option<Arc<content_index::ContentIndex>>>> =
        Arc::new(Mutex::new(if content_cfg.enabled {
            match content_index::ContentIndex::open() {
                Ok(idx) => Some(Arc::new(idx)),
                Err(e) => {
                    eprintln!("[content] failed to open index: {e}");
                    None
                }
            }
        } else {
            None
        }));

    // Extension KV store: opened once, shared by host functions, sync, and
    // the Tauri commands.
    let ext_kv: Arc<extensions::kv::ExtensionKv> =
        Arc::new(extensions::kv::ExtensionKv::open().unwrap_or_else(|e| {
            eprintln!("[extensions] failed to open kv store ({e}) - falling back to in-memory");
            extensions::kv::ExtensionKv::open_in_memory(e.to_string())
        }));
    let extensions_cfg_startup = cfg.extensions.clone();

    // Clipboard OCR cache: opened unconditionally (cheap SQLite file). The
    // background OCR pass and clipboard_list read/write it; gated per-pass by
    // config.clipboard.ocr_images.
    let clipboard_ocr_state: ClipboardOcrState = match clipboard_ocr::ClipboardOcrStore::open() {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            eprintln!("[clipboard] failed to open OCR cache: {e} - image OCR search disabled");
            None
        }
    };

    let registry: Registry = Arc::new(RwLock::new(providers::PluginRegistry::new(max_results)));
    if dict_cfg.enabled {
        registry
            .write()
            .unwrap()
            .set_dict_fill(Some((dict_cfg.fill_threshold, dict_cfg.fill_max)));
    }
    let bg_registry = Arc::clone(&registry);

    // Opened even when frecency is disabled: pins live in the same DB. The
    // recording gate and the zeroed bonus in the ranking weights keep a
    // disabled frecency from tracking or boosting anything.
    let frecency_state: FrecencyState = match frecency::FrecencyStore::open(frecency_cfg.half_life_days)
    {
        Ok(store) => {
            store.set_recording(frecency_cfg.enabled);
            let arc = Arc::new(store);
            registry.write().unwrap().set_frecency(Arc::clone(&arc));
            Some(arc)
        }
        Err(e) => {
            eprintln!("[frecency] failed to open DB: {e} - frecency + pins disabled");
            None
        }
    };
    registry
        .write()
        .unwrap()
        .set_ranking_weights(providers::ranking::RankingWeights::from_config(
            &cfg.ranking,
            frecency_cfg.enabled,
        ));

    tauri::Builder::default()
        .manage(registry)
        .manage(frecency_state.clone())
        .manage(config_state)
        .manage(Arc::clone(&content_state))
        .manage(clipboard_ocr_state)
        .manage(extensions::ExtensionKvState(Arc::clone(&ext_kv)))
        .setup(move |app| {
            // Resolve bundled native assets (pdfium, poppler, tessdata) before
            // any provider or preview path needs them.
            runtime_assets::init(app.path().resource_dir().ok());

            // Let extension logging push live-tail events to Settings.
            extensions::logs::set_app_handle(app.handle().clone());
            // Let the apps provider widen the asset scope to the icons it finds.
            providers::apps::set_app_handle(app.handle().clone());
            // Async extension query orchestration (streams `search-stream`).
            extensions::query::init(app.handle().clone());

            if let Some(window) = app.get_webview_window("main") {
                if layer_shell_enabled {
                    layer_shell::apply(&window);
                }
                let _ = window.hide();
            }

            // Pre-create the settings window hidden so it's ready instantly when opened.
            let _ = tauri::WebviewWindowBuilder::new(
                app,
                "settings",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Portunus Settings")
            .inner_size(1000.0, 620.0)
            .decorations(false)
            .transparent(true)
            .always_on_top(false)
            .skip_taskbar(false)
            .center()
            .visible(false)
            .build();

            // progress_cb is built once and shared by reindex_fn, reload_fn, and the watcher.
            let progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
                make_progress_cb(app.handle().clone());

            // notify_cb fires "search-invalidated" so the frontend re-runs the current query.
            let notify_handle = app.handle().clone();
            let notify_cb: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = notify_handle.emit("search-invalidated", ());
            });

            // keybinds_cb pushes a changed [keybinds] section to both windows.
            // The payload carries the section itself so the frontend never has
            // to re-read a possibly stale config snapshot.
            let keybinds_handle = app.handle().clone();
            let keybinds_cb: Arc<dyn Fn(&config::KeybindsConfig) + Send + Sync> =
                Arc::new(move |kb| {
                    let _ = keybinds_handle.emit("keybinds-changed", kb.clone());
                });

            // reindex_fn: used by the --reindex socket command. Always called from
            // a spawned thread (socket handler), so blocking on the content lock is fine.
            let reindex_ci = Arc::clone(&content_state);
            let reindex_reg = Arc::clone(&bg_registry);
            let reindex_cb = Arc::clone(&progress_cb);
            let reindex_notify = Arc::clone(&notify_cb);
            let reindex_watcher_tx = Arc::clone(&content_watcher_tx);
            // --reindex is a poweruser hard rebuild: always a full clear.
            let reindex_fn: Option<Arc<dyn Fn() + Send + Sync>> = Some(Arc::new(move || {
                run_full_reindex(&reindex_ci, &reindex_reg, &reindex_cb, &reindex_notify, &reindex_watcher_tx, true);
            }));

            // trigger_reindex_fn: called by the trigger_full_reindex Tauri command
            // (settings "Apply & Reindex"). `full` is chosen by the UI - true only
            // when OCR settings changed; otherwise an incremental, cache-preserving run.
            let tri_ci = Arc::clone(&content_state);
            let tri_reg = Arc::clone(&bg_registry);
            let tri_cb = Arc::clone(&progress_cb);
            let tri_notify = Arc::clone(&notify_cb);
            let tri_watcher_tx = Arc::clone(&content_watcher_tx);
            let trigger_reindex_fn: Arc<dyn Fn(bool) + Send + Sync> = Arc::new(move |full: bool| {
                run_full_reindex(&tri_ci, &tri_reg, &tri_cb, &tri_notify, &tri_watcher_tx, full);
            });
            app.manage(TriggerFullReindexFn(Arc::clone(&trigger_reindex_fn)));
            // Share the watcher's diff baseline with trigger_full_reindex so the
            // Apply path can mark content as already-applied (see that command).
            app.manage(WatcherBaseline(Arc::clone(&last_cfg)));

            // Build reload_fn closure shared between socket listener and config watcher.
            let reload_shared = Arc::clone(&shared_config);
            let reload_registry = Arc::clone(&bg_registry);
            let reload_last = Arc::clone(&last_cfg);
            let reload_ci = Arc::clone(&content_state);
            let reload_cb = Arc::clone(&progress_cb);
            let reload_watcher_tx = Arc::clone(&content_watcher_tx);
            let reload_notify = Arc::clone(&notify_cb);
            let reload_file_entries = Arc::clone(&file_entries);
            let reload_file_watcher_tx = Arc::clone(&file_watcher_tx);
            let reload_ext_kv = Arc::clone(&ext_kv);
            let reload_frecency = frecency_state.clone();
            let reload_keybinds = Arc::clone(&keybinds_cb);
            let reload_config_state = Arc::clone(&watcher_config_state);
            let reload_app = app.handle().clone();
            let reload_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let new_cfg = config::Config::load();
                {
                    let mut last = reload_last.lock().unwrap();
                    provider_reload::rebuild_providers(
                        &new_cfg,
                        &last,
                        &reload_shared,
                        &reload_registry,
                        &reload_ci,
                        &reload_cb,
                        &reload_watcher_tx,
                        &reload_notify,
                        &reload_keybinds,
                        &reload_file_entries,
                        &reload_file_watcher_tx,
                        &reload_ext_kv,
                        &reload_frecency,
                    );
                    *last = new_cfg.clone();
                }
                // Re-apply appearance live: the frontend only calls applyTheme on
                // mount or on this event, so --reload-config would refresh the
                // config state without visibly switching the theme otherwise.
                let _ = reload_app.emit("appearance-changed", new_cfg.appearance.clone());
                // Keep the frontend-visible config in sync with the file.
                *reload_config_state.lock().unwrap() = new_cfg;
            });

            // --reload-extensions: force-rebuild every loaded extension so a
            // recompiled wasm is picked up - the extension author's hot-reload loop.
            let ext_reload_registry = Arc::clone(&bg_registry);
            let ext_reload_kv = Arc::clone(&ext_kv);
            let ext_reload_frecency = frecency_state.clone();
            let ext_reload_notify = Arc::clone(&notify_cb);
            let ext_reload_app = app.handle().clone();
            let reload_extensions_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let extensions_cfg = config::Config::load().extensions;
                extensions::sync(
                    &ext_reload_registry,
                    &extensions_cfg,
                    &ext_reload_kv,
                    &ext_reload_frecency,
                    true,
                    Some(Arc::clone(&ext_reload_notify)),
                );
                eprintln!("[extensions] reloaded");
                ext_reload_notify();
                // Distinct signal so the frontend drops its extension preview
                // cache - a rebuilt wasm may render previews differently.
                let _ = ext_reload_app.emit("extensions-reloaded", ());
            });

            // reload-extension:<name> - targeted rebuild of one extension,
            // `portunus ext dev`'s hot-reload loop.
            let ext_one_registry = Arc::clone(&bg_registry);
            let ext_one_kv = Arc::clone(&ext_kv);
            let ext_one_notify = Arc::clone(&notify_cb);
            let ext_one_app = app.handle().clone();
            let reload_one_extension_fn: Arc<dyn Fn(String) + Send + Sync> =
                Arc::new(move |name| {
                    let extensions_cfg = config::Config::load().extensions;
                    extensions::sync_one(
                        &ext_one_registry,
                        &name,
                        &extensions_cfg,
                        &ext_one_kv,
                        Some(Arc::clone(&ext_one_notify)),
                    );
                    eprintln!("[ext:{name}] reloaded");
                    ext_one_notify();
                    let _ = ext_one_app.emit("extensions-reloaded", ());
                });

            ipc::start_socket_listener(
                app.handle().clone(),
                reindex_fn,
                Arc::clone(&reload_fn),
                Arc::clone(&reload_extensions_fn),
                reload_one_extension_fn,
            );

            // Start watchers before start_config_watcher so that file_watcher_tx /
            // content_watcher_tx are populated before any config-change event can fire.
            // (Previously both were started inside the background startup thread, which
            // created a race where a config change during startup found tx = None.)
            if providers_cfg.files {
                let tx = watcher::start_file_watcher(
                    Arc::clone(&file_entries),
                    files_cfg.clone(),
                    Arc::clone(&notify_cb),
                );
                *file_watcher_tx.lock().unwrap() = Some(tx);
            }
            // Always start the content watcher, even if content is currently disabled,
            // so that re-enabling later will immediately receive filesystem events.
            {
                let tx = watcher::start_content_watcher(
                    Arc::clone(&content_state),
                    content_cfg.clone(),
                    Arc::clone(&notify_cb),
                    Arc::clone(&shared_config),
                );
                *content_watcher_tx.lock().unwrap() = Some(tx);
            }

            watcher::start_config_watcher(
                Arc::clone(&shared_config),
                Arc::clone(&bg_registry),
                Arc::clone(&last_cfg),
                Arc::clone(&watcher_config_state),
                Arc::clone(&content_state),
                Arc::clone(&progress_cb),
                Arc::clone(&content_watcher_tx),
                Arc::clone(&notify_cb),
                Arc::clone(&keybinds_cb),
                Arc::clone(&file_entries),
                Arc::clone(&file_watcher_tx),
                Arc::clone(&ext_kv),
                frecency_state.clone(),
            );

            preview::setup(app.handle(), Arc::clone(&shared_config));

            if providers::clipboard::ClipboardProvider::is_available() {
                bg_registry
                    .write()
                    .unwrap()
                    .register(providers::clipboard::ClipboardProvider);
            }
            if providers_cfg.calc {
                bg_registry
                    .write()
                    .unwrap()
                    .register(providers::calc::CalcProvider::new(
                        &calc_cfg,
                        providers::calc::currency::shared(),
                    ));
            }
            // Spawned even when calc is off so a later toggle-on has fresh rates.
            if calc_cfg.currency {
                providers::calc::currency::spawn_refresh_thread(
                    providers::calc::currency::shared(),
                    calc_cfg.rate_max_age_hours,
                );
            }

            // Marketplace: register the scope provider (cheap - searches the
            // in-memory index cache) and refresh the index off-thread if stale.
            {
                let store = Arc::clone(extensions::marketplace::store());
                store.set_index_url(&marketplace_cfg.index_url);
                bg_registry
                    .write()
                    .unwrap()
                    .register(providers::marketplace::MarketplaceProvider::new(Arc::clone(&store)));
                let mp_handle = app.handle().clone();
                std::thread::spawn(move || match store.refresh(false) {
                    Ok(true) => {
                        let _ = mp_handle.emit("search-invalidated", ());
                        let _ = mp_handle.emit("marketplace-index-updated", ());
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("[marketplace] index refresh failed: {e}"),
                });
            }
            // Notify-only release check; reads the enable/interval settings live
            // from config_state on each wake, so no reload plumbing is needed.
            app_update::spawn_checker(app.handle().clone(), Arc::clone(&update_config_state));

            let handle = app.handle().clone();
            let shared_bg = Arc::clone(&shared_config);
            let startup_ci = Arc::clone(&content_state);
            let startup_cb = Arc::clone(&progress_cb);
            let startup_file_entries = Arc::clone(&file_entries);
            let startup_ext_kv = Arc::clone(&ext_kv);
            let startup_frecency = frecency_state.clone();
            let startup_notify = Arc::clone(&notify_cb);

            // Interval scheduler for extensions with [background] refresh.
            extensions::start_refresh_scheduler(
                Arc::clone(&bg_registry),
                Arc::clone(&notify_cb),
            );

            std::thread::spawn(move || {
                // Built here, not in the setup closure: DictProvider::new() now
                // builds an embedded word index (BK-tree), too heavy for the
                // window-show path.
                if dict_cfg.enabled {
                    let dict_provider = providers::dict::DictProvider::new(&dict_cfg);
                    if dict_provider.available {
                        bg_registry.write().unwrap().register(dict_provider);
                    } else {
                        eprintln!("[portunus] dict: `dict` not found - dictionary provider disabled");
                    }
                }
                if providers_cfg.files {
                    providers::files::FileProvider::populate(&startup_file_entries, &files_cfg);
                    let file_provider = providers::files::FileProvider::with_entries(
                        Arc::clone(&startup_file_entries),
                        Arc::clone(&shared_bg),
                    );
                    bg_registry.write().unwrap().register(file_provider);
                }
                if providers_cfg.apps {
                    let app_provider = providers::apps::AppProvider::new(
                        Arc::clone(&shared_bg),
                        icon_theme_cfg.as_deref(),
                    );
                    bg_registry.write().unwrap().register(app_provider);
                }

                // Register ContentProvider immediately so existing index is searchable,
                // then run the indexer in a sub-thread so it doesn't block apps-ready.
                let initial_idx = startup_ci.lock().unwrap().as_ref().map(Arc::clone);
                if let Some(idx) = initial_idx {
                    bg_registry
                        .write()
                        .unwrap()
                        .register(providers::content::ContentProvider::new(Arc::clone(&idx), max_results));
                    let cc = content_cfg.clone();
                    let cb = Arc::clone(&startup_cb);
                    std::thread::spawn(move || {
                        content_index::run_content_indexer(idx, &cc, Some(cb));
                    });
                }

                APPS_READY.store(true, Ordering::Release);
                let _ = handle.emit("apps-ready", ());

                // Extensions load after apps-ready: wasm compilation must never
                // delay the launcher becoming usable.
                extensions::install::sweep_stale_dirs();
                extensions::sync(
                    &bg_registry,
                    &extensions_cfg_startup,
                    &startup_ext_kv,
                    &startup_frecency,
                    false,
                    Some(startup_notify),
                );
            });
            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            let label = window.label();
            // Both windows are long-lived and reused (main via --show, settings
            // via open_settings_window). The compositor's "close active window"
            // (e.g. Super+C / killactive) sends CloseRequested; the default
            // destroys the window - for main that exits the app, for settings it
            // leaves get_webview_window("settings") empty so it can't reopen.
            // Hide instead and stay alive.
            if label != "main" && label != "settings" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            // Core
            search,
            search_explain,
            pin_result,
            unpin_result,
            list_pins,
            list_commands,
            list_icon_themes,
            command_used,
            content_match_section,
            content_match_keys,
            calc_eval,
            launch_app,
            reveal_file,
            hide_window,
            show_window,
            cancel_search,
            is_apps_ready,
            get_config,
            get_custom_theme_css,
            save_config,
            open_settings_window,
            trigger_full_reindex,
            estimate_dir_index,
            is_content_index_empty,
            check_dependencies,
            take_config_error,
            de_setup::de_setup_info,
            de_setup::get_autostart,
            de_setup::set_autostart,
            app_update::app_update_status,
            app_update::app_update_check,
            // File preview
            preview::render_pdf_page,
            preview::pdf_match_rects,
            preview::pdf_text_layer,
            preview::image_match_rects,
            preview::image_text_layer,
            preview::read_text_preview,
            preview::render_image_preview,
            preview::list_folder,
            preview::render_office_doc,
            // Clipboard provider
            providers::clipboard::paste_clipboard,
            providers::clipboard::copy_text,
            providers::clipboard::clipboard_image_text_layer,
            providers::clipboard::decode_clipboard_entry,
            providers::clipboard::clipboard_list,
            providers::clipboard::index_clipboard_ocr,
            providers::clipboard::clipboard_delete,
            providers::clipboard::clipboard_capabilities,
            // Dict provider
            providers::dict::get_dict_definitions,
            // Extensions
            extensions::list_extensions,
            extensions::extension_activate,
            extensions::extension_preview,
            extensions::extension_preview_cancel,
            extensions::rescan_extensions,
            extensions::set_extension_settings,
            extensions::extension_secret_set,
            extensions::extension_secret_clear,
            extensions::secrets_available,
            extensions::get_extension_logs,
            extensions::clear_extension_logs,
            extensions::extension_storage_status,
            extensions::seen_actions::list_extension_actions,
            extensions::install::preview_extension_install,
            extensions::install::confirm_extension_install,
            extensions::install::cancel_extension_install,
            extensions::install::uninstall_extension,
            extensions::install::consent_extension_permissions,
            extensions::marketplace::marketplace_refresh,
            extensions::marketplace::marketplace_updates,
            extensions::marketplace::marketplace_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
