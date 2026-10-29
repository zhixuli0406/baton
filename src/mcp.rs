//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Only the `tools` capability is implemented: `ask_codex`, `ask_grok`,
//! `delegate`, `quota_status`, `list_models` and the four `handoff_*` tools
//! (see `crate::mcp_handoff`). Calls run concurrently; `notifications/cancelled`,
//! stdin EOF and SIGTERM/SIGINT/SIGHUP kill the running agent processes.
//!
//! `delegate` is the path for implementation work: it rotates vendors by
//! quota and picks the model and effort from the task kind and difficulty.
//! `ask_*` pins one vendor. Model choice there is capability-driven: each
//! description carries the vendor's model table from `crate::catalog`, a
//! `capability` tag resolves to a model when none is pinned, and `effort`
//! sets reasoning depth.
//!
//! Tool arguments come from the model, which repo content or web pages may
//! have steered, so `write` requires a git repository: inside Claude Code's
//! starting repo, or rooted under `[write] allowed_roots`. Home, its ancestors
//! and `/` never count as repo or allowed roots; an active target handoff blocks writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use std::path::Path;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::agents::{self, Agent, Effort, ErrorKind, Mode, Request, DEFAULT_TIMEOUT_SECS, MAX_TASK_CHARS, MAX_TIMEOUT_SECS};
use crate::catalog::{self, Catalog};
use crate::config::Config;
use crate::routing::{self, Difficulty, Kind};
use crate::{handoff, hooks, mcp_handoff, paths, quota};

const LIST_MODELS_TOOL: &str = "list_models";

/// Replies longer than this are cut in the tool result; the full text stays
/// in the run log. Claude Code warns above ~10k tokens of MCP output.
const REPLY_CAP_CHARS: usize = 30_000;

pub async fn serve() -> Result<()> {
    let default_cwd = std::env::current_dir()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });

    let inflight: Arc<Mutex<HashMap<String, AbortHandle>>> = Arc::default();
    let mut signals = handoff::Signals::new()?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = tokio::select! {
            r = stdin.read_until(b'\n', &mut buf) => r.unwrap_or(0),
            _ = signals.recv() => 0,
        };
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(error_response(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        match dispatch(&msg) {
            Dispatch::Reply(v) => {
                let _ = tx.send(v);
            }
            Dispatch::Ignore => {}
            Dispatch::Cancel(id) => {
                if let Some(h) = inflight.lock().unwrap().remove(&id) {
                    h.abort();
                }
            }
            Dispatch::Call { id, params } => {
                let tx = tx.clone();
                let key = id.to_string();
                let map = inflight.clone();
                let cwd = default_cwd.clone();
                let task_key = key.clone();
                // Hold the map while spawning so a fast call can't remove
                // its entry before it is inserted.
                let mut guard = inflight.lock().unwrap();
                let handle = tokio::spawn(async move {
                    let result = call_tool(&params, &cwd).await;
                    map.lock().unwrap().remove(&task_key);
                    let _ = tx.send(match result {
                        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                        Err((code, m)) => error_response(id, code, &m),
                    });
                });
                guard.insert(key, handle.abort_handle());
            }
        }
    }
    // Client went away or we were signalled: stop every running agent.
    // Aborting drops each call's future, whose guard kills the agent group.
    let handles: Vec<AbortHandle> = inflight.lock().unwrap().drain().map(|(_, h)| h).collect();
    for h in &handles {
        h.abort();
    }
    // Give the aborted tasks a moment to run their drops.
    for _ in 0..50 {
        if handles.iter().all(AbortHandle::is_finished) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), writer).await;
    Ok(())
}

enum Dispatch {
    Reply(Value),
    Call { id: Value, params: Value },
    Cancel(String),
    Ignore,
}

fn dispatch(msg: &Value) -> Dispatch {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // Notification.
        if method == "notifications/cancelled" {
            if let Some(rid) = params.get("requestId") {
                return Dispatch::Cancel(rid.to_string());
            }
        }
        return Dispatch::Ignore;
    };
    match method {
        "initialize" => {
            // Echo the client's protocol version: we only use the stable
            // `tools` surface, which every revision shares.
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            Dispatch::Reply(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "baton", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Delegate work to other vendors' coding agents (Codex, Grok). They cannot see this conversation: give each call a self-contained task. Choose the agent and model by capability: each ask_* description lists that vendor's models with tags; pass `capability` (e.g. security, frontier, fast, web_search) to let baton pick, or `model` to pin one; `list_models` prints the full table. Defensive security work (vulnerability review, hardening, threat modelling) should go to ask_codex with capability=security. Default to mode=read_only; use write only when the agent should edit files. The handoff_* tools inspect or steer a handoff (another agent continuing the task after Claude hit its usage limit): handoff_list and handoff_show are read-only; handoff_stop and handoff_start change state, so use them only when the user asks. For implementation work prefer `delegate`: it rotates codex → grok by quota and picks model/effort from kind and difficulty; if it returns DELEGATES_EXHAUSTED, do the work yourself. `quota_status` shows remaining quota.",
                }
            }))
        }
        "ping" => Dispatch::Reply(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => {
            // A broken config is reported by the call itself; the list
            // falls back to the built-in catalog so tools still appear.
            let catalog = Config::load().map(|c| c.catalog()).unwrap_or_else(|_| Catalog::with_overrides(&[]));
            Dispatch::Reply(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tool_list(&catalog)}}))
        }
        "tools/call" => Dispatch::Call { id, params },
        _ => Dispatch::Reply(error_response(id, -32601, &format!("method not found: {method}"))),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_name(agent: Agent) -> String {
    format!("ask_{}", agent.name())
}

fn tool_description(agent: Agent, catalog: &Catalog) -> String {
    let intro = match agent {
        Agent::Codex => "Run OpenAI Codex (Codex CLI, user's ChatGPT login) as a coding agent in a working directory. Good for an independent second opinion or code review from a different model family, careful implementation of a well-specified change, debugging, and (with capability=security) defensive security review. Blocks until the agent finishes; returns its final report.",
        Agent::Grok => "Run xAI Grok (Grok Build CLI, user's grok.com login) as a coding agent in a working directory. Has web search, so it suits questions about recent releases, current docs and news, plus a contrasting opinion from another model family. Blocks until the agent finishes; returns its final report.",
    };
    format!("{intro}\n\nModels [capability tags]:\n{}", catalog.summary(agent))
}

