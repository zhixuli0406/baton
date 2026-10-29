//! Turn a Claude Code transcript (`<session>.jsonl`) into a handoff brief
//! another agent can act on without seeing the conversation.
//!
//! The format is internal to Claude Code, so parsing is best effort: lines
//! that don't match are skipped, never fatal.

use std::path::Path;

use serde_json::Value;

const RECENT_USER: usize = 5;
const RECENT_ASSISTANT: usize = 4;
const MSG_CAP: usize = 2500;
const SUMMARY_CAP: usize = 8000;
const FILES_CAP: usize = 40;

#[derive(Debug, Default, PartialEq)]
pub struct Digest {
    pub first_request: Option<String>,
    pub compact_summary: Option<String>,
    pub recent_user: Vec<String>,
    pub recent_assistant: Vec<String>,
    pub todos: Vec<String>,
    pub edited_files: Vec<String>,
}

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

pub fn digest(jsonl: &str) -> Digest {
    let mut d = Digest::default();
    let mut users: Vec<String> = Vec::new();
    let mut assistants: Vec<String> = Vec::new();
    for line in jsonl.lines() {
        let Ok(o) = serde_json::from_str::<Value>(line) else { continue };
        if o.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = o.pointer("/message/content");
        match o.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(text) = content.map(user_text).filter(|t| !t.is_empty()) else { continue };
                if o.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                    d.compact_summary = Some(text);
                    continue;
                }
                if o.get("isMeta").and_then(Value::as_bool) == Some(true) || text.starts_with('<') {
                    continue;
                }
                if d.first_request.is_none() {
                    d.first_request = Some(text.clone());
                }
                users.push(text);
            }
            Some("assistant") => {
                let Some(Value::Array(blocks)) = content else { continue };
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str).map(str::trim) {
                                if !t.is_empty() {
                                    assistants.push(t.to_string());
                                }
                            }
                        }
                        Some("tool_use") => record_tool(&mut d, b),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    d.recent_user = last_n(users, RECENT_USER);
    d.recent_assistant = last_n(assistants, RECENT_ASSISTANT);
    d
}

fn user_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn record_tool(d: &mut Digest, block: &Value) {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    let input = block.get("input");
    if EDIT_TOOLS.contains(&name) {
        let path = input
            .and_then(|i| i.get("file_path").or_else(|| i.get("notebook_path")))
            .and_then(Value::as_str);
        if let Some(p) = path {
            d.edited_files.retain(|f| f != p);
            d.edited_files.push(p.to_string());
            if d.edited_files.len() > FILES_CAP {
                d.edited_files.remove(0);
            }
        }
    } else if name == "TodoWrite" {
        if let Some(Value::Array(todos)) = input.and_then(|i| i.get("todos")) {
            d.todos = todos
                .iter()
                .filter_map(|t| {
                    let content = t.get("content").and_then(Value::as_str)?;
                    let status = t.get("status").and_then(Value::as_str).unwrap_or("?");
                    Some(format!("[{status}] {content}"))
                })
                .collect();
        }
    }
}

fn last_n(v: Vec<String>, n: usize) -> Vec<String> {
    let skip = v.len().saturating_sub(n);
    v.into_iter().skip(skip).collect()
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head} …[cut]")
}

/// Token prefixes of common credentials (OpenAI/Anthropic, GitHub, Slack,
/// AWS, Google). Users paste keys into conversations; the brief goes to a
/// third-party vendor, so these never leave verbatim.
const SECRET_PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "ghu_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "xoxa-", "AKIA", "ASIA", "AIza", "glpat-", "xai-"];

fn looks_secret(tok: &str) -> bool {
    if tok.len() < 20 {
        return false;
    }
    if SECRET_PREFIXES.iter().any(|p| tok.starts_with(p)) {
        return true;
    }
    // JWT: three base64url segments, header starting with `eyJ`.
    tok.starts_with("eyJ") && tok.split('.').filter(|p| !p.is_empty()).count() == 3
}

/// Mask credentials and private key blocks.
pub fn redact(text: &str) -> String {
    let mut s = text.to_string();
    while let Some(start) = s.find("-----BEGIN ").filter(|&i| s[i..].lines().next().is_some_and(|l| l.contains("PRIVATE KEY"))) {
        let end = s[start..]
            .find("-----END ")
            .and_then(|e| {
                let after = start + e + "-----END ".len();
                s[after..].find("-----").map(|x| after + x + "-----".len())
            })
            .unwrap_or(s.len());
        s.replace_range(start..end, "[REDACTED PRIVATE KEY]");
    }
    let is_tok = |c: char| c.is_ascii_alphanumeric() || "_-.+/=".contains(c);
    let mut out = String::with_capacity(s.len());
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut String| {
        out.push_str(if looks_secret(tok) { "[REDACTED]" } else { tok });
        tok.clear();
    };
    for c in s.chars() {
        if is_tok(c) {
            tok.push(c);
        } else {
            flush(&mut tok, &mut out);
            out.push(c);
        }
    }
    flush(&mut tok, &mut out);
    out
}

