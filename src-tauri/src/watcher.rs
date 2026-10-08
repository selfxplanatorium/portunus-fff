use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify_debouncer_full::{new_debouncer, notify::RecursiveMode, notify::Watcher, DebouncedEvent};

use crate::{config, content_index, provider_reload, ContentWatcherTx, Registry, SharedFileIndex};

/// Watches every directory under `root` whose own depth below `root` is `<= max_below`,
/// NON-recursively and WITHOUT following symlinks. Watching a dir surfaces events for its
/// direct children, so a root with index-`depth` D needs dirs at depth `0..=D-1` watched
/// (callers pass `max_below = D-1`).
///
/// Why not `RecursiveMode::Recursive`: notify 6.1.1 ignores depth and hard-follows symlinks
/// (its inotify backend walks with `WalkDir::follow_links(true)`). On a home dir that descends
/// through e.g. a wine `dosdevices/z:` -> `/` symlink and re-watches real dirs under bogus deep
/// paths, which corrupts event-path reconstruction (events arrive under the symlinked path) and
/// explodes the watch count. Bounding depth + `follow_links(false)` avoids both.
fn watch_tree(
    watcher: &mut impl Watcher,
    start: &std::path::Path,
    max_below: usize,
    watched: &mut HashSet<PathBuf>,
) {
    for entry in walkdir::WalkDir::new(start)
        .max_depth(max_below)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let p = entry.path().to_path_buf();
        if watched.insert(p.clone()) {
            if let Err(e) = watcher.watch(&p, RecursiveMode::NonRecursive) {
                eprintln!("[watcher] watch {:?}: {e}", p);
            }
        }
    }
}

/// Watch each configured `(root, depth)`, skipping dirs already in `watched`.
fn watch_roots(
    watcher: &mut impl Watcher,
    roots: &[(PathBuf, usize)],
    watched: &mut HashSet<PathBuf>,
) {
    for (root, depth) in roots {
        if root.is_dir() {
            if let Some(max_below) = depth.checked_sub(1) {
                watch_tree(watcher, root, max_below, watched);
            }
        }
    }
}

