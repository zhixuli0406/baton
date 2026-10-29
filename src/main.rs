mod account;
mod agents;
mod catalog;
mod config;
mod handoff;
mod hooks;
mod install;
mod mcp;
mod mcp_handoff;
mod paths;
mod quota;
mod routing;
mod transcript;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use crate::agents::{Agent, Mode};

#[derive(Parser)]
#[command(name = "baton", version, about = "Let Claude Code delegate to Codex/Grok by capability and hand work over at the usage limit")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server (stdio). Registered by `baton install`.
    Mcp,
    /// Claude Code hook handlers (read hook JSON on stdin).
    Hook {
        #[arg(value_parser = ["stop-failure", "pre-tool-use", "user-prompt-submit"])]
        event: String,
    },
    /// Inspect or control handoffs.
    Handoff {
        #[command(subcommand)]
        command: HandoffCmd,
    },
    /// Show delegate agents' usage quota and the rotation order.
    Quota {
        /// Force a new Codex probe.
        #[arg(long)]
        refresh: bool,
    },
    /// Manage Claude accounts.
    Account {
        #[command(subcommand)]
        command: AccountCmd,
    },
    /// Start claude with an account: `baton run work -- --model opus`.
    Run {
        #[arg(default_value = account::DEFAULT)]
        account: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Start claude with an account and continue this directory's latest conversation.
    Switch {
        account: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Register hooks and the MCP server (dry run unless --apply).
    Install {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum HandoffCmd {
    /// List handoffs.
    List,
    /// Show a handoff's brief and result.
    Show { id: String },
    /// Stop an active handoff.
    Stop { id: String },
    /// Hand this directory's latest Claude conversation over now.
    Start {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, value_parser = ["read_only", "write"])]
        mode: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Run in the foreground instead of detaching.
        #[arg(long)]
        wait: bool,
    },
    /// Background worker (internal).
    #[command(hide = true)]
    Run { id: String },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Create an account dir and log in.
    Add {
        name: String,
        /// Skip the login step.
        #[arg(long)]
        no_login: bool,
    },
    /// List accounts with plan and cached usage.
    List,
    /// Log out and delete an account dir.
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Repair shared symlinks and copy user-scope MCP servers into every account.
    Sync,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli.command) {
        eprintln!("baton: {e:#}");
        std::process::exit(1);
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Mcp => {
            let rt = runtime()?;
            let r = rt.block_on(mcp::serve());
            // The stdin reader sits on a blocking thread that never returns
            // on its own; don't let it hold the process open.
            rt.shutdown_timeout(std::time::Duration::from_millis(200));
            r
        }
        Cmd::Hook { event } => {
            run_hook(&event);
            Ok(())
        }
        Cmd::Handoff { command } => handoff_cmd(command),
        Cmd::Quota { refresh } => {
            let config = config::Config::load()?;
            println!("{}", runtime()?.block_on(quota::report(&config.rotation, refresh)));
            Ok(())
        }
        Cmd::Account { command } => account_cmd(command),
        Cmd::Run { account, args } => account::exec_claude(&account, &args),
        Cmd::Switch { account, args } => {
            let mut all = vec!["--continue".to_string()];
            all.extend(args);
            account::exec_claude(&account, &all)
        }
        Cmd::Install { apply } => install::run(apply),
    }
}

/// Hooks always exit 0: a failing hook must not block Claude.
fn run_hook(event: &str) {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("baton hook {event}: invalid input: {e}");
            return;
        }
    };
    let result = match event {
        "stop-failure" => hooks::stop_failure(&input),
        "pre-tool-use" => hooks::pre_tool_use(&input),
        _ => hooks::user_prompt_submit(&input),
    };
    match result {
        Ok(Some(out)) => println!("{out}"),
        Ok(None) => {}
        Err(e) => eprintln!("baton hook {event}: {e:#}"),
    }
}

