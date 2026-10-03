//! What the agent remembers about the user.
//!
//! * **Facts** the user shared or the agent learned (`remember`), each with
//!   an id so the user can see and remove them (in Settings, or by asking).
//! * **Habits**: which applications the user opens and when, counted
//!   automatically.
//! * **Recent conversation**, so a new conversation picks up where the
//!   last one left off.
//!
//! Everything is stored as JSON in the agent's private directory and
//! summarised into the agent's prompt ([`Memory::prompt_section`]).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vjson::{Value, object};

use crate::deepgram::Role;

/// Limits that keep the memory (and the prompt built from it) small.
pub const MAX_FACTS: usize = 400;
pub const MAX_FACT_LEN: usize = 300;
pub const MAX_TURNS: usize = 40;
const MAX_TURN_LEN: usize = 600;

/// Kinds of facts.
pub const KINDS: [&str; 3] = ["fact", "preference", "habit"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub id: u64,
    pub text: String,
    /// One of [`KINDS`].
    pub kind: String,
    /// When it was learned (Unix seconds).
    pub created: u64,
}

/// How often and when the user opens an application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppUse {
    pub count: u32,
    /// Last opened (Unix seconds).
    pub last: u64,
    /// Openings by hour of the day (local time).
    pub hours: [u16; 24],
}

/// One turn of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub facts: Vec<Fact>,
    next_id: u64,
    pub apps: BTreeMap<String, AppUse>,
    pub turns: Vec<Turn>,
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Words that say nothing about what a fact is about.
const STOPWORDS: &[&str] = &[
    "the", "and", "what", "who", "where", "when", "which", "how", "why", "you", "your", "are", "does", "did", "about",
    "know", "remember", "tell", "that", "this", "with", "for", "from", "have", "has", "user", "users",
];

/// Lower-case words of at least three letters that carry meaning (for
/// matching).
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

impl Memory {
    pub fn new() -> Memory {
        Memory { next_id: 1, ..Default::default() }
    }

    /// Remembers something. A fact that says the same as an older one
    /// replaces it. Returns its id.
    pub fn remember(&mut self, text: &str, kind: &str, now: u64) -> u64 {
        let text = clip(text, MAX_FACT_LEN);
        let kind = if KINDS.contains(&kind) { kind } else { "fact" };
        let norm = text.to_lowercase();
        if let Some(f) = self.facts.iter_mut().find(|f| {
            let old = f.text.to_lowercase();
            old == norm || (old.len() > 12 && norm.contains(&old)) || (norm.len() > 12 && old.contains(&norm))
        }) {
            f.text = text;
            f.kind = kind.into();
            f.created = now;
            return f.id;
        }
        if self.facts.len() >= MAX_FACTS {
            self.facts.remove(0);
        }
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.facts.push(Fact { id, text, kind: kind.into(), created: now });
        id
    }

    pub fn forget(&mut self, id: u64) -> bool {
        let before = self.facts.len();
        self.facts.retain(|f| f.id != id);
        self.facts.len() != before
    }

    /// Forgets every fact matching `query`; returns what was removed.
    pub fn forget_matching(&mut self, query: &str) -> Vec<Fact> {
        let hits: Vec<u64> = self.search(query, usize::MAX).iter().map(|f| f.id).collect();
        let removed: Vec<Fact> = self.facts.iter().filter(|f| hits.contains(&f.id)).cloned().collect();
        self.facts.retain(|f| !hits.contains(&f.id));
        removed
    }

    pub fn forget_all(&mut self) {
        self.facts.clear();
        self.apps.clear();
        self.turns.clear();
    }

    /// Facts sharing words with `query`, best first (all facts for an
    /// empty query, newest first).
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Fact> {
        let q = words(query);
        if q.is_empty() {
            return self.facts.iter().rev().take(limit).collect();
        }
        let mut scored: Vec<(usize, &Fact)> = self
            .facts
            .iter()
            .filter_map(|f| {
                let w = words(&f.text);
                let score = q
                    .iter()
                    .filter(|qw| w.iter().any(|fw| fw.starts_with(qw.as_str()) || qw.starts_with(fw.as_str())))
                    .count();
                (score > 0).then_some((score, f))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.created.cmp(&a.1.created)));
        scored.into_iter().take(limit).map(|(_, f)| f).collect()
    }

