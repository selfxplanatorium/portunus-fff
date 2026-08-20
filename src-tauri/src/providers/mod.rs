pub mod apps;
pub mod breaker;
pub mod calc;
pub mod clipboard;
pub mod command;
pub mod content;
pub mod dict;
pub mod dominant_color;
pub mod files;
pub mod icon_theme;
pub mod marketplace;
pub mod ranking;
pub mod wasm;

pub use command::CommandDescriptor;

use std::collections::HashMap;
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::frecency::FrecencyStore;

// ── Fixed scope-only bases ────────────────────────────────────────────────────
// Root-search categories (calc/app/command/extension/file/dict-fill) are banded
// from `[ranking] category_order` - see `ranking::RankingWeights`. The
// constants below belong to scoped tiers that never compete in root search.

pub const SCORE_CONTENT: f32 = 6_000_000.0;
/// Marketplace scope rows (browse/search the extension index).
pub const SCORE_MARKETPLACE: f32 = 5_000_000.0;
/// Scoped dict lookups (the user entered the Define Word scope).
pub const SCORE_DICT: f32 = 3_000_000.0;
/// Scoped extension results - the user explicitly entered the extension's
/// scope, so results rank on the extension's own relevance, not the root bands.
pub const SCORE_EXTENSION_TRIGGERED: f32 = 4_000_000.0;
/// Width of the band extension relevance (0-100) maps into, on top of the
/// extension category band. Matches FUZZY_MAX_BONUS so relevance and fuzzy
/// quality are on the same scale.
pub const EXTENSION_BAND: f32 = 300_000.0;

// ── File-result penalties (down-rank low-value hits within the file band) ─────
/// Subtracted from a file result that has no preview renderer (archives, video,
/// audio, unknown binaries). Same scale as FUZZY_MAX_BONUS so it outweighs fuzzy
/// quality but never crosses into the folder/app band.
pub const PENALTY_NO_PREVIEW: f32 = 300_000.0;
/// Subtracted from a result whose path has any dot-prefixed component
/// (hidden dir or dotfile). Config/cache noise sinks below normal hits.
pub const PENALTY_HIDDEN: f32 = 200_000.0;

// ── Scoring normalisation constants ──────────────────────────────────────────

/// Nucleo score at which fuzzy bonus is fully awarded. Scores above this are clamped.
pub const FUZZY_REFERENCE: f32 = 1500.0;
/// Max bonus added to a discoverable result's score for a perfect fuzzy match.
pub const FUZZY_MAX_BONUS: f32 = 300_000.0;
/// Frecency score (raw launches after decay) that maps to 100% of history bonus.
pub const FRECENCY_REFERENCE: f32 = 40.0;