/// Markdown brief for the agent taking over, with credentials masked.
pub fn brief(d: &Digest, cwd: &Path, git: &str) -> String {
    redact(&brief_raw(d, cwd, git))
}

fn brief_raw(d: &Digest, cwd: &Path, git: &str) -> String {
    let mut out = String::new();
    out.push_str("# Handoff from Claude Code\n\n");
    out.push_str(&format!("Working directory: `{}`\n\n", cwd.display()));
    if let Some(r) = &d.first_request {
        out.push_str("## Original request\n\n");
        out.push_str(&clip(r, MSG_CAP));
        out.push_str("\n\n");
    }
    if let Some(s) = &d.compact_summary {
        out.push_str("## Summary of the earlier conversation\n\n");
        out.push_str(&clip(s, SUMMARY_CAP));
        out.push_str("\n\n");
    }
    if !d.recent_user.is_empty() {
        out.push_str("## Most recent user messages (oldest first)\n\n");
        for m in &d.recent_user {
            out.push_str(&format!("- {}\n", clip(m, MSG_CAP).replace('\n', "\n  ")));
        }
        out.push('\n');
    }
    if !d.recent_assistant.is_empty() {
        out.push_str("## Claude's last messages (oldest first)\n\n");
        for m in &d.recent_assistant {
            out.push_str(&format!("---\n{}\n", clip(m, MSG_CAP)));
        }
        out.push('\n');
    }
    if !d.todos.is_empty() {
        out.push_str("## Todo list when Claude stopped\n\n");
        for t in &d.todos {
            out.push_str(&format!("- {t}\n"));
        }
        out.push('\n');
    }
    if !d.edited_files.is_empty() {
        out.push_str("## Files Claude edited in this session\n\n");
        for f in &d.edited_files {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    if !git.trim().is_empty() {
        out.push_str("## Git state\n\n```\n");
        out.push_str(git.trim_end());
        out.push_str("\n```\n");
    }
    out
}

/// `git status --short` + `git diff --stat`, or empty when not a repo.
pub fn git_state(cwd: &Path) -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    let Some(status) = run(&["status", "--short", "--branch"]) else { return String::new() };
    let stat = run(&["diff", "--stat", "HEAD"]).unwrap_or_default();
    clip(&format!("$ git status --short --branch\n{status}\n$ git diff --stat HEAD\n{stat}"), 6000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/transcript.jsonl");

    #[test]
    fn digests_fixture() {
        let d = digest(FIXTURE);
        assert_eq!(d.first_request.as_deref(), Some("Add a --verbose flag to the CLI"));
        assert_eq!(d.compact_summary.as_deref(), Some("Earlier: we set up clap."));
        assert_eq!(d.recent_user.last().unwrap(), "also update the README");
        assert!(d.recent_user.iter().all(|u| !u.starts_with('<')));
        assert_eq!(d.recent_assistant.last().unwrap(), "Now updating README.");
        assert_eq!(d.edited_files, ["/repo/README.md", "/repo/src/main.rs"]);
        assert_eq!(d.todos, ["[completed] add flag", "[in_progress] update README"]);
    }

    #[test]
    fn skips_garbage_and_sidechains() {
        let d = digest("not json\n{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"content\":\"sub\"}}\n");
        assert_eq!(d, Digest::default());
    }

    #[test]
    fn brief_has_sections() {
        let d = digest(FIXTURE);
        let b = brief(&d, Path::new("/repo"), "## main\n M src/main.rs\n");
        for s in ["Original request", "Summary of the earlier", "Most recent user", "Claude's last", "Todo list", "Files Claude edited", "Git state"] {
            assert!(b.contains(s), "missing {s}");
        }
    }

    #[test]
    fn redacts_credentials() {
        let t = "key sk-ant-api03-abcdefghijklmnopqrstuv and ghp_abcdefghijklmnopqrstuvwxyz0123 ok\nAKIAABCDEFGHIJKLMNOP, \"AIzaSyA1234567890abcdefghijklmnopqrstu\"\njwt eyJhbGciOi.eyJzdWIiOiIx.c2lnbmF0dXJlc2ln";
        let r = redact(t);
        assert!(!r.contains("sk-ant"), "{r}");
        assert!(!r.contains("ghp_"));
        assert!(!r.contains("AKIA"));
        assert!(!r.contains("AIza"));
        assert!(!r.contains("eyJ"));
        assert!(r.contains("key [REDACTED] and [REDACTED] ok"));
        // Ordinary words and short tokens survive.
        assert_eq!(redact("fix sk-1 in src/main.rs"), "fix sk-1 in src/main.rs");
    }

    #[test]
    fn redacts_private_key_blocks() {
        let t = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let r = redact(t);
        assert!(r.contains("before\n[REDACTED PRIVATE KEY]"), "{r}");
        assert!(r.contains("after"));
        assert!(!r.contains("b3BlbnNzaC1rZXk"));
        assert_eq!(redact("-----BEGIN CERTIFICATE-----"), "-----BEGIN CERTIFICATE-----");
    }

    #[test]
    fn clip_marks_cut() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc …[cut]");
    }
}