    /// Counts an application being opened at `hour` (0..24, local time).
    pub fn note_app_use(&mut self, app: &str, now: u64, hour: u32) {
        if app.is_empty() || app.len() > 64 {
            return;
        }
        let e = self.apps.entry(app.to_string()).or_default();
        e.count = e.count.saturating_add(1);
        e.last = now;
        e.hours[(hour % 24) as usize] = e.hours[(hour % 24) as usize].saturating_add(1);
    }

    /// The most used applications: (app, count), most first.
    pub fn frequent_apps(&self, n: usize) -> Vec<(&str, u32)> {
        let mut v: Vec<(&str, u32)> = self.apps.iter().map(|(k, u)| (k.as_str(), u.count)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.truncate(n);
        v
    }

    /// Keeps a conversation turn.
    pub fn add_turn(&mut self, role: Role, text: &str, at: u64) {
        let text = clip(text, MAX_TURN_LEN);
        if text.is_empty() {
            return;
        }
        // The same turn reported twice (live text and history) is kept once.
        if self.turns.last().is_some_and(|t| t.role == role && t.text == text) {
            return;
        }
        self.turns.push(Turn { role, text, at });
        let excess = self.turns.len().saturating_sub(MAX_TURNS);
        self.turns.drain(..excess);
    }

    /// The most recent turns within `max_chars`, oldest first, for the
    /// next conversation's context. Turns older than `since` are left out.
    pub fn recent_turns(&self, max_chars: usize, since: u64) -> Vec<(Role, String)> {
        let mut out = Vec::new();
        let mut used = 0;
        for t in self.turns.iter().rev() {
            if t.at < since || used + t.text.len() > max_chars {
                break;
            }
            used += t.text.len();
            out.push((t.role, t.text.clone()));
        }
        out.reverse();
        // A context should not start with the agent's reply to a question
        // that is no longer there.
        while out.first().is_some_and(|(r, _)| *r == Role::Assistant) {
            out.remove(0);
        }
        out
    }

    /// What the agent knows, for its prompt.
    pub fn prompt_section(&self) -> String {
        let mut s = String::new();
        if self.facts.is_empty() {
            s.push_str("You do not know anything about the user yet.\n");
        } else {
            s.push_str("What you know about the user (from earlier conversations):\n");
            for f in self.facts.iter().rev().take(80) {
                s.push_str(&format!(
                    "- {}{}\n",
                    f.text,
                    if f.kind == "fact" { String::new() } else { format!(" ({})", f.kind) }
                ));
            }
        }
        let apps = self.frequent_apps(5);
        if !apps.is_empty() {
            let list: Vec<String> = apps.iter().map(|(a, n)| format!("{a} ({n} times)")).collect();
            s.push_str(&format!("Applications the user opens most: {}.\n", list.join(", ")));
        }
        s
    }

    pub fn to_json(&self) -> Value {
        let facts: Vec<Value> = self
            .facts
            .iter()
            .map(|f| object! { "id" => f.id, "text" => f.text.as_str(), "kind" => f.kind.as_str(), "created" => f.created })
            .collect();
        let mut apps = vjson::Map::new();
        for (k, u) in &self.apps {
            let hours: Vec<Value> = u.hours.iter().map(|&h| Value::from(h)).collect();
            apps.insert(k.as_str(), object! { "count" => u.count, "last" => u.last, "hours" => hours });
        }
        let turns: Vec<Value> = self
            .turns
            .iter()
            .map(|t| object! { "role" => t.role.as_str(), "text" => t.text.as_str(), "at" => t.at })
            .collect();
        object! { "version" => 1, "next_id" => self.next_id, "facts" => facts, "apps" => apps, "turns" => turns }
    }

    /// Reads stored memory; malformed entries are skipped.
    pub fn from_json(text: &str) -> Memory {
        let mut m = Memory::new();
        let Ok(v) = vjson::parse(text) else { return m };
        if let Some(facts) = v["facts"].as_array() {
            for f in facts {
                let (Some(id), Some(t)) = (f["id"].as_u64(), f.str("text")) else { continue };
                if m.facts.len() < MAX_FACTS && !t.trim().is_empty() {
                    let kind = f.str("kind").filter(|k| KINDS.contains(k)).unwrap_or("fact");
                    m.facts.push(Fact {
                        id,
                        text: clip(t, MAX_FACT_LEN),
                        kind: kind.into(),
                        created: f["created"].as_u64().unwrap_or(0),
                    });
                }
            }
        }
        let max_id = m.facts.iter().map(|f| f.id).max().unwrap_or(0);
        m.next_id = v["next_id"].as_u64().unwrap_or(1).max(max_id + 1);
        if let Some(apps) = v["apps"].as_object() {
            for (k, a) in apps.iter() {
                let mut u = AppUse {
                    count: a["count"].as_u64().unwrap_or(0) as u32,
                    last: a["last"].as_u64().unwrap_or(0),
                    hours: [0; 24],
                };
                if let Some(h) = a["hours"].as_array() {
                    for (i, x) in h.iter().take(24).enumerate() {
                        u.hours[i] = x.as_u64().unwrap_or(0).min(u16::MAX as u64) as u16;
                    }
                }
                m.apps.insert(k.to_string(), u);
            }
        }
        if let Some(turns) = v["turns"].as_array() {
            for t in turns.iter().rev().take(MAX_TURNS).rev() {
                if let (Some(role), Some(text)) = (t.str("role").and_then(Role::parse), t.str("text")) {
                    m.turns.push(Turn { role, text: clip(text, MAX_TURN_LEN), at: t["at"].as_u64().unwrap_or(0) });
                }
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn remembers_searches_and_forgets() {
        let mut m = Memory::new();
        let a = m.remember("The user's name is Alex", "fact", 100);
        let b = m.remember("Prefers dark wallpapers", "preference", 101);
        let c = m.remember("Has a dog called Pixel", "fact", 102);
        assert_eq!((a, b, c), (1, 2, 3));
        // Saying the same thing again updates instead of duplicating.
        assert_eq!(m.remember("the user's name is alex", "fact", 200), a);
        assert_eq!(m.facts.len(), 3);
        assert_eq!(m.search("my dog", 5)[0].id, c);
        assert_eq!(m.search("", 2).len(), 2);
        assert!(m.search("zebra", 5).is_empty());
        assert_eq!(m.forget_matching("dog").len(), 1);
        assert!(m.forget(b));
        assert!(!m.forget(b));
        assert_eq!(m.facts.len(), 1);
        assert_eq!(m.remember("New", "weird-kind", 300), 4);
        assert_eq!(m.facts.last().unwrap().kind, "fact");
    }

    #[test]
    fn tracks_habits_and_history() {
        let mut m = Memory::new();
        for _ in 0..3 {
            m.note_app_use("music", 10, 20);
        }
        m.note_app_use("editor", 11, 9);
        assert_eq!(m.frequent_apps(5), alloc::vec![("music", 3), ("editor", 1)]);
        m.add_turn(Role::Assistant, "Welcome back", 5);
        m.add_turn(Role::User, "Play some jazz", 10);
        m.add_turn(Role::User, "Play some jazz", 10);
        m.add_turn(Role::Assistant, "Playing jazz now", 11);
        assert_eq!(m.turns.len(), 3);
        // The context starts with the user, and old turns are left out.
        let ctx = m.recent_turns(1000, 0);
        assert_eq!(ctx[0], (Role::User, "Play some jazz".into()));
        assert_eq!(ctx.len(), 2);
        assert!(m.recent_turns(1000, 50).is_empty());
        for i in 0..100 {
            m.add_turn(Role::User, &alloc::format!("turn {i}"), 20 + i);
        }
        assert_eq!(m.turns.len(), MAX_TURNS);
        let section = m.prompt_section();
        assert!(section.contains("music (3 times)"));
    }

    #[test]
    fn survives_storage() {
        let mut m = Memory::new();
        m.remember("Likes tea", "preference", 1);
        m.note_app_use("photos", 2, 3);
        m.add_turn(Role::User, "hello", 4);
        let back = Memory::from_json(&m.to_json().to_string());
        assert_eq!(back, m);
        assert_eq!(back.clone().remember("Another", "fact", 5), 2);
        assert_eq!(Memory::from_json("{broken"), Memory::new());
        let partial = Memory::from_json(r#"{"facts":[{"id":7,"text":"ok"},{"text":"no id"},{"id":8,"text":"  "}]}"#);
        assert_eq!(partial.facts.len(), 1);
        assert_eq!(partial.clone().remember("next", "fact", 0), 8);
    }
}
