//! `baton install`: register the hooks in `~/.claude/settings.json` and the
//! MCP server with `claude mcp add`. Dry run unless `--apply`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{account, paths};

/// (event, matcher, hook subcommand). PreToolUse has no matcher: it blocks
/// only mutating tools, but delivers a finished handoff's report on any tool.
const HOOKS: &[(&str, Option<&str>, &str)] = &[
    ("StopFailure", Some("rate_limit"), "stop-failure"),
    ("PreToolUse", None, "pre-tool-use"),
    ("UserPromptSubmit", None, "user-prompt-submit"),
];

fn quote(exe: &str) -> String {
    if exe.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        exe.to_string()
    } else {
        format!("'{}'", exe.replace('\'', "'\\''"))
    }
}

fn is_baton_hook(command: &str, sub: &str) -> bool {
    command.contains("baton") && command.trim_end().ends_with(&format!("hook {sub}"))
}

/// Add baton's hooks to a settings object, or point existing baton hooks at
/// `exe` if it moved. Returns "added X" / "updated X" lines.
pub fn merge_hooks(settings: &mut Value, exe: &str) -> Vec<String> {
    if !settings.is_object() {
        *settings = json!({});
    }
    if !settings.get("hooks").is_some_and(Value::is_object) {
        settings["hooks"] = json!({});
    }
    let mut added = Vec::new();
    for (event, matcher, sub) in HOOKS {
        let list = settings["hooks"]
            .as_object_mut()
            .expect("hooks is an object")
            .entry(event.to_string())
            .or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        let wanted = format!("{} hook {sub}", quote(exe));
        let mut present = false;
        for group in list.as_array_mut().unwrap() {
            let Some(hs) = group.get_mut("hooks").and_then(Value::as_array_mut) else { continue };
            for h in hs {
                let Some(cmd) = h.get("command").and_then(Value::as_str) else { continue };
                if is_baton_hook(cmd, sub) {
                    present = true;
                    if cmd != wanted {
                        h["command"] = json!(wanted);
                        added.push(format!("updated {event}"));
                    }
                }
            }
        }
        if present {
            continue;
        }
        let mut group = json!({
            "hooks": [{"type": "command", "command": wanted, "timeout": 30}]
        });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        list.as_array_mut().unwrap().push(group);
        added.push(format!("added {event}"));
    }
    added
}

pub fn run(apply: bool) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let exe_s = exe.to_string_lossy().to_string();
    if exe_s.contains("/target/") {
        eprintln!("warning: baton is running from a build directory ({exe_s}).\n         Hooks will point there; prefer `cargo install --path .` first.");
    }
    let settings_path = paths::claude_default_dir().join("settings.json");
    let mut settings: Value = match std::fs::read_to_string(&settings_path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("invalid JSON in {}", settings_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).context("cannot read settings.json"),
    };
    let added = merge_hooks(&mut settings, &exe_s);
    let mcp_args = ["mcp", "add", "--scope", "user", "baton", "--", &exe_s, "mcp"];

    println!("settings file: {}", settings_path.display());
    if added.is_empty() {
        println!("hooks: already installed");
    } else {
        for a in &added {
            println!("hooks: {a}");
        }
        for (event, _, sub) in HOOKS {
            println!("  {event}: {} hook {sub}", quote(&exe_s));
        }
    }
    println!("mcp: claude mcp remove --scope user baton (if present), then claude {}", mcp_args.join(" "));
    println!("accounts: user-scope MCP servers are merged into every baton account");

    if !apply {
        println!("\nDry run. Re-run with --apply to write these changes.");
        return Ok(());
    }
    if !added.is_empty() {
        backup(&settings_path)?;
        let mode = paths::mode_of(&settings_path, 0o644);
        paths::atomic_write(&settings_path, serde_json::to_string_pretty(&settings)? + "\n", mode)?;
        println!("hooks written");
    }
    // Re-register so a moved binary is picked up.
    let _ = account::claude_command(None)
        .args(["mcp", "remove", "--scope", "user", "baton"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status = account::claude_command(None).args(mcp_args).status().context("cannot run claude mcp add")?;
    if !status.success() {
        bail!("`claude mcp add` failed; check `claude mcp list`");
    }
    for name in account::names().into_iter().filter(|n| n != account::DEFAULT) {
        if let Ok(Some(dir)) = account::config_dir(&name) {
            let changed = account::sync_mcp(&dir)?;
            if !changed.is_empty() {
                println!("account {name}: updated MCP servers {}", changed.join(", "));
            }
        }
    }
    Ok(())
}

fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let to = path.with_extension(format!("json.baton-bak-{}", paths::stamp()));
        std::fs::copy(path, &to)?;
        println!("backup: {}", to.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_into_existing_hooks_without_duplicates() {
        let mut s = json!({"model": "opus", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other"}]}]}});
        let added = merge_hooks(&mut s, "/usr/local/bin/baton");
        assert_eq!(added, ["added StopFailure", "added PreToolUse", "added UserPromptSubmit"]);
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["StopFailure"][0]["matcher"], "rate_limit");
        assert!(s["hooks"]["PreToolUse"][1].get("matcher").is_none());
        assert!(s["hooks"]["UserPromptSubmit"][0].get("matcher").is_none());
        assert_eq!(s["hooks"]["StopFailure"][0]["hooks"][0]["command"], "/usr/local/bin/baton hook stop-failure");
        // Second run adds nothing.
        assert!(merge_hooks(&mut s, "/usr/local/bin/baton").is_empty());
        // A moved binary updates in place instead of duplicating.
        let moved = merge_hooks(&mut s, "/opt/bin/baton");
        assert_eq!(moved.len(), 3);
        assert!(moved.iter().all(|m| m.starts_with("updated")));
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["PreToolUse"][1]["hooks"][0]["command"], "/opt/bin/baton hook pre-tool-use");
    }

    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(quote("/a b/baton"), "'/a b/baton'");
        assert_eq!(quote("/a/baton"), "/a/baton");
    }
}
