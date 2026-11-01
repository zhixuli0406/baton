//! Live quota for delegate rotation.
//!
//! ChatGPT/Codex quota is flaky and changes under us, so baton probes it
//! (`codex app-server`, which spends nothing) instead of trusting a stale
//! error. Grok has no quota API, so its state comes only from failed calls.
//! The cache lives in `~/.baton/quota.json`.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::agents::Agent;
use crate::config::RotationConfig;
use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Available,
    Exhausted,
    Unknown,
}

/// Where a status came from. Serialized as `probe` / `error` / `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A live Codex rate-limit probe.
    Probe,
    /// A call that failed on quota.
    Error,
    /// Nothing known (Grok before any failure).
    None,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Probe => "probe",
            Source::Error => "error",
            Source::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// `"primary"` or `"secondary"`.
    pub label: String,
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub agent: Agent,
    pub state: State,
    pub source: Source,
    pub checked_at: i64,
    pub until: Option<i64>,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub detail: String,
    #[serde(default)]
    pub reset_credits: u64,
    #[serde(default)]
    pub reset_eligible: bool,
    #[serde(default, skip_serializing)]
    pub redeemed: Option<String>,
}

/// Phrases vendors use when the account is out of quota. Kept specific:
/// bare "quota" or "rate limit" also appear in code the agent reads.
const QUOTA_NEEDLES: &[&str] = &[
    // codex-cli 0.164.0: "ERROR: You’ve hit your usage limit. Visit …"
    "usage limit",
    "rate limit exceeded",
    "rate limit reached",
    "rate_limit_exceeded",
    "rate_limit_reached",
    "too many requests",
    "insufficient credits",
    "insufficient_quota",
    "out of credits",
    "quota exceeded",
    "exceeded your current quota",
    "resource_exhausted",
    "http 429",
    "status 429",
    "status: 429",
    "\"code\":429",
    "error 429",
    // grok 1.0.70: "API error (status 402 Payment Required): Grok Build usage balance exhausted"
    "payment required",
    "balance exhausted",
    "status 402",
];

/// How many trailing stderr lines may carry the CLI's own fatal error.
/// `codex exec` streams its whole transcript (tool output included) to
/// stderr, so anything earlier may be repository or web content.
const STDERR_TAIL_LINES: usize = 5;

pub fn is_quota_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    QUOTA_NEEDLES.iter().any(|n| lower.contains(n))
}

/// Quota error in the last few non-empty stderr lines of a failed CLI.
pub fn stderr_says_quota(stderr: &str) -> bool {
    stderr.lines().rev().filter(|l| !l.trim().is_empty()).take(STDERR_TAIL_LINES).any(is_quota_error)
}

pub fn parse_codex(result: &Value, now: i64, cooldown_secs: u64) -> Status {
    let mut st = parse_limits(result, now, cooldown_secs);
    st.reset_credits = result["rateLimitResetCredits"]["availableCount"].as_u64().unwrap_or(0);
    let reached = result["rateLimits"]["rateLimitReachedType"].as_str();
    st.reset_eligible = st.state == State::Exhausted && st.reset_credits > 0
        && matches!(reached, None | Some("rate_limit_reached"))
        && result["rateLimits"]["spendControlReached"].as_bool() != Some(true)
        && earliest_reset(&st.windows).is_some();
    st
}

fn parse_limits(result: &Value, now: i64, cooldown_secs: u64) -> Status {
    let Some(limits) = result.get("rateLimits").filter(|v| v.is_object()) else {
        return codex_status(State::Unknown, now, None, None, Vec::new(), "probe returned no rateLimits");
    };
    let windows = ["primary", "secondary"]
        .into_iter()
        .filter_map(|label| window_from(label, limits.get(label)?))
        .collect::<Vec<_>>();
    let plan = limits.get("planType").and_then(Value::as_str).map(str::to_string);
    let credits = limits.get("credits");
    let on_credits = flag(credits, "unlimited") || flag(credits, "hasCredits");
    let reached = limits.get("rateLimitReachedType").and_then(Value::as_str).is_some();
    let full = windows.iter().any(|w| w.used_percent >= 100.0);
    let ordinary_blocked = result.get("ordinaryUsageAllowed").and_then(Value::as_bool) == Some(false);
    let exhausted = reached || full || ordinary_blocked;
    if exhausted && on_credits {
        return codex_status(State::Available, now, None, plan, windows, "limit reached, running on credits");
    }
    if !exhausted {
        let detail = window_detail(&windows);
        let detail = if detail.is_empty() { "usage allowed".into() } else { detail };
        return codex_status(State::Available, now, None, plan, windows, &detail);
    }
    let until = full_reset(&windows).or_else(|| earliest_reset(&windows)).unwrap_or_else(|| after(now, cooldown_secs));
    let detail = window_detail(&windows.iter().filter(|w| w.used_percent >= 100.0).cloned().collect::<Vec<_>>());
    let detail = if detail.is_empty() {
        if reached { "rate limit reached".to_string() } else { "usage not allowed".to_string() }
    } else {
        detail
    };
    codex_status(State::Exhausted, now, Some(until), plan, windows, &detail)
}

fn flag(credits: Option<&Value>, key: &str) -> bool {
    credits.and_then(|c| c.get(key)).and_then(Value::as_bool) == Some(true)
}

fn window_from(label: &str, v: &Value) -> Option<Window> {
    if !v.is_object() {
        return None;
    }
    Some(Window {
        label: label.to_string(),
        used_percent: v.get("usedPercent").and_then(Value::as_f64).unwrap_or(0.0),
        window_minutes: v.get("windowDurationMins").and_then(Value::as_u64),
        resets_at: v.get("resetsAt").and_then(Value::as_i64),
    })
}

fn full_reset(windows: &[Window]) -> Option<i64> {
    windows.iter().filter(|w| w.used_percent >= 100.0).filter_map(|w| w.resets_at).max()
}

fn earliest_reset(windows: &[Window]) -> Option<i64> {
    windows.iter().filter_map(|w| w.resets_at).min()
}

fn after(now: i64, secs: u64) -> i64 {
    now.saturating_add(i64::try_from(secs).unwrap_or(i64::MAX))
}

fn codex_status(state: State, now: i64, until: Option<i64>, plan: Option<String>, windows: Vec<Window>, detail: &str) -> Status {
    Status {
        agent: Agent::Codex,
        state,
        source: Source::Probe,
        checked_at: now,
        until,
        plan,
        windows,
        reset_credits: 0,
        reset_eligible: false,
        redeemed: None,
        detail: detail.to_string(),
    }
}

pub fn is_blocked(s: &Status, now: i64) -> bool {
    s.state == State::Exhausted && s.until.is_none_or(|u| u > now)
}

