//! Small shared helpers.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rusqlite::Connection;

/// Locks a `Mutex`, recovering rather than panicking if a previous holder
/// panicked and poisoned it. Our locks guard caches/indexes where the worst a
/// stale-after-panic read can do is return slightly-off data - far better than
/// cascading a single background panic into a crash on every later access.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// True when `PORTUNUS_PROFILE_SEARCH` is set in the environment. Gates the
/// per-keystroke search timing logs; cached on first read so the hot path pays
/// one atomic load, not an env lookup per query.
pub fn profile_search() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("PORTUNUS_PROFILE_SEARCH").is_some())
}

/// Read-locks an `RwLock`, recovering from poisoning. See [`lock`].
pub fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

/// Write-locks an `RwLock`, recovering from poisoning. See [`lock`].
pub fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Truncates `s` in place to at most `max` bytes, backing up to the nearest
/// UTF-8 char boundary so the result stays valid. No-op when already within the
/// cap. Shared by the log ring buffer, host log fn, toast effect, and the wasm
/// result-field clamp - all of which cap untrusted extension strings.
pub fn truncate_char_boundary(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

/// Spawns `program` with `args` fully detached: stdio nulled and its own
/// process group, so the child outlives Portunus and never blocks it. This is
/// the single home for the detachment contract - shared by app launches,
/// browser/file opens, desktop notifications, and the extension `spawn` effect
/// - so the null-stdio + `process_group(0)` incantation can't drift per call
/// site. Fire-and-forget: nothing is captured back.
pub fn spawn_detached<P, S>(program: P, args: &[S]) -> std::io::Result<std::process::Child>
where
    P: AsRef<std::ffi::OsStr>,
    S: AsRef<std::ffi::OsStr>,
{
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

/// Launches a `.desktop` entry through GIO instead of re-implementing the
/// Desktop Entry spec on top of [`spawn_detached`]. GIO expands the `Exec`
/// field codes (`%f`/`%U`/`%i`/`%c`/`%k`), honors `Terminal=true` by prepending
/// the user's terminal, and resolves a relative `Exec` against `PATH`.
///
/// Detachment is ours to arrange, and it is the whole reason this does not just
/// call `g_app_info_launch`: that convenience wrapper spawns with
/// `G_SPAWN_DO_NOT_REAP_CHILD`, leaving the app a direct child of Portunus in
/// Portunus' process group and session - so a SIGHUP/SIGINT to our group, or
/// stopping Portunus from a terminal, took every launched app down with it.
/// Instead:
///   * `DBusActivatable` entries are activated over the session bus, where the
///     app is a child of the bus/systemd and never ours;
///   * everything else goes through `launch_uris_as_manager` *without*
///     `DO_NOT_REAP_CHILD`, so glib's intermediate fork reparents the app to
///     init, plus a post-fork `setsid()` to give it its own session and process
///     group. Same contract as [`spawn_detached`], just spelled in glib.
pub fn launch_desktop_entry(desktop_file: &str) -> Result<(), String> {
    use gio::prelude::AppInfoExt;
    let info = gio::DesktopAppInfo::from_filename(desktop_file)
        .ok_or_else(|| format!("unusable desktop entry: {desktop_file}"))?;

    if info.boolean("DBusActivatable") {
        return info
            .launch(&[], gio::AppLaunchContext::NONE)
            .map_err(|e| e.to_string());
    }

    info.launch_uris_as_manager(
        &[],
        gio::AppLaunchContext::NONE,
        gio::glib::SpawnFlags::SEARCH_PATH
            | gio::glib::SpawnFlags::STDOUT_TO_DEV_NULL
            | gio::glib::SpawnFlags::STDERR_TO_DEV_NULL,
        Some(Box::new(|| {
            // Post-fork, pre-exec: async-signal-safe, so nothing but the call.
            unsafe {
                libc::setsid();
            }
        })),
        None,
    )
    .map_err(|e| e.to_string())
}

/// Returns true if `bin` is found as an executable file on any PATH entry.
/// Used both to gate providers at startup and to report dependency status
/// to the Settings UI via `check_dependencies`.
pub fn binary_in_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
    })
}

/// Opens a SQLite database, recreating it from scratch if it fails a `quick_check`
/// integrity probe. A corrupt DB (e.g. from a power loss mid-write) otherwise opens
/// fine but then yields cryptic per-query failures - for our caches (frecency,
/// content index) the right recovery is simply to discard and rebuild, so callers
/// get a usable connection instead of silent degradation. The `-wal`/`-shm` sidecars
/// are removed alongside the main file so the recreated DB starts clean.
pub fn open_sqlite_resilient(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let healthy = conn
        .query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
        .map(|s| s == "ok")
        .unwrap_or(false);
    if healthy {
        return Ok(conn);
    }
    eprintln!(
        "[db] integrity check failed for {} - recreating from scratch",
        path.display()
    );
    drop(conn);
    let _ = std::fs::remove_file(path);
    for sidecar in ["-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(sidecar);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    Connection::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path GIO can't turn into a desktop entry must fail *before* anything is
    /// spawned, so `launch_app` still reaches its raw-`Exec` fallback.
    #[test]
    fn launch_desktop_entry_rejects_non_entries() {
        assert!(launch_desktop_entry("/nonexistent/nope.desktop").is_err());
    }

    /// The apps provider hands GIO the absolute path of the file it parsed;
    /// check GIO accepts that exact shape and reads the entry back. Stops short
    /// of launching - `launch()` would start a real process.
    #[test]
    fn gio_reads_the_entries_the_apps_provider_emits() {
        use gio::prelude::AppInfoExt;

        let dir = std::env::temp_dir().join(format!("portunus-desktop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("org.example.Editor.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Café Editor\nExec=true %F\nTerminal=false\n",
        )
        .unwrap();

        let info = gio::DesktopAppInfo::from_filename(&path).expect("entry parsed");
        assert_eq!(info.name(), "Café Editor");
        // GIO owns field-code expansion, so the raw `%F` survives here and is
        // resolved at launch time instead of being stripped by us.
        assert_eq!(info.commandline().unwrap().to_str().unwrap(), "true %F");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The regression this path exists for: a launched app must not stay in
    /// Portunus' session or process group, or signalling/stopping Portunus
    /// takes it down too. The entry reports its own parent, session and group
    /// ids; all three must differ from ours.
    #[test]
    fn launched_entries_are_detached_from_us() {
        let dir = std::env::temp_dir().join(format!("portunus-detach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("ids");
        let path = dir.join("org.example.Detach.desktop");
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=Detach Probe\n                 Exec=sh -c 'ps -o ppid=,sid=,pgid= -p $$ > {}'\nTerminal=false\n",
                out.display()
            ),
        )
        .unwrap();

        launch_desktop_entry(path.to_str().unwrap()).expect("launched");

        let mut ids = String::new();
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if !s.trim().is_empty() {
                    ids = s;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let ids: Vec<i32> = ids
            .split_whitespace()
            .map(|n| n.parse().expect("numeric id"))
            .collect();
        assert_eq!(ids.len(), 3, "probe never reported its ids");

        let us = std::process::id() as i32;
        let our_sid = unsafe { libc::getsid(0) };
        let our_pgid = unsafe { libc::getpgid(0) };
        assert_ne!(ids[0], us, "app is still our direct child");
        assert_ne!(ids[1], our_sid, "app is still in our session");
        assert_ne!(ids[2], our_pgid, "app is still in our process group");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
