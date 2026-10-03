//! The agent's configuration: what it is called, which Deepgram models it
//! listens, thinks and speaks with, and how it behaves. Stored as JSON in
//! the agent's private directory; the Deepgram key is kept apart from it.

use alloc::string::{String, ToString};

use vjson::{Value, object};

/// Defaults: Deepgram's turn-aware recognition, a fast language model of
/// the lower price tier, and a warm voice.
pub const DEFAULT_NAME: &str = "Vera";
pub const DEFAULT_LISTEN: &str = "flux-general-en";
pub const DEFAULT_PROVIDER: &str = "open_ai";
pub const DEFAULT_THINK: &str = "gpt-4.1-mini";
pub const DEFAULT_VOICE: &str = "aura-2-helena-en";
pub const DEFAULT_IDLE_S: u32 = 40;

/// The agent's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// What the agent is called and answers to.
    pub name: String,
    pub listen_model: String,
    pub think_provider: String,
    pub think_model: String,
    pub voice: String,
    /// Speaking rate, 0.7 ..= 1.5.
    pub speed: f32,
    /// Listen for the agent's name while asleep.
    pub listen_for_name: bool,
    /// Seconds of silence that end a conversation.
    pub idle_timeout_s: u32,
    /// Another Voice Agent endpoint (a local simulator in tests). Only the
    /// kernel command line sets it.
    pub endpoint: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            enabled: true,
            name: DEFAULT_NAME.into(),
            listen_model: DEFAULT_LISTEN.into(),
            think_provider: DEFAULT_PROVIDER.into(),
            think_model: DEFAULT_THINK.into(),
            voice: DEFAULT_VOICE.into(),
            speed: 1.0,
            listen_for_name: true,
            idle_timeout_s: DEFAULT_IDLE_S,
            endpoint: None,
        }
    }
}

/// Why a setting was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// Model and voice ids are short ASCII words.
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
}

impl Config {
    /// Checks a configuration from Settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 24 || name.chars().any(|c| c.is_control()) {
            return Err(ConfigError("the name must be 1 to 24 characters"));
        }
        if ![&self.listen_model, &self.think_provider, &self.think_model, &self.voice].iter().all(|s| valid_id(s)) {
            return Err(ConfigError("invalid model or voice"));
        }
        if !crate::deepgram::managed_provider(&self.think_provider) {
            return Err(ConfigError("Deepgram does not run that language model provider"));
        }
        if !(0.7..=1.5).contains(&self.speed) {
            return Err(ConfigError("the speaking rate must be between 0.7 and 1.5"));
        }
        if !(10..=600).contains(&self.idle_timeout_s) {
            return Err(ConfigError("the conversation timeout must be between 10 and 600 seconds"));
        }
        Ok(())
    }

    /// The settings as stored.
    pub fn to_json(&self) -> Value {
        object! {
            "enabled" => self.enabled,
            "name" => self.name.as_str(),
            "listen_model" => self.listen_model.as_str(),
            "think_provider" => self.think_provider.as_str(),
            "think_model" => self.think_model.as_str(),
            "voice" => self.voice.as_str(),
            "speed" => self.speed,
            "listen_for_name" => self.listen_for_name,
            "idle_timeout_s" => self.idle_timeout_s,
        }
    }

    /// Reads stored settings; anything missing or invalid keeps its
    /// default.
    pub fn from_json(text: &str) -> Config {
        let d = Config::default();
        let Ok(v) = vjson::parse(text) else { return d };
        let s = |k: &str, def: &str| v.str(k).filter(|s| !s.is_empty()).unwrap_or(def).to_string();
        let c = Config {
            enabled: v["enabled"].as_bool().unwrap_or(d.enabled),
            name: s("name", &d.name),
            listen_model: s("listen_model", &d.listen_model),
            think_provider: s("think_provider", &d.think_provider),
            think_model: s("think_model", &d.think_model),
            voice: s("voice", &d.voice),
            speed: v["speed"].as_f64().map(|f| f as f32).unwrap_or(d.speed),
            listen_for_name: v["listen_for_name"].as_bool().unwrap_or(d.listen_for_name),
            idle_timeout_s: v["idle_timeout_s"].as_u64().map(|n| n.min(600) as u32).unwrap_or(d.idle_timeout_s),
            endpoint: None,
        };
        if c.validate().is_ok() { c } else { d }
    }
}

/// How the key is shown: its last four characters.
pub fn key_hint(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<String>().chars().rev().collect();
    if key.is_empty() { String::new() } else { alloc::format!("\u{2026}{tail}") }
}

/// A plausible Deepgram key: 20 to 128 visible ASCII characters.
pub fn valid_key(key: &str) -> bool {
    (20..=128).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn round_trips_and_validates() {
        let mut c = Config::default();
        assert!(c.validate().is_ok());
        c.name = "Sam".into();
        c.speed = 1.2;
        c.idle_timeout_s = 90;
        let back = Config::from_json(&c.to_json().to_string());
        assert_eq!(back, c);
        assert_eq!(Config::from_json("garbage"), Config::default());
        assert_eq!(Config::from_json(r#"{"name":""}"#).name, DEFAULT_NAME);
        // An invalid stored value falls back to the defaults.
        assert_eq!(Config::from_json(r#"{"speed":9}"#), Config::default());
        for bad in [
            Config { name: "".into(), ..Config::default() },
            Config { think_provider: "groq".into(), ..Config::default() },
            Config { voice: "bad voice".into(), ..Config::default() },
            Config { idle_timeout_s: 1, ..Config::default() },
        ] {
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn hides_the_key() {
        assert_eq!(key_hint("0123456789abcdefwxyz"), "\u{2026}wxyz");
        assert_eq!(key_hint(""), "");
        assert!(valid_key("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_key("short"));
        assert!(!valid_key("has a space in it 0123456789"));
    }
}
