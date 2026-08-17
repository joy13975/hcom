//! Centralized path resolution and file utilities for hcom.
//!
//! Single source of truth for all hcom directory and file paths.
//! Respects HCOM_DIR env var for worktrees/dev, falls back to ~/.hcom.
//! Also provides atomic file operations and flag counters.

use crate::config::Config;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const LOGS_DIR: &str = ".tmp/logs";
pub const LAUNCH_DIR: &str = ".tmp/launch";
pub const FLAGS_DIR: &str = ".tmp/flags";
pub const LAUNCHES_DIR: &str = "launches";
pub const ARCHIVE_DIR: &str = "archive";
pub const SCRIPTS_DIR: &str = "scripts";

/// Resolve HCOM_DIR from an environment snapshot.
///
/// Returns the normalized path plus whether HCOM_DIR was explicitly set.
/// Normalization behavior:
/// - `~` expands against HOME/USERPROFILE when available
/// - relative paths are resolved against the provided cwd
/// - otherwise falls back to `HOME/.hcom` or `.hcom`
pub fn resolve_hcom_dir_from_env(env: &HashMap<String, String>, cwd: &Path) -> (PathBuf, bool) {
    let home = env.get("HOME").or_else(|| env.get("USERPROFILE"));
    let hcom_dir = env.get("HCOM_DIR").filter(|value| !value.is_empty());

    let resolved = if let Some(dir) = hcom_dir {
        let expanded = if dir.starts_with('~') {
            if let Some(home_dir) = home {
                dir.replacen('~', home_dir, 1)
            } else {
                dir.clone()
            }
        } else {
            dir.clone()
        };

        let path = PathBuf::from(expanded);
        if path.is_relative() {
            cwd.join(path)
        } else {
            path
        }
    } else {
        home.map(|home_dir| PathBuf::from(home_dir).join(".hcom"))
            .unwrap_or_else(|| PathBuf::from(".hcom"))
    };

    (resolved, hcom_dir.is_some())
}

/// Canonicalize a path through its deepest existing ancestor.
///
/// The existing prefix is resolved with `canonicalize` (following symlinks);
/// the not-yet-existing suffix is appended verbatim. A `..` component in that
/// suffix cannot be resolved on the filesystem here, so it is rejected outright
/// rather than folded lexically — otherwise `<tmp>/nope/../../etc` would
/// spuriously appear to sit under the temp prefix.
#[cfg(test)]
fn resolve_deepest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if let Ok(existing) = current.canonicalize() {
            let rest = path.strip_prefix(current).ok()?;
            if rest
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return None;
            }
            return Some(existing.join(rest));
        }
        current = current.parent()?;
    }
}

/// Whether a unit-test path resolves beneath the system temporary directory.
///
/// Both sides are canonicalized through their deepest existing ancestor, so the
/// decision is safe for paths whose final components do not exist yet and
/// rejects a lexical temp path that crosses a symlink to a non-temp target.
#[cfg(test)]
pub(crate) fn is_test_temp_path(path: &Path) -> bool {
    let Some(temp_dir) = resolve_deepest_existing(&std::env::temp_dir()) else {
        return false;
    };
    let Some(resolved) = resolve_deepest_existing(path) else {
        return false;
    };
    resolved.starts_with(temp_dir)
}

/// Registry of test roots a fixture has explicitly claimed as disposable.
///
/// Temp-directory *geography* is not ownership: a real hcom DB can legitimately
/// live under `$TMPDIR` (and `TMPDIR=/` would trust almost everything). So the
/// Config redirect (see `config`) trusts only roots a test fixture registered
/// here, never "it's under /tmp". `open_raw`'s tripwire additionally accepts the
/// temp tree as a disposable backstop for ad-hoc `tempfile` DBs, and registers
/// what it opens so a later Config lookup on the same root stays consistent.
#[cfg(test)]
pub(crate) mod test_roots {
    use super::{Path, PathBuf, resolve_deepest_existing};
    use std::sync::{Mutex, OnceLock};

