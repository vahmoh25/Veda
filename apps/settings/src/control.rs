//! Settings for the voice agent: which page is shown, and the agent's own
//! voice, speaking rate, name and language model — so the user can say
//! "talk a little slower" and the change is made where it belongs, in the
//! system settings. The Deepgram key is never offered.
//!
//! The agent's settings change through the agent service like the Agent
//! page's do (only Settings may change them); voices and models come from
//! Deepgram's catalogue.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vproto::agent::{AgentConfig, AgentError, ModelCatalog, ModelChoice, agent};
use vui::agent::{self as api, Action, AppAgentInfo, Risk, Value, arg_f64, arg_opt_str, arg_str, object};

use crate::{SECTIONS, Section, Settings};

/// Fetching Deepgram's catalogue can take a while on a slow network.
const AGENT_TIMEOUT_NS: u64 = 20_000_000_000;

const PAGES: [&str; 6] = ["personalization", "agent", "network", "display", "system", "about"];

pub fn info() -> AppAgentInfo {
    api::info(
        "System settings: the wallpaper, the agent's voice, name and language model, network, display, system \
         details and version.",
        vec![
            Action::new("show_page", "Shows a page of Settings").choice("page", "The page", true, &PAGES).build(),
            Action::new("set_agent_voice", "Changes how the agent (you) sounds: its voice, its speaking rate, or both")
                .param("voice", "string", "A voice name from list_voices, such as Helena or Orion", false)
                .param("speed", "number", "Speaking rate from 0.7 (slow) to 1.5 (fast); 1 is normal", false)
                .build(),
            Action::new("list_voices", "Lists the voices the agent can speak with")
                .param("accent", "string", "Only voices with this accent, such as British or Australian", false)
                .build(),
            Action::new("list_models", "Lists the language models the agent can think with, and their price tier")
                .build(),
            Action::new(
                "set_agent_model",
                "Changes the language model the agent thinks with (Advanced models cost more per minute)",
            )
            .param("model", "string", "A model's name or id from list_models", true)
            .risk(Risk::Sensitive)
            .build(),
            Action::new("rename_agent", "Changes the agent's name, which it also answers to")
                .param("name", "string", "The new name (1 to 24 characters)", true)
                .risk(Risk::Sensitive)
                .build(),
        ],
    )
}

fn section_name(s: Section) -> &'static str {
    match s {
        Section::Personalization => "personalization",
        Section::Agent => "agent",
        Section::Network => "network",
        Section::Display => "display",
        Section::System => "system",
        Section::About => "about",
    }
}

fn client() -> Result<agent::Client, String> {
    let c = vproto::connect(agent::NAME).map(agent::Client::new).map_err(|_| "the agent service is not running")?;
    c.set_timeout(AGENT_TIMEOUT_NS);
    Ok(c)
}

fn refused(e: AgentError) -> String {
    match e {
        AgentError::BadConfig => "that setting is not valid".into(),
        e => e.to_string(),
    }
}

fn config(c: &agent::Client) -> Result<AgentConfig, String> {
    c.config().map_err(|_| String::from("the agent service did not answer"))?.map_err(refused)
}

fn catalog(c: &agent::Client) -> Result<ModelCatalog, String> {
    c.catalog().map_err(|_| String::from("the agent service did not answer"))?.map_err(refused)
}

fn save(c: &agent::Client, cfg: AgentConfig) -> Result<(), String> {
    c.set_config(cfg).map_err(|_| String::from("the agent service did not answer"))?.map_err(refused)
}

/// The choice called `query` (its name or id, ignoring case).
fn find<'a>(choices: &'a [ModelChoice], query: &str) -> Option<&'a ModelChoice> {
    let q = query.trim().to_lowercase();
    choices
        .iter()
        .find(|c| c.id.to_lowercase() == q || c.name.to_lowercase() == q)
        .or_else(|| choices.iter().find(|c| c.id.to_lowercase().contains(&q)))
}

fn voice_value(v: &ModelChoice) -> Value {
    object! { "name" => v.name.as_str(), "about" => v.detail.as_str() }
}

