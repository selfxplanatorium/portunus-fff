use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::Deserialize;

const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

// ── top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub providers: ProvidersConfig,
    pub files: FilesConfig,
    pub search: SearchConfig,
    pub ranking: RankingConfig,
    pub frecency: FrecencyConfig,
    pub debug: DebugConfig,
    pub content: ContentConfig,
    pub appearance: AppearanceConfig,
    pub dict: DictConfig,
    pub clipboard: ClipboardConfig,
    pub extensions: ExtensionsConfig,
    pub calc: CalcConfig,
    pub marketplace: MarketplaceConfig,
    pub keybinds: KeybindsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            providers: ProvidersConfig::default(),
            files: FilesConfig::default(),
            search: SearchConfig::default(),
            ranking: RankingConfig::default(),
            frecency: FrecencyConfig::default(),
            debug: DebugConfig::default(),
            content: ContentConfig::default(),
            appearance: AppearanceConfig::default(),
            dict: DictConfig::default(),
            clipboard: ClipboardConfig::default(),
            extensions: ExtensionsConfig::default(),
            calc: CalcConfig::default(),
            marketplace: MarketplaceConfig::default(),
            keybinds: KeybindsConfig::default(),
        }
    }
}

/// User keybinds (see the commented `[keybinds]` docs in default_config.toml).
/// Absent key = default binding; empty list (`""`/`[]`) = explicitly cleared.
/// Chord strings are stored opaquely - the backend never validates them on
/// load; the Settings UI and launcher own the grammar (`keybinds.rs` clamps
/// only extension-shipped chords).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct KeybindsConfig {
    /// Built-in launcher chords by builtin id ("quick-look", "contents", …).
    pub builtin: std::collections::HashMap<String, ChordList>,
    /// Command bindings by catalog id ("cmd:settings") - fire anywhere.
    pub commands: std::collections::HashMap<String, ChordList>,
    /// Result-action bindings by action id ("ext:ytm:queue_last").
    pub actions: std::collections::HashMap<String, ChordList>,
}

/// Chords bound to one keybind target. In TOML the value is a single chord
/// string or an array; `""` normalizes to the empty list ("cleared").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChordList(pub Vec<String>);

impl<'de> Deserialize<'de> for ChordList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Compat {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Compat::deserialize(d)? {
            Compat::One(s) if s.is_empty() => ChordList(Vec::new()),
            Compat::One(s) => ChordList(vec![s]),
            Compat::Many(v) => ChordList(v),
        })
    }
}

impl serde::Serialize for ChordList {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // A single chord serializes as a plain string to keep config.toml
        // tidy; zero (cleared) or several round-trip as an array.
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            many => many.serialize(s),
        }
    }
}

/// Extension marketplace settings.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MarketplaceConfig {
    /// URL of the marketplace index. Override only for testing a local or
    /// forked index; `file://` paths are honored for custom URLs.
    pub index_url: String,
}

impl Default for MarketplaceConfig {
    fn default() -> Self {
        Self {
            index_url: crate::extensions::marketplace::DEFAULT_INDEX_URL.to_string(),
        }
    }
}

/// Calculator provider settings (the provider itself is toggled via [providers]).
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct CalcConfig {
    /// Enable currency conversion (fetches exchange rates in the background).
    pub currency: bool,
    /// Refetch exchange rates when the cached ones are older than this.
    pub rate_max_age_hours: u64,
}

impl Default for CalcConfig {
    fn default() -> Self {
        Self {
            currency: true,
            rate_max_age_hours: 24,
        }
    }
}

/// Clipboard history browser settings.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// "auto" = on Enter, paste into the previously focused window (synthesizes
    /// Ctrl+V via wtype); "copy" = only copy to the clipboard and let the user
    /// paste manually. Falls back to copy when wtype is unavailable.
    pub paste_mode: String,
    /// Maximum number of entries loaded into the clipboard browser.
    pub max_entries: usize,
    /// OCR copied images so their visible text is searchable in the browser.
    /// Each image is OCR'd once and cached (see `clipboard_ocr.rs`). The OCR
    /// language is the shared `content.ocr_language` (single canonical setting).
    pub ocr_images: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            paste_mode: "auto".to_string(),
            max_entries: 250,
            ocr_images: true,
        }
    }
}

