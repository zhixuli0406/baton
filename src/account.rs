//! Several Claude accounts, one `CLAUDE_CONFIG_DIR` each (the documented
//! way to keep logins apart). Shared files are symlinked from `~/.claude`
//! so every account sees the same settings, rules, skills and transcripts.
//! Switching is always a manual command; baton never rotates on its own.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::paths;

pub const DEFAULT: &str = "default";

/// Entries of `~/.claude` shared by every account. Credentials and
/// `.claude.json` (login identity) stay per account.
pub const SHARED: &[&str] = &[
    "CLAUDE.md",
    "settings.json",
    "settings.local.json",
    "keybindings.json",
    "rules",
    "skills",
    "agents",
    "commands",
    "output-styles",
    "plugins",
    "projects",
    "file-history",
    "todos",
    "plans",
    "history.jsonl",
];

pub fn claude_bin() -> String {
    match std::env::var("BATON_CLAUDE_BIN") {
        Ok(v) if !v.is_empty() => v,
        _ => "claude".into(),
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("account name must be 1-32 chars of a-z, 0-9, - or _, starting with a letter or digit");
    }
    Ok(())
}

/// Config dir for an account; `None` means `~/.claude` (no env var).
pub fn config_dir(name: &str) -> Result<Option<PathBuf>> {
    if name == DEFAULT {
        return Ok(None);
    }
    validate_name(name)?;
    let dir = paths::accounts_dir().join(name);
    if !dir.is_dir() {
        bail!("no account named {name} (see `baton account list`)");
    }
    Ok(Some(dir))
}

/// `.claude.json` for an account.
fn state_file(dir: Option<&Path>) -> PathBuf {
    match dir {
        Some(d) => d.join(".claude.json"),
        None => paths::home_dir().join(".claude.json"),
    }
}

pub fn claude_command(dir: Option<&Path>) -> Command {
    let mut cmd = Command::new(claude_bin());
    match dir {
        Some(d) => {
            cmd.env("CLAUDE_CONFIG_DIR", d);
        }
        None => {
            cmd.env_remove("CLAUDE_CONFIG_DIR");
        }
    }
    cmd
}

pub fn names() -> Vec<String> {
    let mut v = vec![DEFAULT.to_string()];
    if let Ok(entries) = std::fs::read_dir(paths::accounts_dir()) {
        let mut extra: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| validate_name(n).is_ok())
            .collect();
        extra.sort();
        v.extend(extra);
    }
    v
}

/// Create (or repair) the shared symlinks of an account dir. Returns
/// warnings for entries that exist but are not our symlinks.
pub fn link_shared(dir: &Path, source: &Path) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for entry in SHARED {
        let target = source.join(entry);
        if !target.exists() {
            continue;
        }
        let link = dir.join(entry);
        match std::fs::symlink_metadata(&link) {
            Err(_) => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &link)
                    .with_context(|| format!("cannot link {}", link.display()))?;
            }
            Ok(m) if m.file_type().is_symlink() => {}
            Ok(_) => warnings.push(format!(
                "{} is a real file, not shared with {} (Claude Code may have rewritten it)",
                link.display(),
                target.display()
            )),
        }
    }
    Ok(warnings)
}

/// Merge the default account's user-scope MCP servers into an account's
/// `.claude.json`. Same-named servers take the default's definition; servers
/// only that account has are kept. Returns the names that were updated.
pub fn sync_mcp(dir: &Path) -> Result<Vec<String>> {
    sync_mcp_from(&state_file(None), dir)
}

