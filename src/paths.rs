//! Where baton keeps its state, plus the small filesystem helpers every
//! module shares. Everything lives under `$BATON_HOME` (default `~/.baton`)
//! so tests can point it at a temp dir. That tree holds prompts, code and
//! conversation excerpts, so it is created 0700 and files are written 0600.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn baton_home() -> PathBuf {
    match std::env::var_os("BATON_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir().join(".baton"),
    }
}

pub fn accounts_dir() -> PathBuf {
    baton_home().join("accounts")
}

pub fn handoffs_dir() -> PathBuf {
    baton_home().join("handoffs")
}

pub fn runs_dir() -> PathBuf {
    baton_home().join("runs")
}

pub fn config_file() -> PathBuf {
    baton_home().join("config.toml")
}

pub fn quota_file() -> PathBuf {
    baton_home().join("quota.json")
}

pub fn events_log() -> PathBuf {
    baton_home().join("events.jsonl")
}

/// The default Claude Code config dir (`~/.claude`), i.e. the `default` account.
pub fn claude_default_dir() -> PathBuf {
    home_dir().join(".claude")
}

/// Compact local timestamp used in file and directory names.
pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Create `dir` (under `$BATON_HOME`) and make it and `$BATON_HOME` 0700.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    chmod(&baton_home(), 0o700)?;
    chmod(dir, 0o700)
}

fn open_private(path: &Path, append: bool) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let f = o.open(path)?;
    chmod(path, 0o600)?;
    Ok(f)
}

pub fn write_private(path: &Path, body: impl AsRef<[u8]>) -> std::io::Result<()> {
    open_private(path, false)?.write_all(body.as_ref())
}

pub fn append_private(path: &Path, line: &str) -> std::io::Result<()> {
    writeln!(open_private(path, true)?, "{line}")
}

/// Replace a file atomically, keeping `mode`. Follows a symlink at `path`
/// so a linked file (e.g. a synced settings.json) stays linked.
pub fn atomic_write(path: &Path, body: impl AsRef<[u8]>, mode: u32) -> std::io::Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = real.with_extension("baton-tmp");
    std::fs::write(&tmp, body)?;
    chmod(&tmp, mode)?;
    std::fs::rename(&tmp, &real)
}

/// Current permission bits of a file, or `default` when it doesn't exist.
pub fn mode_of(path: &Path, default: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(default)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        default
    }
}

/// An exclusive `flock` held until dropped.
pub struct FileLock(#[allow(dead_code)] File);

fn flock(path: &Path, nonblocking: bool) -> std::io::Result<Option<FileLock>> {
    let f = open_private(path, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let op = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
        // SAFETY: valid fd owned by `f`.
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            let err = std::io::Error::last_os_error();
            if nonblocking && err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(err);
        }
    }
    Ok(Some(FileLock(f)))
}

/// Block until the lock is ours.
pub fn lock(path: &Path) -> std::io::Result<FileLock> {
    Ok(flock(path, false)?.expect("blocking flock returns a lock"))
}

/// `None` when another process holds the lock.
pub fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    flock(path, true)
}

/// The git top-level of `path`, or `path` itself (canonicalized) outside a
/// repository. Handoffs and write-mode limits work per repository, so a
/// Claude that `cd`-ed into a subdirectory still matches.
pub fn repo_root(path: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    git_toplevel(&canon).unwrap_or(canon)
}

fn git_command(path: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(path);
    for key in [
        "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_NAMESPACE",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

pub fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let out = git_command(path)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!top.is_empty()).then(|| std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(top)))
}

/// Find the nearest physical repository boundary, starting at a canonical path.
pub fn physical_git_boundary(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|dir| {
        let marker = std::fs::symlink_metadata(dir.join(".git")).ok()?;
        // A .git symlink is not a boundary marker; keep walking to a physical
        // directory or regular file (including linked worktrees and submodules).
        (marker.is_dir() || marker.is_file()).then(|| dir.to_path_buf())
    })
}

