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
                return format!("{name}: available again (block expired at {})", fmt_ts(until));
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
        detail: "grok has no quota API; marked exhausted only after a failed call".into(),
    }
}

pub fn load_all() -> HashMap<String, Status> {
    with_store(read_map).unwrap_or_default()
}

pub fn cached(agent: Agent) -> Option<Status> {
    load_all().get(agent.name()).cloned()
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

async fn read_rate_limits(child: &mut Child) -> Result<Value, String> {
    let mut stdin = child.stdin.take().ok_or_else(|| "codex probe has no stdin".to_string())?;
    for msg in probe_messages() {
        let mut line = msg.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.map_err(|e| format!("probe write failed: {e}"))?;
    }
    stdin.flush().await.map_err(|e| format!("probe write failed: {e}"))?;
    // Keep stdin open: the real app-server exits on EOF and drops in-flight requests.
    let _stdin = stdin;
    let stdout = child.stdout.take().ok_or_else(|| "codex probe has no stdout".to_string())?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| format!("probe read failed: {e}"))? {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if !v.is_object() || v.get("id").and_then(Value::as_u64) != Some(2) {
            continue;
        }
        if let Some(msg) = rpc_error(&v) {
            return Err(msg);
        }
        return v.get("result").cloned().ok_or_else(|| "codex probe response has no result".to_string());
    }
    Err("codex probe ended without a rateLimits response".into())
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
            detail: format!("probe failed: {e}"),
        },
    }
}

pub async fn check(agent: Agent, cfg: &RotationConfig) -> Status {
    let now = unix_now();
    let known = cached(agent);
    if let Some(st) = &known {
        if now.saturating_sub(st.checked_at) < i64::try_from(cfg.probe_ttl_secs).unwrap_or(i64::MAX) {
            return st.clone();
        }
    }
    if agent == Agent::Grok {
        return known.unwrap_or_else(grok_unknown);
    }
    let probed = refresh_codex(cfg).await;
    // A broken probe must not erase a block that has not run out yet.
    match known {
        Some(st) if probed.state == State::Unknown && is_blocked(&st, now) => st,
        _ => probed,
    }
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

    const CODEX_LIMIT: &str = "ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 4th, 2026 2:38 PM.";

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
