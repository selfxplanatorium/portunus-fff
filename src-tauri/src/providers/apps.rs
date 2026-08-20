use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{LazyLock, Mutex, OnceLock};

use tauri::Manager;

use super::{dominant_color, icon_theme, ranking, Provider, SearchResult};
use crate::config::SharedConfig;

/// App handle used to widen the asset-protocol scope to the icons we resolve.
/// Unset on CLI paths that never serve icons - those skip the registration.
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

pub fn set_app_handle(app: tauri::AppHandle) {
    let _ = APP.set(app);
}

/// Let the WebView serve the icons we resolved. `assetProtocol.scope` in
/// tauri.conf.json only covers the usual distro roots, but icons live wherever
/// the packaging puts them: the nix store (reached via `XDG_DATA_DIRS`), or an
/// app's own data dir (JetBrains Toolbox ships icons under
/// ~/.local/share/JetBrains/…). Allow those exact files - the alternative,
/// copying them into a servable directory, means a disk copy per icon on any
/// system whose icons all sit outside the static roots.
fn allow_icons(apps: &[DesktopEntry]) {
    let Some(app) = APP.get() else { return };
    let scope = app.asset_protocol_scope();
    for path in apps.iter().filter_map(|a| a.icon_path.as_deref()) {
        // Paths the static scope already covers need no pattern of their own,
        // so the common case adds nothing to match against.
        if !scope.is_allowed(path) {
            if let Err(e) = scope.allow_file(path) {
                eprintln!("[portunus] icon scope: {path}: {e}");
            }
        }
    }
}

/// Memoized icon-path → dominant color. Decoding an icon is done once on the
/// background load thread; a config reload rebuilds the app list but reuses the
/// cached colors instead of re-decoding.
static COLOR_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn dominant_color_for(path: &str) -> Option<String> {
    if let Some(hit) = COLOR_CACHE.lock().unwrap().get(path) {
        return hit.clone();
    }
    let color = dominant_color::extract_from_path(std::path::Path::new(path));
    COLOR_CACHE.lock().unwrap().insert(path.to_string(), color.clone());
    color
}

// ── data types ───────────────────────────────────────────────────────────────

#[derive(Debug)]
struct DesktopEntry {
    name: String,
    #[allow(dead_code)]
    exec: String,
    /// Absolute path of the `.desktop` file this entry came from. Handed to the
    /// frontend so `launch_app` can launch it through GIO (field codes,
    /// `Terminal=`, D-Bus activation) instead of spawning `exec` raw.
    desktop_file: String,
    description: Option<String>,
    icon_path: Option<String>,
    dominant_color: Option<String>,
}

/// Intermediate: raw fields straight from the .desktop file, icon not yet resolved.
struct ParsedEntry {
    name: String,
    exec: String,
    desktop_file: String,
    description: Option<String>,
    icon_name: Option<String>,
}

// ── provider ─────────────────────────────────────────────────────────────────

pub struct AppProvider {
    apps: Vec<DesktopEntry>,
    shared: SharedConfig,
}

impl AppProvider {
    /// `icon_theme` is the configured `[general] icon_theme`; `None` auto-detects.
    /// It is passed explicitly rather than read from `shared`, which is the
    /// narrow search-time snapshot and has no business carrying load-time input.
    pub fn new(shared: SharedConfig, icon_theme: Option<&str>) -> Self {
        Self { apps: load_apps(icon_theme), shared }
    }
}

// ── loading ───────────────────────────────────────────────────────────────────

/// Read every visible `.desktop` file, deduped by file stem.
fn parse_all_entries() -> Vec<(String, ParsedEntry)> {
    let current_desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut parsed = Vec::new();

    for data_dir in crate::paths::xdg_data_dirs() {
        let apps_dir = data_dir.join("applications");
        if !apps_dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&apps_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            // Dedup by file stem (XDG spec): user dirs take priority over system dirs.
            let stem = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !seen.insert(stem.clone()) {
                continue;
            }
            if let Some(e) = parse_desktop(entry.path(), &current_desktop) {
                parsed.push((stem, e));
            }
        }
    }
    parsed
}

