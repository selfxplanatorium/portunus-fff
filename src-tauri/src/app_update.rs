//! Notify-only self-update check against the GitHub Releases API.
//!
//! Asks for the latest release tag on an interval, compares it to the running
//! `CARGO_PKG_VERSION`, and surfaces the result in the Settings About section.
//! Nothing is ever downloaded or installed - the only action offered is opening
//! the release page.
//!
//! Fetching follows the marketplace/currency cache pattern: it runs off the
//! search path on its own thread, conditional requests keep repeats cheap, the
//! result and the check timestamp persist across restarts, and every failure
//! keeps the cached answer instead of surfacing an error.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, TryLockError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A release page is always built locally as `RELEASE_TAG_BASE + tag`; the URL
/// in the API response is deliberately never deserialized, let alone opened.
const RELEASE_TAG_BASE: &str = "https://github.com/selfxplanatorium/portunus/releases/tag/";
const LATEST_API_URL: &str = "https://api.github.com/repos/selfxplanatorium/portunus/releases/latest";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Size cap on the fetched release document.
const MAX_BODY_BYTES: u64 = 256 * 1024;
/// After a failed check, don't retry for this long: a restart loop or a 403 must
/// not burn the 60 req/h unauthenticated budget.
const FAILURE_BACKOFF_SECS: u64 = 6 * 3600;
/// Floor between two explicit "Check now" clicks.
const MIN_MANUAL_SECS: u64 = 60;
/// Delay before the first check so it never competes with app/file indexing or
/// the pdfium warmup.
const STARTUP_DELAY: Duration = Duration::from_secs(15);
/// How often the long-lived thread re-evaluates staleness. Portunus can run for
/// weeks, so a startup-only check would never fire again.
const POLL_INTERVAL: Duration = Duration::from_secs(3600);
/// Cache-file schema this build understands; a mismatch discards the file.
const CACHE_SCHEMA: u32 = 1;
/// Longest release tag we will even try to parse.
const MAX_TAG_LEN: usize = 32;

// ── wire & cache schema ───────────────────────────────────────────────────────

/// The subset of the GitHub release object we read. Every optional field is
/// `serde(default)` so GitHub adding or removing keys is never fatal.
///
/// `html_url` and `body` are deliberately absent: the banner shows a version and
/// a link, the link is rebuilt from the tag, and not storing the changelog keeps
/// the cache file and the IPC payload small.
#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

/// A stable release we are willing to point the user at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// `tag_name` with the leading `v` stripped; always numeric-dotted.
    pub version: String,
    pub tag: String,
    /// Built locally from the tag - never taken off the wire.
    pub url: String,
    #[serde(default)]
    pub published_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    schema: u32,
    /// Unix seconds of the last successful exchange (200 or 304). 0 = never.
    #[serde(default)]
    checked_at: u64,
    /// Unix seconds of the last *attempt*, success or failure. Drives the
    /// failure backoff, and is stamped before the request goes out so a crash
    /// mid-fetch still takes it.
    #[serde(default)]
    attempted_at: u64,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    latest: Option<ReleaseInfo>,
}

/// What the Settings About section reads. Built from memory; no I/O.
///
/// The enable toggle and the interval are deliberately absent: they live in
/// `config.toml`, the frontend already renders them from `Config`, and a second
/// copy here would be a second source of truth to keep in sync.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest: Option<ReleaseInfo>,
    pub update_available: bool,
    /// 0 = never checked (fresh install, or checks disabled).
    pub checked_at: u64,
}

