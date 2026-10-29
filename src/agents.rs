//! Headless calls to the other vendors' coding agents.
//!
//! Each call is one fresh process of the vendor's own CLI, run in the target
//! working directory with the user's existing login. Flags were checked on
//! 2026-10-21 against codex-cli 0.163.0 and grok 1.0.62; effort flags on
//! 2026-10-27 against codex-cli 0.164.0 and grok 1.0.67.
//!
//! Antigravity (`agy`) was removed on 2026-10-27: Google's FAQ says wrapping
//! `agy` inside a third-party coding agent is not supported on consumer
//! plans, and a suspension also blocks Gemini CLI and Code Assist.
//!
//! Unlike an isolated reviewer, these agents are meant to work on the
//! user's repository, so the environment is inherited. Only Anthropic and
//! Claude Code variables are dropped: they are useless to other vendors and
//! may hold a Claude token.
//!
//! A non-zero exit is a quota error only when stderr (or, when there is no
//! reply, the CLI's own error string) says so. The reply text is never
//! scanned: a review of rate-limiting code must not look like a quota hit.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const MAX_TIMEOUT_SECS: u64 = 3600;
const STDERR_TAIL_CHARS: usize = 2000;

const GROK_READ_ONLY_TOOLS: &str = "read_file,grep,list_dir,web_search";
const GROK_READ_ONLY_DENIED: &str = "search_replace,write,run_terminal_cmd,monitor,task,Delete,use_tool,search_tool,web_fetch";
/// Write mode can edit files but not run commands or fetch URLs: grok's
/// shell is not sandboxed and inherits the user's environment.
const GROK_WRITE_TOOLS: &str = "read_file,grep,list_dir,web_search,search_replace,write";
const GROK_WRITE_DENIED: &str = "run_terminal_cmd,monitor,task,use_tool,search_tool,web_fetch";
/// Longest accepted task. Tasks travel over stdin (codex) or a prompt file
/// (grok), so this is a sanity cap, not an OS limit.
pub const MAX_TASK_CHARS: usize = 100_000;
const OUTPUT_CAP_BYTES: usize = 4 * 1024 * 1024;
/// After the CLI exits, how long to wait for its pipes to drain.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Codex,
    Grok,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Codex, Agent::Grok];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Codex => "codex",
            Agent::Grok => "grok",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.name() == s)
    }

    /// The CLI that serves this agent; `BATON_<NAME>_BIN` overrides it.
    pub fn program(self) -> String {
        let (var, default) = match self {
            Agent::Codex => ("BATON_CODEX_BIN", "codex"),
            Agent::Grok => ("BATON_GROK_BIN", "grok"),
        };
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => v,
            _ => default.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ReadOnly,
    Write,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read_only",
            Mode::Write => "write",
        }
    }
}

/// Reasoning depth. Both CLIs accept the same five names; codex passes it as
/// a config override, grok as a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];

    pub fn name(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Effort> {
        Effort::ALL.into_iter().find(|e| e.name() == s)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub agent: Agent,
    pub mode: Mode,
    pub task: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub timeout: Duration,
}

/// Where the final reply is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The CLI writes its last message to this file (stdout as fallback).
    File(PathBuf),
    /// stdout is a JSON object; the reply is the string at this field.
    JsonField(&'static str),
}

/// A fully resolved command line. Pure data, so it can be unit tested.
#[derive(Debug, Clone)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub output: Output,
    /// Files to write into the scratch dir before starting.
    pub files: Vec<(PathBuf, String)>,
}

