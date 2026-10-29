//! Which agent, model and effort a task should use.
//!
//! Easy work should not burn the frontier model. `delegate` asks for a kind
//! and a difficulty; this module turns that into one step per agent, then
//! puts agents that actually have the requested capability first (so research
//! goes to Grok, which is the one with web search).

use crate::agents::{Agent, Effort};
use crate::catalog::Catalog;

pub const KINDS: &[&str] = &["code", "security", "research"];
pub const DIFFICULTIES: &[&str] = &["easy", "normal", "hard"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Code,
    Security,
    Research,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "code" => Some(Kind::Code),
            "security" => Some(Kind::Security),
            "research" => Some(Kind::Research),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difficulty {
    Easy,
    Normal,
    Hard,
}

impl Difficulty {
    pub fn parse(s: &str) -> Option<Difficulty> {
        match s {
            "easy" => Some(Difficulty::Easy),
            "normal" => Some(Difficulty::Normal),
            "hard" => Some(Difficulty::Hard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub agent: Agent,
    pub model: Option<String>,
    pub effort: Effort,
    pub tag: String,
    pub fallback: bool,
}

pub fn plan(kind: Kind, difficulty: Difficulty, effort: Option<Effort>, catalog: &Catalog, order: &[Agent]) -> Vec<Step> {
    let primary = primary_tag(kind, difficulty);
    let fallback_tag = fallback_tag(kind);
    let effort = effort.unwrap_or_else(|| default_effort(kind, difficulty));
    let mut steps: Vec<Step> = order
        .iter()
        .copied()
        .map(|agent| step_for(agent, primary, fallback_tag, effort, catalog))
        .collect();
    // Stable: agents that have the primary tag stay in `order`.
    steps.sort_by_key(|s| s.fallback);
    steps
}

fn primary_tag(kind: Kind, difficulty: Difficulty) -> &'static str {
    match kind {
        Kind::Security => "security",
        Kind::Research => "web_search",
        Kind::Code => match difficulty {
            Difficulty::Easy => "fast",
            Difficulty::Normal => "workhorse",
            Difficulty::Hard => "frontier",
        },
    }
}

fn fallback_tag(kind: Kind) -> &'static str {
    match kind {
        Kind::Code => "workhorse",
        Kind::Security => "frontier",
        Kind::Research => "workhorse",
    }
}

fn default_effort(kind: Kind, difficulty: Difficulty) -> Effort {
    match (kind, difficulty) {
        (Kind::Security, Difficulty::Easy) => Effort::Medium,
        (Kind::Security, Difficulty::Normal) => Effort::High,
        (Kind::Security, Difficulty::Hard) => Effort::Xhigh,
        (_, Difficulty::Easy) => Effort::Low,
        (_, Difficulty::Normal) => Effort::Medium,
        (_, Difficulty::Hard) => Effort::High,
    }
}

fn step_for(agent: Agent, primary: &str, fallback_tag: &str, effort: Effort, catalog: &Catalog) -> Step {
    if let Some(entry) = catalog.resolve(agent, primary) {
        return Step {
            agent,
            model: Some(entry.model.clone()),
            effort,
            tag: primary.to_string(),
            fallback: false,
        };
    }
    if let Some(entry) = catalog.resolve(agent, fallback_tag) {
        return Step {
            agent,
            model: Some(entry.model.clone()),
            effort,
            tag: fallback_tag.to_string(),
            fallback: true,
        };
    }
    Step {
        agent,
        model: None,
        effort,
        tag: fallback_tag.to_string(),
        fallback: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalog {
        Catalog::with_overrides(&[])
    }

    fn step(agent: Agent, model: &str, effort: Effort, tag: &str, fallback: bool) -> Step {
        Step {
            agent,
            model: Some(model.to_string()),
            effort,
            tag: tag.to_string(),
            fallback,
        }
    }

    #[test]
    fn code_easy_uses_fast_models_in_order() {
        let steps = plan(Kind::Code, Difficulty::Easy, None, &cat(), &Agent::ALL);
        assert_eq!(
            steps,
            vec![
                step(Agent::Codex, "gpt-6-luna", Effort::Low, "fast", false),
                step(Agent::Grok, "grok-4.7-build-fast", Effort::Low, "fast", false),
            ]
        );
    }

    #[test]
    fn code_hard_uses_frontier_models() {
        let steps = plan(Kind::Code, Difficulty::Hard, None, &cat(), &Agent::ALL);
        assert_eq!(
            steps,
            vec![
                step(Agent::Codex, "gpt-6-astra", Effort::High, "frontier", false),
                step(Agent::Grok, "grok-4.7", Effort::High, "frontier", false),
            ]
        );
    }

    #[test]
    fn security_prefers_the_agent_that_has_the_tag() {
        let steps = plan(Kind::Security, Difficulty::Normal, None, &cat(), &Agent::ALL);
        assert_eq!(
            steps,
            vec![
                step(Agent::Codex, "gpt-daybreak-blue-latest", Effort::High, "security", false),
                step(Agent::Grok, "grok-4.7", Effort::High, "frontier", true),
            ]
        );
    }

    #[test]
    fn research_puts_grok_first() {
        let steps = plan(Kind::Research, Difficulty::Normal, None, &cat(), &Agent::ALL);
        assert_eq!(
            steps,
            vec![
                step(Agent::Grok, "grok-4.7", Effort::Medium, "web_search", false),
                step(Agent::Codex, "gpt-6.1-sol", Effort::Medium, "workhorse", true),
            ]
        );
    }

    #[test]
    fn effort_override_applies_to_every_step() {
        let steps = plan(Kind::Code, Difficulty::Normal, Some(Effort::Max), &cat(), &Agent::ALL);
        assert!(steps.iter().all(|s| s.effort == Effort::Max && !s.fallback));
        assert_eq!(steps[0].model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(steps[1].model.as_deref(), Some("grok-4.7"));
    }

    #[test]
    fn a_short_order_yields_one_step() {
        let steps = plan(Kind::Code, Difficulty::Easy, None, &cat(), &[Agent::Grok]);
        assert_eq!(steps, vec![step(Agent::Grok, "grok-4.7-build-fast", Effort::Low, "fast", false)]);
    }

    #[test]
    fn names_parse() {
        assert_eq!(Kind::parse("code"), Some(Kind::Code));
        assert_eq!(Kind::parse("security"), Some(Kind::Security));
        assert_eq!(Kind::parse("research"), Some(Kind::Research));
        assert_eq!(Kind::parse("review"), None);
        assert_eq!(Difficulty::parse("easy"), Some(Difficulty::Easy));
        assert_eq!(Difficulty::parse("normal"), Some(Difficulty::Normal));
        assert_eq!(Difficulty::parse("hard"), Some(Difficulty::Hard));
        assert_eq!(Difficulty::parse("extreme"), None);
    }
}