    fn roots() -> &'static Mutex<Vec<PathBuf>> {
        static ROOTS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
        ROOTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Claim `path` (canonicalized through its deepest existing ancestor) as a
    /// disposable test root. Idempotent.
    pub(crate) fn register(path: &Path) {
        if let Some(canonical) = resolve_deepest_existing(path) {
            let mut roots = roots().lock().unwrap();
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
    }

    /// Whether `path` resolves at or beneath a registered disposable root.
    pub(crate) fn is_registered(path: &Path) -> bool {
        let Some(canonical) = resolve_deepest_existing(path) else {
            return false;
        };
        roots()
            .lock()
            .unwrap()
            .iter()
            .any(|root| canonical.starts_with(root))
    }
}

/// Directory components that some AI tools (codex, claude, gemini) treat as
/// protected metadata under any writable root. Placing HCOM_DIR beneath one of
/// these means the parent tool's sandbox/permission layer will block writes to
/// the hcom DB, with no escalation path for codex apply_patch.
///
/// - `.git`: codex (apply_patch hard-deny via FileSystemSandboxPolicy), claude
///   (DANGEROUS_DIRECTORIES auto-edit gate), gemini (GOVERNANCE_FILES).
/// - `.codex`, `.agents`: codex protected metadata.
/// - `.claude`: claude DANGEROUS_DIRECTORIES.
const PROTECTED_HCOM_DIR_COMPONENTS: &[&str] = &[".git", ".codex", ".claude", ".agents", ".omp"];

/// If `path` sits at or beneath a protected metadata component, return that
/// component name. Component-wise match — `.gitfoo` and `dot.git` do not trigger.
pub fn protected_hcom_dir_component(path: &Path) -> Option<&'static str> {
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            for protected in PROTECTED_HCOM_DIR_COMPONENTS {
                if name == std::ffi::OsStr::new(*protected) {
                    return Some(*protected);
                }
            }
        }
    }
    None
}

/// Get the hcom base directory.
///
/// Uses centralized Config (HCOM_DIR env var or ~/.hcom fallback).
pub fn hcom_dir() -> PathBuf {
    Config::get().hcom_dir
}

/// Build path under hcom directory, optionally ensuring parent exists.
pub fn hcom_path(parts: &[&str]) -> PathBuf {
    let mut path = hcom_dir();
    for part in parts {
        path = path.join(part);
    }
    path
}

/// Get project root (parent of hcom_dir). Used for anchoring tool config files.
///
/// Uses cached Config — for test-friendly env-reactive resolution, use
/// `runtime_env::tool_config_root()` instead.
pub fn get_project_root() -> PathBuf {
    hcom_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Get the database path (hcom_dir/hcom.db)
pub fn db_path() -> PathBuf {
    hcom_dir().join("hcom.db")
}

/// Get the log file path (hcom_dir/.tmp/logs/hcom.log)
pub fn log_path() -> PathBuf {
    hcom_dir().join(".tmp").join("logs").join("hcom.log")
}

/// Get the pidtrack file path (hcom_dir/.tmp/launched_pids.json)
pub fn pidtrack_path() -> PathBuf {
    hcom_dir().join(".tmp").join("launched_pids.json")
}

/// Get the config TOML path (hcom_dir/config.toml)
pub fn config_toml_path() -> PathBuf {
    hcom_dir().join("config.toml")
}

/// Get the scripts directory (hcom_dir/scripts/)
pub fn scripts_dir() -> PathBuf {
    hcom_dir().join(SCRIPTS_DIR)
}

/// Ensure all critical HCOM directories exist. Idempotent, safe to call repeatedly.
/// Called at hook entry to support opt-in scenarios where hooks execute before CLI commands.
/// Returns true on success, false on failure.
pub fn ensure_hcom_directories() -> bool {
    ensure_hcom_directories_at(&hcom_dir())
}

/// Ensure directories under a given base (testable without global config).
pub fn ensure_hcom_directories_at(base: &Path) -> bool {
    if ensure_private_directory(base).is_err() {
        return false;
    }
    for dir_name in [LOGS_DIR, LAUNCH_DIR, FLAGS_DIR, LAUNCHES_DIR, ARCHIVE_DIR] {
        if fs::create_dir_all(base.join(dir_name)).is_err() {
            return false;
        }
    }
    true
}

/// Create an hcom-owned directory and keep it private on POSIX (`0o700`).
pub(crate) fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    crate::sys::fs::set_private_dir(path)
}