fn sync_mcp_from(default_state_file: &Path, dir: &Path) -> Result<Vec<String>> {
    let default_state = read_json(default_state_file)?;
    let Some(Value::Object(servers)) = default_state.get("mcpServers").cloned() else { return Ok(vec![]) };
    let path = state_file(Some(dir));
    let mut state = read_json(&path)?;
    if !state.is_object() {
        state = Value::Object(Default::default());
    }
    if !state.get("mcpServers").is_some_and(Value::is_object) {
        state["mcpServers"] = Value::Object(Default::default());
    }
    let target = state["mcpServers"].as_object_mut().expect("object");
    let mut changed = Vec::new();
    for (name, def) in servers {
        if target.get(&name) != Some(&def) {
            changed.push(name.clone());
            target.insert(name, def);
        }
    }
    if !changed.is_empty() {
        let mode = paths::mode_of(&path, 0o600);
        paths::atomic_write(&path, serde_json::to_vec_pretty(&state)?, mode)?;
    }
    Ok(changed)
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("invalid JSON in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn add(name: &str, login: bool) -> Result<PathBuf> {
    validate_name(name)?;
    if name == DEFAULT {
        bail!("`default` is ~/.claude itself and always exists");
    }
    let dir = paths::accounts_dir().join(name);
    if dir.exists() {
        bail!("account {name} already exists at {}", dir.display());
    }
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    for w in link_shared(&dir, &paths::claude_default_dir())? {
        eprintln!("warning: {w}");
    }
    sync_mcp(&dir)?;
    if login {
        let status = claude_command(Some(&dir)).args(["auth", "login"]).status()
            .context("cannot run `claude auth login`")?;
        if !status.success() {
            bail!("login did not complete; finish it later with `baton run {name}` and /login");
        }
    }
    Ok(dir)
}

pub fn remove(name: &str) -> Result<()> {
    let Some(dir) = config_dir(name)? else { bail!("cannot remove the default account") };
    // Log out first so the Keychain entry for this dir is cleared.
    let _ = claude_command(Some(&dir)).args(["auth", "logout"]).status();
    // remove_dir_all removes symlinks themselves, never their targets.
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct Info {
    pub name: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub logged_in: bool,
    pub five_hour: Option<(i64, String)>,
    pub seven_day: Option<(i64, String)>,
}

pub fn info(name: &str) -> Result<Info> {
    let dir = config_dir(name)?;
    let mut i = Info { name: name.into(), ..Default::default() };
    if let Ok(out) = claude_command(dir.as_deref()).args(["auth", "status", "--json"]).output() {
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            i.logged_in = v.get("loggedIn").and_then(Value::as_bool).unwrap_or(false);
            i.email = v.get("email").and_then(Value::as_str).map(str::to_string);
            i.plan = v.get("subscriptionType").and_then(Value::as_str).map(str::to_string);
        }
    }
    let state = read_json(&state_file(dir.as_deref())).unwrap_or_default();
    let window = |w: &str| {
        let u = state.pointer(&format!("/cachedUsageUtilization/utilization/{w}"))?;
        Some((u.get("utilization")?.as_i64()?, u.get("resets_at").and_then(Value::as_str).unwrap_or("").to_string()))
    };
    i.five_hour = window("five_hour");
    i.seven_day = window("seven_day");
    Ok(i)
}

/// Replace this process with `claude` running under the account.
pub fn exec_claude(name: &str, args: &[String]) -> Result<()> {
    let dir = config_dir(name)?;
    let mut cmd = claude_command(dir.as_deref());
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err).context("cannot exec claude")
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        for ok in ["work", "a", "acct-2", "x_y", "9lives"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Work", "-x", "a/b", "..", "a b", &"x".repeat(33)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sync_merges_and_keeps_account_only_servers() {
        let home = tempfile::tempdir().unwrap();
        let acct = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".claude.json"), r#"{"mcpServers":{"a":{"command":"new"},"b":{"command":"b"}},"oauthAccount":{"x":1}}"#).unwrap();
        std::fs::write(acct.path().join(".claude.json"), r#"{"mcpServers":{"a":{"command":"old"},"mine":{"command":"m"}},"oauthAccount":{"y":2}}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(acct.path().join(".claude.json"), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let changed = sync_mcp_from(&home.path().join(".claude.json"), acct.path()).unwrap();
        assert_eq!(changed, ["a", "b"]);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(acct.path().join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["a"]["command"], "new");
        assert_eq!(v["mcpServers"]["mine"]["command"], "m");
        assert_eq!(v["oauthAccount"]["y"], 2);
        assert!(v.get("x").is_none());
        assert_eq!(paths::mode_of(&acct.path().join(".claude.json"), 0), 0o600);
    }

    #[test]
    fn links_only_existing_entries_and_reports_real_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("rules")).unwrap();
        std::fs::write(src.path().join("settings.json"), "{}").unwrap();
        std::fs::write(src.path().join("CLAUDE.md"), "x").unwrap();
        std::fs::write(dst.path().join("CLAUDE.md"), "local").unwrap();
        let warnings = link_shared(dst.path(), src.path()).unwrap();
        assert!(std::fs::symlink_metadata(dst.path().join("rules")).unwrap().file_type().is_symlink());
        assert!(std::fs::symlink_metadata(dst.path().join("settings.json")).unwrap().file_type().is_symlink());
        assert!(!dst.path().join("skills").exists());
        assert_eq!(warnings.len(), 1);
        // Idempotent.
        assert_eq!(link_shared(dst.path(), src.path()).unwrap().len(), 1);
    }
}