pub fn describe(s: &Status, now: i64) -> String {
    let name = s.agent.name();
    if s.state == State::Exhausted {
        if let Some(until) = s.until {
            if until <= now {
                let credits = if s.reset_credits > 0 { format!("; {} reset credit{} available", s.reset_credits, if s.reset_credits == 1 { "" } else { "s" }) } else { String::new() };
                return format!("{name}: available again (block expired at {}{credits})", fmt_ts(until));
            }
            return format!("{name}: exhausted until {} ({})", fmt_ts(until), paren(s, now));
        }
    }
    if s.state == State::Unknown && s.source == Source::None {
        return format!("{name}: unknown (no quota API; marked exhausted only after a failed call)");
    }
    let word = match s.state {
        State::Available => "available",
        State::Exhausted => "exhausted",
        State::Unknown => "unknown",
    };
    format!("{name}: {word} ({})", paren(s, now))
}

fn paren(s: &Status, now: i64) -> String {
    let mut parts = vec![format!("{}, {}m ago", s.source.name(), age_mins(s.checked_at, now))];
    match s.state {
        State::Available => parts.push(available_clause(s)),
        _ => {
            if !s.detail.is_empty() {
                parts.push(s.detail.clone());
            }
        }
    }
    if s.state == State::Exhausted {
        if let Some(plan) = &s.plan {
            parts.push(format!("plan {plan}"));
        }
    }
    if s.reset_credits > 0 {
        parts.push(format!("{} reset credit{} available", s.reset_credits, if s.reset_credits == 1 { "" } else { "s" }));
    }
    parts.join("; ")
}

fn available_clause(s: &Status) -> String {
    if s.detail == "limit reached, running on credits" || s.windows.is_empty() {
        return s.detail.clone();
    }
    s.windows
        .iter()
        .map(|w| {
            let mut part = format!("{} {}% used", window_name(w.window_minutes), pct(w.used_percent));
            if let Some(t) = w.resets_at {
                part.push_str(&format!(", resets {}", fmt_ts(t)));
            }
            part
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn window_detail(windows: &[Window]) -> String {
    windows
        .iter()
        .map(|w| format!("{} window {}% used", window_name(w.window_minutes), pct(w.used_percent)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn window_name(minutes: Option<u64>) -> String {
    match minutes {
        Some(10080) => "weekly".into(),
        Some(300) => "5h".into(),
        Some(n) => format!("{n}m"),
        None => "window".into(),
    }
}

fn pct(p: f64) -> String {
    if (p - p.round()).abs() < 1e-6 {
        format!("{}", p.round() as i64)
    } else {
        format!("{p:.1}")
    }
}

fn age_mins(checked_at: i64, now: i64) -> i64 {
    (now - checked_at).max(0) / 60
}

fn fmt_ts(secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_else(|| secs.to_string())
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn grok_unknown() -> Status {
    Status {
        agent: Agent::Grok,
        state: State::Unknown,
        source: Source::None,
        checked_at: unix_now(),
        until: None,
        plan: None,
        windows: Vec::new(),
        reset_credits: 0,
        reset_eligible: false,
        redeemed: None,
        detail: "grok has no quota API; marked exhausted only after a failed call".into(),
    }
}

pub fn load_all() -> HashMap<String, Status> {
    with_store(read_map).unwrap_or_default()
}

pub fn cached(agent: Agent) -> Option<Status> {
    load_all().get(agent.name()).filter(|st| st.agent == agent).cloned()
}

/// Best effort: a lost write only means the next call probes or fails again.
pub fn record(s: &Status) {
    update(|all| {
        all.insert(s.agent.name().to_string(), s.clone());
    });
}

fn forget(agent: Agent) {
    update(|all| {
        all.remove(agent.name());
    });
}

fn update(f: impl FnOnce(&mut HashMap<String, Status>)) {
    with_store(|| {
        let mut all = read_map();
        f(&mut all);
        if let Ok(body) = serde_json::to_vec_pretty(&all) {
            if let Err(e) = paths::atomic_write(&paths::quota_file(), body, 0o600) {
                eprintln!("baton: cannot write {}: {e}", paths::quota_file().display());
            }
        }
    });
}

fn with_store<T>(f: impl FnOnce() -> T) -> Option<T> {
    paths::ensure_dir(&paths::baton_home()).ok()?;
    let _lock = paths::lock(&paths::baton_home().join("quota.lock")).ok()?;
    Some(f())
}

fn read_map() -> HashMap<String, Status> {
    let Ok(text) = std::fs::read_to_string(paths::quota_file()) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub async fn probe_codex(timeout: Duration) -> Result<Value, String> {
    let mut child = spawn_probe()?;
    // app-server starts MCP servers of its own; take the whole group down.
    let _group = crate::agents::GroupGuard(child.id());
    let read = tokio::time::timeout(timeout, read_rate_limits(&mut child)).await;
    let _ = child.start_kill();
    match read {
        Ok(result) => result,
        Err(_) => Err(format!("codex probe timed out after {}s", timeout.as_secs())),
    }
}

fn spawn_probe() -> Result<Child, String> {
    let program = Agent::Codex.program();
    let mut cmd = Command::new(&program);
    cmd.arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        if crate::agents::scrubbed(&k.to_string_lossy()) {
            cmd.env_remove(k);
        }
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("codex CLI not found on PATH ({program})")
        } else {
            format!("cannot start {program}: {e}")
        }
    })
}

struct Session {
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
}

impl Session {
    fn new(child: &mut Child) -> Result<Self, String> {
        Ok(Self {
            stdin: child.stdin.take().ok_or("codex probe has no stdin")?,
            lines: BufReader::new(child.stdout.take().ok_or("codex probe has no stdout")?).lines(),
            id: 0,
        })
    }

    async fn send(&mut self, msg: Value) -> Result<(), String> {
        self.stdin.write_all(format!("{msg}\n").as_bytes()).await.map_err(|e| format!("probe write failed: {e}"))?;
        self.stdin.flush().await.map_err(|e| format!("probe write failed: {e}"))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params})).await?;
        while let Some(line) = self.lines.next_line().await.map_err(|e| format!("probe read failed: {e}"))? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("id").and_then(Value::as_u64) != Some(self.id) { continue; }
            if let Some(e) = rpc_error(&v) { return Err(e); }
            return v.get("result").cloned().ok_or("codex response has no result".into());
        }
        Err("codex probe ended without a response".into())
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.id = 1;
        self.send(probe_messages()[0].clone()).await?;
        self.send(json!({"jsonrpc":"2.0", "method":"initialized"})).await
    }

    async fn read(&mut self) -> Result<Value, String> {
        self.request("account/rateLimits/read", json!({})).await
    }
}

async fn read_rate_limits(child: &mut Child) -> Result<Value, String> {
    let mut session = Session::new(child)?;
    session.initialize().await?;
    session.read().await
}

fn probe_messages() -> [Value; 3] {
    [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "baton", "version": env!("CARGO_PKG_VERSION")}}
        }),
        json!({"jsonrpc": "2.0", "method": "initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read"}),
    ]
}