/// Spawns a watcher thread that watches dirs derived from `initial_cfg`.
/// New dirs added by subsequent config updates (sent via the returned Sender) are added to the watch set.
/// `get_dirs` extracts the list of `(directory, index-depth)` pairs to watch from a config snapshot.
/// `on_events` is called for each debounced batch of filesystem events.
fn run_dir_watcher<Cfg>(
    initial_cfg: Cfg,
    get_dirs: impl Fn(&Cfg) -> Vec<(PathBuf, usize)> + Send + 'static,
    mut on_events: impl FnMut(&[DebouncedEvent], &Cfg) + Send + 'static,
) -> std::sync::mpsc::Sender<Cfg>
where
    Cfg: Send + Clone + 'static,
{
    let (cfg_tx, cfg_rx) = std::sync::mpsc::channel::<Cfg>();

    std::thread::spawn(move || {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(500), None, move |res| {
            let _ = ev_tx.send(res);
        }) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[watcher] failed to create debouncer: {e}");
                return;
            }
        };

        let mut current_cfg = initial_cfg;
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();

        // Watch dirs from the initial config.
        watch_roots(debouncer.watcher(), &get_dirs(&current_cfg), &mut watched_dirs);

        loop {
            // Drain any pending config updates, adding newly-mentioned dirs to the watch set.
            while let Ok(new_cfg) = cfg_rx.try_recv() {
                watch_roots(debouncer.watcher(), &get_dirs(&new_cfg), &mut watched_dirs);
                current_cfg = new_cfg;
            }

            match ev_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(events)) => {
                    // NonRecursive watches don't auto-cover dirs created after startup
                    // (RecursiveMode::Recursive did). Watch any newly-created in-scope dir
                    // before processing, so its children fire events.
                    let roots = get_dirs(&current_cfg);
                    for ev in &events {
                        for p in &ev.event.paths {
                            if !p.is_dir() {
                                continue;
                            }
                            for (root, depth) in &roots {
                                let Ok(rel) = p.strip_prefix(root) else { continue };
                                let r = rel.components().count();
                                // Need this dir's children in scope: r <= depth-1.
                                if let Some(max_below) = (depth.saturating_sub(1)).checked_sub(r) {
                                    watch_tree(debouncer.watcher(), p, max_below, &mut watched_dirs);
                                }
                                break;
                            }
                        }
                    }
                    on_events(&events, &current_cfg);
                }
                Ok(Err(errs)) => {
                    for e in errs {
                        eprintln!("[watcher] {e}");
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    cfg_tx
}

// ── Config file watcher ───────────────────────────────────────────────────────

pub fn start_config_watcher(
    shared: config::SharedConfig,
    registry: Registry,
    last_cfg: Arc<Mutex<config::Config>>,
    config_state: crate::ConfigState,
    content_state: Arc<Mutex<Option<Arc<content_index::ContentIndex>>>>,
    progress_cb: Arc<dyn Fn(usize, usize) + Send + Sync>,
    content_watcher_tx: ContentWatcherTx,
    notify_cb: Arc<dyn Fn() + Send + Sync>,
    keybinds_cb: Arc<dyn Fn(&config::KeybindsConfig) + Send + Sync>,
    file_index: SharedFileIndex,
    ext_kv: Arc<crate::extensions::kv::ExtensionKv>,
    frecency: crate::FrecencyState,
) {
    use notify_debouncer_full::notify::Watcher as _;

    let config_dir = crate::paths::config_dir();

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();

        // Watch the DIRECTORY (not the file directly) because many editors save via
        // atomic rename: write temp file → rename over target. Watching the directory
        // catches IN_MOVED_TO which inotify fires on rename.
        // The config dir normally exists by now (Config::load creates it at startup),
        // so this retry loop is a safety net. Bound it instead of spinning forever:
        // if it never appears, give up and disable live reload rather than leaking a
        // thread that wakes every 2s for the life of the process.
        const MAX_ATTEMPTS: u32 = 30; // ~60s
        let mut attempts = 0;
        let debouncer = loop {
            attempts += 1;
            let tx2 = tx.clone();
            match new_debouncer(Duration::from_millis(500), None, move |res| {
                let _ = tx2.send(res);
            }) {
                Ok(mut d) => {
                    if config_dir.exists() {
                        if d.watcher().watch(&config_dir, RecursiveMode::NonRecursive).is_ok() {
                            break d;
                        }
                    }
                    if attempts >= MAX_ATTEMPTS {
                        eprintln!(
                            "[config-watcher] {} did not appear after {attempts} attempts; \
                             live config reload disabled",
                            config_dir.display()
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(e) => {
                    eprintln!("[config-watcher] failed to create debouncer: {e}");
                    return;
                }
            }
        };
        let _ = debouncer; // keep alive

        for res in rx {
            let events = match res {
                Ok(evs) => evs,
                Err(errs) => {
                    for e in errs {
                        eprintln!("[config-watcher] {e}");
                    }
                    continue;
                }
            };

            let touched = events.iter().any(|e| {
                e.event.paths.iter().any(|p| {
                    p.file_name().and_then(|n| n.to_str()) == Some("config.toml")
                })
            });
            if !touched {
                continue;
            }

            eprintln!("[config] change detected, reloading…");
            let new_cfg = config::Config::load();
            {
                let mut last = last_cfg.lock().unwrap();
                provider_reload::rebuild_providers(
                    &new_cfg,
                    &last,
                    &shared,
                    &registry,
                    &content_state,
                    &progress_cb,
                    &content_watcher_tx,
                    &notify_cb,
                    &keybinds_cb,
                    &file_index,
                    &ext_kv,
                    &frecency,
                );
                *last = new_cfg.clone();
            }
            // Also refresh the frontend-visible config (get_config reads it,
            // save_config writes it back wholesale) - otherwise a manual file
            // edit is silently clobbered by the next Settings autosave.
            *config_state.lock().unwrap() = new_cfg;
        }
    });
}

// ── Content filesystem watcher ────────────────────────────────────────────────

pub fn start_content_watcher(
    content_state: Arc<Mutex<Option<Arc<content_index::ContentIndex>>>>,
    initial_cfg: config::ContentConfig,
    notify_cb: Arc<dyn Fn() + Send + Sync>,
    shared: config::SharedConfig,
) -> std::sync::mpsc::Sender<config::ContentConfig> {
    run_dir_watcher(
        initial_cfg,
        |cfg: &config::ContentConfig| {
            cfg.dirs.iter().map(|d| (config::Config::expand_path(&d.path), d.depth)).collect()
        },
        move |events, current_cfg| {
            if !current_cfg.enabled {
                return;
            }
            let log = shared.read().unwrap().log_watcher;
            let idx = content_state.lock().unwrap().as_ref().map(Arc::clone);
            if let Some(idx) = idx {
                let mut changed = false;
                for ev in events {
                    if log {
                        eprintln!("[content-watcher] event {:?} paths={:?}", ev.event.kind, ev.event.paths);
                    }
                    for path in &ev.event.paths {
                        let result = content_index::process_event_path(&idx, path, current_cfg, log);
                        if log {
                            eprintln!("[content-watcher] process_event_path({:?}) -> {}", path, result);
                        }
                        if result {
                            changed = true;
                        }
                    }
                }
                if log {
                    eprintln!("[content-watcher] batch done: changed={changed}");
                }
                if changed {
                    notify_cb();
                }
            } else if log {
                eprintln!("[content-watcher] event received but content index is None");
            }
        },
    )
}