/// Icon names to look up for one entry, in preference order. The `-symbolic`
/// variant covers themes whose only artwork for a name is monochrome (modern
/// Adwaita ships app icons exclusively that way); the `.desktop` stem is the
/// XDG convention fallback for entries whose `Icon` field is missing or dead
/// (e.g. JetBrains Toolbox).
fn icon_candidates(stem: &str, icon_name: Option<&str>) -> Vec<String> {
    let mut names = Vec::with_capacity(3);
    if let Some(raw) = icon_name {
        let base = icon_theme::strip_icon_extension(raw);
        if !base.is_empty() && !raw.starts_with('/') {
            names.push(base.to_string());
            if !base.ends_with("-symbolic") {
                names.push(format!("{base}-symbolic"));
            }
        }
    }
    names.push(stem.to_string());
    names
}

fn load_apps(preferred_theme: Option<&str>) -> Vec<DesktopEntry> {
    let parsed = parse_all_entries();

    // Two-phase: collect the names we need, then resolve them in one pass over
    // the theme chain. Probing per name would cost hundreds of thousands of
    // syscalls on a deep inherit chain.
    let resolver = icon_theme::IconResolver::new(preferred_theme);
    let wanted: HashSet<String> = parsed
        .iter()
        .flat_map(|(stem, e)| icon_candidates(stem, e.icon_name.as_deref()))
        .collect();
    let resolved = resolver.resolve_all(&wanted);

    let mut unresolved: Vec<&str> = Vec::new();
    let mut apps: Vec<DesktopEntry> = parsed
        .iter()
        .map(|(stem, parsed)| {
            // An absolute `Icon=` path is the icon the app intends, so it wins
            // over anything the theme offers under the same stem.
            let icon_path = parsed
                .icon_name
                .as_deref()
                .filter(|n| n.starts_with('/'))
                .and_then(resolve_absolute_icon)
                .or_else(|| {
                    icon_candidates(stem, parsed.icon_name.as_deref())
                        .iter()
                        .find_map(|n| resolved.get(n).cloned())
                });
            if icon_path.is_none() {
                unresolved.push(parsed.icon_name.as_deref().unwrap_or(stem));
            }
            // Sample the icon's dominant color here on the background load
            // thread (memoized) so the search path only clones a String.
            let dominant_color = icon_path.as_deref().and_then(dominant_color_for);
            DesktopEntry {
                name: parsed.name.clone(),
                exec: parsed.exec.clone(),
                desktop_file: parsed.desktop_file.clone(),
                description: parsed.description.clone(),
                icon_path,
                dominant_color,
            }
        })
        .collect();

    if !unresolved.is_empty() {
        unresolved.sort_unstable();
        eprintln!(
            "[portunus] icons: theme={} unresolved={}/{}: {}",
            resolver.theme(),
            unresolved.len(),
            apps.len(),
            unresolved.join(", ")
        );
    }

    allow_icons(&apps);
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}

/// Resolve an absolute `Icon=` path. A dead path returns `None` so the caller
/// falls through to the theme lookup - guessing from the file stem would let a
/// name like "toolbox" collide with an unrelated theme icon.
fn resolve_absolute_icon(icon: &str) -> Option<String> {
    let p = std::path::Path::new(icon);
    let hit = if p.exists() {
        Some(p.to_path_buf())
    } else {
        // Some entries name an extensionless absolute path.
        ["svg", "png"]
            .iter()
            .map(|ext| p.with_extension(ext))
            .find(|q| q.exists())
    }?;
    let resolved = fs::canonicalize(&hit).unwrap_or(hit);
    Some(resolved.to_string_lossy().into_owned())
}

// ── .desktop parser ───────────────────────────────────────────────────────────

