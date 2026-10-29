//! `~/.baton/config.toml`. A missing file means defaults; a broken file is an
//! error so a typo never silently turns the handoff off.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::agents::{Agent, Mode};
use crate::catalog;
use crate::paths;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub handoff: HandoffConfig,
    pub write: WriteConfig,
    /// Default model per agent name (`codex`, `grok`).
    pub models: HashMap<String, String>,
    /// Extra or replacement rows for the model catalog (`[[catalog]]`).
    pub catalog: Vec<catalog::Entry>,
    pub rotation: RotationConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RotationConfig {
    pub enabled: bool,
    pub order: Vec<Agent>,
    pub probe_ttl_secs: u64,
    pub cooldown_secs: u64,
}

impl Default for RotationConfig {
    fn default() -> Self {
        RotationConfig {
            enabled: true,
            order: vec![Agent::Codex, Agent::Grok],
            probe_ttl_secs: 300,
            cooldown_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriteConfig {
    pub allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    pub enabled: bool,
    pub agent: Agent,
    pub mode: Mode,
    pub timeout_secs: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        HandoffConfig {
            enabled: true,
            agent: Agent::Codex,
            mode: Mode::Write,
            timeout_secs: 3600,
        }
    }
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(text)?;
        for root in &mut config.write.allowed_roots {
            if let Ok(rest) = root.strip_prefix("~") {
                *root = paths::home_dir().join(rest);
            }
            anyhow::ensure!(root.is_absolute(), "[write] allowed_roots entries must be absolute paths or start with ~/; got {}", root.display());
        }
        validate_rotation(&config.rotation)?;
        Ok(config)
    }

    pub fn model_for(&self, agent: Agent) -> Option<String> {
        self.models.get(agent.name()).cloned()
    }

    pub fn catalog(&self) -> catalog::Catalog {
        catalog::Catalog::with_overrides(&self.catalog)
    }
}

fn validate_rotation(rotation: &RotationConfig) -> Result<()> {
    if rotation.order.is_empty() {
        anyhow::bail!("[rotation] order must not be empty");
    }
    for (i, agent) in rotation.order.iter().enumerate() {
        if rotation.order[..i].contains(agent) {
            anyhow::bail!("[rotation] order lists {} more than once", agent.name());
        }
    }
    for (key, secs) in [("probe_ttl_secs", rotation.probe_ttl_secs), ("cooldown_secs", rotation.cooldown_secs)] {
        anyhow::ensure!(secs <= MAX_ROTATION_SECS, "[rotation] {key} must be at most {MAX_ROTATION_SECS} (30 days), got {secs}");
    }
    Ok(())
}

const MAX_ROTATION_SECS: u64 = 30 * 24 * 3600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c = Config::parse("").unwrap();
        assert!(c.write.allowed_roots.is_empty());
        assert!(c.handoff.enabled);
        assert_eq!(c.handoff.agent, Agent::Codex);
        assert_eq!(c.handoff.mode, Mode::Write);
    }

    #[test]
    fn parses_overrides() {
        let c = Config::parse(
            "[handoff]\nagent = \"grok\"\nmode = \"read_only\"\n[models]\ncodex = \"m1\"\n",
        )
        .unwrap();
        assert_eq!(c.handoff.agent, Agent::Grok);
        assert_eq!(c.handoff.mode, Mode::ReadOnly);
        assert_eq!(c.model_for(Agent::Codex).as_deref(), Some("m1"));
        assert_eq!(c.model_for(Agent::Grok), None);
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("[write]\nallowed_root = []\n").is_err());
        assert!(Config::parse("[handoff]\nenabeld = false\n").is_err());
        assert!(Config::parse("[handoff]\nagent = \"gpt\"\n").is_err());
        assert!(Config::parse("[handoff]\nagent = \"gemini\"\n").is_err());
        assert!(Config::parse("[[catalog]]\nagent = \"codex\"\nmodel = \"x\"\ntag = [\"fast\"]\n").is_err());
    }

    #[test]
    fn parses_write_roots_and_expands_home() {
        let c = Config::parse("[write]\nallowed_roots = [\"/tmp/projects\", \"~/x\", \"~\"]\n").unwrap();
        assert_eq!(c.write.allowed_roots, vec![
            std::path::PathBuf::from("/tmp/projects"),
            paths::home_dir().join("x"),
            paths::home_dir(),
        ]);
    }

    #[test]
    fn rejects_relative_write_roots() {
        for root in ["Project", "./Project", "../Project", "", "~someone/Project"] {
            let text = format!("[write]\nallowed_roots = [{root:?}]\n");
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains("[write] allowed_roots") && err.contains("absolute"), "{err}");
        }
    }

    #[test]
    fn parses_catalog_rows() {
        let c = Config::parse(
            "[[catalog]]\nagent = \"grok\"\nmodel = \"grok-5\"\ntags = [\"security\"]\nnote = \"new\"\n",
        )
        .unwrap();
        assert_eq!(c.catalog.len(), 1);
        assert_eq!(c.catalog().resolve(Agent::Grok, "security").unwrap().model, "grok-5");
    }

    #[test]
    fn rotation_defaults() {
        let c = Config::parse("").unwrap();
        assert!(c.rotation.enabled);
        assert_eq!(c.rotation.order, vec![Agent::Codex, Agent::Grok]);
        assert_eq!(c.rotation.probe_ttl_secs, 300);
        assert_eq!(c.rotation.cooldown_secs, 3600);
    }

    #[test]
    fn parses_rotation_override() {
        let c = Config::parse("[rotation]\nenabled = false\norder = [\"grok\"]\nprobe_ttl_secs = 12\ncooldown_secs = 34\n").unwrap();
        assert!(!c.rotation.enabled);
        assert_eq!(c.rotation.order, vec![Agent::Grok]);
        assert_eq!(c.rotation.probe_ttl_secs, 12);
        assert_eq!(c.rotation.cooldown_secs, 34);
        // Unspecified fields keep the documented defaults, not the type zero.
        let partial = Config::parse("[rotation]\nenabled = false\n").unwrap();
        assert!(!partial.rotation.enabled);
        assert_eq!(partial.rotation.order, vec![Agent::Codex, Agent::Grok]);
        assert_eq!(partial.rotation.probe_ttl_secs, 300);
        assert_eq!(partial.rotation.cooldown_secs, 3600);
    }

    #[test]
    fn rejects_bad_rotation() {
        let empty = Config::parse("[rotation]\norder = []\n").unwrap_err().to_string();
        assert!(empty.contains("[rotation] order"), "{empty}");
        let dup = Config::parse("[rotation]\norder = [\"codex\", \"codex\"]\n").unwrap_err().to_string();
        assert!(dup.contains("[rotation] order"), "{dup}");
        let dup_later = Config::parse("[rotation]\norder = [\"codex\", \"grok\", \"codex\"]\n").unwrap_err().to_string();
        assert!(dup_later.contains("[rotation] order"), "{dup_later}");
        assert!(Config::parse("[rotation]\nnope = true\n").is_err());
        assert!(Config::parse("[rotation]\norder = [\"claude\"]\n").is_err());
        let huge = Config::parse("[rotation]\ncooldown_secs = 9223372036854775807\n").unwrap_err().to_string();
        assert!(huge.contains("cooldown_secs"), "{huge}");
        assert!(Config::parse("[rotation]\nprobe_ttl_secs = 2592000\n").is_ok());
    }
}