fn cache_path() -> PathBuf {
    crate::paths::data_dir().join("update-check.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── store ─────────────────────────────────────────────────────────────────────

/// Process-wide cache, shared by the checker thread and the two commands.
///
/// It holds no copy of the enable/interval settings: every caller passes the
/// live values in, so a config edit needs no plumbing to reach this struct.
pub struct Store {
    cache: RwLock<CacheFile>,
    /// Debounces a manual "Check now" racing the background loop. A mutex rather
    /// than a flag so a panic mid-check can't leave it permanently held.
    checking: Mutex<()>,
}

/// Lazily reads the cache file on first touch. Call this from a background
/// thread, never from `setup` - the file read must stay off startup.
pub fn store() -> &'static Arc<Store> {
    static STORE: OnceLock<Arc<Store>> = OnceLock::new();
    STORE.get_or_init(|| Arc::new(Store::load_from_disk()))
}

impl Store {
    pub fn empty() -> Self {
        Self {
            cache: RwLock::new(CacheFile { schema: CACHE_SCHEMA, ..Default::default() }),
            checking: Mutex::new(()),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_cache(latest: Option<ReleaseInfo>, checked_at: u64) -> Self {
        let store = Self::empty();
        {
            let mut guard = crate::util::write(&store.cache);
            guard.latest = latest;
            guard.checked_at = checked_at;
            guard.attempted_at = checked_at;
        }
        store
    }

    pub fn load_from_disk() -> Self {
        let store = Self::empty();
        if let Ok(bytes) = std::fs::read(cache_path()) {
            if let Ok(file) = serde_json::from_slice::<CacheFile>(&bytes) {
                if file.schema == CACHE_SCHEMA {
                    *crate::util::write(&store.cache) = file;
                }
            }
        }
        store
    }

    /// True when an automatic check is due: the interval has elapsed since the
    /// last success *and* the backoff has elapsed since the last attempt.
    pub fn is_stale(&self, interval_hours: u64) -> bool {
        // A zero/absurd interval in config must not turn into a request loop.
        let interval = interval_hours.clamp(1, 24 * 365) * 3600;
        let cache = crate::util::read(&self.cache);
        let now = now_unix();
        if now.saturating_sub(cache.attempted_at) < FAILURE_BACKOFF_SECS
            && cache.attempted_at > cache.checked_at
        {
            return false; // last attempt failed; wait out the backoff
        }
        now.saturating_sub(cache.checked_at) >= interval
    }

    /// Whether the cached release is newer than this binary. Recomputed on every
    /// read, never stored: a cache written by an older binary must not keep
    /// claiming an update after the upgrade landed.
    fn update_available(&self) -> bool {
        crate::util::read(&self.cache)
            .latest
            .as_ref()
            .is_some_and(|r| is_newer_release(&r.tag, env!("CARGO_PKG_VERSION")))
    }

    pub fn status(&self) -> UpdateStatus {
        let update_available = self.update_available();
        let cache = crate::util::read(&self.cache);
        UpdateStatus {
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            latest: cache.latest.clone(),
            update_available,
            checked_at: cache.checked_at,
        }
    }

    /// Automatic check: a no-op unless enabled and the interval has elapsed.
    /// Blocking network I/O - never call this on the search path. `Ok(true)`
    /// means a newer release just became visible.
    pub fn check_if_due(&self, enabled: bool, interval_hours: u64) -> Result<bool, String> {
        if !enabled || !self.is_stale(interval_hours) {
            return Ok(false);
        }
        self.check_guarded()
    }

    /// Explicit "Check now": ignores both the interval and the enable toggle -
    /// the click is the consent - but still honors a 60-second floor so a
    /// button-mash can't burn the rate-limit budget.
    pub fn check_now(&self) -> Result<bool, String> {
        let since = now_unix().saturating_sub(crate::util::read(&self.cache).attempted_at);
        if since < MIN_MANUAL_SECS {
            return Err(format!("checked {since}s ago - try again in a moment"));
        }
        self.check_guarded()
    }

    /// Serializes checks. The guard is RAII, so an unwind can't wedge the store;
    /// a poisoned lock is recovered rather than propagated, matching the rest of
    /// the codebase (see [`crate::util::lock`]).
    fn check_guarded(&self) -> Result<bool, String> {
        let _guard = match self.checking.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(false),
        };
        self.check_inner()
    }

    fn check_inner(&self) -> Result<bool, String> {
        let etag = {
            // Stamp the attempt before the request goes out, so a crash or a
            // 403 mid-flight still takes the backoff on the next launch.
            let mut guard = crate::util::write(&self.cache);
            guard.attempted_at = now_unix();
            persist(&guard);
            guard.etag.clone()
        };

        let was_available = self.update_available();

        match fetch_latest(etag.as_deref())? {
            Fetched::NotModified => {
                let mut guard = crate::util::write(&self.cache);
                guard.checked_at = now_unix();
                persist(&guard);
                Ok(false)
            }
            Fetched::Release(rel, new_etag) => {
                let latest = sanitize(rel);
                let mut guard = crate::util::write(&self.cache);
                guard.checked_at = now_unix();
                guard.etag = new_etag;
                guard.latest = latest;
                persist(&guard);
                drop(guard);
                Ok(!was_available && self.update_available())
            }
        }
    }
}

/// Long-lived checker thread: one GitHub Releases GET per interval, notify only.
/// Its own thread, delayed so it never competes with app/file indexing or the
/// pdfium warmup, and it never touches search.
///
/// A loop, not a one-shot: Portunus can run for weeks, so a startup-only check
/// would never fire again. `check_if_due` makes the hourly wake a no-op until the
/// interval elapses, and re-reading the config each wake is what makes a settings
/// toggle take effect without a restart.
pub fn spawn_checker(app: tauri::AppHandle, config: crate::ConfigState) {
    std::thread::spawn(move || {
        std::thread::sleep(STARTUP_DELAY);
        // First touch reads the cache file - deliberately here and not in
        // `setup`, so that read stays off startup.
        let store = store();
        loop {
            let (enabled, interval_hours) = {
                let cfg = crate::util::lock(&config);
                (cfg.general.check_for_updates, cfg.general.update_check_interval_hours)
            };
            match store.check_if_due(enabled, interval_hours) {
                Ok(true) => {
                    use tauri::Emitter;
                    let _ = app.emit("app-update-available", ());
                }
                Ok(false) => {}
                Err(e) => eprintln!("[update] release check failed: {e}"),
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

// ── fetch ─────────────────────────────────────────────────────────────────────

enum Fetched {
    NotModified,
    Release(GhRelease, Option<String>),
}

fn fetch_latest(etag: Option<&str>) -> Result<Fetched, String> {
    debug_assert!(LATEST_API_URL.starts_with("https://"));
    // `ureq` already sends a `ureq/x.y.z` User-Agent, so this is not what keeps
    // GitHub from rejecting the request - it is rate-limit attribution and
    // support triage. Pinning the API version keeps a future default-version
    // bump from reshaping the JSON under us.
    let mut req = ureq::get(LATEST_API_URL)
        .timeout(FETCH_TIMEOUT)
        .set("User-Agent", concat!("portunus/", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(etag) = etag {
        req = req.set("If-None-Match", etag);
    }
    // `ureq` returns Err for any status >= 400, so 403/429 rate limiting lands
    // on the failure path and therefore takes the backoff.
    let resp = req.call().map_err(|e| format!("release check failed: {e}"))?;
    if resp.status() == 304 {
        return Ok(Fetched::NotModified);
    }
    let new_etag = resp.header("ETag").map(|s| s.to_string());
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("release check failed: {e}"))?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(format!("release document exceeds the {MAX_BODY_BYTES}-byte cap"));
    }
    let rel: GhRelease =
        serde_json::from_slice(&bytes).map_err(|e| format!("bad release document: {e}"))?;
    Ok(Fetched::Release(rel, new_etag))
}

/// Turns a release object into something we are willing to offer. `None` means
/// "no release we would ever point at": a draft, a prerelease, or a tag that
/// isn't plain numerics. A rejected release leaves the banner hidden.
fn sanitize(rel: GhRelease) -> Option<ReleaseInfo> {
    if rel.draft || rel.prerelease {
        return None;
    }
    let tag = rel.tag_name.trim();
    if parse_tag(tag).is_none() {
        eprintln!("[update] ignoring release tag \"{tag}\": not a plain numeric version");
        return None;
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    Some(ReleaseInfo {
        version,
        url: format!("{RELEASE_TAG_BASE}{tag}"),
        tag: tag.to_string(),
        published_at: rel.published_at.unwrap_or_default(),
    })
}

fn persist(file: &CacheFile) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_vec(file) {
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

// ── versions ──────────────────────────────────────────────────────────────────

/// Parses a release tag into numeric components. Accepts an optional leading
/// `v` and 1..=4 dot-separated decimal segments and nothing else: a prerelease
/// suffix, build metadata, or any non-numeric segment yields `None`.
fn parse_tag(tag: &str) -> Option<Vec<u64>> {
    let tag = tag.trim();
    if tag.len() > MAX_TAG_LEN {
        return None;
    }
    let tag = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    if tag.is_empty() {
        return None;
    }
    let parts: Vec<&str> = tag.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        // `len() <= 9` keeps the parse inside u64 and rejects zero-padding runs.
        if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push(p.parse::<u64>().ok()?);
    }
    Some(out)
}

/// True only when `tag` names a stable release strictly newer than `running`.
///
/// Fails closed: if either side doesn't parse, the answer is `false`. This is
/// the opposite of [`crate::extensions::marketplace::version_newer`], which
/// fails open because the marketplace index is authoritative for what it ships.
/// Here a `v0.7.0-rc1` or `nightly` tag must never be pushed at stable users,
/// so the two comparators stay separate rather than sharing a fallback.
pub fn is_newer_release(tag: &str, running: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (parse_tag(tag), parse_tag(running)) else {
        return false;
    };
    // Zero-pad to equal length so missing trailing segments compare as 0 (making
    // 1.1 == 1.1.0), then lean on `Vec<u64>`'s lexicographic ordering.
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Passive read of what we already know. Pure memory: no network, no I/O, safe
/// on every Settings mount and every `app-update-available` event.
#[tauri::command]
pub fn app_update_status() -> UpdateStatus {
    store().status()
}

/// Explicit "Check now". Forces a fetch regardless of the interval and the
/// enable toggle, and emits `app-update-available` when a newer release just
/// became visible.
#[tauri::command]
pub async fn app_update_check(app: tauri::AppHandle) -> Result<UpdateStatus, String> {
    let newly = tauri::async_runtime::spawn_blocking(|| store().check_now())
        .await
        .map_err(|e| e.to_string())??;
    if newly {
        use tauri::Emitter;
        let _ = app.emit("app-update-available", ());
    }
    Ok(store().status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> GhRelease {
        GhRelease {
            tag_name: tag.to_string(),
            published_at: Some("2026-01-01T00:00:00Z".to_string()),
            prerelease: false,
            draft: false,
        }
    }

    #[test]
    fn newer_stable_release_is_offered() {
        assert!(is_newer_release("v0.7.0", "0.6.1"));
        assert!(is_newer_release("0.7.0", "0.6.1"));
        // The classic lexicographic trap.
        assert!(is_newer_release("v0.10.0", "0.9.9"));
        assert!(is_newer_release("v1.2.3.4", "1.2.3.3"));
    }

    #[test]
    fn same_or_older_is_not_offered() {
        assert!(!is_newer_release("0.6.1", "0.6.1"));
        assert!(!is_newer_release("v0.6.1", "0.6.1"));
        assert!(!is_newer_release("0.6.0", "0.6.1"));
        // Missing trailing segments are zeros, so these are equal.
        assert!(!is_newer_release("1.1", "1.1.0"));
        assert!(!is_newer_release("1.1.0", "1.1"));
    }

    #[test]
    fn unparseable_tags_fail_closed() {
        assert!(!is_newer_release("v0.7.0-rc1", "0.6.1"));
        assert!(!is_newer_release("0.7.0+build9", "0.6.1"));
        assert!(!is_newer_release("nightly", "0.6.1"));
        assert!(!is_newer_release("", "0.6.1"));
        assert!(!is_newer_release("v", "0.6.1"));
        assert!(!is_newer_release("0.7.0", ""));
        assert!(!is_newer_release("v1.2.3.4.5", "1.2.3.4"));
        assert!(!is_newer_release("1..2", "0.6.1"));
        assert!(!is_newer_release("0.7.0 ; rm -rf /", "0.6.1"));
        assert!(!is_newer_release("0.0000000001", "0.6.1"));
    }

    #[test]
    fn sanitize_builds_the_url_from_the_tag() {
        let info = sanitize(release("v0.7.0")).expect("stable numeric tag");
        assert_eq!(info.version, "0.7.0");
        assert_eq!(info.tag, "v0.7.0");
        assert_eq!(info.url, "https://github.com/selfxplanatorium/portunus/releases/tag/v0.7.0");
    }

    #[test]
    fn sanitize_drops_drafts_prereleases_and_odd_tags() {
        let mut draft = release("v0.7.0");
        draft.draft = true;
        assert!(sanitize(draft).is_none());

        let mut pre = release("v0.7.0");
        pre.prerelease = true;
        assert!(sanitize(pre).is_none());

        assert!(sanitize(release("v0.7.0-rc1")).is_none());
        assert!(sanitize(release("nightly")).is_none());
    }

    #[test]
    fn status_recomputes_availability_against_the_running_version() {
        let current = env!("CARGO_PKG_VERSION");
        // A cache entry naming the running version must not claim an update,
        // even though it was written back when it was newer.
        let stale = sanitize(release(current)).expect("own version parses");
        let store = Store::with_cache(Some(stale), 1);
        assert!(!store.status().update_available);
        assert_eq!(store.status().current_version, current);

        let ahead = sanitize(release("v999.0.0")).expect("numeric tag");
        let store = Store::with_cache(Some(ahead), 1);
        assert!(store.status().update_available);
    }

    #[test]
    fn cache_file_round_trips_and_tolerates_unknown_fields() {
        let file = CacheFile {
            schema: CACHE_SCHEMA,
            checked_at: 42,
            attempted_at: 42,
            etag: Some("\"abc\"".to_string()),
            latest: sanitize(release("v0.7.0")),
        };
        let json = serde_json::to_vec(&file).expect("serialize");
        let back: CacheFile = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.checked_at, 42);
        assert_eq!(back.latest.map(|r| r.version).as_deref(), Some("0.7.0"));

        let sparse: CacheFile =
            serde_json::from_str(r#"{"schema":1,"checked_at":7,"future_key":true}"#)
                .expect("unknown fields are ignored");
        assert_eq!(sparse.checked_at, 7);
        assert!(sparse.latest.is_none());
    }

    #[test]
    fn staleness_honors_the_interval_and_the_failure_backoff() {
        let now = now_unix();

        // Never checked: due immediately.
        assert!(Store::empty().is_stale(24));

        // Checked just now: not due.
        assert!(!Store::with_cache(None, now).is_stale(24));

        // Checked two days ago with a 24h interval: due.
        assert!(Store::with_cache(None, now - 2 * 24 * 3600).is_stale(24));

        // Last attempt failed an hour ago (attempted_at > checked_at): the 6h
        // backoff suppresses the retry even though the interval elapsed.
        let store = Store::with_cache(None, now - 2 * 24 * 3600);
        crate::util::write(&store.cache).attempted_at = now - 3600;
        assert!(!store.is_stale(24));

        // Same failure, but seven hours ago: past the backoff, so due again.
        crate::util::write(&store.cache).attempted_at = now - 7 * 3600;
        assert!(store.is_stale(24));
    }

    #[test]
    fn a_disabled_or_fresh_check_makes_no_request() {
        assert_eq!(Store::empty().check_if_due(false, 24), Ok(false));
        assert_eq!(Store::with_cache(None, now_unix()).check_if_due(true, 24), Ok(false));
    }

    #[test]
    fn an_absurd_interval_cannot_become_a_request_loop() {
        assert!(!Store::with_cache(None, now_unix()).is_stale(0));
    }

    #[test]
    #[ignore = "network: run manually with cargo test -- --ignored"]
    fn live_release_fetch() {
        match fetch_latest(None) {
            Ok(Fetched::Release(rel, _)) => {
                assert!(sanitize(rel).is_some(), "latest release should be a plain numeric tag");
            }
            Ok(Fetched::NotModified) => panic!("no ETag was sent, so 304 is impossible"),
            Err(e) => panic!("{e}"),
        }
    }
}