fn tool_list(catalog: &Catalog) -> Vec<Value> {
    let tags: Vec<&str> = catalog::TAGS.iter().map(|(t, _)| *t).collect();
    let efforts: Vec<&str> = Effort::ALL.iter().map(|e| e.name()).collect();
    let mut tools: Vec<Value> = Agent::ALL
        .into_iter()
        .map(|a| {
            json!({
                "name": tool_name(a),
                "description": tool_description(a, catalog),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": TASK_DESC},
                        "mode": {"type": "string", "enum": ["read_only", "write"], "default": "read_only", "description": MODE_DESC},
                        "cwd": {"type": "string", "description": CWD_DESC},
                        "model": {"type": "string", "description": "Model ID for that CLI (see the list above). Overrides `capability`. Omit both to use the configured or CLI default."},
                        "capability": {"type": "string", "enum": tags, "description": "What the task needs; baton picks the first listed model carrying this tag. Ignored when `model` is set."},
                        "effort": {"type": "string", "enum": efforts, "description": "Reasoning depth. Omit for the model's default; use high or above for hard problems."},
                        "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS, "default": DEFAULT_TIMEOUT_SECS}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }
            })
        })
        .collect();
    tools.push(delegate_tool(&efforts));
    tools.push(quota_status_tool());
    tools.push(json!({
        "name": LIST_MODELS_TOOL,
        "description": "List every model baton can route to, per vendor, with capability tags and notes. Read-only, runs no agent. Use it when unsure which `model` or `capability` to pass to ask_codex / ask_grok.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
    }));
    tools.extend(mcp_handoff::tool_defs());
    tools
}

const TASK_DESC: &str = "Complete, self-contained instructions. The agent cannot see this conversation: include goal, relevant files, constraints and the expected shape of the answer.";
const MODE_DESC: &str = "read_only: may read, must not edit files. write: may edit files inside cwd; requires a git repository inside Claude Code's starting repo or rooted under [write] allowed_roots in ~/.baton/config.toml. Home, its ancestors and / never count as repo or allowed roots; blocked during a target-repo handoff.";
const CWD_DESC: &str = "Absolute working directory. Defaults to the directory Claude Code was started in.";

fn delegate_tool(efforts: &[&str]) -> Value {
    json!({
        "name": "delegate",
        "description": "Preferred way to hand work to another vendor's coding agent with automatic rotation. baton tries agents in the configured order (default codex, then grok), skips any whose usage quota is exhausted (Codex: live probe; Grok: recorded quota errors) and falls through to the next one if a call fails on quota. Model and reasoning effort come from `kind` and `difficulty`, so easy work does not burn the most expensive model. If every agent is out of quota the result starts with DELEGATES_EXHAUSTED: then do the task yourself. Use ask_codex / ask_grok only to pin a specific vendor or model.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": TASK_DESC},
                "kind": {"type": "string", "enum": routing::KINDS, "default": "code", "description": "code = implement/fix/refactor/review code; security = defensive security review; research = needs current web docs/releases"},
                "difficulty": {"type": "string", "enum": routing::DIFFICULTIES, "default": "normal", "description": "easy = mechanical or small edits (cheap fast model, low effort); normal = everyday work; hard = tricky design/debugging (frontier model, high effort). Pick honestly: do not default to hard."},
                "mode": {"type": "string", "enum": ["read_only", "write"], "default": "read_only", "description": MODE_DESC},
                "cwd": {"type": "string", "description": CWD_DESC},
                "effort": {"type": "string", "enum": efforts, "description": "Optional override for reasoning depth. Omit to let kind and difficulty choose."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS, "default": DEFAULT_TIMEOUT_SECS}
            },
            "required": ["task"],
            "additionalProperties": false
        }
    })
}

fn quota_status_tool() -> Value {
    json!({
        "name": "quota_status",
        "description": "Show each delegate agent's usage quota (Codex via a live probe that spends no quota, Grok from recorded quota errors) and the rotation order `delegate` will use. Read-only. Pass refresh=true to force a new Codex probe.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "refresh": {"type": "boolean", "default": false}
            },
            "additionalProperties": false
        }
    })
}

/// Validated `tools/call` arguments.
#[derive(Debug, PartialEq)]
pub struct CallArgs {
    pub agent: Agent,
    pub mode: Mode,
    pub task: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub timeout_secs: u64,
}

/// Returns `Err((code, message))` for protocol errors; argument problems are
/// reported as a tool result with `isError` so Claude can fix and retry.
pub fn parse_call(params: &Value, default_cwd: &std::path::Path, catalog: &Catalog, allowed_roots: &[PathBuf]) -> Result<Result<CallArgs, String>, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let agent = name
        .strip_prefix("ask_")
        .and_then(Agent::parse)
        .ok_or((-32602, format!("unknown tool: {name}")))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    Ok(parse_args(agent, args, default_cwd, catalog, allowed_roots))
}

struct SharedArgs {
    mode: Mode,
    task: String,
    cwd: PathBuf,
    effort: Option<Effort>,
    timeout_secs: u64,
}

fn parse_shared(args: &Value, default_cwd: &Path, allowed_roots: &[PathBuf]) -> Result<SharedArgs, String> {
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("`task` is required and must be a non-empty string")?
        .to_string();
    if task.chars().count() > MAX_TASK_CHARS {
        return Err(format!("`task` is longer than {MAX_TASK_CHARS} characters; put large material in a file and point the agent at it"));
    }
    let mode = match args.get("mode").and_then(Value::as_str) {
        None | Some("read_only") => Mode::ReadOnly,
        Some("write") => Mode::Write,
        Some(other) => return Err(format!("`mode` must be read_only or write, got {other}")),
    };
    let cwd = match args.get("cwd").and_then(Value::as_str) {
        None => default_cwd.to_path_buf(),
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.is_absolute() {
                return Err("`cwd` must be an absolute path".into());
            }
            p
        }
    };
    if !cwd.is_dir() {
        return Err(format!("`cwd` is not an existing directory: {}", cwd.display()));
    }
    let cwd = if mode == Mode::Write {
        check_write_scope(&cwd, default_cwd, allowed_roots)?
    } else {
        cwd
    };
    let effort = match args.get("effort").and_then(Value::as_str).map(str::trim).filter(|e| !e.is_empty()) {
        None => None,
        Some(e) => Some(Effort::parse(e).ok_or_else(|| {
            let names: Vec<&str> = Effort::ALL.iter().map(|e| e.name()).collect();
            format!("`effort` must be one of {}, got {e}", names.join(", "))
        })?),
    };
    let timeout_secs = match args.get("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_TIMEOUT_SECS).contains(&n) => n,
            _ => return Err(format!("`timeout_secs` must be an integer between 1 and {MAX_TIMEOUT_SECS}")),
        },
    };
    Ok(SharedArgs { mode, task, cwd, effort, timeout_secs })
}