pub fn state(s: &Settings) -> Value {
    let agent = client().ok().and_then(|c| config(&c).ok()).map(|c| {
        object! {
            "name" => c.name.as_str(),
            "voice" => c.voice.as_str(),
            "speaking_rate" => c.speed,
            "language_model" => c.think_model.as_str(),
            "speech_recognition" => c.listen_model.as_str(),
            "answers_to_its_name" => c.listen_for_name,
            "enabled" => c.enabled,
            "has_deepgram_key" => c.has_key,
        }
    });
    object! {
        "page" => section_name(s.section),
        "pages" => Value::from(PAGES.iter().map(|p| Value::from(*p)).collect::<Vec<Value>>()),
        "wallpaper" => (!s.current.is_empty()).then(|| Settings::title_of(&s.current)),
        "screen" => s.screen.map(|i| format!("{} x {}", i.width, i.height)),
        "agent" => agent,
    }
}

impl Settings {
    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        match action {
            "show_page" => {
                let page = arg_str(args, "page")?.trim().to_lowercase();
                let section = SECTIONS
                    .iter()
                    .map(|s| s.0)
                    .find(|&s| section_name(s) == page)
                    .ok_or_else(|| format!("Settings has no page called {page}; it has {}", PAGES.join(", ")))?;
                self.section = section;
                Ok(object! { "showing" => section_name(section) })
            }
            "set_agent_voice" => {
                let voice = arg_opt_str(args, "voice");
                let speed = match args.get("speed") {
                    Some(_) => Some(arg_f64(args, "speed")? as f32),
                    None => None,
                };
                if voice.is_none() && speed.is_none() {
                    return Err("say which voice or speaking rate".into());
                }
                let c = client()?;
                let mut cfg = config(&c)?;
                if let Some(v) = voice {
                    let voices = catalog(&c)?.voices;
                    let found = find(&voices, v).or_else(|| find(&voices, &format!("aura-2-{v}")));
                    let found = found.ok_or_else(|| {
                        let names: Vec<&str> = voices.iter().map(|v| v.name.as_str()).collect();
                        format!("there is no voice called {v}; voices: {}", names.join(", "))
                    })?;
                    cfg.voice = found.id.clone();
                }
                if let Some(sp) = speed {
                    if !(0.7..=1.5).contains(&sp) {
                        return Err("the speaking rate goes from 0.7 to 1.5".into());
                    }
                    cfg.speed = (sp * 20.0 + 0.5) as i32 as f32 / 20.0;
                }
                let (voice, speed) = (cfg.voice.clone(), cfg.speed);
                save(&c, cfg)?;
                self.reload_agent_page();
                Ok(object! { "voice" => voice, "speaking_rate" => speed })
            }
            "list_voices" => {
                let voices = catalog(&client()?)?.voices;
                let accent = arg_opt_str(args, "accent").map(str::to_lowercase);
                let list: Vec<Value> = voices
                    .iter()
                    .filter(|v| accent.as_ref().is_none_or(|a| v.detail.to_lowercase().contains(a.as_str())))
                    .take(60)
                    .map(voice_value)
                    .collect();
                Ok(object! { "voices" => list, "of" => voices.len() })
            }
            "list_models" => {
                let models = catalog(&client()?)?.think;
                let list: Vec<Value> = models
                    .iter()
                    .map(|m| object! { "name" => m.name.as_str(), "id" => m.id.as_str(), "tier" => m.detail.as_str() })
                    .collect();
                Ok(object! { "models" => list })
            }
            "set_agent_model" => {
                let q = arg_str(args, "model")?;
                let c = client()?;
                let models = catalog(&c)?.think;
                let m = find(&models, q).ok_or_else(|| format!("there is no language model called {q}"))?;
                let mut cfg = config(&c)?;
                cfg.think_provider = m.provider.clone();
                cfg.think_model = m.id.clone();
                let (name, tier) = (m.name.clone(), m.detail.clone());
                save(&c, cfg)?;
                self.reload_agent_page();
                Ok(object! { "model" => name, "tier" => tier })
            }
            "rename_agent" => {
                let name = arg_str(args, "name")?.trim().to_string();
                if name.is_empty() || name.chars().count() > 24 {
                    return Err("the name must be 1 to 24 characters".into());
                }
                let c = client()?;
                let mut cfg = config(&c)?;
                cfg.name = name.clone();
                save(&c, cfg)?;
                self.reload_agent_page();
                Ok(object! { "name" => name })
            }
            other => Err(format!("Settings has no action called {other}")),
        }
    }

    /// The Agent page shows the agent's settings as they are now.
    fn reload_agent_page(&mut self) {
        if let Some(p) = &mut self.agent {
            p.reload();
        }
    }
}