/// Score threshold (in nucleo score units) that scales with query length.
/// Two-phase ramp so the jump to full threshold is gradual:
///   len 1 →   0%    len 2 → 33%    len 3 → 66%
///   len 4 →  77%    len 5 → 88%    len 6+ → 100%
pub fn quality_threshold(min_quality: f32, query_len: usize) -> f32 {
    let base = min_quality * FUZZY_REFERENCE;
    if query_len <= 1 {
        return 0.0;
    }
    if query_len <= 3 {
        let factor = (query_len - 1) as f32;
        return base * factor / 3.0;
    }
    let extra = (query_len - 3).min(3) as f32;
    let low = base * 2.0 / 3.0;
    low + (base - low) * extra / 3.0
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// FTS snippet with \x02/\x03 as highlight start/end markers.
    pub snippet: Option<String>,
    pub kind: String,
    pub score: f32,
    pub exec: Option<String>,
    /// Absolute path of the `.desktop` file behind an app result. The frontend
    /// hands it back to `launch_app`, which launches it through GIO - `exec` is
    /// only the fallback for when that fails. Apps-provider results only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_file: Option<String>,
    pub icon_path: Option<String>,
    /// Pre-built `data:` URI for a validated extension-supplied icon. The
    /// frontend renders it directly; never read `ext.icon` (unvalidated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_data_uri: Option<String>,
    pub file_size: Option<u64>,
    pub created: Option<u64>,
    pub modified: Option<u64>,
    /// Dominant color (#rrggbb) sampled from this result's icon/album art, for the
    /// accent-bleed look. None → frontend falls back to the theme accent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dominant_color: Option<String>,
    /// 0-based page of a PDF where the content query mainly matched. Set only by
    /// the content provider for PDF results; drives which page the preview opens on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_page: Option<u32>,
    /// Original extension DTO for `ext:` results. The frontend passes it back
    /// verbatim on activate/preview so extensions stay stateless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<portunus_ext_sdk::ExtensionResult>,
    /// Name of the extension command that produced an `ext:` result; passed
    /// back on activate/preview so the guest can dispatch on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_command: Option<String>,
    /// The descriptor behind a `kind: "command"` entry row; the frontend uses
    /// it to enter the command's mode (or run it) on launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandDescriptor>,
    /// Payload of a `kind: "marketplace"` row - the index entry plus install
    /// state; the preview panel renders it as the consent surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market: Option<marketplace::MarketplaceResult>,
    /// Raw scoring inputs; the registry composes `score` from these against
    /// the live ranking weights. None (scoped/content rows) = the provider's
    /// own `score` stands. Host-internal, never serialized.
    #[serde(skip)]
    pub parts: Option<ranking::ScoreParts>,
    /// True when a pin for the typed query boosted this result to the top.
    pub pinned: bool,
    /// When true, this row opts out of frecency (no ranking bonus), set from
    /// the producing command's `frecency = false`. Host-internal, not serialized.
    #[serde(skip)]
    pub frecency_exempt: bool,
    /// Score composition, filled only by `search_explain` for the playground.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<ranking::ScoreBreakdown>,
}

impl Default for SearchResult {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            subtitle: None,
            snippet: None,
            kind: String::new(),
            score: 0.0,
            exec: None,
            desktop_file: None,
            icon_path: None,
            icon_data_uri: None,
            file_size: None,
            created: None,
            modified: None,
            dominant_color: None,
            match_page: None,
            ext: None,
            ext_command: None,
            command: None,
            market: None,
            parts: None,
            pinned: false,
            frecency_exempt: false,
            breakdown: None,
        }
    }
}

/// Returns a configured `(Pattern, Matcher, char_buf)` for fuzzy matching `query`.
/// The fuzzy providers (apps, files) use identical nucleo settings.
pub fn fuzzy_setup(query: &str) -> (nucleo_matcher::pattern::Pattern, nucleo_matcher::Matcher, Vec<char>) {
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher};
    let pattern = Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy);
    let matcher = Matcher::new(Config::DEFAULT);
    (pattern, matcher, Vec::new())
}

/// Best weighted nucleo score across `fields` (each a `(haystack, weight)`),
/// plus the index of the winning field. `weight` scales a field's raw score so
/// a secondary field (e.g. a description at 0.8) down-ranks against the primary
/// name. Returns `None` when nothing matched. Shared by the apps and command
/// matchers so both search identical nucleo settings across multiple fields.
pub fn fuzzy_best(
    pattern: &nucleo_matcher::pattern::Pattern,
    matcher: &mut nucleo_matcher::Matcher,
    buf: &mut Vec<char>,
    fields: &[(&str, f32)],
) -> Option<(usize, u32)> {
    let mut best: Option<(usize, u32)> = None;
    for (i, (hay, weight)) in fields.iter().enumerate() {
        if let Some(raw) = pattern.score(nucleo_matcher::Utf32Str::new(hay, buf), matcher) {
            let score = (raw as f32 * weight) as u32;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i, score));
            }
        }
    }
    best
}