fn parse_args(agent: Agent, args: &Value, default_cwd: &Path, catalog: &Catalog, allowed_roots: &[PathBuf]) -> Result<CallArgs, String> {
    let shared = parse_shared(args, default_cwd, allowed_roots)?;
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    if let Some(m) = &model {
        if !agents::valid_model(m) {
            return Err(format!("`model` has unexpected characters: {m}"));
        }
    }
    let capability = args
        .get("capability")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let model = match (model, capability) {
        (Some(m), _) => Some(m),
        (None, None) => None,
        (None, Some(tag)) => {
            if !catalog::known_tag(tag) {
                let known: Vec<&str> = catalog::TAGS.iter().map(|(t, _)| *t).collect();
                return Err(format!("unknown `capability` {tag}; known tags: {}", known.join(", ")));
            }
            match catalog.resolve(agent, tag) {
                Some(e) => Some(e.model.clone()),
                None => {
                    return Err(format!(
                        "no {} model is tagged `{tag}`; this agent offers: {}. Call list_models for the full table or try the other agent",
                        agent.name(),
                        catalog.tags_for(agent).join(", ")
                    ))
                }
            }
        }
    };
    Ok(CallArgs {
        agent,
        mode: shared.mode,
        task: shared.task,
        cwd: shared.cwd,
        model,
        effort: shared.effort,
        timeout_secs: shared.timeout_secs,
    })
}

struct DelegateArgs {
    kind: Kind,
    difficulty: Difficulty,
    mode: Mode,
    task: String,
    cwd: PathBuf,
    effort: Option<Effort>,
    timeout_secs: u64,
}

fn parse_delegate_args(args: &Value, default_cwd: &Path, allowed_roots: &[PathBuf]) -> Result<DelegateArgs, String> {
    let shared = parse_shared(args, default_cwd, allowed_roots)?;
    let kind = parse_choice(args, "kind", Kind::Code, Kind::parse, routing::KINDS)?;
    let difficulty = parse_choice(args, "difficulty", Difficulty::Normal, Difficulty::parse, routing::DIFFICULTIES)?;
    Ok(DelegateArgs {
        kind,
        difficulty,
        mode: shared.mode,
        task: shared.task,
        cwd: shared.cwd,
        effort: shared.effort,
        timeout_secs: shared.timeout_secs,
    })
}

fn parse_choice<T>(args: &Value, key: &str, default: T, parse: fn(&str) -> Option<T>, names: &[&str]) -> Result<T, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => {
            let s = s.trim();
            parse(s).ok_or_else(|| format!("`{key}` must be one of {}, got {s}", names.join(", ")))
        }
        Some(v) => Err(format!("`{key}` must be one of {}, got {v}", names.join(", "))),
    }
}

fn parse_refresh(args: &Value) -> Result<bool, String> {
    match args.get("refresh") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("`refresh` must be a boolean, got {v}")),
    }
}

/// Resolve an allowed root; missing paths, home and its ancestors cannot grant scope.
fn canonical_write_root(root: &Path, home: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    (!home.starts_with(&root)).then_some(root)
}

fn check_repo_contains(path: &Path, top_level: &Path) -> Result<(), String> {
    if !path.starts_with(top_level) {
        return Err(format!("reported git repository top-level {} does not contain {}", top_level.display(), path.display()));
    }
    Ok(())
}

fn check_repo_boundary(path: &Path, top_level: &Path) -> Result<(), String> {
    if paths::physical_git_boundary(path).as_deref() != Some(top_level) {
        return Err(format!("physical .git boundary for {} does not match reported git repository top-level {}; relocated core.worktree is not allowed", path.display(), top_level.display()));
    }
    Ok(())
}

/// `write` requires a safe target repo inside Claude's starting repo or
/// rooted under an explicitly configured allowed root.
pub(crate) fn check_write_scope(cwd: &Path, project: &Path, allowed_roots: &[PathBuf]) -> Result<PathBuf, String> {
    let target = std::fs::canonicalize(cwd).map_err(|e| format!("cannot resolve cwd: {e}"))?;
    let target_root = paths::git_toplevel(&target).ok_or_else(|| {
        format!("write mode requires a git repository; {} is outside any git repository", target.display())
    })?;
    check_repo_contains(&target, &target_root)?;
    check_repo_boundary(&target, &target_root)?;
    let home = std::fs::canonicalize(paths::home_dir()).unwrap_or_else(|_| paths::home_dir());
    if home.starts_with(&target_root) {
        return Err("write mode is disabled when the target git repository is your home directory, an ancestor of home or /".into());
    }
    let project = std::fs::canonicalize(project).map_err(|e| format!("cannot resolve project: {e}"))?;
    let project_root = paths::git_toplevel(&project);
    if let Some(root) = &project_root {
        check_repo_contains(&project, root)?;
        check_repo_boundary(&project, root)?;
        if home.starts_with(root) {
            return Err("write mode is disabled when the project is your home directory, an ancestor of home or /".into());
        }
    }
    if allowed_roots.iter().filter_map(|root| canonical_write_root(root, &home)).any(|root| target_root.starts_with(root)) {
        return Ok(target);
    }
    let Some(project_root) = project_root else {
        if !allowed_roots.is_empty() {
            return Err(format!("{} is outside the configured write scope and Claude Code was not started inside a git repository. Configure [write] allowed_roots in ~/.baton/config.toml to allow this repository", target.display()));
        }
        return Err("write mode needs Claude Code to be started inside a git repository (so changes can be reviewed and reverted); use read_only here".into());
    };
    if !target.starts_with(&project_root) {
        return Err(format!("write mode is limited to the current project ({}) or configured roots; {} is outside them. Configure [write] allowed_roots in ~/.baton/config.toml to allow other repositories", project_root.display(), target.display()));
    }
    Ok(target)
}