/// Per-extension state, keyed by extension name:
///
/// ```toml
/// [extensions.emoji]
/// enabled = true
/// [extensions.emoji.settings]
/// skin_tone = "medium"
/// ```
///
/// A discovered extension absent from the map is DISABLED - dropping a folder
/// into the extensions dir must never run code until the user reviews its
/// permissions and enables it in Settings.
pub type ExtensionsConfig = std::collections::HashMap<String, ExtensionEntry>;

#[derive(Debug, Clone, PartialEq, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ExtensionEntry {
    pub enabled: bool,
    /// Values for the extension's `[[settings]]` schema. Validated against
    /// the schema when applied; unknown keys are ignored, never fatal.
    pub settings: toml::Table,
}

// ── sections ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub max_results: usize,
    /// Set to true once the first-launch onboarding wizard has been completed
    /// (or skipped). When false, the launcher shows the wizard on next show.
    pub onboarding_completed: bool,
    /// Use a wlr-layer-shell overlay surface on Wayland for compositor-agnostic
    /// always-on-top launcher behavior. Linux/Wayland only; takes effect on restart.
    pub layer_shell: bool,
    /// Icon theme used to resolve app icons. `None`/empty auto-detects from the
    /// GTK settings, then gsettings, then falls back to `hicolor`. Skipped on
    /// write when unset so the file keeps no `icon_theme = ""` noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_theme: Option<String>,
    /// Check GitHub for a newer Portunus release and show it in Settings →
    /// About. Notify only - nothing is ever downloaded or installed. Sends one
    /// unauthenticated GET to api.github.com per interval; set false to opt out
    /// of any network contact.
    pub check_for_updates: bool,
    /// Hours between automatic release checks. The last check time is persisted
    /// in `update-check.json`, so a restart doesn't re-check.
    pub update_check_interval_hours: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_results: 20,
            onboarding_completed: false,
            layer_shell: true,
            icon_theme: None,
            check_for_updates: true,
            update_check_interval_hours: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub apps: bool,
    pub files: bool,
    pub calc: bool,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            apps: true,
            files: true,
            calc: true,
        }
    }
}

/// Dictionary provider settings. Moved out of `[providers]` so the sparse-fill
/// behavior and scoring constants are tunable.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DictConfig {
    /// Master on/off for the dict provider (explicit lookups + fill).
    pub enabled: bool,
    /// Fill sparse result lists with dictionary matches for the typed word.
    pub fill_sparse: bool,
    /// Allow edit-distance (typo) matches when filling; false = exact lemma only.
    pub correct_misspellings: bool,
    /// Ctrl+C on a dict result copies the first definition; false = copies the word.
    pub copy_definition: bool,
    /// Only fill when fewer than this many non-dict results exist.
    pub fill_threshold: usize,
    /// Max dictionary rows added when filling.
    pub fill_max: usize,
}

impl Default for DictConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fill_sparse: true,
            correct_misspellings: true,
            copy_definition: true,
            fill_threshold: 3,
            fill_max: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct DirEntry {
    pub path: String,
    /// Keep the index current with per-directory inotify watches. Off for huge
    /// trees that rarely change (`/`): they are re-walked on startup, on a
    /// config change and on `--reindex` instead of costing an inotify watch per
    /// directory.
    #[serde(default = "default_true")]
    pub watch: bool,
}

fn default_true() -> bool {
    true
}

fn default_depth() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct FilesConfig {
    /// Roots indexed by fff, crawled to full depth. `/` stands for every
    /// top-level directory except pseudo filesystems (/proc, /sys, ...).
    pub dirs: Vec<DirEntry>,
    pub show_dotfiles: bool,
    pub colored_icons: bool,
    /// Extra directory names hidden from results, matched exactly against a
    /// path component. fff already prunes gitignored trees and common
    /// build/cache dirs during its walk.
    pub ignore: Vec<String>,
}

/// Names that are almost always machine-generated. Deliberately conservative:
/// each one is a build/cache convention, not a plausible name for a document
/// directory the user would want to find.
fn default_ignore() -> Vec<String> {
    ["node_modules", "target", ".git", ".cache", "__pycache__", ".venv"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            dirs: vec![
                DirEntry { path: "~".into(), watch: true },
                DirEntry { path: "/".into(), watch: false },
            ],
            show_dotfiles: false,
            colored_icons: true,
            ignore: default_ignore(),
        }
    }
}