/// Applies the frecency bonus in place on the weights' balance-scaled scale.
/// Content results are skipped by kind - they are already ranked by FTS5/BM25
/// relevance (bands are user-configurable, so a score threshold can't gate this).
pub fn apply_frecency_weights(
    results: &mut [SearchResult],
    scores: &HashMap<String, f32>,
    weights: &ranking::RankingWeights,
    explain: bool,
) {
    for r in results.iter_mut() {
        if r.kind == "content" || r.frecency_exempt {
            continue;
        }
        if let Some(&fs) = scores.get(&r.id) {
            let bonus = weights.frecency_bonus(fs);
            r.score += bonus;
            if explain {
                if let Some(b) = &mut r.breakdown {
                    b.frecency_bonus = bonus;
                }
            }
        }
    }
}

/// Boosts results matched by a pin for the typed query to the absolute top.
/// `pinned_ids` comes from `FrecencyStore::pin_bonus_ids(query)`: pins apply
/// while the user is still typing toward their query (typed is a prefix).
pub fn apply_pins(
    results: &mut [SearchResult],
    pinned_ids: &std::collections::HashSet<String>,
    explain: bool,
) {
    if pinned_ids.is_empty() {
        return;
    }
    for r in results.iter_mut() {
        if pinned_ids.contains(&r.id) {
            r.score += ranking::PIN_SCORE;
            r.pinned = true;
            if explain {
                if let Some(b) = &mut r.breakdown {
                    b.pin_bonus = ranking::PIN_SCORE;
                }
            }
        }
    }
}

/// Scores a streamed async-query batch with the exact same building blocks as
/// the sync keystroke path (`apply_ranking` → frecency → pins), so the two
/// paths cannot drift. The sync path inlines the same calls because dict
/// sparse-fill gating has to run between composition and frecency.
pub fn finalize_results(
    results: &mut Vec<SearchResult>,
    weights: &ranking::RankingWeights,
    frecency: Option<&FrecencyStore>,
    query: &str,
    drop_hidden: bool,
    explain: bool,
) {
    ranking::apply_ranking(results, weights, drop_hidden, explain);
    if let Some(store) = frecency {
        apply_frecency_weights(results, &store.all_scores(), weights, explain);
        apply_pins(results, &store.pin_bonus_ids(query), explain);
    }
}

pub trait Provider: Send + Sync {
    #[allow(dead_code)]
    fn id(&self) -> &str;
    fn search(&self, query: &str) -> Vec<SearchResult>;
    /// Commands this provider contributes to the catalog. Default: none.
    fn commands(&self) -> Vec<CommandDescriptor> {
        Vec::new()
    }
    /// Scoped search, called only while one of this provider's Scope commands
    /// is the active mode. `command_id` is the descriptor id; providers with a
    /// single scope can ignore it. Default: fall back to root search.
    fn search_scoped(&self, _command_id: &str, query: &str) -> Vec<SearchResult> {
        self.search(query)
    }
}

pub struct PluginRegistry {
    providers: Vec<Box<dyn Provider>>,
    /// WASM extensions, keyed by extension name. Kept out of `providers` so
    /// search can fan them out in parallel instead of paying each one's
    /// timeout sequentially per keystroke.
    extensions: HashMap<String, Arc<wasm::WasmProvider>>,
    max_results: usize,
    frecency: Option<Arc<FrecencyStore>>,
    /// Live ranking weights, shared with streamed async-query workers so a
    /// config edit re-scores the very next batch without respawning anything.
    ranking: Arc<RwLock<ranking::RankingWeights>>,
    /// (fill_threshold, fill_max) for dict sparse-fill gating. None disables
    /// gating (dict rows pass through untouched).
    dict_fill: Option<(usize, usize)>,
}

/// What `search_inner` skipped, for `search_explain`'s playground note.
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    /// Extensions that would have run on this query (gated) but were skipped.
    pub skipped_extensions: Vec<String>,
}

impl PluginRegistry {
    pub fn new(max_results: usize) -> Self {
        Self {
            providers: Vec::new(),
            extensions: HashMap::new(),
            max_results,
            frecency: None,
            ranking: Arc::new(RwLock::new(ranking::RankingWeights::default())),
            dict_fill: None,
        }
    }