fn parse_desktop(path: &std::path::Path, current_desktop: &str) -> Option<ParsedEntry> {
    let content = fs::read_to_string(path).ok()?;

    let mut in_entry = false;
    let mut fields: HashMap<String, String> = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line == "[Desktop Entry]" {
            in_entry = true;
            continue;
        }
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            continue;
        }
        if in_entry && !line.starts_with('#') {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                if !k.contains('[') {
                    fields.insert(k.to_string(), v.trim().to_string());
                }
            }
        }
    }

    if fields.get("Type").map(String::as_str) != Some("Application") {
        return None;
    }
    if fields.get("NoDisplay").map(String::as_str) == Some("true") {
        return None;
    }
    if fields.get("Hidden").map(String::as_str) == Some("true") {
        return None;
    }

    // XDG_CURRENT_DESKTOP is colon-separated; OnlyShowIn/NotShowIn are semicolon-separated.
    if !current_desktop.is_empty() {
        let desktops: Vec<&str> = current_desktop.split(':').collect();
        if let Some(only_in) = fields.get("OnlyShowIn") {
            let allowed: Vec<&str> = only_in.split(';').filter(|s| !s.is_empty()).collect();
            if !desktops.iter().any(|d| allowed.contains(d)) {
                return None;
            }
        }
        if let Some(not_in) = fields.get("NotShowIn") {
            let blocked: Vec<&str> = not_in.split(';').filter(|s| !s.is_empty()).collect();
            if desktops.iter().any(|d| blocked.contains(d)) {
                return None;
            }
        }
    }

    let name = fields.remove("Name")?;
    let exec = fields.remove("Exec")?;
    let description = fields
        .remove("Comment")
        .or_else(|| fields.remove("GenericName"));
    let icon_name = fields.remove("Icon");

    Some(ParsedEntry {
        name,
        exec,
        desktop_file: path.to_string_lossy().into_owned(),
        description,
        icon_name,
    })
}

// ── Provider impl ─────────────────────────────────────────────────────────────

impl Provider for AppProvider {
    fn id(&self) -> &str {
        "apps"
    }

    fn search(&self, query: &str) -> Vec<SearchResult> {
        let q = query.trim();
        if q.is_empty() {
            return vec![];
        }

        let cfg = self.shared.read().unwrap();
        let min_quality = cfg.min_quality;
        let log_scores = cfg.log_scores;
        drop(cfg);

        let (pattern, mut matcher, mut char_buf) = super::fuzzy_setup(query);

        let threshold = super::quality_threshold(min_quality, query.chars().count());

        let mut candidates: Vec<(u32, SearchResult)> = self
            .apps
            .iter()
            .filter_map(|app| {
                // Match name (primary) and description (down-weighted). A
                // name hit stays app-band; a description-only hit demotes to
                // the file band so apps matched only by blurb don't outrank
                // apps matched by name.
                let (idx, score) = match app.description.as_deref() {
                    Some(desc) => super::fuzzy_best(
                        &pattern,
                        &mut matcher,
                        &mut char_buf,
                        &[(app.name.as_str(), 1.0), (desc, 0.8)],
                    ),
                    None => super::fuzzy_best(
                        &pattern,
                        &mut matcher,
                        &mut char_buf,
                        &[(app.name.as_str(), 1.0)],
                    ),
                }?;
                let parts = if idx == 0 {
                    ranking::ScoreParts::new(
                        ranking::Category::App,
                        ranking::detect_tier(&app.name, q),
                        score,
                    )
                } else {
                    // Description-only hit: demote to the file band with no
                    // tier boost, so blurb matches don't outrank name matches.
                    ranking::ScoreParts::new(
                        ranking::Category::File,
                        ranking::MatchTier::Fuzzy,
                        score,
                    )
                };
                Some((score, SearchResult {
                    id: format!("app:{}", app.name),
                    title: app.name.clone(),
                    subtitle: app.description.clone(),
                    kind: "app".to_string(),
                    exec: Some(app.exec.clone()),
                    desktop_file: Some(app.desktop_file.clone()),
                    icon_path: app.icon_path.clone(),
                    dominant_color: app.dominant_color.clone(),
                    parts: Some(parts),
                    ..Default::default()
                }))
            })
            .collect();

        candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0));

        // Adaptive floor: relax threshold so top 3 always survive.
        let floor = candidates.get(2).map(|c| c.0).unwrap_or(0) as f32;
        let effective = threshold.min(floor);

        candidates
            .into_iter()
            .filter(|(score, _)| {
                if log_scores {
                    eprintln!(
                        "[apps] nucleo={} effective_threshold={:.1}",
                        score, effective
                    );
                }
                (*score as f32) >= effective
            })
            .map(|(_, result)| result)
            .collect()
    }
}