fn rpc_error(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    if err.is_null() {
        return None;
    }
    Some(err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string()))
}

/// A forced re-check. Grok has nothing to probe, so this forgets its
/// recorded quota error; the next failed call records it again.
pub async fn refresh(agent: Agent, cfg: &RotationConfig) -> Status {
    match agent {
        Agent::Codex => refresh_codex(cfg).await,
        Agent::Grok => {
            forget(Agent::Grok);
            grok_unknown()
        }
    }
}

async fn refresh_codex(cfg: &RotationConfig) -> Status {
    match probe_codex(Duration::from_secs(20)).await {
        Ok(result) => {
            let st = parse_codex(&result, unix_now(), cfg.cooldown_secs);
            if st.state != State::Unknown {
                record(&st);
            }
            st
        }
        Err(e) => Status {
            agent: Agent::Codex,
            state: State::Unknown,
            source: Source::Probe,
            checked_at: unix_now(),
            until: None,
            plan: None,
            windows: Vec::new(),
            reset_credits: 0,
            reset_eligible: false,
            redeemed: None,
            detail: format!("probe failed: {e}"),
        },
    }
}

/// Only execution gates may spend a reset credit.
pub async fn ensure(agent: Agent, cfg: &RotationConfig) -> Status {
    let st = check(agent, cfg).await;
    if agent != Agent::Codex { return st; }
    redeem_if_exhausted(st, cfg).await
}

fn needs_reset(st: &Status, cfg: &RotationConfig) -> bool {
    let now = unix_now();
    st.agent == Agent::Codex && cfg.auto_reset && st.reset_eligible && is_blocked(st, now)
        && st.until.is_some_and(|u| u.saturating_sub(now) >= i64::try_from(cfg.reset_min_wait_secs).unwrap_or(i64::MAX))
}

fn reset_failure(mut st: Status, error: &str) -> Status {
    st.reset_eligible = false;
    st.detail.push_str(&format!("; reset credit: {error}"));
    st.checked_at = unix_now();
    record(&st);
    st
}

/// A shared window key survives timeouts and independent baton processes.
fn reset_key(result: &Value, st: &Status) -> String {
    let account = result["accountId"].as_str().filter(|s| {
        s.len() == 36 && s.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) { c == b'-' } else { c.is_ascii_digit() || (b'a'..=b'f').contains(&c) }
        })
    }).unwrap_or("00000000-0000-4000-8000-000000000000");
    let until = full_reset(&st.windows).or_else(|| earliest_reset(&st.windows)).unwrap_or(0);
    format!("{}{:012x}", &account[..24], (until as u64) & 0xffffffffffff)
}

fn pick_credit(result: &Value, now: i64) -> Result<Option<String>, &'static str> {
    let credits = &result["rateLimitResetCredits"]["credits"];
    if credits.is_null() { return Ok(None); }
    let rows = credits.as_array().ok_or("invalid credits list")?;
    if rows.iter().any(|v| v["status"] == "redeeming") { return Err("a credit is already redeeming"); }
    rows.iter().filter(|v| v["status"] == "available" && v["resetType"] == "codexRateLimits"
        && (v["expiresAt"].is_null() || v["expiresAt"].as_i64().is_some_and(|t| t > now)))
        .filter_map(|v| Some((v["id"].as_str()?, v["expiresAt"].as_i64())))
        .min_by_key(|(_, expiry)| expiry.unwrap_or(i64::MAX))
        .map(|(id, _)| Some(id.to_string())).ok_or("no eligible reset credit")
}

pub async fn redeem_if_exhausted(st: Status, cfg: &RotationConfig) -> Status {
    if !needs_reset(&st, cfg) { return st; }
    let original = st.clone();
    let cfg = cfg.clone();
    match tokio::spawn(async move { redeem_transaction(st, &cfg).await }).await {
        Ok(st) => st,
        Err(e) => reset_failure(original, &format!("reset task failed: {e}")),
    }
}

async fn redeem_transaction(st: Status, cfg: &RotationConfig) -> Status {
    if let Err(e) = paths::ensure_dir(&paths::baton_home()) { return reset_failure(st, &e.to_string()); }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let _lock = loop {
        match paths::try_lock(&paths::baton_home().join("quota-reset.lock")) {
            Ok(Some(lock)) => break lock,
            Ok(None) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            Ok(None) => return st,
            Err(e) => return reset_failure(st, &e.to_string()),
        }
    };
    // Second-resolution timestamps can tie; a suppressed or restored peer result still wins.
    if let Some(peer) = cached(Agent::Codex).filter(|p| p.checked_at >= st.checked_at && (!is_blocked(p, unix_now()) || !p.reset_eligible)) {
        return peer;
    }
    let mut latest = st;
    let mut attempt = None;
    let run = reset_session(&mut latest, cfg, &mut attempt);
    let outcome = match tokio::time::timeout(Duration::from_secs(45), run).await {
        Ok(result) => result,
        Err(_) => Err("reset session timed out after 45s".into()),
    };
    if let Some(mut audit) = attempt {
        if audit["outcome"].is_null() {
            audit["outcome"] = json!(outcome.as_ref().err().map(String::as_str).unwrap_or("unknown outcome"));
        }
        if let Err(e) = paths::append_private(&paths::resets_log(), &audit.to_string()) {
            return reset_failure(latest, &format!("audit write failed: {e}"));
        }
    }
    match outcome {
        Ok(()) => latest,
        Err(e) => reset_failure(latest, &e),
    }
}

async fn reset_session(latest: &mut Status, cfg: &RotationConfig, attempt: &mut Option<Value>) -> Result<(), String> {
    let mut child = spawn_probe()?;
    let _group = crate::agents::GroupGuard(child.id());
    let mut session = Session::new(&mut child)?;
    session.initialize().await?;
    let result = session.read().await?;
    *latest = parse_codex(&result, unix_now(), cfg.cooldown_secs);
    record(latest);
    if !needs_reset(latest, cfg) { return Ok(()); }
    let credit = pick_credit(&result, unix_now()).map_err(str::to_string)?;
    let key = reset_key(&result, latest);
    let mut params = json!({"idempotencyKey":key});
    if let Some(id) = &credit { params["creditId"] = json!(id); }
    let mut audit = json!({"ts":unix_now(), "credit_id":credit, "idempotency_key":key,
        "outcome":"pending", "until":latest.until, "windows":latest.windows, "available_count_before":latest.reset_credits});
    paths::append_private(&paths::resets_log(), &audit.to_string()).map_err(|e| format!("pending audit write failed: {e}"))?;
    audit["outcome"] = Value::Null;
    *attempt = Some(audit);
    let consumed = session.request("account/rateLimitResetCredit/consume", params).await;
    let outcome = match consumed {
        Ok(v) => v["outcome"].as_str().unwrap_or("unknown outcome").to_string(),
        Err(e) => {
            attempt.as_mut().unwrap()["outcome"] = json!(&e);
            return Err(e);
        }
    };
    attempt.as_mut().unwrap()["outcome"] = json!(&outcome);
    if !matches!(outcome.as_str(), "reset" | "alreadyRedeemed") { return Err(format!("consume returned {outcome}")); }
    for poll in 0..3 {
        if poll > 0 { tokio::time::sleep(Duration::from_secs(2)).await; }
        let result = session.read().await?;
        let polled = parse_codex(&result, unix_now(), cfg.cooldown_secs);
        if polled.state == State::Unknown { return Err(format!("{outcome}, but quota poll returned {}", polled.detail)); }
        *latest = polled;
        if latest.state != State::Exhausted { break; }
    }
    if latest.state != State::Available { return Err(format!("{outcome}, but quota is still {:?}", latest.state)); }
    record(latest);
    latest.redeemed = Some(if outcome == "reset" {
        format!("codex: reset credit redeemed ({} left)", latest.reset_credits)
    } else {
        format!("codex: reset credit redeemed by another session ({} left)", latest.reset_credits)
    });
    Ok(())
}

