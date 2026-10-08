# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
bun tauri dev                                    # dev mode (hot reload)
bun tauri build                                  # production build (OCR always on; needs libtesseract-dev + libleptonica-dev; linuxdeploy for AppImage)
cargo check --manifest-path src-tauri/Cargo.toml --workspace # type-check Rust only (fastest inner loop, ~3s)
cargo test --manifest-path src-tauri/Cargo.toml --workspace  # unit tests (inline #[cfg(test)] modules: providers incl. files/ranking/calc, extensions, office, text, …)
bun x tsc --noEmit                               # type-check TypeScript only
```

Package manager is **bun**, not npm or yarn. There is no eslint/prettier/rustfmt config — Rust defaults; `bun run build` runs `tsc` before vite. There are no frontend tests; the Rust unit tests above are the only automated tests.

Commit style: terse, subject-only, no trailers (see `git log`). Never use the word "plugins" for wasm modules in code, UI, or docs — they are **extensions**.

## Releasing

CI (`.github/workflows/release.yml`) builds the AppImage + .deb on every push/PR (uploads to a GitHub Release only on `v*` tags) using `bun tauri build --config src-tauri/tauri.bundle.conf.json`, which bundles libpdfium, poppler tools, tesseract data. A plain local `bun tauri build` produces bundles *without* those assets. On tag pushes CI also renders `packaging/aur/PKGBUILD` (a template with `@PKGVER@`/`@SHA256@` placeholders) with the built .deb's checksum and attaches it — **no manual sha256 editing**.

Bundle filenames come from the **`src-tauri/Cargo.toml` `version`** field (Tauri reads it; there is no `version` in `tauri.conf.json`). Before tagging `vX.Y.Z`, bump **both** `src-tauri/Cargo.toml` and `package.json` versions to `X.Y.Z` — a mismatch makes the AUR download URL (`portunus_$pkgver_amd64.deb`) 404.

## Build speed

`src-tauri/` is the **workspace root** (`members = [".", "crates/*"]`), so `target/` and every CI/bundle path stay where they were. Two leaf crates are split out of the host so an edit there does not recompile them:

- `crates/portunus-text` — `normalize`/`tokenize`/`stem`/`query_keys`, std-only.
- `crates/portunus-office` — the DOCX/PPTX/XLSX/ODF stack, 38k lines, ~64% of the old host crate. Depends on `portunus-text` only.

`lib.rs` aliases both back (`pub(crate) use portunus_office as office;`), so `crate::office::…` and `crate::content_match::…` paths are unchanged. Keep new bulk parsing code in these crates rather than growing the host crate again.

Dependency features are trimmed deliberately — do not "restore defaults" when adding a feature:

- `image` has no default features: the default set pulls `ravif`/`rav1e` (an AVIF *encoder*) plus exr/dds/hdr. Add a format explicitly if a decoder is genuinely missing.
- `resvg` runs without `text`/`system-fonts`: dominant-color sampling only averages pixels.
- `extism` runs with `default-features = false`, which drops wasmtime's `profiling`/`threads` and `wasm-compose`. The direct `wasmtime` dependency exists **only** to unify `wasmtime/{std,anyhow}` back on — extism's code needs `wasmtime::Error: std::error::Error`. Nothing in `src/` imports wasmtime; keep its version in step with extism's.
- `[profile.dev]` is `debug = "line-tables-only"` + no debuginfo for dependencies. Backtraces keep `file:line`; a debugger cannot inspect variables. Raise it locally if you need gdb.

The mold linker is opt-in per developer via a gitignored `src-tauri/.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

Changing that file (or any profile/feature above) invalidates the whole target dir, so make such edits in one batch.

## Architecture

Portunus is a Tauri 2 app: a Rust backend exposed via Tauri IPC to a React 19 / TypeScript frontend. The window is decorationless, transparent, always-on-top, and hidden at startup — it surfaces only when signaled via `portunus --show` (Unix socket IPC).

### Backend (`src-tauri/src/`)

Tauri commands are registered in `lib.rs` (`invoke_handler` near the bottom lists all of them, grouped: core search/config, `preview.rs` file-preview commands, clipboard, dict, and `extensions/` commands).