/// SQLite sidecar path (`-wal` / `-shm`), appending the suffix to the *full*
/// database filename so custom names like `state.sqlite` map to
/// `state.sqlite-wal`, not `state.db-wal`.
pub(crate) fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push(suffix);
    os.into()
}

/// Keep an hcom SQLite database and any WAL/SHM sidecars owner-private on POSIX
/// (`0o600`). No-op on `:memory:` and on Windows.
///
/// This secures the *files* only; the containing directory's `0o700` mode is
/// owned by the caller that creates the hcom directory (`ensure_private_db` is
/// also handed arbitrary temp paths under a shared, sometimes un-chmoddable
/// parent, so it must not touch the parent's mode).
///
/// Newly created sidecars inherit `0o600` from the main file via SQLite's Unix
/// VFS, but a pre-existing broad `-wal`/`-shm` (legacy install) is not
/// re-chmodded by SQLite on reopen — so we repair existing ones explicitly.
pub(crate) fn ensure_private_db(db_path: &Path) -> std::io::Result<()> {
    if db_path == Path::new(":memory:") {
        return Ok(());
    }
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    // Create the main db private, or repair an existing broad mode.
    match crate::sys::fs::create_private_new(db_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::sys::fs::set_private(db_path)?;
        }
        Err(e) => return Err(e),
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            crate::sys::fs::set_private(&sidecar)?;
        }
    }
    Ok(())
}

/// Maximum symlink hops followed when resolving an atomic-write target. Bounded
/// so a symlink cycle fails loudly instead of spinning.
const MAX_WRITE_SYMLINK_HOPS: usize = 8;

/// Resolve the path an atomic write should actually land on, following symlinks.
///
/// An atomic write replaces its target by rename, and a rename onto a symlink
/// REPLACES THE LINK with a regular file. That silently detaches config files
/// which users symlink into a dotfiles repo (`~/.claude/settings.json` ->
/// `~/dotfiles/claude/settings.json`): the edit lands in a new local file, the
/// repo copy stops receiving updates, and the two diverge with no error and no
/// warning. Following the link first means the write updates the file the user
/// actually pointed at, and the link survives.
///
/// Following a symlink is OPT-IN, exposed only through
/// `atomic_write_following_symlinks[_io]`. The default primitive
/// (`atomic_write[_io]`) never follows, so internal state files (flag counters,
/// pidfiles, device-id, update flags under `~/.hcom/`) keep the pre-existing
/// redirection immunity: a pre-planted link at their path is destroyed by the
/// rename rather than written through to whatever it targets. Symlink following
/// only makes sense for files the USER maintains and may link into a dotfiles
/// repo (tool configs); it is never applied to hcom's own internal state.
///
/// A dangling link resolves to its missing target on purpose: that is the
/// "linked into a checkout that has not created the file yet" case, where
/// creating the target is what the user meant.
///
/// Resolution is ADVISORY, not a security boundary. The resolved path is handed
/// to a separate create_dir_all + temp-create + rename sequence, so a co-resident
/// process could swap an intermediate directory component for a symlink between
/// resolution and the rename (a classic resolve-then-rename TOCTOU); the kernel
/// would then follow that swap at rename time. This is acceptable under hcom's
/// per-user threat model (an attacker who can rewrite entries inside the user's
/// own config tree already has broader reach). If a stronger guarantee is ever
/// needed, open the resolved parent as an `O_DIRECTORY` fd and persist relative
/// to it so the rename cannot be redirected.
fn resolve_write_target(filepath: &Path) -> std::io::Result<PathBuf> {
    let mut current = filepath.to_path_buf();
    for _ in 0..MAX_WRITE_SYMLINK_HOPS {
        // symlink_metadata does not follow links, so this inspects the link itself.
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            // NotFound is the only benign case: the current (possibly partially
            // resolved) path does not exist yet, so the write creates it there —
            // the dangling-link / not-yet-created-target case. Every other stat
            // error (EACCES, EOVERFLOW, transient IO, ...) means we could not tell
            // whether this is a symlink; propagating it loudly is mandatory so we
            // never rename over — and silently detach — an actual link we failed
            // to inspect. Matches read_link's `?` propagation two lines below.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let destination = fs::read_link(&current)?;
        current = if destination.is_absolute() {
            destination
        } else {
            // `current` is an existing symlink, so it is neither the empty path
            // nor a root — `parent()` is therefore always `Some` here (a bare
            // relative link like `link.json` yields `Some("")`). The `expect`
            // documents that invariant and fails loud if it is ever violated,
            // rather than silently falling back to a CWD-relative resolution.
            let parent = current
                .parent()
                .expect("an existing symlink path always has a parent component");
            parent.join(destination)
        };
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "symlink chain at {} exceeds {MAX_WRITE_SYMLINK_HOPS} hops",
            filepath.display()
        ),
    ))
}