    /// Adds, replaces, or (with None) removes an extension. The single
    /// mutation path for extension state - build instances before taking the
    /// write lock, this only swaps pointers.
    pub fn set_extension(&mut self, name: &str, provider: Option<Arc<wasm::WasmProvider>>) {
        // A reloaded/unloaded extension must not keep streaming from its old
        // instance - cancel its in-flight async query (the worker's own Arc
        // keeps the old provider alive until it unwinds).
        if let Some(qm) = crate::extensions::query::manager() {
            qm.cancel_ext(name);
        }
        match provider {
            Some(p) => {
                self.extensions.insert(name.to_string(), p);
            }
            None => {
                self.extensions.remove(name);
            }
        }
    }

    pub fn extension(&self, name: &str) -> Option<Arc<wasm::WasmProvider>> {
        self.extensions.get(name).cloned()
    }

    pub fn extension_names(&self) -> Vec<String> {
        self.extensions.keys().cloned().collect()
    }

    /// Configure dict sparse-fill gating: `(fill_threshold, fill_max)`, or None
    /// to disable gating.
    pub fn set_dict_fill(&mut self, fill: Option<(usize, usize)>) {
        self.dict_fill = fill;
    }

    pub fn register(&mut self, provider: impl Provider + 'static) {
        self.providers.push(Box::new(provider));
    }

    /// Replace a provider by id, or remove it if `new` is None.
    /// Acquires the write lock only for the retain+push (microseconds),
    /// so index building should happen before calling this.
    pub fn replace(&mut self, id: &str, new: Option<Box<dyn Provider>>) {
        self.providers.retain(|p| p.id() != id);
        if let Some(p) = new {
            self.providers.push(p);
        }
    }

    pub fn set_frecency(&mut self, store: Arc<FrecencyStore>) {
        self.frecency = Some(store);
    }

    /// Frecency store + live ranking weights, for scoring streamed async-query
    /// results with the exact same composition as the sync path.
    pub fn stream_params(
        &self,
    ) -> (Option<Arc<FrecencyStore>>, Arc<RwLock<ranking::RankingWeights>>) {
        (self.frecency.clone(), Arc::clone(&self.ranking))
    }

    pub fn update_settings(&mut self, max_results: usize) {
        self.max_results = max_results;
    }

    /// Swap in freshly resolved ranking weights (config load/reload).
    pub fn set_ranking_weights(&self, weights: ranking::RankingWeights) {
        *self.ranking.write().unwrap() = weights;
    }