**Provider system** — `providers/mod.rs` defines the `Provider` trait, `PluginRegistry`, and the scope-only scoring constants; `providers/ranking.rs` owns root-search score composition. Built-in providers: `apps`, `files`, `clipboard`, `calc` (fend-core + datetime + currency), `dict`, `content`, `command` (searchable command catalog), plus wasm extensions (`wasm.rs`). `PluginRegistry::search()` merges results, composes scores from the live `RankingWeights`, applies frecency + pins, sorts, truncates to `max_results` (default 9). The `content` provider runs only via `search_content()` — backend of the Tab-activated "Contents" scope.

**File index** (`providers/files.rs`) — backed by `fff-search`: one fff `FilePicker` per `[[files.dirs]]` root (`FileIndex`, shared as `SharedFileIndex`), each with its own parallel gitignore-aware walk, optional per-directory inotify watcher (`watch = true`) and SIMD typo-tolerant matcher. `/` fans out into its top-level dirs minus pseudo filesystems; a path inside a nested root is served only by the most specific root. `configure()` reconciles roots on reload (unchanged roots keep their index); `--reindex` also calls `rescan()` (the only refresh unwatched roots get). fff's score is mapped onto the registry's fuzzy scale relative to the best hit; `ignore`/`show_dotfiles` are per-search filters, not walk options. fff runs with its own frecency/git-recency off — portunus' `frecency.rs` stays the only history signal.

**Scoring** — user-configurable via `[ranking]` (`providers/ranking.rs`). Providers emit `ScoreParts` (category, title match tier, nucleo score, intra-band offset); the registry composes: band from `category_order` (1M per band; default calc > app > command > extension > file > dict-fill) + category/extension weight offset (±500k; weight 0 hides from root search, scopes unaffected) + match-tier boost (exact/prefix/word-start, 100k per config point — exact deliberately jumps bands) + balance-scaled fuzzy bonus (`match_vs_history`) + frecency bonus + pin bonus (20M, always on top). Scope-only tiers (content, ext-triggered, scoped dict) keep fixed constants in `providers/mod.rs`; `apply_frecency_weights` skips content by *kind*, not score. `finalize_results()` is shared by the sync and streamed (`extensions/query.rs`) paths so the formula can't drift. `search_explain` returns per-result breakdowns for the Settings ranking playground and never executes extensions. Keep magnitudes high — they are deliberate. Pins live in `frecency.db` (`pins` table; the typed query prefix-matches the stored pin query).

**Frecency** (`frecency.rs`) — SQLite at `$XDG_DATA_HOME/portunus/frecency.db`. Half-life exponential decay: `new_score = old_score × 2^(−elapsed_days / half_life) + 1.0`. Frecency is the *only* history signal (a separate recency bonus was removed on purpose).

**Content index** (`content_index.rs`) — SQLite FTS5 (Porter stemming) over file contents; `portunus-office` extracts DOCX/PPTX/XLSX/ODF text (zip-bomb caps), `portunus-text` normalizes queries/diacritics. Stopwords are stripped from queries because FTS5 bm25 has no top-k early exit. OCR via Tesseract (`leptess`) always compiled in. Progress events: `content-index-progress { indexed, total }`. `clipboard_ocr.rs` is a separate OCR cache for clipboard images.

**Config** (`config.rs`, defaults in `default_config.toml`) — TOML at `~/.config/portunus/config.toml`. Hot-reloaded via `watcher.rs`; `provider_reload.rs` rebuilds affected providers and emits `search-invalidated`. Pre-release project: breaking config-schema changes are fine, no migration code needed.

**IPC** (`ipc.rs`) — Unix socket at `$XDG_RUNTIME_DIR/portunus.sock`; CLI flags from a second instance: `--show`, `--close`, `--toggle`, `--clipboard`, `--reindex` (content index + file-root rescan), `--reload-config`, `--reload-extensions`, `--reload-extension <name>`, `--reload-theme` (`cli.rs`).

