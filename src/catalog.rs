//! Which model of which vendor is good at what.
//!
//! Claude picks an agent and a model by capability instead of memorising
//! vendor model names: `ask_codex` / `ask_grok` accept a `capability` tag,
//! and `list_models` shows the whole table. The built-in table was taken
//! from the vendors' own catalogues on 2026-10-27 (`~/.codex/models_cache.json`
//! for codex-cli 0.164.0, `grok models` for grok 1.0.67); descriptions are
//! the vendors' words. `[[catalog]]` entries in `~/.baton/config.toml` add to
//! or replace rows, so a new model never needs a baton release.

use serde::{Deserialize, Serialize};

use crate::agents::Agent;

/// One model of one vendor CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub agent: Agent,
    pub model: String,
    /// Capability tags from [`TAGS`]; the first row carrying a tag wins.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// Capability vocabulary: (tag, what it means). Kept short on purpose so
/// the model can choose without reading a manual.
pub const TAGS: &[(&str, &str)] = &[
    ("security", "defensive cybersecurity: vulnerability review, threat modelling, hardening, secure code review"),
    ("frontier", "strongest reasoning for the hardest design or debugging problems"),
    ("workhorse", "everyday coding and review at a good quality/speed balance"),
    ("fast", "quick, cheap answers for easy or mechanical tasks"),
    ("long_context", "largest context window for reading a lot of code at once"),
    ("web_search", "can search the web for current docs, releases and news"),
    ("legacy", "previous generations; only for comparison against a newer model"),
];

pub fn known_tag(tag: &str) -> bool {
    TAGS.iter().any(|(t, _)| *t == tag)
}

fn row(agent: Agent, model: &str, tags: &[&str], note: &str) -> Entry {
    Entry {
        agent,
        model: model.to_string(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        note: note.to_string(),
    }
}

/// Vendor-published models, in preference order within each agent.
pub fn builtin() -> Vec<Entry> {
    use Agent::{Codex, Grok};
    vec![
        row(Codex, "gpt-daybreak-blue-latest", &["security"], "Daybreak Blue: latest frontier agentic coding model for broad defensive cybersecurity work"),
        row(Codex, "gpt-6-astra", &["frontier"], "GPT-6-Astra: frontier intelligence for the most demanding work (272k context)"),
        row(Codex, "gpt-6.1-sol", &["workhorse", "long_context"], "GPT-6.1-Sol: latest workhorse for coding and everyday work (500k context, codex default)"),
        row(Codex, "gpt-6-luna", &["fast"], "GPT-6-Luna: fast and affordable model for easier tasks"),
        row(Codex, "gpt-6-sol", &["legacy"], "GPT-6-Sol: previous generation workhorse"),
        row(Codex, "gpt-5.6-sol", &["legacy"], "GPT-5.6-Sol: older generation workhorse"),
        row(Codex, "gpt-5.6-terra", &["legacy"], "GPT-5.6-Terra: older balanced model"),
        row(Codex, "gpt-5.6-luna", &["legacy"], "GPT-5.6-Luna: older fast model"),
        row(Grok, "grok-4.7", &["workhorse", "frontier", "web_search"], "grok-4.7: current default, has web search"),
        row(Grok, "grok-4.7-build-fast", &["fast", "web_search"], "grok-4.7-build-fast: faster variant of 4.7"),
        row(Grok, "grok-4.6", &["legacy", "web_search"], "grok-4.6: previous generation"),
        row(Grok, "grok-4.5", &["legacy", "web_search"], "grok-4.5: older generation"),
    ]
}

#[derive(Debug, Clone)]
pub struct Catalog {
    entries: Vec<Entry>,
}

impl Catalog {
    /// Built-in rows, then the user's: a user row with the same agent and
    /// model replaces the built-in one in place, new rows go to the end.
    pub fn with_overrides(extra: &[Entry]) -> Catalog {
        let mut entries = builtin();
        for e in extra {
            match entries.iter_mut().find(|b| b.agent == e.agent && b.model == e.model) {
                Some(b) => *b = e.clone(),
                None => entries.push(e.clone()),
            }
        }
        Catalog { entries }
    }

    pub fn for_agent(&self, agent: Agent) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.agent == agent)
    }

    /// First model of `agent` tagged `tag`.
    pub fn resolve(&self, agent: Agent, tag: &str) -> Option<&Entry> {
        self.for_agent(agent).find(|e| e.tags.iter().any(|t| t == tag))
    }

    /// Tags that resolve to something for `agent`.
    pub fn tags_for(&self, agent: Agent) -> Vec<&str> {
        TAGS.iter().map(|(t, _)| *t).filter(|t| self.resolve(agent, t).is_some()).collect()
    }

    /// One line per model, for a tool description.
    pub fn summary(&self, agent: Agent) -> String {
        self.for_agent(agent)
            .map(|e| format!("{} [{}]: {}", e.model, e.tags.join(", "), e.note))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full table for `list_models`.
    pub fn render(&self) -> String {
        let mut out = String::from("Capability tags:\n");
        for (t, d) in TAGS {
            out.push_str(&format!("  {t}: {d}\n"));
        }
        for agent in Agent::ALL {
            out.push_str(&format!("\n{} (tool ask_{}):\n", agent.name(), agent.name()));
            for e in self.for_agent(agent) {
                out.push_str(&format!("  {} [{}]\n      {}\n", e.model, e.tags.join(", "), e.note));
            }
        }
        out.push_str("\nPass `model` to pin one, or `capability` to let baton pick the first model with that tag. `effort` (low|medium|high|xhigh|max) sets reasoning depth on either CLI.\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tags_are_known_and_models_valid() {
        for e in builtin() {
            assert!(crate::agents::valid_model(&e.model), "{}", e.model);
            for t in &e.tags {
                assert!(known_tag(t), "{} has unknown tag {t}", e.model);
            }
        }
    }

    #[test]
    fn security_routes_to_daybreak() {
        let c = Catalog::with_overrides(&[]);
        assert_eq!(c.resolve(Agent::Codex, "security").unwrap().model, "gpt-daybreak-blue-latest");
        assert_eq!(c.resolve(Agent::Codex, "frontier").unwrap().model, "gpt-6-astra");
        assert_eq!(c.resolve(Agent::Grok, "fast").unwrap().model, "grok-4.7-build-fast");
        assert!(c.resolve(Agent::Grok, "security").is_none());
        assert!(c.tags_for(Agent::Grok).contains(&"web_search"));
        assert!(!c.tags_for(Agent::Codex).contains(&"web_search"));
    }

    #[test]
    fn user_rows_replace_or_append() {
        let user = vec![
            row(Agent::Grok, "grok-4.7", &["frontier"], "mine"),
            row(Agent::Grok, "grok-5", &["security"], "new"),
        ];
        let c = Catalog::with_overrides(&user);
        let g47 = c.for_agent(Agent::Grok).find(|e| e.model == "grok-4.7").unwrap();
        assert_eq!(g47.note, "mine");
        assert_eq!(c.resolve(Agent::Grok, "security").unwrap().model, "grok-5");
        assert_eq!(c.for_agent(Agent::Grok).count(), 5);
        assert!(c.render().contains("grok-5"));
        assert!(c.summary(Agent::Codex).lines().count() >= 4);
    }
}