    pub fn ranking_weights(&self) -> ranking::RankingWeights {
        self.ranking.read().unwrap().clone()
    }

    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let weights = self.ranking.read().unwrap().clone();
        self.search_inner(query, &weights, false, false).results
    }

    /// The full search pipeline. `skip_extensions` (playground) never touches
    /// wasm and instead reports which extensions would have run; `explain`
    /// fills per-result score breakdowns.
    pub fn search_inner(
        &self,
        query: &str,
        weights: &ranking::RankingWeights,
        skip_extensions: bool,
        explain: bool,
    ) -> SearchOutcome {
        // Fan extensions out first so their wall-clock overlaps the built-ins.
        // Threads are detached: a hung extension is abandoned at the deadline
        // (its own watchdog cancels it; repeated failures bench it), so the
        // keystroke path is bounded by one budget regardless of extension count.
        let mut skipped_extensions: Vec<String> = Vec::new();
        let pending = if self.extensions.is_empty() || query.is_empty() {
            None
        } else {
            let (tx, rx) = mpsc::channel();
            let mut budget_ms = 0;
            let mut spawned = 0;
            for (ext_name, ext) in &self.extensions {
                // Benched extensions would fail instantly anyway - don't pay
                // a thread spawn per keystroke for them.
                if ext.is_benched() {
                    continue;
                }
                // Root-search gating happens here, before any thread exists: a
                // keystroke with no `always` command spawns nothing and adds
                // nothing to the deadline budget. gate() resolves at most ONE
                // command per extension, preserving the one-thread-per-
                // extension invariant with multi-command manifests. Root results
                // are discovery-band (intent = false); explicit invocation is the
                // scope path (search_scope), which bands higher.
                let Some(gc) = ext.gate(query) else {
                    continue;
                };
                // The playground path never executes wasm - it only reports
                // which extensions a real search would have run.
                if skip_extensions {
                    skipped_extensions.push(ext_name.clone());
                    continue;
                }
                budget_ms = budget_ms.max(ext.search_budget_ms());
                let ext = ext.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    // Root fan-out: no scope frame, so the guest sees `scope: None`.
                    let _ = tx.send(ext.search_gated(gc, false, None));
                });
                spawned += 1;
            }
            (spawned > 0).then_some((rx, spawned, budget_ms + 50))
        };

        // The content provider runs only in its scope (search_scope); the
        // normal launcher never competes file-contents against names/apps.
        let mut results: Vec<SearchResult> = self
            .providers
            .iter()
            .filter(|p| p.id() != "content")
            .flat_map(|p| p.search(query))
            .collect();

        // Searchable command entries ("Define Word", "Clipboard History", …).
        results.extend(command::match_entries(&self.commands(), query));

        if let Some((rx, count, budget_ms)) = pending {
            let deadline = Instant::now() + Duration::from_millis(budget_ms);
            for _ in 0..count {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(remaining) {
                    Ok(batch) => results.extend(batch),
                    Err(_) => break,
                }
            }
        }

        // Compose scores + drop hidden categories first: the sparse-fill gate
        // below must count what the user will actually see.
        ranking::apply_ranking(&mut results, weights, true, explain);

        // Dict sparse-fill gating: dict rows are fill candidates - keep them
        // only when other results are sparse, capped at fill_max. (Explicit
        // lookups happen in the Dict scope, not root search, so there is no
        // longer a query shape that bypasses this.)
        if let Some((fill_threshold, fill_max)) = self.dict_fill {
            let is_dict = |r: &SearchResult| r.kind == "dict" || r.kind == "dict-hint";
            let non_dict = results.iter().filter(|r| !is_dict(r)).count();
            if non_dict >= fill_threshold {
                results.retain(|r| !is_dict(r));
            } else {
                let mut kept = 0;
                results.retain(|r| {
                    if !is_dict(r) {
                        return true;
                    }
                    kept += 1;
                    kept <= fill_max
                });
            }
        }

        // Frecency + pins before sort so heavily-used and pinned items surface.
        if let Some(store) = &self.frecency {
            apply_frecency_weights(&mut results, &store.all_scores(), weights, explain);
            apply_pins(&mut results, &store.pin_bonus_ids(query), explain);
        }

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Deduplicate: keep highest-scored occurrence (already first after sort).
        // Apps dedup by display title, not exec - GNOME's Evince/Papers split
        // ships two distinct binaries (`evince` vs `papers`) both named
        // "Document Viewer", and a second desktop-environment install multiplies
        // that across prefixes. Identical rows the user can't tell apart collapse
        // to the best-scored one. Other kinds keep the exec key.
        let mut seen = std::collections::HashSet::new();
        results.retain(|r| {
            let key = if r.kind == "app" {
                Some(format!("app-title:{}", r.title))
            } else {
                r.exec.clone()
            };
            match key {
                Some(k) => seen.insert(k),
                None => true,
            }
        });
        results.truncate(self.max_results);
        SearchOutcome { results, skipped_extensions }
    }

    /// The full command catalog: built-in provider commands plus (once
    /// extensions declare them) extension commands. Recomputed per call - the
    /// descriptor lists are tiny and this keeps reload invalidation free.
    pub fn commands(&self) -> Vec<CommandDescriptor> {
        let mut out: Vec<CommandDescriptor> = builtin_commands();
        out.extend(self.providers.iter().flat_map(|p| p.commands()));
        for ext in self.extensions.values() {
            out.extend(ext.commands());
        }
        out
    }

    /// Scoped search for an active Scope command (`command_id` is the
    /// descriptor id). Routes to the owning provider/extension; results keep
    /// the provider's own ranking (no frecency/dict-fill - scopes are already
    /// intent-filtered). Unknown ids return empty.
    pub fn search_scope(
        &self,
        command_id: &str,
        query: &str,
        scope_data: Option<String>,
    ) -> Vec<SearchResult> {
        let commands = self.commands();
        let Some(cmd) = commands.iter().find(|c| c.id == command_id) else {
            return Vec::new();
        };
        let mut results = match &cmd.route {
            command::CommandRoute::Builtin { provider_id } => self
                .providers
                .iter()
                .filter(|p| p.id() == provider_id.as_str())
                .flat_map(|p| p.search_scoped(command_id, query))
                .collect(),
            // Sync tier of an extension scope; the async `query` tier is
            // dispatched by the command layer (lib.rs) alongside this.
            command::CommandRoute::Extension { name, command } => match self.extensions.get(name) {
                Some(ext) if !ext.is_benched() => ext
                    .gate_scoped(command, query)
                    .map(|gc| ext.search_gated(gc, true, scope_data))
                    .unwrap_or_default(),
                _ => Vec::new(),
            },
            command::CommandRoute::UiTakeover => Vec::new(),
            // Action route: activated frontend-side, never scope-searched.
            command::CommandRoute::Invoke { .. } => Vec::new(),
        };
        // Compose parts-bearing rows; scopes never drop hidden categories
        // (weight 0 hides from root search only).
        ranking::apply_ranking(&mut results, &self.ranking.read().unwrap(), false, false);
        sort_by_score(&mut results);
        // Browse-the-catalog scopes (marketplace) show the full list; a hidden
        // tail past max_results would make entries unreachable in browse mode.
        if !cmd.uncapped {
            results.truncate(self.max_results);
        }
        results
    }
}