pub fn plan(req: &Request, scratch: &Path) -> Plan {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let cwd = req.cwd.to_string_lossy().to_string();
    match req.agent {
        Agent::Codex => {
            let out = scratch.join("last-message.txt");
            let sandbox = match req.mode {
                Mode::ReadOnly => "read-only",
                Mode::Write => "workspace-write",
            };
            let mut args = s(&["exec", "--sandbox", sandbox]);
            if req.mode == Mode::ReadOnly {
                args.push("--skip-git-repo-check".into());
            }
            args.extend(s(&["--color", "never", "-C"]));
            args.push(cwd);
            args.push("-o".into());
            args.push(out.to_string_lossy().to_string());
            if let Some(m) = &req.model {
                args.push(format!("--model={m}"));
            }
            if let Some(e) = req.effort {
                // `-c` values are parsed as TOML, so quote the string.
                args.push("-c".into());
                args.push(format!("model_reasoning_effort=\"{}\"", e.name()));
            }
            args.push("-".into());
            Plan {
                program: req.agent.program(),
                args,
                stdin: Some(req.task.clone()),
                output: Output::File(out),
                files: vec![],
            }
        }
        Agent::Grok => {
            let prompt = scratch.join("prompt.txt");
            let permission = match req.mode {
                Mode::ReadOnly => "plan",
                Mode::Write => "acceptEdits",
            };
            let mut args = vec!["--prompt-file".to_string(), prompt.to_string_lossy().to_string()];
            args.extend(s(&["--output-format", "json", "--permission-mode", permission]));
            args.extend(s(&["--no-subagents", "--cwd"]));
            args.push(cwd);
            // `plan` alone does NOT stop headless grok from editing, and a
            // denylist alone is escaped through `monitor` (both seen
            // 2026-10-21, grok 1.0.62). An allowlist plus removing the
            // command and MCP meta-tools held in every test.
            let (tools, denied) = match req.mode {
                Mode::ReadOnly => (GROK_READ_ONLY_TOOLS, GROK_READ_ONLY_DENIED),
                Mode::Write => (GROK_WRITE_TOOLS, GROK_WRITE_DENIED),
            };
            args.extend(s(&["--tools", tools, "--disallowed-tools", denied]));
            if let Some(m) = &req.model {
                args.push(format!("--model={m}"));
            }
            if let Some(e) = req.effort {
                args.push(format!("--reasoning-effort={}", e.name()));
            }
            Plan {
                program: req.agent.program(),
                args,
                stdin: None,
                output: Output::JsonField("text"),
                files: vec![(prompt, req.task.clone())],
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub text: String,
    pub exit_code: Option<i32>,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Quota,
    Unavailable,
    Timeout,
    Failed,
}

#[derive(Debug, Clone)]
pub struct AgentError(pub String, pub ErrorKind);

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AgentError {}

/// Kills the whole process group when dropped, so a timeout or a cancelled
/// MCP call never leaves the CLI's children running.
pub(crate) struct GroupGuard(pub(crate) Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // SAFETY: plain syscall; a stale pgid just returns ESRCH.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

pub(crate) fn scrubbed(name: &str) -> bool {
    name.starts_with("ANTHROPIC_") || name.starts_with("CLAUDE")
}

/// Model IDs are passed as `--model=<id>`; keep them to a plain charset
/// so a value can never be read as another flag.
pub fn valid_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 128
        && m.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && m.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c))
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            // Keep draining past the cap so the child never blocks on a full pipe.
            Ok(n) => {
                if out.len() < OUTPUT_CAP_BYTES {
                    out.extend_from_slice(&buf[..n.min(OUTPUT_CAP_BYTES - out.len())]);
                }
            }
        }
    }
    out
}