async fn call_tool(params: &Value, default_cwd: &std::path::Path) -> Result<Value, (i64, String)> {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return Ok(tool_text(&format!("baton config error: {e:#}"), true)),
    };
    let catalog = config.catalog();
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name == LIST_MODELS_TOOL {
        return Ok(tool_text(&catalog.render(), false));
    }
    let empty = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty);
    if let Some(r) = mcp_handoff::call(name, arguments, default_cwd, &config) {
        return Ok(r);
    }
    if name == "delegate" {
        return Ok(call_delegate(arguments, default_cwd, &config).await);
    }
    if name == "quota_status" {
        return Ok(match parse_refresh(arguments) {
            Ok(refresh) => tool_text(&quota::report(&config.rotation, refresh).await, false),
            Err(msg) => tool_text(&msg, true),
        });
    }
    let args = match parse_call(params, default_cwd, &catalog, &config.write.allowed_roots)? {
        Ok(a) => a,
        Err(msg) => return Ok(tool_text(&msg, true)),
    };
    if args.mode == Mode::Write {
        if let Some(active) = handoff::active_for(&args.cwd) {
            return Ok(tool_text(&hooks::active_message(&active), true));
        }
    }
    // Same TTL re-probe as delegate, so a top-up is noticed without a manual refresh.
    let st = quota::check(args.agent, &config.rotation).await;
    let now = chrono::Utc::now().timestamp();
    if quota::is_blocked(&st, now) {
        return Ok(tool_text(
            &format!(
                "{}. Use delegate (rotates automatically) or the other ask_* tool; if you believe this is stale, call quota_status with refresh=true.",
                quota::describe(&st, now)
            ),
            true,
        ));
    }
    let req = Request {
        agent: args.agent,
        mode: args.mode,
        model: args.model.clone().or_else(|| config.model_for(args.agent)),
        effort: args.effort,
        task: args.task.clone(),
        cwd: args.cwd.clone(),
        timeout: Duration::from_secs(args.timeout_secs),
    };
    let result = agents::run(&req).await;
    let log_note = note_of(&write_run_log(&req, &result));
    Ok(match result {
        Ok(o) => {
            let body = cap(&o.text, REPLY_CAP_CHARS, &log_note);
            tool_text(
                &format!(
                    "[{} · {} · {} · {:.1}s · {}]\n{}",
                    req.agent.name(),
                    model_label(&req),
                    req.mode.name(),
                    o.duration_secs,
                    log_note,
                    body
                ),
                false,
            )
        }
        Err(e) => {
            let mut body = cap(&e.0, REPLY_CAP_CHARS, &log_note);
            if e.1 == ErrorKind::Quota {
                let st = quota::record_quota_error(req.agent, &e.0, &config.rotation).await;
                body.push_str(&format!(
                    "\n\n{}. Use delegate or the other ask_* tool.",
                    quota::describe(&st, chrono::Utc::now().timestamp())
                ));
            }
            tool_text(
                &format!("[{} · {} · {} · {}]\n{body}", req.agent.name(), model_label(&req), req.mode.name(), log_note),
                true,
            )
        }
    })
}

enum StepOutcome {
    Done(Value),
    Stop(Value),
    Next,
}

async fn call_delegate(args: &Value, default_cwd: &Path, config: &Config) -> Value {
    let parsed = match parse_delegate_args(args, default_cwd, &config.write.allowed_roots) {
        Ok(a) => a,
        Err(msg) => return tool_text(&msg, true),
    };
    if parsed.mode == Mode::Write {
        if let Some(active) = handoff::active_for(&parsed.cwd) {
            return tool_text(&hooks::active_message(&active), true);
        }
    }
    let order = if config.rotation.enabled {
        config.rotation.order.clone()
    } else {
        vec![config.rotation.order[0]]
    };
    let catalog = config.catalog();
    let steps = routing::plan(parsed.kind, parsed.difficulty, parsed.effort, &catalog, &order);
    let mut skipped = Vec::new();
    for step in steps {
        let now = chrono::Utc::now().timestamp();
        let st = quota::check(step.agent, &config.rotation).await;
        if quota::is_blocked(&st, now) {
            skipped.push(quota::describe(&st, now));
            continue;
        }
        match run_delegated(&parsed, &step, config, &mut skipped).await {
            StepOutcome::Done(v) | StepOutcome::Stop(v) => return v,
            StepOutcome::Next => {}
        }
    }
    tool_text(&exhausted_text(&skipped), true)
}