**Extensions** (`extensions/`, wire contract in `extension-sdk/`, **api = 5**, docs in `EXTENSIONS.md`) — sandboxed Extism/wasm providers. Manifest v5 declares `[[commands]]` (searchable launcher entries, `mode = "scope"` or `"action"`), `[permissions]` (incl. `bus` = companion-process channel), `[limits]`, `[background]`, `[[settings]]` (including `secret` type stored via keyring, `secrets.rs`). Guest exports: `search` (sync fast path), `query` (async/streaming), `activate` (returns declarative effects: copy_text/open_url/show_toast/show_form/paste/hide/keep_open/refresh_results/set_query/spawn_process), `preview` (lazy, streamable), `refresh` (background). Key host pieces: `manifest.rs` (parse/validate), `install.rs` (`.portext` two-phase install, consents.toml permission snapshots, update check), `hostfns.rs` (kv/clipboard/open_url/settings/emit/bus), `bus.rs` (companion message bus over the `ext-attach:<name>` socket channel; `native_host.rs` = browser native-messaging shim, `portunus native-host`), `query.rs` (async query tier), `logs.rs` (per-extension ring buffer for Settings), `providers/wasm.rs` (instance slots, output-size caps), `providers/breaker.rs` (3-strike failure breaker with escalating cooldown). Developer CLI: `portunus ext new/dev/validate/pack` (`cli_ext.rs`, scaffolds from `templates/extension/`). Reference extensions live in the separate `portunus-extensions` repo (https://github.com/SzilBalazs/portunus-extensions): emoji = offline scope, cheatsh = network+cache+refresh, gh = multi-command/secrets/streaming/forms.

**Update check** (`app_update.rs`) — polls this fork's GitHub releases (`selfxplanatorium/portunus-fff`), never upstream's.

**Startup** — `CalcProvider` registers synchronously; `AppProvider` loads in a background thread, where the file index also spawns its fff scans (non-blocking; `search-invalidated` fires when they land); `apps-ready` event clears the frontend loading state. A tiny embedded PDF (`warmup.pdf`) is rendered at startup to prime fontconfig.

### Critical backend invariants

- **pdfium**: not `Send`; one long-lived worker thread owns it (`preview.rs`). `Pdfium::new` calls global `FPDF_InitLibrary` and Drop calls `FPDF_DestroyLibrary` — constructing a second instance deadlocks the worker. Availability probes must only *bind*, never construct.
- **Extism cancellation is engine-wide** (epoch-based): each instance slot in `wasm.rs` gets its own `CompiledPlugin` (own engine) — `instance` (search/activate), `preview_instance`, `bg_instance`, `query_instance`. Never share a compiled plugin across slots or cancelling one call kills the others.
- **Extension output is untrusted**: `wasm.rs` clamps output bytes, result count, field lengths, icon/image payloads, and whitelists image MIME types. Keep new fields behind similar caps.

### Frontend (`src/`)

`main.tsx` routes by window label to `App.tsx` (launcher) or `Settings.tsx`.

**App.tsx** — launcher state machine: `query` → debounced `invoke("search", { query, queryId, scope })` → sync `results` + per-extension `streamed` batches (via `search-stream` events, correlated by monotonic `queryId` to drop stale arrivals). Selection is pinned by result id across reorders. Modal states: QuickLook (Shift+Enter), ActionPicker (Alt+Enter/Ctrl+K), extension form modal. Scope commands (`mode`): clipboard is a UI-takeover mode, contents is Tab-toggled; Backspace on empty query exits a mode, Escape unwinds overlay → query → mode → hide.

**Provider plugin pattern** (`providers/registry.ts` + `providers/*.tsx`) — each result kind registers `{ kinds, Preview, handleLaunch, handleKeyDown }` via `registerProvider()`; `App.tsx` dispatches through the registry. New result kinds get a new provider registration, not special cases in App.tsx.

**Settings.tsx** — sidebar sections (General, Keybinds, Providers, Clipboard, Extensions, Dict, Files, Ranking, Content, Appearance, Debug, About). Autosaves cheap edits with 800 ms debounce; heavy content-index fields (dirs, extensions list, OCR options, size increases) are staged behind "Apply & Reindex". **Always build settings UI from the shared primitives** in `components/settings/` — `SettingsField`, `SettingsGroup`, `SectionHeader`, `Toggle`, `TextInput`, `Select`, `Slider`, `NumberStepper`, `Badge`, `Modal` — never bespoke per-file markup.

**Styling** — plain CSS (`App.css`, `settings.css`, `themes.css`), no CSS modules/frameworks. Theme via CSS custom properties on `:root`; named themes as `:root[data-theme="…"]`; Matugen CSS injected at runtime (`theme.ts`). Behavior toggles ride data attributes (`data-animate-results`, `data-accent-bleed`, …). Use existing tokens (`--accent`, `--bg-*`, `--fg-*`) instead of hard-coded colors. Fonts go through `--font-ui` / `--font-mono` (never a literal stack; `[appearance] font_family`/`mono_font_family` override them in `theme.ts`). `[appearance] opacity` fades the launcher's surface tokens (`--bg-card`, `--bg-preview`, …) inline in `theme.ts`; `blur` is compositor work (`layer_shell::apply_compositor_blur`): Hyprland via hyprctl; KDE Plasma via `kde_blur.rs`, which speaks `ext_background_effect_manager_v1` (preferred, only with the `blur` capability; what Better Blur DX implements) or else `org_kde_kwin_blur` on GDK's own `wl_display` (wayland-backend `client_system`, private event queue, GTK main thread only) and blurs just the rects the launcher reports through `set_blur_region` (`src/blurRegion.ts`: the card's rounded outline as stacked strips, one call per frame). GDK swaps the `wl_surface` on every hide/show, so the per-surface object is destroyed on `unmap` (before GDK destroys the surface: ext-background-effect makes any later request a fatal protocol error) and recreated per surface. Clearing: ext = `set_blur_region(None)`; kwin = empty region, never null (null blurs the whole surface). No protocol found = re-probe on the next show; only non-Wayland latches off. Window motion ("window motion" block in `App.css`): `.launcher[data-expanded]` morphs the card between the 62px bar and full height by animating only `.card` height (`.card-clip` stays full size, so contents never re-layout); `data-enter` alternates per show to restart the entrance keyframes; `data-phase="hidden"` (set on `visibilitychange`) parks the card invisible between shows.

**Events** — subscribe with the `useTauriListener` hook. Backend events: `window-show`, `apps-ready`, `search-stream`, `search-invalidated`, `content-index-progress`, `appearance-changed`, `theme-css-changed`.

`types.ts` holds the shared interfaces (`Config`, `SearchResult`, …) — keep them in sync with the Rust structs they mirror.

### Two-window setup

`tauri.conf.json` defines `main` (960×640, transparent, hidden at startup); `settings` (1000×620) is built in `lib.rs` setup, pre-created hidden and shown via `open_settings_window`. Both are WebKit2GTK 4.1 WebViews. Asset protocol is enabled to load system icons from `~/.local/share/icons` and XDG icon dirs.

## System dependencies

Packaged AppImage/.deb builds bundle libpdfium, the poppler tools, and English tesseract data (see `runtime_assets.rs` + `tauri.bundle.conf.json`); the items below are needed only for source builds or non-bundled features.

- `libpdfium.so` — PDF preview via `pdfium-render`. Arch: `yay -S pdfium-bin`. `runtime_assets::bind_pdfium` prefers a bundled `libpdfium.so`, else `Pdfium::bind_to_system_library()`.
- `dict` (dictd) — dictionary lookups. Silently disabled if absent. Not bundled.
- `cliphist` + `wl-clipboard` — clipboard history provider. Not bundled.
- `poppler` — PDF content indexing (not preview). Bundled in packages; system binary for source builds.
- `tesseract` data — OCR (always compiled in). English bundled; other languages need system `tesseract-data-<lang>`.
- `libgtk-layer-shell` — wlr-layer-shell overlay for the launcher (`layer_shell.rs`, opt-in via `[general] layer_shell`). Build-time requirement on Linux. Arch: `gtk-layer-shell`; Debian: `libgtk-layer-shell-dev`. Wayland-only at runtime; no-op on X11.

## Testing conventions

Never use real personal data (names, emails) in test fixtures — use neutral strings (`café`, `naïve`, `example.org`). When debugging, don't create new scratch files/dirs in the repo; edit existing code or reason from it.
