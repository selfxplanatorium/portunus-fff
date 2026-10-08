use std::sync::{Arc, Mutex};

use crate::extensions::kv::ExtensionKv;
use crate::{
    config, content_index, providers, ContentWatcherTx, FrecencyState, Registry,
    SharedFileIndex,
};

/// Spawn a thread that calls `build()` to produce an optional provider, replaces
/// it in the registry under `id`, logs `name`, then fires `notify_cb`.
/// Used for the simple (apps) rebuild cases where the only difference is
/// what gets constructed.
fn spawn_rebuild(
    registry: &Registry,
    notify_cb: &Arc<dyn Fn() + Send + Sync>,
    id: &'static str,
    name: &'static str,
    build: impl FnOnce() -> Option<Box<dyn providers::Provider>> + Send + 'static,
) {
    let reg = Arc::clone(registry);
    let ncb = Arc::clone(notify_cb);
    std::thread::spawn(move || {
        let new = build();
        reg.write().unwrap().replace(id, new);
        eprintln!("[config] {name} provider rebuilt");
        ncb();
    });
}

pub fn rebuild_providers(
    new_cfg: &config::Config,
    old_cfg: &config::Config,
    shared: &config::SharedConfig,
    registry: &Registry,
    content_state: &Arc<Mutex<Option<Arc<content_index::ContentIndex>>>>,
    progress_cb: &Arc<dyn Fn(usize, usize) + Send + Sync>,
    content_watcher_tx: &ContentWatcherTx,
    notify_cb: &Arc<dyn Fn() + Send + Sync>,
    keybinds_cb: &Arc<dyn Fn(&config::KeybindsConfig) + Send + Sync>,
    file_index: &SharedFileIndex,
    ext_kv: &Arc<ExtensionKv>,
    frecency: &FrecencyState,
) {
    // Update per-search scalars instantly (no rebuild needed).
    shared.write().unwrap().update_from(new_cfg);

    // Keybinds are dispatched frontend-side: push the new section as-is, no
    // provider rebuild for a keybinds-only edit.
    if new_cfg.appearance.blur != old_cfg.appearance.blur {
        let enabled = new_cfg.appearance.blur;
        std::thread::spawn(move || crate::layer_shell::apply_compositor_blur(enabled));
    }

    if new_cfg.keybinds != old_cfg.keybinds {
        keybinds_cb(&new_cfg.keybinds);
    }

    // Update registry-level settings (max_results) and resolve the ranking
    // weights - every `[ranking]` knob applies on the next keystroke.
    {
        let mut reg = registry.write().unwrap();
        reg.update_settings(new_cfg.general.max_results);
        reg.set_ranking_weights(providers::ranking::RankingWeights::from_config(
            &new_cfg.ranking,
            new_cfg.frecency.enabled,
        ));
    }
    if let Some(store) = frecency {
        store.set_recording(new_cfg.frecency.enabled);
    }

    // ── Selectively rebuild index-backed providers ────────────────────────────

    let files_index_changed = !new_cfg.files.index_eq(&old_cfg.files)
        || new_cfg.providers.files != old_cfg.providers.files;
    if files_index_changed {
        let files_cfg = new_cfg.files.clone();
        let was_enabled = old_cfg.providers.files;
        let now_enabled = new_cfg.providers.files;
        let shared2 = Arc::clone(shared);
        let reg2 = Arc::clone(registry);
        let ncb = Arc::clone(notify_cb);
        let index = Arc::clone(file_index);
        std::thread::spawn(move || {
            if now_enabled {
                // Unchanged roots keep their live index; only added or edited
                // ones are walked, and `ncb` fires once those scans land.
                index.configure(&files_cfg, Some(Arc::clone(&ncb)));
                if !was_enabled {
                    let p = providers::files::FileProvider::new(Arc::clone(&index), shared2);
                    reg2.write().unwrap().replace("files", Some(Box::new(p)));
                }
            } else {
                reg2.write().unwrap().replace("files", None);
                index.clear();
            }
            eprintln!("[config] files provider rebuilt");
            ncb();
        });
    } else if new_cfg.files != old_cfg.files {
        // Per-search filters and display flags: applied on the next keystroke.
        notify_cb();
    }

    // A new icon theme means every app icon has to be re-resolved, so it needs
    // the same full rebuild as toggling the provider.
    if new_cfg.providers.apps != old_cfg.providers.apps
        || new_cfg.general.icon_theme != old_cfg.general.icon_theme
    {
        let enabled = new_cfg.providers.apps;
        let shared2 = Arc::clone(shared);
        let icon_theme = new_cfg.general.icon_theme.clone();
        spawn_rebuild(registry, notify_cb, "apps", "apps", move || {
            enabled.then(|| {
                Box::new(providers::apps::AppProvider::new(shared2, icon_theme.as_deref())) as _
            })
        });
    }

    // ── Cheap providers: toggle under write lock directly ─────────────────────

    if new_cfg.providers.calc != old_cfg.providers.calc || new_cfg.calc != old_cfg.calc {
        let mut reg = registry.write().unwrap();
        if new_cfg.providers.calc {
            let p = providers::calc::CalcProvider::new(
                &new_cfg.calc,
                providers::calc::currency::shared(),
            );
            reg.replace("calc", Some(Box::new(p)));
            eprintln!("[config] calc provider rebuilt");
        } else {
            reg.replace("calc", None);
            eprintln!("[config] calc provider disabled");
        }
        notify_cb();
    }

    if new_cfg.marketplace.index_url != old_cfg.marketplace.index_url {
        let store = Arc::clone(crate::extensions::marketplace::store());
        store.set_index_url(&new_cfg.marketplace.index_url);
        let ncb = Arc::clone(notify_cb);
        std::thread::spawn(move || match store.refresh(true) {
            Ok(true) => ncb(),
            Ok(false) => {}
            Err(e) => eprintln!("[marketplace] index refresh failed: {e}"),
        });
        eprintln!("[config] marketplace index url changed");
    }

    // `[general] check_for_updates` / `update_check_interval_hours` need no hook
    // here: the checker thread re-reads them from config_state on every wake, and
    // the About section has a "Check now" button for immediate feedback.

    if new_cfg.dict != old_cfg.dict {
        let mut reg = registry.write().unwrap();
        if new_cfg.dict.enabled {
            let p = providers::dict::DictProvider::new(&new_cfg.dict);
            if p.available {
                reg.replace("dict", Some(Box::new(p)));
            } else {
                reg.replace("dict", None);
            }
            reg.set_dict_fill(Some((new_cfg.dict.fill_threshold, new_cfg.dict.fill_max)));
            eprintln!("[config] dict provider enabled");
        } else {
            reg.replace("dict", None);
            reg.set_dict_fill(None);
            eprintln!("[config] dict provider disabled");
        }
        notify_cb();
    }

    if new_cfg.extensions != old_cfg.extensions {
        // Targeted reload: only extensions whose entry (enabled flag or
        // settings table) actually changed are rebuilt; everything else keeps
        // its warm instances.
        let changed: Vec<String> = new_cfg
            .extensions
            .keys()
            .chain(old_cfg.extensions.keys())
            .filter(|n| new_cfg.extensions.get(*n) != old_cfg.extensions.get(*n))
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let extensions_cfg = new_cfg.extensions.clone();
        let reg2 = Arc::clone(registry);
        let ncb = Arc::clone(notify_cb);
        let kv = Arc::clone(ext_kv);
        // Wasm compilation is slow - build instances off-thread and only
        // take the registry write lock for pointer swaps.
        std::thread::spawn(move || {
            for name in &changed {
                crate::extensions::sync_one(
                    &reg2,
                    name,
                    &extensions_cfg,
                    &kv,
                    Some(Arc::clone(&ncb)),
                );
            }
            eprintln!("[config] {} extension(s) reloaded", changed.len());
            ncb();
        });
    }

    if new_cfg.content != old_cfg.content {
        let new_content_cfg = new_cfg.content.clone();
        let old_content_cfg = old_cfg.content.clone();
        // Notify the filesystem watcher of the new config so it can watch any added dirs.
        if let Some(tx) = content_watcher_tx.lock().unwrap().as_ref() {
            let _ = tx.send(new_content_cfg.clone());
        }
        let reg2 = Arc::clone(registry);
        let ci_state = Arc::clone(content_state);
        let cb = Arc::clone(progress_cb);
        let ncb = Arc::clone(notify_cb);
        let max_results = new_cfg.general.max_results;
        std::thread::spawn(move || {
            // Hold the lock for the full operation so two rapid config saves
            // can't race each other on the same DB tables.
            let mut guard = ci_state.lock().unwrap();
            if new_content_cfg.enabled {
                let idx = match guard.as_ref() {
                    Some(existing) => Arc::clone(existing),
                    None => match content_index::ContentIndex::open() {
                        Ok(idx) => {
                            let arc = Arc::new(idx);
                            *guard = Some(Arc::clone(&arc));
                            arc
                        }
                        Err(e) => {
                            eprintln!("[content] failed to open index: {e}");
                            return;
                        }
                    },
                };

                // Register provider with the current index so existing data is
                // immediately searchable, even before any reindex completes.
                reg2.write().unwrap().replace(
                    "content",
                    Some(Box::new(providers::content::ContentProvider::new(
                        Arc::clone(&idx),
                        max_results,
                    ))),
                );

                // "Heavy" changes require a full clear+rebuild, which is expensive.
                // We never trigger that automatically - the settings UI stages these
                // edits and the user confirms via "Apply & Reindex" (trigger_full_reindex),
                // or a poweruser runs `portunus --reindex` after a manual config edit.
                // Here we only register the provider (above) and apply cheap incremental
                // changes; heavy changes are left for the explicit reindex path.
                let ocr_changed = old_content_cfg.ocr_images != new_content_cfg.ocr_images
                    || old_content_cfg.ocr_pdf_fallback != new_content_cfg.ocr_pdf_fallback
                    || old_content_cfg.ocr_language != new_content_cfg.ocr_language;
                // First-enable: content was disabled before AND the index is empty.
                // Re-enabling a populated index is just a cheap incremental run.
                let first_enable = !old_content_cfg.enabled && idx.is_empty();
                let max_bytes_increased =
                    new_content_cfg.max_file_bytes > old_content_cfg.max_file_bytes;

                if ocr_changed || first_enable || max_bytes_increased {
                    eprintln!(
                        "[content] heavy settings change detected; full reindex deferred \
                         (apply via Settings or `portunus --reindex`)"
                    );
                } else if old_content_cfg.contents_eq(&new_content_cfg) {
                    // Only indexing-speed settings (threads) changed - the index
                    // contents are unaffected, so a reindex would be pure waste and
                    // would race the progress bar against any in-flight run.
                    eprintln!("[content] non-content settings change; skipping reindex");
                } else {
                    // Cheap, non-destructive incremental update - same as the startup routine.
                    // Picks up added dirs, extension/depth changes, and removed dirs.
                    // Guarded so a config save mid-reindex doesn't start a second run.
                    match content_index::ReindexGuard::acquire() {
                        Some(_guard) => {
                            content_index::run_content_indexer(idx, &new_content_cfg, Some(cb));
                            eprintln!("[content] incremental reindex complete");
                            ncb();
                        }
                        None => eprintln!(
                            "[content] reindex already in progress; skipping incremental update"
                        ),
                    }
                }
            } else {
                *guard = None;
                reg2.write().unwrap().replace("content", None);
                eprintln!("[content] content provider disabled");
                ncb();
            }
        });
    }

    eprintln!("[config] reload complete");
}