impl FilesConfig {
    /// True when the fields that decide which pickers run match. Display and
    /// per-search filters (`colored_icons`, `show_dotfiles`, `ignore`) apply on
    /// the next keystroke without touching the index.
    pub fn index_eq(&self, other: &Self) -> bool {
        self.dirs == other.dirs
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Minimum fuzzy match quality, 0.0-1.0. Applied as a fraction of FUZZY_REFERENCE.
    pub min_quality: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self { min_quality: 0.06 }
    }
}

/// User-tunable ranking: category priority/weights, match-quality boosts,
/// match-vs-history balance, per-extension weights. Resolved into
/// `providers::ranking::RankingWeights` on load/reload; every field applies
/// live with no provider rebuild.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct RankingConfig {
    /// Category priority, first = highest band. Known keys: calc, app,
    /// command, extension, file, dict. Unknown keys are ignored; missing
    /// categories append in default order.
    pub category_order: Vec<String>,
    /// 0 = pure match quality, 100 = pure launch history. 50 keeps both at
    /// their classic strengths.
    pub match_vs_history: u8,
    /// Per-category weight, 0-100, 50 neutral (±half band). 0 hides the
    /// category from root search (scoped access still works).
    pub category_weights: std::collections::HashMap<String, u8>,
    pub match_boost: MatchBoostConfig,
    /// Per-extension weight by extension name, same semantics as
    /// category_weights, applied within the extension band.
    pub extension_weights: std::collections::HashMap<String, u8>,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            category_order: vec![
                "calc".into(),
                "app".into(),
                "command".into(),
                "extension".into(),
                "file".into(),
                "dict".into(),
            ],
            match_vs_history: 50,
            category_weights: std::collections::HashMap::new(),
            match_boost: MatchBoostConfig::default(),
            extension_weights: std::collections::HashMap::new(),
        }
    }
}

/// Title match-quality boosts, 0-100 each; every point adds 100k score
/// (1 band = 1M = 10 points). Defaults are sized so exact (7M) beats any
/// non-pinned rival even from the lowest band (worst case: top band 6M +
/// word-start 400k + max fuzzy/frecency ≈ 7.9M < 1M + 7M) and prefix (2.5M)
/// jumps a couple of bands, while word-start (400k) only breaks ties within
/// neighboring bands.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MatchBoostConfig {
    pub exact: u8,
    pub prefix: u8,
    pub word_start: u8,
}