async fn run_delegated(parsed: &DelegateArgs, step: &routing::Step, config: &Config, skipped: &mut Vec<String>) -> StepOutcome {
    let req = Request {
        agent: step.agent,
        mode: parsed.mode,
        task: parsed.task.clone(),
        cwd: parsed.cwd.clone(),
        model: step.model.clone().or_else(|| config.model_for(step.agent)),
        effort: Some(step.effort),
        timeout: Duration::from_secs(parsed.timeout_secs),
    };
    let before = (req.mode == Mode::Write).then(|| paths::worktree_fingerprint(&req.cwd));
    let result = agents::run(&req).await;
    let log_note = note_of(&write_run_log(&req, &result));
    let skipped_line = |skipped: &[String]| if skipped.is_empty() { String::new() } else { format!("skipped: {}\n", skipped.join("; ")) };
    let e = match result {
        Ok(o) => {
            let text = format!(
                "[delegate → {} · {} · {} · {:.1}s · {log_note}]\n{}{}",
                req.agent.name(),
                model_label(&req),
                req.mode.name(),
                o.duration_secs,
                skipped_line(skipped),
                cap(&o.text, REPLY_CAP_CHARS, &log_note)
            );
            return StepOutcome::Done(tool_text(&text, false));
        }
        Err(e) => e,
    };
    let fell_over = matches!(e.1, ErrorKind::Quota | ErrorKind::Unavailable);
    if e.1 == ErrorKind::Quota {
        let st = quota::record_quota_error(step.agent, &e.0, &config.rotation).await;
        skipped.push(quota::describe(&st, chrono::Utc::now().timestamp()));
    }
    // Rotate only over an untouched tree; never stack a second vendor's edits on a half-done change.
    let touched = before.is_some_and(|b| b.is_none() || b != paths::worktree_fingerprint(&req.cwd));
    let why = match (fell_over, touched) {
        (true, false) => {
            if e.1 == ErrorKind::Unavailable {
                let line = e.0.lines().next().unwrap_or(e.0.as_str());
                skipped.push(format!("{}: CLI not available ({line})", step.agent.name()));
            }
            return StepOutcome::Next;
        }
        (true, true) => format!(
            "Not handed to another agent: the working tree changed while {} ran, before it failed. Review it (git status / git diff) and decide how to continue.",
            step.agent.name()
        ),
        (false, _) if req.mode == Mode::Write => "Not retried on another agent because this one may already have changed files.".to_string(),
        (false, _) => "Not retried on another agent: this failure is not a quota or availability problem.".to_string(),
    };
    let text = format!(
        "[delegate → {} · {} · {} · {log_note}]\n{}{}\n{why}",
        req.agent.name(),
        model_label(&req),
        req.mode.name(),
        skipped_line(skipped),
        cap(&e.0, REPLY_CAP_CHARS, &log_note)
    );
    StepOutcome::Stop(tool_text(&text, true))
}

fn exhausted_text(skipped: &[String]) -> String {
    let mut lines = vec!["DELEGATES_EXHAUSTED: no delegate agent is available for this task.".to_string()];
    for s in skipped {
        lines.push(format!("- {s}"));
    }
    lines.push("Per the rotation policy, do this task yourself (Claude). Call quota_status with refresh=true to re-check later.".into());
    lines.join("\n")
}

fn note_of(log: &std::io::Result<PathBuf>) -> String {
    match log {
        Ok(p) => format!("log: {}", p.display()),
        Err(e) => format!("log not written: {e}"),
    }
}

/// `gpt-x@high`, `gpt-x`, or `cli default` for the result header and log.
fn model_label(req: &Request) -> String {
    let model = req.model.as_deref().unwrap_or("cli default");
    match req.effort {
        Some(e) => format!("{model}@{}", e.name()),
        None => model.to_string(),
    }
}

pub(crate) fn tool_text(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn cap(text: &str, max: usize, note: &str) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n\n[truncated at {max} chars; full text in {note}]")
}