/// Write content to file atomically (temp file + rename), WITHOUT following
/// symlinks: the rename lands on `filepath` literally, so a pre-planted symlink
/// at that path is destroyed rather than written through. This is the safe
/// default for internal state files; callers that must preserve a user's
/// dotfiles symlink use `atomic_write_following_symlinks_io` instead.
///
/// Returns the underlying IO error on failure for callers that need error detail.
pub fn atomic_write_io(filepath: &Path, content: &str) -> std::io::Result<()> {
    write_atomically(filepath, content)
}

/// Like `atomic_write_io`, but follows a symlink at the final path component so
/// the write lands on the file the user pointed at and the link survives (see
/// `resolve_write_target`). For user-maintained config files that may be linked
/// into a dotfiles repo — never for hcom's internal state.
pub fn atomic_write_following_symlinks_io(filepath: &Path, content: &str) -> std::io::Result<()> {
    let resolved = resolve_write_target(filepath)?;
    write_atomically(resolved.as_path(), content)
}

/// Core atomic write: temp file + fsync + rename onto `filepath` as given.
fn write_atomically(filepath: &Path, content: &str) -> std::io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = filepath.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write to temp file in the same directory (same filesystem for rename)
    let tmp = tempfile::NamedTempFile::new_in(filepath.parent().unwrap_or_else(|| Path::new(".")))?;

    // Write content and fsync before rename to ensure data is on disk
    std::io::Write::write_all(&mut &tmp, content.as_bytes())?;
    tmp.as_file().sync_all()?;

    // Preserve the destination's existing permission mode. NamedTempFile creates
    // its file 0600 on unix, and persist-by-rename copies the temp file's mode —
    // NOT the destination's — so without this an atomic write over an existing
    // file would silently reset it to 0600, stripping group/other access the
    // user (or their dotfiles repo) had set. Only when the destination is a
    // real regular file do we copy its mode; a fresh path or a not-followed
    // symlink keeps the private 0600 default.
    preserve_target_mode(&tmp, filepath)?;

    // Persist atomically (temp file → target path via rename)
    persist_temp_file(tmp, filepath)?;
    Ok(())
}