impl Default for MatchBoostConfig {
    fn default() -> Self {
        Self { exact: 70, prefix: 25, word_start: 4 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct DebugConfig {
    pub log_scores: bool,
    pub log_watcher: bool,
    pub log_pdf: bool,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self { log_scores: false, log_watcher: false, log_pdf: false }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct FrecencyConfig {
    pub enabled: bool,
    pub half_life_days: f32,
}

impl Default for FrecencyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            half_life_days: 14.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct ContentDirEntry {
    pub path: String,
    #[serde(default = "default_depth")]
    pub depth: usize,
    pub extensions: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ContentConfig {
    pub enabled: bool,
    pub dirs: Vec<ContentDirEntry>,
    pub extensions: Vec<String>,
    pub max_file_bytes: u64,
    pub ocr_images: bool,
    pub ocr_pdf_fallback: bool,
    pub ocr_language: String,
    pub threads: usize,
    /// Highlight matched terms over OCR'd image previews (Contents mode). Preview-only:
    /// does not change what is indexed, so toggling never triggers a reindex (excluded
    /// from `contents_eq`). When off, `image_match_rects` returns no boxes.
    pub ocr_highlight: bool,
    /// Cache OCR word boxes in the index at index time so image highlights are instant
    /// (no per-preview OCR). HEAVY: changes what indexing stores, so it is part of
    /// `contents_eq` and toggling forces a full reindex (re-OCRs images). Only meaningful
    /// when `ocr_highlight` is also on.
    pub ocr_highlight_cache: bool,
}

impl ContentConfig {
    /// True if the two configs would produce an identical index. Compares every
    /// field that affects *what* gets indexed, ignoring `threads` (which only
    /// affects indexing speed). A threads-only change must not trigger a reindex.
    pub fn contents_eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.dirs == other.dirs
            && self.extensions == other.extensions
            && self.max_file_bytes == other.max_file_bytes
            && self.ocr_images == other.ocr_images
            && self.ocr_pdf_fallback == other.ocr_pdf_fallback
            && self.ocr_language == other.ocr_language
            // ocr_highlight is preview-only and deliberately excluded (no reindex).
            && self.ocr_highlight_cache == other.ocr_highlight_cache
    }
}

impl Default for ContentConfig {
    fn default() -> Self {
        let home = crate::paths::home();
        Self {
            enabled: false,
            dirs: vec![ContentDirEntry { path: format!("{home}/Documents"), depth: 3, extensions: None }],
            extensions: vec![
                "pdf".into(),
                "txt".into(), "md".into(), "rst".into(), "csv".into(), "log".into(),
                "toml".into(), "yaml".into(), "yml".into(), "json".into(), "xml".into(),
                "sh".into(), "py".into(), "rs".into(), "js".into(), "ts".into(),
                "go".into(), "c".into(), "cpp".into(), "h".into(),
                "jpg".into(), "jpeg".into(), "png".into(), "webp".into(),
                "bmp".into(), "tiff".into(), "tif".into(),
                "docx".into(), "xlsx".into(), "pptx".into(),
                "odt".into(), "ods".into(), "odp".into(),
            ],
            max_file_bytes: 5 * 1024 * 1024,
            ocr_images: true,
            ocr_pdf_fallback: true,
            ocr_language: "eng".to_string(),
            threads: 2,
            ocr_highlight: false,
            ocr_highlight_cache: false,
        }
    }
}

/// Result-list animation tier. Serialized lowercase ("off"/"slide"/"flip").
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultAnimation {
    Off,
    Slide,
    Flip,
}

/// Accept the legacy boolean form (`true` → Slide, `false` → Off) as well as the
/// new string enum, so upgrading doesn't fail the whole-config parse and reset
/// every setting to defaults.
fn de_result_animation<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ResultAnimation, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Compat {
        Bool(bool),
        Enum(ResultAnimation),
    }
    Ok(match Compat::deserialize(d)? {
        Compat::Bool(true) => ResultAnimation::Slide,
        Compat::Bool(false) => ResultAnimation::Off,
        Compat::Enum(e) => e,
    })
}

/// Per-item accent-bleed intensity. Serialized lowercase ("off"/"subtle"/"bold").
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccentBleed {
    Off,
    Subtle,
    Bold,
}

/// Accept the legacy boolean form (`true` → Subtle, `false` → Off) as well as
/// the new string enum, so upgrading doesn't fail the whole-config parse and
/// reset every setting to defaults.
fn de_accent_bleed<'de, D: serde::Deserializer<'de>>(d: D) -> Result<AccentBleed, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Compat {
        Bool(bool),
        Enum(AccentBleed),
    }
    Ok(match Compat::deserialize(d)? {
        Compat::Bool(true) => AccentBleed::Subtle,
        Compat::Bool(false) => AccentBleed::Off,
        Compat::Enum(e) => e,
    })
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: String,
    pub font_size: u32,
    #[serde(deserialize_with = "de_result_animation")]
    pub animate_results: ResultAnimation,
    pub show_metadata: bool,
    /// Slide a single highlight layer between rows instead of a static per-row one.
    pub slide_selection: bool,
    /// Film-grain noise overlay opacity (0.0 = off .. 0.25 = strong).
    pub grain: f32,
    /// Per-item accent bleed: tint the selected item + its preview with the
    /// dominant color sampled from its own icon/album art.
    #[serde(deserialize_with = "de_accent_bleed")]
    pub accent_bleed: AccentBleed,
    /// Launcher UI font family; empty = the built-in system stack.
    pub font_family: String,
    /// Monospace font for code, paths and key hints; empty = built-in stack.
    pub mono_font_family: String,
    /// Opacity of the launcher's background surfaces (0.3 .. 1.0). Text,
    /// icons and selection stay opaque.
    pub opacity: f32,
    /// Ask the compositor to blur behind translucent surfaces (see
    /// `layer_shell::apply_compositor_blur`).
    pub blur: bool,
    /// Animate the launcher window: entrance on show, and the morph between
    /// the bare search bar and the full results window. Off = instant.
    pub window_animations: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "warm-dark".to_string(),
            font_size: 13,
            animate_results: ResultAnimation::Slide,
            show_metadata: true,
            slide_selection: true,
            grain: 0.07,
            accent_bleed: AccentBleed::Subtle,
            font_family: String::new(),
            mono_font_family: String::new(),
            opacity: 1.0,
            blur: false,
            window_animations: true,
        }
    }
}