pub async fn run(req: &Request) -> Result<Outcome, AgentError> {
    if !req.cwd.is_dir() {
        return Err(AgentError(format!("cwd is not a directory: {}", req.cwd.display()), ErrorKind::Failed));
    }
    if let Some(m) = &req.model {
        if !valid_model(m) {
            return Err(AgentError(format!("invalid model id: {m}"), ErrorKind::Failed));
        }
    }
    let scratch = Scratch::new().map_err(|e| AgentError(format!("cannot create scratch dir: {e}"), ErrorKind::Failed))?;
    let plan = plan(req, scratch.path());
    for (path, body) in &plan.files {
        crate::paths::write_private(path, body)
            .map_err(|e| AgentError(format!("cannot write {}: {e}", path.display()), ErrorKind::Failed))?;
    }

    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .current_dir(&req.cwd)
        .stdin(if plan.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        if scrubbed(&k.to_string_lossy()) {
            cmd.env_remove(&k);
        }
    }
    #[cfg(unix)]
    cmd.process_group(0);

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AgentError(
                format!("{} CLI not found on PATH ({})", req.agent.name(), plan.program),
                ErrorKind::Unavailable,
            )
        } else {
            AgentError(format!("cannot start {}: {e}", plan.program), ErrorKind::Failed)
        }
    })?;
    let guard = GroupGuard(child.id());
    if let (Some(input), Some(mut stdin)) = (plan.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
    }
    let out_task = tokio::spawn(read_capped(child.stdout.take().expect("piped stdout")));
    let err_task = tokio::spawn(read_capped(child.stderr.take().expect("piped stderr")));

    // Wait for the process, not for its pipes: a background child the agent
    // started (dev server, watcher) may hold them open forever.
    let status = match tokio::time::timeout(req.timeout, child.wait()).await {
        Ok(r) => r.map_err(|e| AgentError(format!("{} failed: {e}", plan.program), ErrorKind::Failed))?,
        Err(_) => {
            return Err(AgentError(
                format!("{} timed out after {}s and was killed", req.agent.name(), req.timeout.as_secs()),
                ErrorKind::Timeout,
            ))
        }
    };
    // Kill whatever the agent left running in its group, then drain.
    drop(guard);
    let collect = |t: tokio::task::JoinHandle<Vec<u8>>| async move {
        tokio::time::timeout(DRAIN_GRACE, t).await.ok().and_then(Result::ok).unwrap_or_default()
    };
    let stdout = String::from_utf8_lossy(&collect(out_task).await).to_string();
    let stderr = String::from_utf8_lossy(&collect(err_task).await).to_string();
    let parsed = extract_reply(&plan.output, &stdout);
    let duration_secs = started.elapsed().as_secs_f64();

    let success = status.success();
    let code = status.code();
    match parsed {
        Ok(text) if success && !text.trim().is_empty() => Ok(Outcome {
            text,
            exit_code: code,
            duration_secs,
        }),
        Ok(text) if !text.trim().is_empty() => Err(AgentError(
            format!(
                "{} exited with {code:?}. Partial reply:\n{text}\n--- stderr (tail) ---\n{}",
                req.agent.name(),
                tail(&stderr)
            ),
            if crate::quota::stderr_says_quota(&stderr) { ErrorKind::Quota } else { ErrorKind::Failed },
        )),
        other => {
            let why = match other {
                Err(e) => e,
                Ok(_) => "empty reply".to_string(),
            };
            // Only the CLI's own trailing error counts: `why` may embed the reply JSON.
            let kind = if crate::quota::stderr_says_quota(&stderr) { ErrorKind::Quota } else { ErrorKind::Failed };
            Err(AgentError(
                format!(
                    "{} returned no usable reply ({why}); exit {code:?}\n--- stderr (tail) ---\n{}\n--- stdout (tail) ---\n{}",
                    req.agent.name(),
                    tail(&stderr),
                    tail(&stdout)
                ),
                kind,
            ))
        }
    }
}

/// Pull the reply text out of the CLI's output.
pub fn extract_reply(output: &Output, stdout: &str) -> Result<String, String> {
    match output {
        Output::File(path) => match std::fs::read_to_string(path) {
            Ok(t) if !t.trim().is_empty() => Ok(t),
            _ => Ok(stdout.to_string()),
        },
        Output::JsonField(field) => {
            let value = last_json_object(stdout).ok_or("stdout is not JSON")?;
            if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
                if !status.eq_ignore_ascii_case("success") {
                    return Err(format!("status {status}: {value}"));
                }
            }
            let denied = value.get("denied_actions").and_then(|v| v.as_array()).filter(|a| !a.is_empty());
            let empty = value.get(*field).and_then(|v| v.as_str()).is_none_or(|t| t.trim().is_empty());
            if let (Some(denied), true) = (denied, empty) {
                return Err(format!("the CLI auto-denied tools it needed in headless mode: {}", serde_json::Value::Array(denied.clone())));
            }
            if value.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                return Err(format!("is_error: {value}"));
            }
            value
                .get(*field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("no string field `{field}` in JSON output"))
        }
    }
}

/// The whole stdout as one JSON object, or else the last line that is one
/// (some CLIs print notices before the result).
fn last_json_object(stdout: &str) -> Option<serde_json::Value> {
    if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(stdout.trim()) {
        return Some(v);
    }
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.is_object())
}