pub async fn check(agent: Agent, cfg: &RotationConfig) -> Status {
    if let Some(st) = fresh(cached(agent), cfg, unix_now()) {
        return st;
    }
    if agent == Agent::Grok {
        return cached(Agent::Grok).unwrap_or_else(grok_unknown);
    }
    // Parallel delegate calls queue here so only the first one probes; the
    // rest find its answer (or its failure) when their turn comes.
    let mut last_failure = probe_turn().lock().await;
    let now = unix_now();
    let known = cached(agent);
    if let Some(st) = fresh(known.clone(), cfg, now) {
        return st;
    }
    let program = Agent::Codex.program();
    let remembered = last_failure.clone().filter(|(p, _)| *p == program).map(|(_, st)| st);
    let probed = match fresh(remembered, cfg, now) {
        Some(st) => st,
        None => refresh_codex(cfg).await,
    };
    *last_failure = (probed.state == State::Unknown).then(|| (program, probed.clone()));
    // A broken probe must not erase a block that has not run out yet.
    match known {
        Some(st) if probed.state == State::Unknown && is_blocked(&st, now) => st,
        _ => probed,
    }
}

fn fresh(st: Option<Status>, cfg: &RotationConfig, now: i64) -> Option<Status> {
    let ttl = i64::try_from(cfg.probe_ttl_secs).unwrap_or(i64::MAX);
    st.filter(|st| now.saturating_sub(st.checked_at) < ttl)
}

/// One Codex probe at a time per process; holds the last failed probe
/// (never written to disk), keyed by the codex binary it ran.
fn probe_turn() -> &'static tokio::sync::Mutex<Option<(String, Status)>> {
    static TURN: std::sync::OnceLock<tokio::sync::Mutex<Option<(String, Status)>>> = std::sync::OnceLock::new();
    TURN.get_or_init(|| tokio::sync::Mutex::new(None))
}

pub async fn record_quota_error(agent: Agent, error_text: &str, cfg: &RotationConfig) -> Status {
    let mut block = cfg.cooldown_secs;
    if agent == Agent::Codex {
        let probed = refresh_codex(cfg).await;
        if probed.state == State::Exhausted {
            return probed;
        }
        // The probe disagrees with the call (it happens): trust the call,
        // but only until the next probe is due.
        if probed.state == State::Available {
            block = block.min(cfg.probe_ttl_secs);
        }
    }
    let now = unix_now();
    let st = Status {
        agent,
        state: State::Exhausted,
        source: Source::Error,
        checked_at: now,
        until: Some(after(now, block)),
        plan: None,
        windows: Vec::new(),
        reset_credits: 0,
        reset_eligible: false,
        redeemed: None,
        detail: quota_line(error_text),
    };
    record(&st);
    st
}

fn quota_line(text: &str) -> String {
    let line = text.lines().find(|l| is_quota_error(l)).unwrap_or(text);
    line.trim().chars().take(200).collect()
}

pub async fn report(cfg: &RotationConfig, refresh_now: bool) -> String {
    let mut lines = Vec::new();
    for agent in Agent::ALL {
        let st = if refresh_now { refresh(agent, cfg).await } else { check(agent, cfg).await };
        lines.push(describe(&st, unix_now()));
    }
    lines.push(rotation_line(cfg));
    lines.join("\n")
}