fn handoff_cmd(cmd: HandoffCmd) -> Result<()> {
    match cmd {
        HandoffCmd::List => {
            let all = handoff::list();
            if all.is_empty() {
                println!("no handoffs");
            }
            for m in all {
                let mut flags = String::new();
                if !m.status.is_active() && !handoff::is_acked(&m.id) {
                    flags.push_str("  (not yet seen by Claude)");
                }
                if !m.sensitive_changes.is_empty() {
                    flags.push_str("  ⚠ changed agent/git config");
                }
                println!(
                    "{}  {:<8} {:<7} {:<9} {}{}",
                    m.id,
                    format!("{:?}", m.status).to_lowercase(),
                    m.agent.name(),
                    m.mode.name(),
                    paths::sanitize_for_terminal(&m.cwd.display().to_string()),
                    flags
                );
            }
            Ok(())
        }
        HandoffCmd::Show { id } => {
            let m = handoff::load(&id)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            let dir = handoff::dir_of(&id);
            let brief = std::fs::read_to_string(dir.join("brief.md")).unwrap_or_default();
            println!("\n===== brief.md =====\n{}", paths::sanitize_for_terminal(&brief));
            println!("===== result.md =====\n{}", paths::sanitize_for_terminal(&handoff::result_text(&id)));
            Ok(())
        }
        HandoffCmd::Stop { id } => {
            handoff::stop(&id)?;
            println!("stop requested for {id}");
            Ok(())
        }
        HandoffCmd::Start { agent, mode, cwd, wait } => {
            let config = config::Config::load()?;
            let cwd = match cwd {
                Some(c) => c,
                None => std::env::current_dir()?,
            };
            let agent = match agent {
                None => config.handoff.agent,
                Some(a) => match Agent::parse(&a) {
                    Some(a) => a,
                    None => bail!("unknown agent {a} (codex, grok)"),
                },
            };
            let mode = match mode.as_deref() {
                None => config.handoff.mode,
                Some("write") => Mode::Write,
                Some(_) => Mode::ReadOnly,
            };
            let transcript = handoff::latest_transcript(&cwd);
            if transcript.is_none() {
                eprintln!("warning: no Claude transcript found for {}; the brief will only hold git state", cwd.display());
            }
            let meta = handoff::start(
                &handoff::NewHandoff {
                    session_id: "",
                    cwd: &cwd,
                    transcript_path: transcript.as_deref(),
                    trigger: "manual",
                    agent,
                    mode,
                },
                &config,
                !wait,
            )?;
            println!("handoff {} created ({} · {})", meta.id, agent.name(), mode.name());
            if wait {
                runtime()?.block_on(handoff::run(&meta.id))?;
                println!("{}", paths::sanitize_for_terminal(&handoff::result_text(&meta.id)));
            } else {
                println!("running in the background; `baton handoff show {}` for progress", meta.id);
            }
            Ok(())
        }
        HandoffCmd::Run { id } => runtime()?.block_on(handoff::run(&id)),
    }
}

fn account_cmd(cmd: AccountCmd) -> Result<()> {
    match cmd {
        AccountCmd::Add { name, no_login } => {
            let dir = account::add(&name, !no_login)?;
            println!("account {name} ready at {}", dir.display());
            println!("start it with `baton run {name}` or continue a conversation with `baton switch {name}`");
            Ok(())
        }
        AccountCmd::List => {
            println!("{:<12} {:<32} {:<8} {:<16} {:<16}", "ACCOUNT", "EMAIL", "PLAN", "5H USED", "7D USED");
            for name in account::names() {
                let i = account::info(&name)?;
                let pct = |w: &Option<(i64, String)>| match w {
                    Some((_, r)) if reset_passed(r) => "reset since".into(),
                    Some((u, r)) => format!("{u}% → {}", short_time(r)),
                    None => "-".into(),
                };
                let email = match (&i.email, i.logged_in) {
                    (Some(e), true) => e.clone(),
                    _ => "(not logged in)".into(),
                };
                println!(
                    "{:<12} {:<32} {:<8} {:<16} {:<16}",
                    i.name,
                    email,
                    i.plan.as_deref().unwrap_or("-"),
                    pct(&i.five_hour),
                    pct(&i.seven_day)
                );
            }
            println!("\nUsage comes from Claude Code's local cache of that account's last session, so it can be out of date.");
            Ok(())
        }
        AccountCmd::Remove { name, yes } => {
            if !yes {
                bail!("this logs {name} out and deletes its account dir; re-run with --yes");
            }
            account::remove(&name)?;
            println!("removed {name}");
            Ok(())
        }
        AccountCmd::Sync => {
            for name in account::names().into_iter().filter(|n| n != account::DEFAULT) {
                let Some(dir) = account::config_dir(&name)? else { continue };
                for w in account::link_shared(&dir, &paths::claude_default_dir())? {
                    eprintln!("warning: {w}");
                }
                let changed = account::sync_mcp(&dir)?;
                if changed.is_empty() {
                    println!("{name}: up to date");
                } else {
                    println!("{name}: updated MCP servers {}", changed.join(", "));
                }
            }
            Ok(())
        }
    }
}

fn reset_passed(rfc3339: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(rfc3339).is_ok_and(|t| t < chrono::Local::now())
}

/// `2026-10-21T15:00:00.123+00:00` → local `10-21 23:00`.
fn short_time(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}