fn write_run_log(req: &Request, result: &Result<agents::Outcome, agents::AgentError>) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = paths::runs_dir();
    paths::ensure_dir(&dir)?;
    let path = dir.join(format!(
        "{}-{}-{}-{}.md",
        paths::stamp(),
        req.agent.name(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let (status, body) = match result {
        Ok(o) => (format!("ok ({:.1}s, exit {:?})", o.duration_secs, o.exit_code), o.text.as_str()),
        Err(e) => ("error".to_string(), e.0.as_str()),
    };
    let text = format!(
        "# {} run\n\n- time: {}\n- mode: {}\n- model: {}\n- cwd: {}\n- status: {}\n\n## Task\n\n{}\n\n## Reply\n\n{}\n",
        req.agent.name(),
        paths::now_rfc3339(),
        req.mode.name(),
        model_label(req),
        req.cwd.display(),
        status,
        req.task,
        body
    );
    paths::write_private(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        std::env::temp_dir()
    }

    fn cat() -> Catalog {
        Catalog::with_overrides(&[])
    }

    fn parse(p: &Value) -> Result<Result<CallArgs, String>, (i64, String)> {
        parse_call(p, &dir(), &cat(), &[])
    }

    #[test]
    fn initialize_echoes_version() {
        let m = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}});
        let Dispatch::Reply(r) = dispatch(&m) else { panic!() };
        assert_eq!(r["result"]["protocolVersion"], "2099-01-01");
        assert_eq!(r["result"]["serverInfo"]["name"], "baton");
        let instructions = r["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("DELEGATES_EXHAUSTED"), "{instructions}");
        assert!(instructions.contains("quota_status"), "{instructions}");
    }

    #[test]
    fn lists_tools_with_catalog() {
        let m = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        let Dispatch::Reply(r) = dispatch(&m) else { panic!() };
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names[..5], ["ask_codex", "ask_grok", "delegate", "quota_status", LIST_MODELS_TOOL]);
        assert_eq!(names[5..], mcp_handoff::TOOLS);
        let delegate = tools.iter().find(|t| t["name"] == "delegate").unwrap();
        assert_eq!(delegate["inputSchema"]["properties"]["kind"]["enum"], json!(["code", "security", "research"]));
        assert_eq!(delegate["inputSchema"]["properties"]["difficulty"]["enum"], json!(["easy", "normal", "hard"]));
        assert_eq!(delegate["inputSchema"]["additionalProperties"], false);
        assert_eq!(delegate["inputSchema"]["properties"]["task"]["description"], tools[0]["inputSchema"]["properties"]["task"]["description"]);
        let codex = &tools[0];
        assert!(codex["description"].as_str().unwrap().contains("gpt-daybreak-blue-latest"));
        assert!(codex["inputSchema"]["properties"]["capability"]["enum"].as_array().unwrap().iter().any(|v| v == "security"));
        assert_eq!(codex["inputSchema"]["properties"]["effort"]["enum"].as_array().unwrap().len(), 5);
        assert!(!r.to_string().contains("gemini"));
    }

    #[test]
    fn capability_resolves_to_a_model() {
        let p = json!({"name":"ask_codex","arguments":{"task":"audit","capability":"security","effort":"high"}});
        let a = parse(&p).unwrap().unwrap();
        assert_eq!(a.model.as_deref(), Some("gpt-daybreak-blue-latest"));
        assert_eq!(a.effort, Some(Effort::High));
        // An explicit model wins over the tag.
        let p = json!({"name":"ask_codex","arguments":{"task":"x","capability":"security","model":"gpt-6-luna"}});
        assert_eq!(parse(&p).unwrap().unwrap().model.as_deref(), Some("gpt-6-luna"));
        // Grok has no security model: a tool error naming what it does offer.
        let p = json!({"name":"ask_grok","arguments":{"task":"x","capability":"security"}});
        let e = parse(&p).unwrap().unwrap_err();
        assert!(e.contains("web_search") && e.contains("list_models"), "{e}");
        for bad in [json!({"task":"x","capability":"magic"}), json!({"task":"x","effort":"ultra"})] {
            assert!(parse(&json!({"name":"ask_codex","arguments":bad})).unwrap().is_err());
        }
    }

    #[test]
    fn notifications_and_unknown_methods() {
        let n = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(matches!(dispatch(&n), Dispatch::Ignore));
        let c = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}});
        assert!(matches!(dispatch(&c), Dispatch::Cancel(id) if id == "7"));
        let u = json!({"jsonrpc":"2.0","id":3,"method":"resources/list"});
        let Dispatch::Reply(r) = dispatch(&u) else { panic!() };
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn parses_valid_call_with_defaults() {
        let p = json!({"name":"ask_grok","arguments":{"task":"  hi  "}});
        let a = parse(&p).unwrap().unwrap();
        assert_eq!(a.agent, Agent::Grok);
        assert_eq!(a.mode, Mode::ReadOnly);
        assert_eq!(a.task, "hi");
        assert_eq!(a.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(a.cwd, dir());
    }

    #[test]
    fn unknown_tool_is_protocol_error() {
        let p = json!({"name":"ask_gpt","arguments":{"task":"x"}});
        assert_eq!(parse(&p).unwrap_err().0, -32602);
    }

    #[test]
    fn bad_arguments_are_tool_errors() {
        for args in [
            json!({}),
            json!({"task":""}),
            json!({"task":"x","mode":"rw"}),
            json!({"task":"x","cwd":"relative"}),
            json!({"task":"x","cwd":"/definitely/not/here"}),
            json!({"task":"x","timeout_secs":0}),
            json!({"task":"x","timeout_secs":99999}),
        ] {
            let p = json!({"name":"ask_codex","arguments":args});
            assert!(parse(&p).unwrap().is_err(), "{args}");
        }
    }

    #[test]
    fn write_scope_is_confined_to_the_project_repo() {
        let repo = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(check_write_scope(&sub, &root, &[]).is_ok());
        assert!(check_write_scope(&root, &sub, &[]).is_ok());
        let outside = tempfile::tempdir().unwrap();
        assert!(check_write_scope(outside.path(), &root, &[]).unwrap_err().contains("outside"));
        // Not a git repo at all.
        assert!(check_write_scope(outside.path(), outside.path(), &[]).unwrap_err().contains("git repository"));
        // read_only is not restricted.
        let p = json!({"name":"ask_codex","arguments":{"task":"x","cwd": outside.path()}});
        assert!(parse_call(&p, &root, &cat(), &[]).unwrap().is_ok());
        let p = json!({"name":"ask_codex","arguments":{"task":"x","mode":"write","cwd": outside.path()}});
        assert!(parse_call(&p, &root, &cat(), &[]).unwrap().is_err());
    }

    #[test]
    fn write_scope_requires_reported_repo_to_contain_path() {
        let root = Path::new("/repos/project");
        assert!(check_repo_contains(root, root).is_ok());
        assert!(check_repo_contains(&root.join("sub"), root).is_ok());
        for outside in [Path::new("/repos/other"), Path::new("/repos/project-other"), Path::new("/repos")] {
            let err = check_repo_contains(outside, root).unwrap_err();
            assert!(err.contains("reported git repository top-level") && err.contains("does not contain"), "{err}");
        }
    }

    // An empty repository fixture without running any git state-changing commands.
    fn empty_repo(root: &Path) {
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn relocated_repo(root: &Path, parent: &Path, worktree_config: bool) {
        empty_repo(root);
        let config = format!("[core]\n\trepositoryformatversion = 0\n\tworktree = {}\n", parent.display());
        if worktree_config {
            std::fs::write(root.join(".git/config"), "[core]\n\trepositoryformatversion = 0\n[extensions]\n\tworktreeConfig = true\n").unwrap();
            std::fs::write(root.join(".git/config.worktree"), config).unwrap();
        } else {
            std::fs::write(root.join(".git/config"), config).unwrap();
        }
    }

    #[test]
    fn write_scope_rejects_relocated_project_before_granting_scope() {
        for worktree_config in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let parent = std::fs::canonicalize(parent.path()).unwrap();
            let project = parent.join("project");
            relocated_repo(&project, &parent, worktree_config);
            let sibling = parent.join("sibling");
            empty_repo(&sibling);
            assert_eq!(paths::git_toplevel(&project), Some(parent));
            for roots in [vec![], vec![sibling.clone()]] {
                let err = check_write_scope(&sibling, &project, &roots).unwrap_err();
                assert!(err.contains("core.worktree") || err.contains("does not match"), "{err}");
            }
        }
    }

    #[test]
    fn write_scope_rejects_relocated_target_under_allowed_root() {
        for worktree_config in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let parent = std::fs::canonicalize(parent.path()).unwrap();
            let target = parent.join("target");
            relocated_repo(&target, &parent, worktree_config);
            let project = parent.join("project");
            empty_repo(&project);
            assert_eq!(paths::git_toplevel(&target), Some(parent.clone()));
            let err = check_write_scope(&target, &project, &[parent]).unwrap_err();
            assert!(err.contains("core.worktree") || err.contains("does not match"), "{err}");
        }
    }

    #[test]
    fn write_scope_allows_linked_worktree_under_configured_root() {
        let project = tempfile::tempdir().unwrap();
        empty_repo(project.path());
        let allowed = tempfile::tempdir().unwrap();
        let main = allowed.path().join("main");
        empty_repo(&main);
        let target = allowed.path().join("linked");
        std::fs::create_dir(&target).unwrap();
        let gitdir = main.join(".git/worktrees/linked");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(gitdir.join("gitdir"), format!("{}\n", target.join(".git").display())).unwrap();
        std::fs::write(target.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let sub = target.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let canonical = std::fs::canonicalize(&sub).unwrap();
        assert_eq!(paths::git_toplevel(&canonical), Some(std::fs::canonicalize(&target).unwrap()));
        assert_eq!(check_write_scope(&sub, project.path(), &[allowed.path().to_path_buf()]).unwrap(), canonical);
        assert!(check_write_scope(&main, project.path(), &[allowed.path().to_path_buf()]).is_ok());
    }

    #[test]
    fn write_scope_allows_other_repos_under_configured_roots() {
        let project = tempfile::tempdir().unwrap();
        empty_repo(project.path());
        let allowed = tempfile::tempdir().unwrap();
        let target = allowed.path().join("repo");
        empty_repo(&target);
        let sub = target.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let roots = vec![allowed.path().to_path_buf()];
        assert!(check_write_scope(&sub, project.path(), &roots).is_ok());
        let err = check_write_scope(&sub, project.path(), &[]).unwrap_err();
        assert!(err.contains("outside") && err.contains("[write] allowed_roots in ~/.baton/config.toml"), "{err}");
        let p = json!({"name":"ask_codex","arguments":{"task":"x","mode":"write","cwd": sub}});
        assert!(parse_call(&p, project.path(), &cat(), &roots).unwrap().is_ok());
        assert!(parse_call(&p, project.path(), &cat(), &[]).unwrap().is_err());
        // A subdirectory of a repo cannot grant scope to the whole enclosing repo.
        assert!(check_write_scope(&sub, project.path(), std::slice::from_ref(&sub)).is_err());
    }

    #[test]
    fn write_scope_allows_a_non_git_start_only_with_configured_scope() {
        let project = tempfile::tempdir().unwrap();
        let allowed = tempfile::tempdir().unwrap();
        let target = allowed.path().join("repo");
        empty_repo(&target);
        assert!(check_write_scope(&target, project.path(), &[allowed.path().to_path_buf()]).is_ok());
        let err = check_write_scope(&target, project.path(), &[]).unwrap_err();
        assert!(err.contains("Claude Code to be started inside a git repository"), "{err}");
        let err = check_write_scope(&target, project.path(), &[project.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("outside") && err.contains("[write] allowed_roots in ~/.baton/config.toml"), "{err}");
    }

    #[test]
    fn write_scope_ignores_home_root_and_missing_roots() {
        let home = tempfile::tempdir().unwrap();
        let home_path = std::fs::canonicalize(home.path()).unwrap();
        assert!(canonical_write_root(home.path(), &home_path).is_none());
        assert!(canonical_write_root(home_path.parent().unwrap(), &home_path).is_none());
        assert!(canonical_write_root(&home.path().join(".."), &home_path).is_none());
        assert!(canonical_write_root(Path::new("/"), &home_path).is_none());
        assert!(canonical_write_root(&home.path().join("missing"), &home_path).is_none());
        let allowed = home.path().join("Project");
        std::fs::create_dir(&allowed).unwrap();
        assert_eq!(canonical_write_root(&allowed, &home_path), Some(home_path.join("Project")));
        let target = tempfile::tempdir().unwrap();
        empty_repo(target.path());
        let project = tempfile::tempdir().unwrap();
        empty_repo(project.path());
        assert!(check_write_scope(target.path(), project.path(), &[
            paths::home_dir(), PathBuf::from("/"), target.path().join("missing"),
        ]).is_err());
    }

    #[test]
    fn write_scope_rejects_non_git_targets_under_allowed_roots() {
        let allowed = tempfile::tempdir().unwrap();
        let target = allowed.path().join("not-a-repo");
        std::fs::create_dir(&target).unwrap();
        let err = check_write_scope(&target, allowed.path(), &[allowed.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("git repository"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn write_scope_canonicalizes_symlinks_before_granting_scope() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        empty_repo(project.path());
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        empty_repo(outside.path());
        let link = allowed.path().join("escaped-repo");
        symlink(outside.path(), &link).unwrap();
        assert!(check_write_scope(&link, project.path(), &[allowed.path().to_path_buf()]).is_err());
        let root_link = project.path().join("allowed-link");
        symlink(outside.path(), &root_link).unwrap();
        assert!(check_write_scope(outside.path(), project.path(), &[root_link]).is_ok());
        let home = std::fs::canonicalize(project.path()).unwrap();
        let home_link = allowed.path().join("home-link");
        symlink(&home, &home_link).unwrap();
        assert!(canonical_write_root(&home_link, &home).is_none());
        let ancestor_link = allowed.path().join("home-ancestor-link");
        symlink(home.parent().unwrap(), &ancestor_link).unwrap();
        assert!(canonical_write_root(&ancestor_link, &home).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn write_scope_binds_canonical_cwd_to_call_args() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let allowed = tempfile::tempdir().unwrap();
        let target = allowed.path().join("repo");
        empty_repo(&target);
        let sub = target.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let link = project.path().join("repo-link");
        symlink(&target, &link).unwrap();
        let cwd = link.join("sub");
        let canonical = std::fs::canonicalize(&sub).unwrap();
        let roots = [allowed.path().to_path_buf()];
        assert_ne!(cwd, canonical);
        assert_eq!(check_write_scope(&cwd, project.path(), &roots).unwrap(), canonical);
        let p = json!({"name":"ask_codex","arguments":{"task":"x","mode":"write","cwd": cwd}});
        let args = parse_call(&p, project.path(), &cat(), &roots).unwrap().unwrap();
        assert_eq!(args.cwd, canonical);
        // Swapping the original link cannot redirect the already parsed cwd.
        std::fs::remove_file(&link).unwrap();
        symlink(project.path(), &link).unwrap();
        assert_eq!(std::fs::canonicalize(&args.cwd).unwrap(), canonical);
        let p = json!({"name":"ask_codex","arguments":{"task":"x","cwd": link}});
        assert_eq!(parse_call(&p, project.path(), &cat(), &roots).unwrap().unwrap().cwd, link);
    }

    #[test]
    fn rejects_flag_like_models_and_huge_tasks() {
        let p = json!({"name":"ask_grok","arguments":{"task":"x","model":"--yolo"}});
        assert!(parse(&p).unwrap().is_err());
        let p = json!({"name":"ask_grok","arguments":{"task":"x".repeat(MAX_TASK_CHARS + 1)}});
        assert!(parse(&p).unwrap().is_err());
    }

    #[test]
    fn delegate_defaults_and_rejects_bad_kind() {
        let a = parse_delegate_args(&json!({"task": "  hi  "}), &dir(), &[]).unwrap();
        assert_eq!(a.kind, Kind::Code);
        assert_eq!(a.difficulty, Difficulty::Normal);
        assert_eq!(a.mode, Mode::ReadOnly);
        assert_eq!(a.task, "hi");
        assert_eq!(a.effort, None);
        assert_eq!(a.timeout_secs, DEFAULT_TIMEOUT_SECS);
        for args in [
            json!({"task": "x", "kind": "magic"}),
            json!({"task": "x", "difficulty": "extreme"}),
            json!({"task": "x", "kind": "code", "difficulty": ""}),
        ] {
            assert!(parse_delegate_args(&args, &dir(), &[]).is_err(), "{args}");
        }
    }

    #[test]
    fn cap_truncates_with_note() {
        let s = cap(&"x".repeat(50), 10, "log: f");
        assert!(s.starts_with("xxxxxxxxxx\n"));
        assert!(s.contains("log: f"));
        assert_eq!(cap("short", 10, "n"), "short");
    }

    const CODEX_LIMIT: &str = "ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 4th, 2026 2:38 PM.";
    const GROK_LIMIT: &str = r#"Error: Internal error: { "message": "API error (status 402 Payment Required): Grok Build usage balance exhausted", "http_status": 402 }"#;

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Fake CLIs and a private BATON_HOME; the guards restore the env on drop.
    #[cfg(unix)]
    struct Fakes {
        _vars: Vec<paths::EnvVar>,
        dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    fn fakes(codex_probe: Value, codex_exec: &str, grok: &str) -> Fakes {
        let dir = tempfile::tempdir().unwrap();
        let codex = script(dir.path(), "codex", &quota::test_support::fake_codex(&codex_probe.to_string(), CODEX_LIMIT));
        let codex = match codex_exec {
            "" => codex,
            body => script(dir.path(), "codex-exec", &format!("#!/bin/sh\nif [ \"$1\" = app-server ]; then exec {} app-server; fi\n{body}\n", codex.display())),
        };
        let grok = script(dir.path(), "grok", &format!("#!/bin/sh\n{grok}\n"));
        let home = dir.path().join("home");
        let vars = vec![
            paths::EnvVar::set("BATON_HOME", &home),
            paths::EnvVar::set("BATON_CODEX_BIN", &codex),
            paths::EnvVar::set("BATON_GROK_BIN", &grok),
        ];
        Fakes { _vars: vars, dir }
    }

    fn probe_result(used: u32, reached: bool) -> Value {
        let resets = chrono::Utc::now().timestamp() + 86_400;
        json!({
            "ordinaryUsageAllowed": !reached,
            "rateLimits": {
                "primary": {"usedPercent": used, "windowDurationMins": 10080, "resetsAt": resets},
                "credits": {"hasCredits": false, "unlimited": false},
                "planType": "promax",
                "rateLimitReachedType": if reached { json!("rate_limit_reached") } else { Value::Null }
            }
        })
    }

    fn text_of(v: &Value) -> (String, bool) {
        (v["content"][0]["text"].as_str().unwrap().to_string(), v["isError"].as_bool().unwrap())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn delegate_skips_exhausted_codex_and_uses_grok() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fakes(probe_result(100, true), "", "cat >/dev/null; echo '{\"text\":\"PONG\"}'");
        let (text, err) = text_of(&call_delegate(&json!({"task":"x","difficulty":"easy"}), f.dir.path(), &Config::default()).await);
        assert!(!err, "{text}");
        assert!(text.starts_with("[delegate → grok · grok-4.7-build-fast@low"), "{text}");
        assert!(text.contains("skipped: codex: exhausted until"), "{text}");
        assert!(text.ends_with("PONG"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn delegate_reports_exhausted_when_every_agent_is_out() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fakes(probe_result(100, true), "", &format!("printf '%s\\n' '{GROK_LIMIT}' >&2; exit 1"));
        let (text, err) = text_of(&call_delegate(&json!({"task":"x"}), f.dir.path(), &Config::default()).await);
        assert!(err);
        assert!(text.starts_with("DELEGATES_EXHAUSTED"), "{text}");
        assert!(text.contains("- codex: exhausted until") && text.contains("- grok: exhausted until"), "{text}");
        assert!(text.contains("do this task yourself"), "{text}");
        let grok = quota::cached(Agent::Grok).unwrap();
        assert_eq!((grok.state, grok.source.as_str()), (quota::State::Exhausted, "error"));
        // The recorded block now short-circuits a pinned call without running grok.
        let p = json!({"name":"ask_grok","arguments":{"task":"x"}});
        let (text, err) = text_of(&call_tool(&p, f.dir.path()).await.unwrap());
        assert!(err && text.contains("grok: exhausted until") && text.contains("quota_status"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn delegate_falls_through_when_a_stale_probe_says_available() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fakes(probe_result(13, false), "", "cat >/dev/null; echo '{\"text\":\"PONG\"}'");
        let (text, err) = text_of(&call_delegate(&json!({"task":"x"}), f.dir.path(), &Config::default()).await);
        assert!(!err, "{text}");
        assert!(text.contains("delegate → grok") && text.contains("skipped: codex: exhausted until"), "{text}");
        let codex = quota::cached(Agent::Codex).unwrap();
        assert_eq!((codex.state, codex.source.as_str()), (quota::State::Exhausted, "error"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn delegate_does_not_rotate_after_files_changed() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exec = format!("cat >/dev/null; echo half > half-done.txt; printf '%s\\n' '{CODEX_LIMIT}' >&2; exit 1");
        let f = fakes(probe_result(13, false), &exec, "cat >/dev/null; echo '{\"text\":\"SHOULD NOT RUN\"}'");
        let repo = std::fs::canonicalize(f.dir.path()).unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success());
        let (text, err) = text_of(&call_delegate(&json!({"task":"x","mode":"write"}), &repo, &Config::default()).await);
        assert!(err, "{text}");
        assert!(text.contains("Not handed to another agent: the working tree changed while codex ran"), "{text}");
        assert!(!text.contains("SHOULD NOT RUN"), "{text}");
        assert!(repo.join("half-done.txt").exists());
    }
}