fn rotation_line(cfg: &RotationConfig) -> String {
    if !cfg.enabled {
        let only = cfg.order.first().map(|a| a.name()).unwrap_or("none");
        return format!("rotation: disabled (only {only})");
    }
    let order = cfg.order.iter().map(|a| a.name()).collect::<Vec<_>>().join(" → ");
    format!("rotation: enabled, order {order}")
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};

    pub const CODEX_LIMIT: &str = "ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 4th, 2026 2:38 PM.";
    pub const GROK_LIMIT: &str = r#"Error: Internal error: { "message": "API error (status 402 Payment Required): Grok Build usage balance exhausted", "http_status": 402 }"#;

    /// An executable `/bin/sh` script at `dir/name`.
    #[cfg(unix)]
    pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// A `account/rateLimits/read` result with one weekly window resetting in a day.
    pub fn probe_result(used: u32, reached: bool) -> Value {
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

    #[cfg(unix)]
    pub fn reset_codex(dir: &Path, result: &Value, outcome: &str) -> String {
        let state = serde_json::to_string(&dir.join("reset-state").to_string_lossy()).unwrap();
        let log = serde_json::to_string(&dir.join("consume-params").to_string_lossy()).unwrap();
        format!(r#"#!/usr/bin/python3
import json, os, sys, time
state = {state}
log = {log}
result = json.loads({result})
outcome = {outcome}
if sys.argv[1] != 'app-server':
    sys.stdin.read()
    if outcome == 'quota-again':
        if os.path.exists(state):
            os.unlink(state)
    elif os.path.exists(state):
        print('codex done')
        sys.exit(0)
    print('ERROR: usage limit reached', file=sys.stderr)
    sys.exit(1)
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialized':
        continue
    reply = {{'id':msg['id'], 'result':{{}}}}
    if method == 'account/rateLimits/read':
        value = json.loads(json.dumps(result))
        spent = len(open(log).readlines()) if os.path.exists(log) else 0
        value['rateLimitResetCredits']['availableCount'] = max(0, result['rateLimitResetCredits']['availableCount'] - spent)
        credits = value['rateLimitResetCredits'].get('credits')
        if isinstance(credits, list):
            value['rateLimitResetCredits']['credits'] = credits[spent:]
        if os.path.exists(state) and outcome == 'unknown-poll':
            reply['result'] = {{}}
            print(json.dumps(reply), flush=True)
            continue
        if os.path.exists(state) and outcome != 'stuck':
            value['ordinaryUsageAllowed'] = True
            value['rateLimits']['rateLimitReachedType'] = None
            for label in ['primary', 'secondary']:
                if value['rateLimits'].get(label):
                    value['rateLimits'][label]['usedPercent'] = 0
        reply['result'] = value
    if method == 'account/rateLimitResetCredit/consume':
        with open(log, 'a') as f:
            f.write(json.dumps(msg['params']) + '\n')
        if outcome == 'hang':
            time.sleep(120)
        if outcome == 'slow-reset':
            time.sleep(1)
        if outcome == 'rpc-error':
            reply = {{'id':msg['id'], 'error':{{'message':'consume RPC failed'}}}}
        else:
            if outcome in ['reset', 'alreadyRedeemed', 'stuck', 'unknown-poll', 'slow-reset', 'quota-again']:
                open(state, 'w').close()
            reply['result'] = {{'outcome':'reset' if outcome in ['stuck', 'unknown-poll', 'slow-reset', 'quota-again'] else outcome}}
    print(json.dumps(reply), flush=True)
"#, state = state, log = log, result = serde_json::to_string(&result.to_string()).unwrap(), outcome = serde_json::to_string(outcome).unwrap())
    }

    /// A `/bin/sh` stand-in for the codex CLI. `app-server` answers
    /// `account/rateLimits/read` with `result_json` asynchronously and, like
    /// the real one, drops the answer when stdin closes first. Any other
    /// subcommand prints `exec_stderr` and exits 1.
    pub fn fake_codex(result_json: &str, exec_stderr: &str) -> String {
        let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let response = format!(r#"{{"jsonrpc":"2.0","id":2,"result":{result_json}}}"#);
        format!(
            r#"#!/bin/sh
if [ "$1" != "app-server" ]; then
  cat >/dev/null
  printf '%s\n' {stderr} >&2
  exit 1
fi
bg=
while IFS= read -r line; do
  case "$line" in
    *account/rateLimits/read*)
      printf '%s\n' '{{"jsonrpc":"2.0","method":"account/updated"}}'
      ( sleep 0.3; printf '%s\n' {response} ) & bg=$! ;;
  esac
done
[ -n "$bg" ] && kill "$bg" 2>/dev/null
exit 0
"#,
            stderr = q(exec_stderr),
            response = q(&response),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_support::CODEX_LIMIT;

    fn reset_fixture() -> Value {
        let mut v = test_support::probe_result(100, true);
        v["accountId"] = json!("77f51905-bf9d-4802-b11d-12aa4bf7a783");
        v["rateLimitResetCredits"] = json!({"availableCount":1,"credits":[{"id":"credit-1", "status":"available", "resetType":"codexRateLimits", "expiresAt":null}]});
        v
    }

    fn consume_params(dir: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("consume-params")).unwrap_or_default().lines().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    #[test]
    fn reset_keys_follow_the_blocking_window_and_validate_account() {
        let mut v = reset_fixture();
        let st = parse_codex(&v, unix_now(), 3600);
        let key = reset_key(&v, &st);
        assert_eq!(key, reset_key(&v, &st));
        assert_eq!(key.len(), 36);
        assert_eq!(&key[..24], "77f51905-bf9d-4802-b11d-");
        assert!(key.bytes().enumerate().all(|(i, c)| if [8,13,18,23].contains(&i) { c == b'-' } else { c.is_ascii_hexdigit() }));
        v["rateLimits"]["primary"]["resetsAt"] = json!(st.until.unwrap() + 1);
        assert_ne!(key, reset_key(&v, &parse_codex(&v, unix_now(), 3600)));
        for account in [Value::Null, json!("NOT-A-UUID"), json!("77F51905-bf9d-4802-b11d-12aa4bf7a783")] {
            v["accountId"] = account;
            assert!(reset_key(&v, &st).starts_with("00000000-0000-4000-8000-"));
        }
        v["rateLimits"]["secondary"] = json!({"usedPercent":100,"resetsAt":st.until.unwrap()+100});
        let both = parse_codex(&v, unix_now(), 3600);
        assert!(reset_key(&v, &both).ends_with(&format!("{:012x}", both.until.unwrap())));
        let mut old = serde_json::to_value(st).unwrap();
        old.as_object_mut().unwrap().remove("reset_credits");
        old.as_object_mut().unwrap().remove("reset_eligible");
        let old: Status = serde_json::from_value(old).unwrap();
        assert_eq!(old.reset_credits, 0);
        assert!(!old.reset_eligible);
        assert!(old.redeemed.is_none());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn reset_ensure_and_parallel_calls_spend_once() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, dir) = home_lock();
        let v = reset_fixture();
        let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &v, "reset"));
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
        let cfg = RotationConfig::default();
        let c = || ensure(Agent::Codex, &cfg);
        let results = tokio::join!(c(), c(), c(), c(), c());
        let all = [results.0, results.1, results.2, results.3, results.4];
        assert!(all.iter().all(|s| s.state == State::Available));
        assert_eq!(all.iter().filter(|s| s.redeemed.is_some()).count(), 1);
        let params = consume_params(dir.path());
        assert_eq!(params, vec![json!({"creditId":"credit-1", "idempotencyKey":reset_key(&v, &parse_codex(&v, unix_now(),3600))})]);
        let audit = std::fs::read_to_string(paths::resets_log()).unwrap();
        assert_eq!(audit.lines().count(), 2);
        let lines: Vec<Value> = audit.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!(lines[0]["outcome"], "pending");
        assert_eq!(lines[0]["credit_id"], lines[1]["credit_id"]);
        assert_eq!(lines[0]["idempotency_key"], lines[1]["idempotency_key"]);
        let audit = &lines[1];
        assert_eq!(audit["outcome"], "reset");
        assert_eq!(audit["available_count_before"], 1);
        assert_eq!(audit["windows"].as_array().unwrap().len(), 1);
        assert_eq!(paths::mode_of(&paths::resets_log(), 0), 0o600);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn reset_selection_and_non_spending_guards() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = RotationConfig::default();
        let base = reset_fixture();
        let now = unix_now();
        let row = |id: &str, status: &str, kind: &str, expiry: Value| json!({"id":id,"status":status,"resetType":kind,"expiresAt":expiry});
        let rows = json!([
            row("late","available","codexRateLimits",json!(now+200)),
            row("expired","available","codexRateLimits",json!(now-1)),
            row("used","redeemed","codexRateLimits",json!(now+1)),
            row("unknown","available","unknown",json!(now+1)),
            row("early","available","codexRateLimits",json!(now+100)),
            row("forever","available","codexRateLimits",Value::Null)
        ]);
        let mut cases = Vec::new();
        let mut selected = base.clone(); selected["rateLimitResetCredits"]["credits"] = rows;
        cases.push(("earliest", selected, cfg.clone(), Some(Some("early"))));
        let mut null = base.clone(); null["rateLimitResetCredits"]["credits"] = Value::Null;
        cases.push(("null", null, cfg.clone(), Some(None)));
        let mut empty = base.clone(); empty["rateLimitResetCredits"]["credits"] = json!([]);
        cases.push(("empty", empty, cfg.clone(), None));
        let mut redeeming = base.clone(); redeeming["rateLimitResetCredits"]["credits"].as_array_mut().unwrap().push(row("flight","redeeming","unknown",Value::Null));
        cases.push(("redeeming", redeeming, cfg.clone(), None));
        let mut disabled = cfg.clone(); disabled.auto_reset = false;
        cases.push(("disabled", base.clone(), disabled, None));
        let mut soon = base.clone(); soon["rateLimits"]["primary"]["resetsAt"] = json!(now+300);
        cases.push(("soon", soon, cfg.clone(), None));
        for kind in ["workspace_owner_credits_depleted", "workspace_member_credits_depleted", "workspace_owner_usage_limit_reached", "workspace_member_usage_limit_reached"] {
            let mut v = base.clone(); v["rateLimits"]["rateLimitReachedType"] = json!(kind);
            cases.push((kind,v,cfg.clone(),None));
        }
        let mut spend = base.clone(); spend["rateLimits"]["spendControlReached"] = json!(true);
        cases.push(("spend",spend,cfg.clone(),None));
        let mut zero = base.clone(); zero["rateLimitResetCredits"]["availableCount"] = json!(0);
        cases.push(("zero",zero,cfg.clone(),None));
        let mut healthy = base.clone(); healthy["rateLimits"]["primary"]["usedPercent"] = json!(10); healthy["rateLimits"]["rateLimitReachedType"] = Value::Null; healthy["ordinaryUsageAllowed"] = json!(true);
        cases.push(("healthy",healthy,cfg.clone(),None));
        for (name,v,cfg,want) in cases {
            let (_home,dir) = home_lock();
            let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &v, "reset"));
            let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
            let st = ensure(Agent::Codex,&cfg).await;
            let params = consume_params(dir.path());
            match want {
                Some(id) => {
                    assert_eq!(params.len(),1,"{name}: {st:?}");
                    assert_eq!(params[0].get("creditId").and_then(Value::as_str),id,"{name}");
                    assert_eq!(st.state, State::Available,"{name}");
                }
                None => assert!(params.is_empty(),"{name}: {params:?}"),
            }
        }
        // Fast guards cannot spawn even a broken executable.
        let (_home,_dir) = home_lock();
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN","/does-not-exist");
        let mut cfg = cfg;
        cfg.auto_reset = false;
        let original = parse_codex(&base,unix_now(),3600);
        assert_eq!(redeem_if_exhausted(original.clone(),&cfg).await,original);
        let grok = grok_unknown();
        assert_eq!(redeem_if_exhausted(grok.clone(),&cfg).await,grok);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn quota_read_only_surfaces_never_consume() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home,dir) = home_lock();
        let v = reset_fixture();
        let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &v, "reset"));
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
        let cfg = RotationConfig::default();
        assert_eq!(check(Agent::Codex,&cfg).await.state,State::Exhausted);
        assert_eq!(refresh(Agent::Codex,&cfg).await.state,State::Exhausted);
        assert!(report(&cfg,true).await.contains("1 reset credit available"));
        assert!(report(&cfg,false).await.contains("1 reset credit available"));
        assert!(record_quota_error(Agent::Codex,CODEX_LIMIT,&cfg).await.reset_eligible);
        assert!(consume_params(dir.path()).is_empty());
        assert!(!paths::resets_log().exists());
        let mut available = parse_codex(&v, unix_now(),3600);
        available.state = State::Available;
        available.reset_credits = 2;
        assert!(describe(&available,unix_now()).contains("2 reset credits available"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn reset_failures_are_audited_and_suppressed_until_probe() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = RotationConfig::default();
        for outcome in ["noCredit","nothingToReset","rpc-error","unknown-new-outcome","stuck","unknown-poll","hang","alreadyRedeemed"] {
            let (_home,dir) = home_lock();
            let v = reset_fixture();
            let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &v, outcome));
            let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
            let st = ensure(Agent::Codex,&cfg).await;
            if outcome == "alreadyRedeemed" {
                assert_eq!(st.state,State::Available);
                assert!(st.redeemed.unwrap().contains("another session"));
            } else {
                assert_eq!(st.state,State::Exhausted,"{outcome}");
                assert!(!st.reset_eligible,"{outcome}");
                assert!(st.detail.contains("reset credit:"),"{outcome}: {}",st.detail);
                let again = ensure(Agent::Codex,&cfg).await;
                assert!(!again.reset_eligible);
            }
            assert_eq!(consume_params(dir.path()).len(),1,"{outcome}");
            let audit = std::fs::read_to_string(paths::resets_log()).unwrap();
            assert_eq!(audit.lines().count(),2,"{outcome}");
            let lines: Vec<Value> = audit.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
            assert_eq!(lines[0]["outcome"], "pending");
            assert_eq!(lines[0]["credit_id"], lines[1]["credit_id"]);
            assert_eq!(lines[0]["idempotency_key"], lines[1]["idempotency_key"]);
            let audit = &lines[1];
            assert!(audit["outcome"].as_str().is_some_and(|s| !s.is_empty()),"{outcome}");
            if outcome == "hang" { assert!(audit["outcome"].as_str().unwrap().contains("timed out")); }
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn grok_ignores_a_codex_status_under_its_cache_key() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home,dir) = home_lock();
        let v = reset_fixture();
        let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &v, "reset"));
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
        let st = parse_codex(&v, unix_now(), 3600);
        std::fs::write(paths::quota_file(), json!({"grok":st}).to_string()).unwrap();
        assert!(cached(Agent::Grok).is_none());
        let st = ensure(Agent::Grok, &RotationConfig::default()).await;
        assert_eq!(st.agent, Agent::Grok);
        assert_eq!(st.state, State::Unknown);
        assert!(consume_params(dir.path()).is_empty());
        assert!(!paths::resets_log().exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn reset_transaction_survives_caller_cancellation() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home,dir) = home_lock();
        let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &reset_fixture(), "slow-reset"));
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
        let task = tokio::spawn(async { ensure(Agent::Codex, &RotationConfig::default()).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(paths::resets_log()).unwrap_or_default().contains("pending") { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let audit = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let audit = std::fs::read_to_string(paths::resets_log()).unwrap();
                if audit.lines().count() == 2 { break audit; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let lines: Vec<Value> = audit.lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!(lines[0]["outcome"], "pending");
        assert_eq!(lines[1]["outcome"], "reset");
        assert_eq!(lines[0]["credit_id"], lines[1]["credit_id"]);
        assert_eq!(lines[0]["idempotency_key"], lines[1]["idempotency_key"]);
        assert_eq!(consume_params(dir.path()).len(), 1);
        assert_eq!(cached(Agent::Codex).unwrap().state, State::Available);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn pending_audit_failure_spends_nothing() {
        let _lock = paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home,dir) = home_lock();
        let bin = test_support::script(dir.path(), "reset", &test_support::reset_codex(dir.path(), &reset_fixture(), "reset"));
        let _bin = paths::EnvVar::set("BATON_CODEX_BIN", bin);
        std::fs::create_dir(paths::resets_log()).unwrap();
        let st = ensure(Agent::Codex, &RotationConfig::default()).await;
        assert!(st.detail.contains("pending audit write failed"), "{st:?}");
        assert!(!st.reset_eligible);
        assert!(consume_params(dir.path()).is_empty());
    }

    fn exhausted_fixture() -> Value {
        json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "normalModelSlug": null,
                "primary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1793774326},
                "secondary": null,
                "credits": {"hasCredits": false, "unlimited": false, "balance": "0"},
                "individualLimit": null,
                "spendControlReached": false,
                "planType": "promax",
                "rateLimitReachedType": "rate_limit_reached"
            }
        })
    }

    fn healthy_fixture() -> Value {
        json!({
            "ordinaryUsageAllowed": true,
            "rateLimits": {
                "primary": {"usedPercent": 13.0, "windowDurationMins": 10080, "resetsAt": 1793513449},
                "secondary": null,
                "credits": {"hasCredits": false, "unlimited": false},
                "planType": "plus",
                "rateLimitReachedType": null
            }
        })
    }

    #[test]
    fn quota_phrases_match_and_ordinary_errors_do_not() {
        assert!(is_quota_error(CODEX_LIMIT));
        assert!(is_quota_error("HTTP 429 from the gateway"));
        assert!(is_quota_error("resource_exhausted"));
        assert!(is_quota_error(r#"Error: Internal error: { "message": "API error (status 402 Payment Required): Grok Build usage balance exhausted", "http_status": 402 }"#));
        assert!(!is_quota_error("connection refused"));
        assert!(!is_quota_error("error: timed out"));
    }

    #[test]
    fn parses_exhausted_healthy_credits_and_missing() {
        let now = 1_700_000_000;
        let exhausted = parse_codex(&exhausted_fixture(), now, 3600);
        assert_eq!(exhausted.state, State::Exhausted);
        assert_eq!(exhausted.until, Some(1793774326));
        assert_eq!(exhausted.plan.as_deref(), Some("promax"));
        assert_eq!(exhausted.source, Source::Probe);
        assert_eq!(exhausted.windows.len(), 1);
        assert_eq!(exhausted.windows[0].label, "primary");
        assert_eq!(exhausted.windows[0].used_percent, 100.0);
        assert_eq!(exhausted.detail, "weekly window 100% used");

        let healthy = parse_codex(&healthy_fixture(), now, 3600);
        assert_eq!(healthy.state, State::Available);
        assert_eq!(healthy.until, None);
        assert_eq!(healthy.windows[0].used_percent, 13.0);

        let credits = parse_codex(
            &json!({
                "ordinaryUsageAllowed": false,
                "rateLimits": {
                    "primary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1793774326},
                    "secondary": null,
                    "credits": {"hasCredits": true, "unlimited": false},
                    "planType": "promax",
                    "rateLimitReachedType": "rate_limit_reached"
                }
            }),
            now,
            3600,
        );
        assert_eq!(credits.state, State::Available);
        assert_eq!(credits.detail, "limit reached, running on credits");
        assert_eq!(credits.until, None);

        let unlimited = parse_codex(
            &json!({
                "rateLimits": {
                    "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 50},
                    "credits": {"hasCredits": false, "unlimited": true},
                    "rateLimitReachedType": "rate_limit_reached"
                }
            }),
            now,
            3600,
        );
        assert_eq!(unlimited.state, State::Available);
        assert_eq!(unlimited.detail, "limit reached, running on credits");

        let missing = parse_codex(&json!({"ordinaryUsageAllowed": false}), now, 3600);
        assert_eq!(missing.state, State::Unknown);
        assert_eq!(missing.detail, "probe returned no rateLimits");
        assert_eq!(missing.until, None);
    }

    #[test]
    fn until_prefers_full_windows_then_earliest_reset_then_cooldown() {
        let now = 1_000;
        let both = parse_codex(
            &json!({
                "rateLimits": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 10080, "resetsAt": 5000},
                    "secondary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 4000},
                    "credits": {"hasCredits": false, "unlimited": false},
                    "rateLimitReachedType": null
                }
            }),
            now,
            3600,
        );
        assert_eq!(both.state, State::Exhausted);
        assert_eq!(both.until, Some(4000));
        assert_eq!(both.detail, "5h window 100% used");

        let reached = parse_codex(
            &json!({
                "ordinaryUsageAllowed": true,
                "rateLimits": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 10080, "resetsAt": 4000},
                    "secondary": {"usedPercent": 20, "windowDurationMins": 300, "resetsAt": 9000},
                    "credits": {"hasCredits": false, "unlimited": false},
                    "rateLimitReachedType": "rate_limit_reached"
                }
            }),
            now,
            3600,
        );
        // Reached with no full window: the earliest reset, so a 5h block never becomes a weekly one.
        assert_eq!(reached.until, Some(4000));

        let no_reset = parse_codex(
            &json!({
                "rateLimits": {
                    "primary": {"usedPercent": 100, "windowDurationMins": 10080},
                    "credits": {"hasCredits": false, "unlimited": false},
                    "rateLimitReachedType": "rate_limit_reached"
                }
            }),
            now,
            3600,
        );
        assert_eq!(no_reset.until, Some(now + 3600));
    }

    #[test]
    fn blocked_only_while_the_window_is_still_closed() {
        let now = 1_700_000_000;
        let st = parse_codex(&exhausted_fixture(), now, 3600);
        assert!(is_blocked(&st, 1793774326 - 1));
        assert!(!is_blocked(&st, 1793774326));
        assert!(!is_blocked(&st, 1793774326 + 10));
        let mut open = st.clone();
        open.until = None;
        assert!(is_blocked(&open, now));
        let healthy = parse_codex(&healthy_fixture(), now, 3600);
        assert!(!is_blocked(&healthy, now));
    }

    #[test]
    fn describe_exhausted_available_and_unknown() {
        let now = 1_700_000_180;
        let exhausted = parse_codex(&exhausted_fixture(), now - 180, 3600);
        let text = describe(&exhausted, now);
        assert!(text.starts_with("codex: exhausted until "), "{text}");
        assert!(text.contains("3m ago"), "{text}");
        assert!(text.contains("weekly window 100% used"), "{text}");
        assert!(text.contains("plan promax"), "{text}");
        assert!(text.contains("probe"), "{text}");

        let healthy = parse_codex(&healthy_fixture(), now, 3600);
        let text = describe(&healthy, now);
        assert!(text.starts_with("codex: available ("), "{text}");
        assert!(text.contains("0m ago"), "{text}");
        assert!(text.contains("weekly 13% used"), "{text}");
        assert!(text.contains("resets "), "{text}");

        let expired = describe(&exhausted, 1793774326);
        assert!(expired.contains("available again"), "{expired}");
        assert!(expired.contains("block expired at "), "{expired}");

        let grok = Status {
            agent: Agent::Grok,
            state: State::Unknown,
            source: Source::None,
            checked_at: now,
            until: None,
            plan: None,
            windows: Vec::new(),
            reset_credits: 0,
            reset_eligible: false,
            redeemed: None,
            detail: "grok has no quota API; marked exhausted only after a failed call".into(),
        };
        assert_eq!(describe(&grok, now), "grok: unknown (no quota API; marked exhausted only after a failed call)");
    }

    fn home_lock() -> (crate::paths::EnvVar, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::paths::EnvVar::set("BATON_HOME", dir.path());
        (home, dir)
    }

    #[test]
    fn store_round_trips_and_ignores_corrupt_json() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _dir) = home_lock();
        assert!(cached(Agent::Codex).is_none());
        let st = parse_codex(&exhausted_fixture(), 1_700_000_000, 3600);
        record(&st);
        let got = cached(Agent::Codex).unwrap();
        assert_eq!(got.state, State::Exhausted);
        assert_eq!(got.until, Some(1793774326));
        assert_eq!(got.plan.as_deref(), Some("promax"));
        assert_eq!(load_all().len(), 1);
        assert_eq!(paths::mode_of(&paths::quota_file(), 0), 0o600);
        std::fs::write(paths::quota_file(), b"{not json").unwrap();
        assert!(load_all().is_empty());
        assert!(cached(Agent::Grok).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn cache_ttl_and_failed_probe_do_not_clobber() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _dir) = home_lock();
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", "/nonexistent/baton-codex");
        let cfg = RotationConfig::default();
        let marker = Status {
            agent: Agent::Codex,
            state: State::Available,
            source: Source::Probe,
            checked_at: unix_now(),
            until: None,
            plan: None,
            windows: Vec::new(),
            reset_credits: 0,
            reset_eligible: false,
            redeemed: None,
            detail: "cached-marker".into(),
        };
        record(&marker);
        let hit = check(Agent::Codex, &cfg).await;
        assert_eq!(hit.detail, "cached-marker");

        let mut stale = cfg.clone();
        stale.probe_ttl_secs = 0;
        let missed = check(Agent::Codex, &stale).await;
        assert_eq!(missed.state, State::Unknown);
        assert!(missed.detail.contains("probe failed"), "{}", missed.detail);
        assert_eq!(cached(Agent::Codex).unwrap().detail, "cached-marker");

        let grok = check(Agent::Grok, &cfg).await;
        assert_eq!(grok.state, State::Unknown);
        assert_eq!(grok.source, Source::None);
        assert!(cached(Agent::Grok).is_none());

        let marked = record_quota_error(Agent::Grok, "ignored\nERROR: usage limit hit\n", &cfg).await;
        assert_eq!(marked.state, State::Exhausted);
        assert_eq!(marked.source, Source::Error);
        assert_eq!(marked.until, Some(marked.checked_at + cfg.cooldown_secs as i64));
        assert!(marked.detail.contains("usage limit"));
        assert!(is_blocked(&cached(Agent::Grok).unwrap(), marked.checked_at));

        let codex_err = record_quota_error(Agent::Codex, "usage limit hit", &cfg).await;
        assert_eq!(codex_err.state, State::Exhausted);
        assert_eq!(codex_err.source, Source::Error);
        assert_eq!(cached(Agent::Codex).unwrap().source, Source::Error);

        // A failed probe keeps a block that has not run out; refresh clears grok's error block.
        assert_eq!(check(Agent::Codex, &stale).await.source, Source::Error);
        assert!(is_blocked(&check(Agent::Grok, &stale).await, unix_now()));
        let cleared = refresh(Agent::Grok, &cfg).await;
        assert_eq!((cleared.state, cleared.source), (State::Unknown, Source::None));
        assert!(cached(Agent::Grok).is_none());
    }

    /// Five parallel checks, one app-server start: for a good probe and for a dead one.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn parallel_checks_share_one_probe() {
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = RotationConfig::default();
        for (name, answers) in [("good", true), ("dead", false)] {
            let (_home, dir) = home_lock();
            let count = dir.path().join("starts");
            let inner = test_support::script(dir.path(), "inner", &test_support::fake_codex(&test_support::probe_result(100, true).to_string(), ""));
            let body = if answers {
                format!("#!/bin/sh\necho x >> {}\nexec {} \"$@\"\n", count.display(), inner.display())
            } else {
                format!("#!/bin/sh\necho x >> {}\nexit 1\n", count.display())
            };
            let bin = test_support::script(dir.path(), name, &body);
            let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &bin);
            let c = || check(Agent::Codex, &cfg);
            let all = tokio::join!(c(), c(), c(), c(), c());
            let starts = std::fs::read_to_string(&count).unwrap().lines().count();
            assert_eq!(starts, 1, "{name}: {starts} probes");
            let want = if answers { State::Exhausted } else { State::Unknown };
            for st in [all.0, all.1, all.2, all.3, all.4] {
                assert_eq!(st.state, want, "{name}");
            }
        }
    }

    #[test]
    fn source_keeps_its_on_disk_names() {
        let v = json!({"agent":"grok","state":"exhausted","source":"error","checked_at":1,"until":2,"plan":null,"windows":[],"detail":"x"});
        let st: Status = serde_json::from_value(v).unwrap();
        assert_eq!(st.source, Source::Error);
        for src in [Source::Probe, Source::Error, Source::None] {
            assert_eq!(serde_json::to_value(src).unwrap(), json!(src.name()));
        }
    }

    #[test]
    fn only_the_stderr_tail_counts() {
        assert!(stderr_says_quota(&format!("progress\n{CODEX_LIMIT}\n{CODEX_LIMIT}\n\n")));
        let echoed = format!("{CODEX_LIMIT}\n{}ERROR: sandbox denied\n", "tool output\n".repeat(5));
        assert!(!stderr_says_quota(&echoed));
        assert!(!is_quota_error("see the rate limiting middleware and quota.rs"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn probe_codex_reads_the_rate_limit_response() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::paths::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.sh");
        std::fs::write(&path, test_support::fake_codex(&exhausted_fixture().to_string(), "")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _bin = crate::paths::EnvVar::set("BATON_CODEX_BIN", &path);
        let result = probe_codex(Duration::from_secs(5)).await.expect("probe");
        let st = parse_codex(&result, 1_700_000_000, 3600);
        assert_eq!(st.state, State::Exhausted);
        assert_eq!(st.until, Some(1793774326));
        assert_eq!(st.plan.as_deref(), Some("promax"));
    }
}