/// Built-in commands that belong to no single provider (app-level actions).
fn builtin_commands() -> Vec<CommandDescriptor> {
    use command::{CommandRoute, CommandSource, ModeKind};
    vec![CommandDescriptor {
        id: "cmd:settings".to_string(),
        title: "Open Settings".to_string(),
        chip: "Settings".to_string(),
        subtitle: Some("Portunus".to_string()),
        source: CommandSource::Builtin,
        mode_kind: ModeKind::Action,
        keywords: vec![
            "settings".into(),
            "preferences".into(),
            "config".into(),
            "configuration".into(),
            "options".into(),
        ],
        placeholder: None,
        min_query_len: 0,
        result_kind: "command".to_string(),
        glyph: Some("settings".to_string()),
        icon_data_uri: None,
        default_shortcut: None,
        opens_form: false,
        uncapped: false,
        volatile: false,
        route: CommandRoute::Invoke { command: "open_settings_window".to_string(), args: None },
    },
    CommandDescriptor {
        id: "cmd:reindex".to_string(),
        title: "Rebuild Content Index".to_string(),
        chip: "Reindex".to_string(),
        subtitle: Some("Content search".to_string()),
        source: CommandSource::Builtin,
        mode_kind: ModeKind::Action,
        keywords: vec![
            "reindex".into(),
            "rebuild".into(),
            "index".into(),
            "content".into(),
            "refresh".into(),
        ],
        placeholder: None,
        min_query_len: 0,
        result_kind: "command".to_string(),
        glyph: Some("refresh".to_string()),
        icon_data_uri: None,
        default_shortcut: None,
        opens_form: false,
        uncapped: false,
        volatile: false,
        route: CommandRoute::Invoke {
            command: "trigger_full_reindex".to_string(),
            args: Some(serde_json::json!({ "full": false })),
        },
    }]
}

fn sort_by_score(results: &mut [SearchResult]) {
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}