/// A hash of everything an agent could have changed in the working tree:
/// status, both diffs, and size/mtime of untracked files (their content is in
/// no diff). Gitignored paths are left out on purpose: build output would
/// count as a change. `None` when git cannot tell; callers treat that as changed.
pub fn worktree_fingerprint(path: &Path) -> Option<String> {
    let run = |args: &[&str]| {
        git_command(path)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    let status = run(&["status", "--porcelain=v1", "-uall", "--ignore-submodules=none"])?;
    let mut out = format!(
        "{status}\0{}\0{}",
        run(&["diff", "--no-ext-diff", "--no-textconv", "--binary"])?,
        run(&["diff", "--cached", "--no-ext-diff", "--no-textconv", "--binary"])?
    );
    let top = git_toplevel(path)?;
    for name in status.lines().filter_map(|l| l.strip_prefix("?? ")) {
        let meta = std::fs::symlink_metadata(top.join(name.trim_matches('"'))).ok();
        let stamp = meta.and_then(|m| Some((m.len(), m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())));
        out.push_str(&format!("\0{name}:{stamp:?}"));
    }
    // Only compared within one process, so the std hasher is enough.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    out.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

/// Same repository: equal roots, or one inside the other (for non-git dirs).
pub fn same_repo(a: &Path, b: &Path) -> bool {
    let (ra, rb) = (repo_root(a), repo_root(b));
    ra.starts_with(&rb) || rb.starts_with(&ra)
}

/// Replace terminal control characters (except newline and tab) so text
/// written by another agent can't drive the terminal when printed.
pub fn sanitize_for_terminal(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\t' { '\u{fffd}' } else { c })
        .collect()
}

/// Serialises tests that set `BATON_HOME` or `BATON_*_BIN`. Those variables
/// are process-global and the suite runs tests in parallel.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets an env var and removes it on drop, so a panic cannot leak it.
#[cfg(test)]
pub(crate) struct EnvVar(&'static str, Option<std::ffi::OsString>);

#[cfg(test)]
impl EnvVar {
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvVar {
        let old = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvVar(key, old)
    }
}

#[cfg(test)]
impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_fingerprint_sees_untracked_and_tracked_edits() {
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
        let empty = worktree_fingerprint(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one").unwrap();
        let added = worktree_fingerprint(&root).unwrap();
        assert_ne!(empty, added);
        std::fs::write(root.join("a.txt"), "two, longer").unwrap();
        let edited = worktree_fingerprint(&root).unwrap();
        assert_ne!(added, edited);
        assert_eq!(edited, worktree_fingerprint(&root).unwrap());
        assert!(std::process::Command::new("git").args(["add", "a.txt"]).current_dir(&root).status().unwrap().success());
        let staged = worktree_fingerprint(&root).unwrap();
        std::fs::write(root.join("a.txt"), "three").unwrap();
        assert_ne!(staged, worktree_fingerprint(&root).unwrap());
        let outside = tempfile::tempdir().unwrap();
        assert!(worktree_fingerprint(outside.path()).is_none());
    }

    #[test]
    fn git_command_removes_ambient_repository_and_config_overrides() {
        let cmd = git_command(Path::new("/"));
        for key in [
            "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_CONFIG", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS", "GIT_NAMESPACE",
        ] {
            assert!(cmd.get_envs().any(|(k, v)| k == key && v.is_none()), "{key} must be removed");
        }
    }

    #[test]
    fn git_toplevel_finds_repository_from_subdirectory() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.path().join(".git/refs/heads")).unwrap();
        std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(git_toplevel(&sub), Some(root.clone()));
        assert_eq!(repo_root(&sub), root);
    }

    #[test]
    fn physical_git_boundary_uses_nearest_directory_or_file() {
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        let nested = root.join("nested");
        let sub = nested.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(physical_git_boundary(&sub), None);
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(physical_git_boundary(&sub), Some(root.clone()));
        std::fs::write(nested.join(".git"), "gitdir: /unused\n").unwrap();
        assert_eq!(physical_git_boundary(&sub), Some(nested.clone()));
        assert_eq!(physical_git_boundary(&nested), Some(nested));
        assert_eq!(physical_git_boundary(&root), Some(root));
    }

    #[cfg(unix)]
    #[test]
    fn physical_git_boundary_skips_git_symlinks() {
        use std::os::unix::fs::symlink;
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        symlink(root.join(".git"), nested.join(".git")).unwrap();
        assert_eq!(physical_git_boundary(&nested), Some(root.clone()));
        std::fs::remove_dir(root.join(".git")).unwrap();
        assert_eq!(physical_git_boundary(&nested), None);
    }

    #[test]
    fn sanitizes_escape_sequences() {
        assert_eq!(sanitize_for_terminal("a\u{1b}]52;c;x\u{7}b\n\tc"), "a\u{fffd}]52;c;x\u{fffd}b\n\tc");
    }

    #[test]
    fn lock_is_exclusive_across_handles() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l");
        let held = lock(&p).unwrap();
        assert!(try_lock(&p).unwrap().is_none());
        drop(held);
        // Other tests fork children concurrently; a fork briefly shares our
        // fd until its exec closes it (CLOEXEC), so allow a short retry.
        let free = (0..50).any(|_| {
            let got = try_lock(&p).unwrap().is_some();
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            got
        });
        assert!(free);
    }

    #[test]
    fn private_files_are_0600() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        write_private(&p, "x").unwrap();
        assert_eq!(mode_of(&p, 0), 0o600);
    }

    #[test]
    fn same_repo_by_prefix_outside_git() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(same_repo(d.path(), &sub));
        let other = tempfile::tempdir().unwrap();
        assert!(!same_repo(d.path(), other.path()));
    }
}