/// Copy the destination file's permission mode onto the temp file before it is
/// renamed into place, so an atomic write preserves rather than resets the mode.
/// No-op when the destination does not exist as a real regular file.
#[cfg(unix)]
fn preserve_target_mode(tmp: &tempfile::NamedTempFile, filepath: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // symlink_metadata, not metadata: a symlink here is the no-follow path (the
    // link is about to be destroyed by the rename), so there is no existing
    // regular-file mode to carry over — leave the temp file at its 0600 default.
    let mode = match fs::symlink_metadata(filepath) {
        Ok(meta) if meta.file_type().is_file() => meta.permissions().mode(),
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn preserve_target_mode(_tmp: &tempfile::NamedTempFile, _filepath: &Path) -> std::io::Result<()> {
    // Permission bits are a unix concept; nothing to carry over on other platforms.
    Ok(())
}

#[cfg(not(windows))]
fn persist_temp_file(tmp: tempfile::NamedTempFile, filepath: &Path) -> std::io::Result<()> {
    tmp.persist(filepath).map(|_| ()).map_err(|e| e.error)
}

#[cfg(windows)]
fn persist_temp_file(mut tmp: tempfile::NamedTempFile, filepath: &Path) -> std::io::Result<()> {
    // MoveFileExW can transiently return ERROR_ACCESS_DENIED while antivirus,
    // indexing, or another reader briefly holds the destination. Preserve the
    // same temp file and retry the atomic replacement for a short bounded
    // window instead of failing a config update immediately.
    const MAX_ATTEMPTS: u64 = 6;
    for attempt in 1..=MAX_ATTEMPTS {
        match tmp.persist(filepath) {
            Ok(_) => return Ok(()),
            Err(err)
                if err.error.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < MAX_ATTEMPTS =>
            {
                tmp = err.file;
                std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
            }
            Err(err) => return Err(err.error),
        }
    }
    unreachable!("persist loop returns on success or final error")
}

/// Write content to file atomically (temp file + rename), WITHOUT following
/// symlinks. Returns true on success, false on failure.
pub fn atomic_write(filepath: &Path, content: &str) -> bool {
    atomic_write_io(filepath, content).is_ok()
}

/// Like `atomic_write`, but follows a symlink at the target so a user's
/// dotfiles link survives (see `atomic_write_following_symlinks_io`).
/// Returns true on success, false on failure.
pub fn atomic_write_following_symlinks(filepath: &Path, content: &str) -> bool {
    atomic_write_following_symlinks_io(filepath, content).is_ok()
}

/// Increment a counter in .tmp/flags/{name} and return new value.
pub fn increment_flag_counter(name: &str) -> i32 {
    increment_flag_counter_at(&hcom_dir(), name)
}

/// Increment flag counter under a given base (testable).
pub fn increment_flag_counter_at(base: &Path, name: &str) -> i32 {
    let flag_file = base.join(FLAGS_DIR).join(name);
    let _ = fs::create_dir_all(flag_file.parent().unwrap());

    let count = read_flag_file(&flag_file) + 1;
    atomic_write(&flag_file, &count.to_string());
    count
}

fn read_flag_file(path: &Path) -> i32 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_is_test_temp_path_accepts_temp_child() {
        let tmp = TempDir::new().unwrap();
        assert!(is_test_temp_path(
            &tmp.path().join("nested").join("hcom.db")
        ));
    }

    #[test]
    fn test_is_test_temp_path_rejects_non_temp() {
        assert!(!is_test_temp_path(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hcom.db")
        ));
    }

    #[test]
    fn test_is_test_temp_path_rejects_parent_dir_escape() {
        // A `..` in the not-yet-existing suffix lexically starts_with the temp
        // dir but resolves outside it on the real filesystem. Must fail closed.
        let escape = std::env::temp_dir()
            .join("nonexistent")
            .join("..")
            .join("..")
            .join("etc")
            .join(".hcom")
            .join("hcom.db");
        assert!(!is_test_temp_path(&escape));
    }

    #[test]
    fn test_atomic_write_io_replaces_plain_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("settings.json");
        fs::write(&target, "old").unwrap();
        atomic_write_io(&target, "new").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(!fs::symlink_metadata(&target).unwrap().is_symlink());
    }

    /// Redirection immunity: the DEFAULT primitive must NOT follow symlinks, so
    /// a pre-planted link at an internal state path (e.g. a flag counter under
    /// `~/.hcom/.tmp/flags/`) is destroyed by the rename rather than written
    /// through to whatever it targets. Without this the shared primitive would
    /// give every internal caller a redirection surface (settings.json et al.).
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_io_does_not_follow_symlink() {
        let tmp = TempDir::new().unwrap();
        let sensitive = tmp.path().join("sensitive.json");
        let planted = tmp.path().join("flag");
        fs::write(&sensitive, "do-not-touch").unwrap();
        // Attacker plants the internal-state path as a link to a sensitive file.
        std::os::unix::fs::symlink(&sensitive, &planted).unwrap();

        atomic_write_io(&planted, "1").unwrap();

        assert!(
            !fs::symlink_metadata(&planted).unwrap().is_symlink(),
            "no-follow write must replace the planted link with a real file"
        );
        assert_eq!(
            fs::read_to_string(&planted).unwrap(),
            "1",
            "the write must land at the literal path"
        );
        assert_eq!(
            fs::read_to_string(&sensitive).unwrap(),
            "do-not-touch",
            "the symlink target must NOT be written through"
        );
    }

    /// Regression: a rename onto a symlink used to replace the link with a
    /// regular file, detaching config that users link into a dotfiles repo. The
    /// opt-in following variant preserves the link and writes through it.
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_following_symlinks_writes_through_and_keeps_link() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let live = tmp.path().join("live");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&live).unwrap();
        let real = repo.join("settings.json");
        let link = live.join("settings.json");
        fs::write(&real, "from-repo").unwrap();
        // A dotfiles-tracked config is typically group/other-readable.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write_following_symlinks_io(&link, "written-by-hcom").unwrap();

        assert!(
            fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the symlink must survive an atomic write"
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "written-by-hcom",
            "content must land in the link target, not a new local file"
        );
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644,
            "the target's existing mode must survive; the write must not reset it to 0600"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_write_following_symlinks_follows_relative_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("target.json");
        let link = tmp.path().join("link.json");
        fs::write(&real, "before").unwrap();
        // Relative destinations resolve against the link's own directory.
        std::os::unix::fs::symlink("target.json", &link).unwrap();

        atomic_write_following_symlinks_io(&link, "after").unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "after");
    }

    /// A multi-hop chain (link1 -> link2 -> real) must resolve all the way
    /// through and land in `real`, with every link surviving. Mixes an absolute
    /// and a relative hop so a regression in second-hop relative joining (e.g.
    /// joining against the original path's parent instead of the current link's)
    /// is caught.
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_following_symlinks_follows_multi_hop_chain() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let real = nested.join("real.json");
        let link2 = nested.join("link2.json");
        let link1 = tmp.path().join("link1.json");
        fs::write(&real, "before").unwrap();
        // link2 -> real via a RELATIVE hop, resolved against link2's own dir.
        std::os::unix::fs::symlink("real.json", &link2).unwrap();
        // link1 -> link2 via an ABSOLUTE hop.
        std::os::unix::fs::symlink(&link2, &link1).unwrap();

        atomic_write_following_symlinks_io(&link1, "after").unwrap();

        assert!(fs::symlink_metadata(&link1).unwrap().is_symlink());
        assert!(fs::symlink_metadata(&link2).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "after",
            "a 2-hop chain must resolve through to the real target"
        );
    }

    /// The class invariant, pinned on the default (no-follow) primitive too:
    /// atomic_write over an existing regular file preserves that file's
    /// permission mode instead of resetting it to the tempfile's 0600.
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_io_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("config.json");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_io(&target, "new").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644,
            "an atomic overwrite must preserve the destination's mode, not reset it to 0600"
        );
    }

    /// A brand-new file (no existing destination) must keep the private 0600
    /// default — mode preservation only carries an EXISTING mode over.
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_io_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("fresh.json");

        atomic_write_io(&target, "x").unwrap();

        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600,
            "a freshly created file must stay 0600"
        );
    }

    /// A link into a checkout that has not created the file yet must create the
    /// target, not clobber the link.
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_following_symlinks_creates_dangling_target() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("not-yet").join("settings.json");
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&missing, &link).unwrap();

        atomic_write_following_symlinks_io(&link, "created").unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&missing).unwrap(), "created");
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_write_following_symlinks_rejects_cycle() {
        let tmp = TempDir::new().unwrap();
        let first = tmp.path().join("a");
        let second = tmp.path().join("b");
        std::os::unix::fs::symlink(&second, &first).unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();

        // Fails loudly rather than spinning or silently clobbering a link.
        let err = atomic_write_following_symlinks_io(&first, "content").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("symlink chain"));
    }

    /// A non-NotFound stat failure on the final component (e.g. EACCES because a
    /// parent lacks search permission) must PROPAGATE, not be silently treated as
    /// the dangling/new-file case — otherwise a symlink we merely failed to
    /// inspect would be renamed over and silently detached.
    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_propagates_non_notfound_stat_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("locked");
        fs::create_dir(&dir).unwrap();
        let inside = dir.join("settings.json");
        fs::write(&inside, "x").unwrap();
        // Drop search permission so lstat on a path inside fails EACCES, not
        // NotFound. A uid-0 process bypasses the check; skip the assertion there.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let bypasses_perms = fs::symlink_metadata(&inside).is_ok();

        let result = resolve_write_target(&inside);

        // Restore so TempDir can recurse in and clean up.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        if bypasses_perms {
            return; // running with a permission bypass (root); nothing to assert
        }
        let err = result.expect_err("a non-NotFound lstat error must propagate");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_ensure_hcom_directories_at() {
        let tmp = TempDir::new().unwrap();
        assert!(ensure_hcom_directories_at(tmp.path()));

        // Verify all directories were created
        assert!(tmp.path().join(LOGS_DIR).is_dir());
        assert!(tmp.path().join(LAUNCH_DIR).is_dir());
        assert!(tmp.path().join(FLAGS_DIR).is_dir());
        assert!(tmp.path().join(LAUNCHES_DIR).is_dir());
        assert!(tmp.path().join(ARCHIVE_DIR).is_dir());

        // Idempotent — second call succeeds too
        assert!(ensure_hcom_directories_at(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_hcom_directories_creates_private_base_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("state").join(".hcom");

        assert!(ensure_hcom_directories_at(&base));

        let mode = fs::metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_hcom_directories_restricts_existing_base_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join(".hcom");
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(ensure_hcom_directories_at(&base));

        let mode = fs::metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn test_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let filepath = tmp.path().join("test.txt");

        assert!(atomic_write(&filepath, "hello world"));
        assert_eq!(fs::read_to_string(&filepath).unwrap(), "hello world");

        // Overwrite
        assert!(atomic_write(&filepath, "new content"));
        assert_eq!(fs::read_to_string(&filepath).unwrap(), "new content");
    }

    #[test]
    fn test_atomic_write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let filepath = tmp.path().join("a").join("b").join("test.txt");

        assert!(atomic_write(&filepath, "nested"));
        assert_eq!(fs::read_to_string(&filepath).unwrap(), "nested");
    }

    #[test]
    fn test_flag_counters() {
        let tmp = TempDir::new().unwrap();

        // Counter starts at 0 (read raw flag file)
        assert_eq!(
            read_flag_file(&tmp.path().join(FLAGS_DIR).join("test_flag")),
            0
        );

        assert_eq!(increment_flag_counter_at(tmp.path(), "test_flag"), 1);
        assert_eq!(
            read_flag_file(&tmp.path().join(FLAGS_DIR).join("test_flag")),
            1
        );

        assert_eq!(increment_flag_counter_at(tmp.path(), "test_flag"), 2);
        assert_eq!(
            read_flag_file(&tmp.path().join(FLAGS_DIR).join("test_flag")),
            2
        );

        // Different flag is independent
        assert_eq!(
            read_flag_file(&tmp.path().join(FLAGS_DIR).join("other_flag")),
            0
        );
    }

    #[test]
    fn test_get_project_root_logic() {
        // get_project_root returns parent of hcom_dir
        // Test the logic directly without relying on global Config
        let base = Path::new("/home/test/.hcom");
        assert_eq!(
            base.parent().unwrap().to_path_buf(),
            PathBuf::from("/home/test")
        );
    }

    #[test]
    fn test_resolve_hcom_dir_default() {
        let env = HashMap::from([("HOME".to_string(), "/home/test".to_string())]);
        let (path, overridden) = resolve_hcom_dir_from_env(&env, Path::new("/worktree"));
        assert_eq!(path, PathBuf::from("/home/test/.hcom"));
        assert!(!overridden);
    }

    #[test]
    fn test_resolve_hcom_dir_expands_tilde() {
        let env = HashMap::from([
            ("HOME".to_string(), "/home/test".to_string()),
            ("HCOM_DIR".to_string(), "~/custom/.hcom".to_string()),
        ]);
        let (path, overridden) = resolve_hcom_dir_from_env(&env, Path::new("/worktree"));
        assert_eq!(path, PathBuf::from("/home/test/custom/.hcom"));
        assert!(overridden);
    }

    #[test]
    fn test_protected_hcom_dir_component() {
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/proj/.git/hcom")),
            Some(".git")
        );
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/.codex/hcom")),
            Some(".codex")
        );
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/.claude/.hcom")),
            Some(".claude")
        );
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/.agents/.hcom")),
            Some(".agents")
        );
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/.hcom")),
            None
        );
        // Component-wise match: '.gitfoo' must not trigger.
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/.gitfoo/.hcom")),
            None
        );
        assert_eq!(
            protected_hcom_dir_component(Path::new("/home/u/proj/.hcom/sub")),
            None
        );
    }

    #[test]
    fn test_resolve_hcom_dir_makes_relative_absolute() {
        let env = HashMap::from([("HCOM_DIR".to_string(), "relative/.hcom".to_string())]);
        let (path, overridden) = resolve_hcom_dir_from_env(&env, Path::new("/worktree"));
        assert_eq!(path, PathBuf::from("/worktree").join("relative/.hcom"));
        assert!(overridden);
    }
}