pub fn tail(s: &str) -> String {
    let n = s.chars().count();
    if n <= STDERR_TAIL_CHARS {
        return s.trim_end().to_string();
    }
    let skipped: String = s.chars().skip(n - STDERR_TAIL_CHARS).collect();
    format!("…{}", skipped.trim_end())
}

/// A private temp dir removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Scratch> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "baton-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            crate::paths::stamp()
        ));
        std::fs::create_dir(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Scratch(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(agent: Agent, mode: Mode, model: Option<&str>) -> Request {
        Request {
            agent,
            mode,
            task: "do it".into(),
            cwd: PathBuf::from("/repo"),
            model: model.map(str::to_string),
            effort: None,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn effort_maps_to_each_cli() {
        let mut r = req(Agent::Codex, Mode::ReadOnly, None);
        r.effort = Some(Effort::High);
        let p = plan(&r, Path::new("/s"));
        let i = p.args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(p.args[i + 1], "model_reasoning_effort=\"high\"");
        assert_eq!(p.args.last().unwrap(), "-");
        let mut g = req(Agent::Grok, Mode::ReadOnly, None);
        g.effort = Some(Effort::Xhigh);
        let p = plan(&g, Path::new("/s"));
        assert!(p.args.contains(&"--reasoning-effort=xhigh".to_string()));
        assert!(!plan(&req(Agent::Grok, Mode::ReadOnly, None), Path::new("/s")).args.iter().any(|a| a.starts_with("--reasoning-effort")));
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
        assert_eq!(Effort::parse("ultra"), None);
    }

    #[test]
    fn codex_modes_map_to_sandbox() {
        let p = plan(&req(Agent::Codex, Mode::ReadOnly, None), Path::new("/s"));
        assert_eq!(p.args[..3], ["exec", "--sandbox", "read-only"]);
        assert_eq!(p.stdin.as_deref(), Some("do it"));
        assert_eq!(p.args.last().unwrap(), "-");
        assert!(!p.args.iter().any(|a| a.starts_with("--model")));
        let w = plan(&req(Agent::Codex, Mode::Write, Some("gpt-x")), Path::new("/s"));
        assert_eq!(w.args[2], "workspace-write");
        assert!(w.args.contains(&"--model=gpt-x".to_string()));
        assert!(!w.args.contains(&"--skip-git-repo-check".to_string()));
        assert!(p.args.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(w.output, Output::File(PathBuf::from("/s/last-message.txt")));
    }

    #[test]
    fn grok_uses_prompt_file_and_permission_mode() {
        let p = plan(&req(Agent::Grok, Mode::ReadOnly, None), Path::new("/s"));
        let i = p.args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(p.args[i + 1], "plan");
        assert_eq!(p.files, vec![(PathBuf::from("/s/prompt.txt"), "do it".to_string())]);
        assert!(p.stdin.is_none());
        let i = p.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(p.args[i + 1], GROK_READ_ONLY_TOOLS);
        assert!(p.args.contains(&GROK_READ_ONLY_DENIED.to_string()));
        assert!(!GROK_READ_ONLY_TOOLS.contains("web_fetch"));
        let w = plan(&req(Agent::Grok, Mode::Write, None), Path::new("/s"));
        assert!(w.args.contains(&"acceptEdits".to_string()));
        assert!(w.args.contains(&GROK_WRITE_TOOLS.to_string()));
        assert!(w.args.contains(&GROK_WRITE_DENIED.to_string()));
    }

    #[test]
    fn extracts_json_field_after_notices() {
        let out = "notice: update available\n{\"status\":\"SUCCESS\",\"response\":\"PONG\"}\n";
        assert_eq!(extract_reply(&Output::JsonField("response"), out).unwrap(), "PONG");
    }

    #[test]
    fn json_failure_status_is_error() {
        let out = "{\"status\":\"ERROR\",\"response\":\"\"}";
        assert!(extract_reply(&Output::JsonField("response"), out).is_err());
        assert!(extract_reply(&Output::JsonField("text"), "{\"is_error\":true,\"text\":\"x\"}").is_err());
        assert!(extract_reply(&Output::JsonField("text"), "not json").is_err());
        let denied = "{\"status\":\"SUCCESS\",\"response\":\"\",\"denied_actions\":[{\"action\":\"command\"}]}";
        assert!(extract_reply(&Output::JsonField("response"), denied).unwrap_err().contains("auto-denied"));
    }

    #[test]
    fn file_output_falls_back_to_stdout() {
        let r = extract_reply(&Output::File(PathBuf::from("/nonexistent/x")), "from stdout").unwrap();
        assert_eq!(r, "from stdout");
    }

    #[test]
    fn scrubs_claude_and_anthropic_env() {
        assert!(scrubbed("ANTHROPIC_API_KEY"));
        assert!(scrubbed("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(scrubbed("CLAUDECODE"));
        assert!(!scrubbed("PATH"));
        assert!(!scrubbed("OPENAI_API_KEY"));
    }

    #[test]
    fn model_ids_are_restricted() {
        for ok in ["gpt-6-sol", "gpt-daybreak-blue-latest", "grok-4.7", "claude-opus-4-6-thinking", "org/model:v1"] {
            assert!(valid_model(ok), "{ok}");
        }
        for bad in ["", "--dangerously-bypass-approvals-and-sandbox", "-x", "a b", "a;b", &"m".repeat(129)] {
            assert!(!valid_model(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    fn fake(script: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("fake.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = p.to_string_lossy().to_string();
        (d, s)
    }

    /// A background child holding stdout must not turn a finished call
    /// into a timeout (review finding, reproduced before the fix).
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn background_child_does_not_block_completion() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, bin) = fake("sleep 30 &\necho '{\"text\":\"PONG\"}'\nexit 0");
        let _bin = crate::paths::EnvVar::set("BATON_GROK_BIN", &bin);
        let mut r = req(Agent::Grok, Mode::ReadOnly, None);
        r.cwd = std::env::temp_dir();
        r.timeout = Duration::from_secs(8);
        let t = Instant::now();
        let out = run(&r).await.unwrap();
        assert_eq!(out.text, "PONG");
        assert!(t.elapsed() < Duration::from_secs(6), "took {:?}", t.elapsed());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn usage_limit_stderr_is_quota() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = r#"printf '%s\n' 'ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 4th, 2026 2:38 PM.' >&2
exit 1"#;
        let (_d, bin) = fake(script);
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &bin);
        let mut r = req(Agent::Codex, Mode::ReadOnly, None);
        r.cwd = std::env::temp_dir();
        r.timeout = Duration::from_secs(8);
        let err = run(&r).await.unwrap_err();
        assert_eq!(err.1, ErrorKind::Quota);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn generic_stderr_is_failed_even_if_the_reply_mentions_limits() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_d, bin) = fake("echo boom >&2\nexit 1");
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &bin);
        let mut r = req(Agent::Codex, Mode::ReadOnly, None);
        r.cwd = std::env::temp_dir();
        r.timeout = Duration::from_secs(8);
        let err = run(&r).await.unwrap_err();
        assert_eq!(err.1, ErrorKind::Failed);

        let script = r#"out=
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then
    out=$2
    shift 2
    continue
  fi
  shift
done
if [ -n "$out" ]; then
  printf '%s\n' 'usage limit discussed in the reply' > "$out"
fi
printf '%s\n' boom >&2
exit 1"#;
        let (_d, bin) = fake(script);
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &bin);
        let err = run(&r).await.unwrap_err();
        assert_eq!(err.1, ErrorKind::Failed);
        assert!(err.0.contains("Partial reply"), "{}", err.0);

        // codex streams tool output to stderr: a file the agent read that
        // says "rate limit exceeded" must not turn a later crash into Quota.
        let script = r#"printf '%s\n' 'exec cat docs/limits.md' '429 Too Many Requests: rate limit exceeded, quota exceeded' >&2
for i in 1 2 3 4 5 6; do printf 'line %s\n' "$i" >&2; done
printf '%s\n' 'ERROR: sandbox denied' >&2
exit 1"#;
        let (_d, bin) = fake(script);
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &bin);
        assert_eq!(run(&r).await.unwrap_err().1, ErrorKind::Failed);
    }

    #[test]
    fn tail_keeps_end() {
        let long = "a".repeat(3000) + "END";
        let t = tail(&long);
        assert!(t.ends_with("END"));
        assert!(t.chars().count() <= STDERR_TAIL_CHARS + 1);
    }
}