// ── shared runtime config ─────────────────────────────────────────────────────

/// Per-search scalars shared across all indexed providers.
/// Updated in-place on config reload; providers read it per search() call.
#[derive(Debug, Clone)]
pub struct SharedSearchConfig {
    pub min_quality: f32,
    pub show_dotfiles: bool,
    /// `[files] ignore`: filtered per search, so editing it needs no re-walk.
    pub files_ignore: Vec<String>,
    pub log_scores: bool,
    pub log_watcher: bool,
    pub log_pdf: bool,
}

pub type SharedConfig = Arc<RwLock<SharedSearchConfig>>;

impl SharedSearchConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            min_quality: cfg.search.min_quality,
            show_dotfiles: cfg.files.show_dotfiles,
            files_ignore: cfg.files.ignore.clone(),
            log_scores: cfg.debug.log_scores,
            log_watcher: cfg.debug.log_watcher,
            log_pdf: cfg.debug.log_pdf,
        }
    }

    pub fn update_from(&mut self, cfg: &Config) {
        self.min_quality = cfg.search.min_quality;
        self.show_dotfiles = cfg.files.show_dotfiles;
        self.files_ignore = cfg.files.ignore.clone();
        self.log_scores = cfg.debug.log_scores;
        self.log_watcher = cfg.debug.log_watcher;
        self.log_pdf = cfg.debug.log_pdf;
    }
}

// ── loader ────────────────────────────────────────────────────────────────────

/// Set when `load()` finds an unparseable config file. Drained by the
/// `take_config_error` command so the settings UI can warn the user that their
/// file failed to parse (and was backed up) rather than silently resetting it.
pub static LAST_LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&path, DEFAULT_CONFIG) {
                    Ok(_) => eprintln!("[config] wrote default config to {}", path.display()),
                    Err(e) => eprintln!("[config] could not write default config to {}: {e}", path.display()),
                }
                // Return the parsed bundled config, not Self::default(): the two
                // diverge (e.g. content dirs), so returning defaults would make the
                // first session behave differently from every restart afterwards.
                return toml::from_str::<Config>(DEFAULT_CONFIG).unwrap_or_default();
            }
        };
        match toml::from_str::<Config>(&content) {
            Ok(mut cfg) => {
                // Pre-v2 configs stored `[extensions.enabled] name = bool`; under
                // the per-extension map that parses as a junk extension literally
                // named "enabled". Drop it leniently (an extension can't be named
                // "enabled" without colliding here anyway) - never fail the whole
                // config over the old shape.
                if cfg.extensions.remove("enabled").is_some() {
                    eprintln!(
                        "[config] dropped pre-v2 [extensions.enabled] table - re-enable extensions in Settings"
                    );
                }
                eprintln!("[config] loaded from {}", path.display());
                cfg
            }
            Err(e) => {
                eprintln!("[config] failed to parse {}: {e} - using defaults", path.display());
                // Preserve the unparseable file so a clobbering autosave can't destroy
                // it, and record the error so the settings UI can warn the user rather
                // than silently resetting every setting to defaults.
                let backup = path.with_extension("toml.bak");
                let detail = match std::fs::copy(&path, &backup) {
                    Ok(_) => {
                        eprintln!("[config] backed up unparseable config to {}", backup.display());
                        format!("{e}\n\nYour previous config was backed up to {}.", backup.display())
                    }
                    Err(err) => {
                        eprintln!("[config] could not back up config: {err}");
                        format!("{e}")
                    }
                };
                if let Ok(mut slot) = LAST_LOAD_ERROR.lock() {
                    *slot = Some(detail);
                }
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize config: {e}"))?;
        let path = config_path();
        std::fs::write(&path, toml_str)
            .map_err(|e| format!("failed to write config to {}: {e}", path.display()))
    }

    /// Expand a path string: replace a leading `~` with $HOME.
    pub fn expand_path(p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/") {
            PathBuf::from(crate::paths::home()).join(rest)
        } else if p == "~" {
            PathBuf::from(crate::paths::home())
        } else {
            PathBuf::from(p)
        }
    }
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_CONFIG_HOME/portunus` (or `~/.config/portunus`). Holds `config.toml` and
/// the external `matugen.css` theme file.
pub fn config_dir() -> PathBuf {
    crate::paths::config_dir()
}
