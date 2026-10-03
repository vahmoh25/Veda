//! Serving Settings: the agent's configuration, the Deepgram key, the
//! models and voices to choose from, voice previews, memory and
//! permissions.
//!
//! Only the Settings application gets here (the main loop checks the
//! registry's identity of every connection). Its requests may wait on the
//! network — checking the key, fetching catalogues — so each Settings
//! connection is served on a thread of its own, sharing the agent's state.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vagent::config::{self, Config};
use vagent::deepgram;
use vproto::agent::{
    AgentConfig, AgentError, AgentStatus, MemoryItem, ModelCatalog, ModelChoice, Permission, UiLink, agent,
};
use vproto::audio::{OutputStream, StreamSpec, audio};
use vrt::object::Channel;
use vrt::sync::Mutex;
use vrt::time::Duration;
use vweb::WebError;

use crate::shared::Shared;

const NET_TIMEOUT: Duration = Duration::from_secs(15);
/// What a voice preview says.
const PREVIEW: &str = "Hi, this is how I sound. I'm here whenever you need me.";

/// Serves one Settings connection until it closes.
pub fn spawn(channel: Channel, shared: Arc<Mutex<Shared>>, status: Arc<Mutex<AgentStatus>>) {
    let r = vrt::thread::Builder::new().name("settings").spawn(move || {
        let mut s = Admin { shared, status };
        loop {
            match channel.read() {
                Ok(msg) => {
                    if let Ok(reply) = agent::dispatch(&mut s, msg) {
                        let _ = reply.send(&channel);
                    }
                }
                Err(vabi::Error::ShouldWait) => {
                    let _ = vrt::object::wait_many(
                        &mut [vabi::WaitItem {
                            handle: channel.raw(),
                            signals: vabi::signals::READABLE | vabi::signals::PEER_CLOSED,
                            ..Default::default()
                        }],
                        vabi::DEADLINE_INFINITE,
                    );
                }
                Err(_) => return,
            }
        }
    });
    if let Err(e) = r {
        vrt::println!("cannot serve Settings: {}", e);
    }
}

struct Admin {
    shared: Arc<Mutex<Shared>>,
    status: Arc<Mutex<AgentStatus>>,
}

/// The configuration as Settings sees it (never the key itself).
pub fn to_message(c: &Config, key: Option<&str>) -> AgentConfig {
    AgentConfig {
        enabled: c.enabled,
        name: c.name.clone(),
        listen_model: c.listen_model.clone(),
        think_provider: c.think_provider.clone(),
        think_model: c.think_model.clone(),
        voice: c.voice.clone(),
        speed: c.speed,
        listen_for_name: c.listen_for_name,
        idle_timeout_s: c.idle_timeout_s,
        has_key: key.is_some(),
        key_hint: key.map(config::key_hint).unwrap_or_default(),
    }
}

impl Admin {
    fn key(&self) -> Result<String, AgentError> {
        self.shared.lock().key.clone().ok_or(AgentError::NoKey)
    }

    /// GET with the key; maps failures to agent errors.
    fn get(&self, url: &str) -> Result<String, AgentError> {
        let auth = alloc::format!("Token {}", self.key()?);
        match vweb::http::fetch("GET", url, &[("Authorization", &auth)], &[], NET_TIMEOUT, 4 << 20) {
            Ok(r) if r.status == 401 || r.status == 403 => Err(AgentError::BadKey),
            Ok(r) if (200..300).contains(&r.status) => Ok(r.text()),
            Ok(r) => {
                vrt::println!("Deepgram answered {} for {}", r.status, url);
                Err(AgentError::Network)
            }
            Err(e) => {
                vrt::println!("cannot reach Deepgram: {}", e);
                Err(AgentError::Network)
            }
        }
    }
}

impl agent::Server for Admin {
    fn status(&mut self) -> AgentStatus {
        self.status.lock().clone()
    }

    fn register_app(&mut self, _app: Channel) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn attach_ui(&mut self) -> Result<UiLink, AgentError> {
        Err(AgentError::Denied)
    }

    fn decide(&mut self, _id: u64, _allow: bool, _always: bool) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn wake(&mut self) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn sleep(&mut self) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn set_window_open(&mut self, _open: bool) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn set_muted(&mut self, _muted: bool) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn config(&mut self) -> Result<AgentConfig, AgentError> {
        let s = self.shared.lock();
        Ok(to_message(&s.config, s.key.as_deref()))
    }

    fn set_config(&mut self, c: AgentConfig) -> Result<(), AgentError> {
        let mut s = self.shared.lock();
        let new = Config {
            enabled: c.enabled,
            name: c.name.trim().to_string(),
            listen_model: c.listen_model,
            think_provider: c.think_provider,
            think_model: c.think_model,
            voice: c.voice,
            speed: c.speed,
            listen_for_name: c.listen_for_name,
            idle_timeout_s: c.idle_timeout_s,
            endpoint: s.config.endpoint.clone(),
        };
        if let Err(e) = new.validate() {
            vrt::println!("refused settings: {}", e);
            return Err(AgentError::BadConfig);
        }
        s.config = new;
        s.save_config();
        vrt::println!("settings changed");
        Ok(())
    }

    fn set_api_key(&mut self, key: String) -> Result<(), AgentError> {
        let key = key.trim().to_string();
        let mut s = self.shared.lock();
        if key.is_empty() {
            s.key = None;
            s.save_key();
            vrt::println!("Deepgram key removed");
            return Ok(());
        }
        if !config::valid_key(&key) {
            return Err(AgentError::BadConfig);
        }
        s.key = Some(key);
        s.save_key();
        vrt::println!("Deepgram key stored");
        Ok(())
    }

    fn check_key(&mut self) -> Result<String, AgentError> {
        let text = self.get(deepgram::PROJECTS_URL)?;
        let v = vjson::parse(&text).unwrap_or_default();
        let name = v.pointer("/projects/0/name").and_then(|n| n.as_str()).unwrap_or("your project");
        Ok(alloc::format!("The key works ({name})."))
    }

    fn catalog(&mut self) -> Result<ModelCatalog, AgentError> {
        let think = deepgram::parse_think_models(&self.get(deepgram::THINK_MODELS_URL)?);
        let voices = deepgram::parse_voices(&self.get(deepgram::MODELS_URL)?);
        Ok(ModelCatalog {
            think: think
                .into_iter()
                .map(|m| ModelChoice { id: m.id, name: m.name, provider: m.provider, detail: m.tier.to_string() })
                .collect(),
            voices: voices
                .into_iter()
                .map(|v| ModelChoice {
                    id: v.id,
                    name: v.name,
                    provider: String::new(),
                    detail: if v.accent.is_empty() {
                        v.description
                    } else {
                        alloc::format!("{}, {}", v.accent, v.description)
                    },
                })
                .collect(),
            listen: deepgram::LISTEN_MODELS
                .iter()
                .map(|(id, name, detail)| ModelChoice {
                    id: id.to_string(),
                    name: name.to_string(),
                    provider: String::new(),
                    detail: detail.to_string(),
                })
                .collect(),
        })
    }

    fn preview_voice(&mut self, voice: String) -> Result<(), AgentError> {
        if voice.is_empty() || voice.len() > 80 || !voice.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(AgentError::BadConfig);
        }
        let auth = alloc::format!("Token {}", self.key()?);
        let body = deepgram::speak_body(PREVIEW);
        let r = vweb::http::fetch(
            "POST",
            &deepgram::speak_url(&voice),
            &[("Authorization", &auth), ("Content-Type", "application/json")],
            body.as_bytes(),
            NET_TIMEOUT,
            8 << 20,
        )
        .map_err(|e: WebError| {
            vrt::println!("voice preview failed: {}", e);
            AgentError::Network
        })?;
        match r.status {
            200..=299 => {}
            401 | 403 => return Err(AgentError::BadKey),
            _ => return Err(AgentError::Network),
        }
        let pcm: Vec<i16> = r.body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        play(&pcm);
        Ok(())
    }

    fn memories(&mut self) -> Result<Vec<MemoryItem>, AgentError> {
        let s = self.shared.lock();
        Ok(s.memory
            .facts
            .iter()
            .rev()
            .map(|f| MemoryItem { id: f.id, text: f.text.clone(), kind: f.kind.clone(), created: f.created })
            .collect())
    }

    fn forget(&mut self, id: u64) -> Result<(), AgentError> {
        let mut s = self.shared.lock();
        if !s.memory.forget(id) {
            return Err(AgentError::NotFound);
        }
        s.save_memory();
        Ok(())
    }

    fn forget_all(&mut self) -> Result<(), AgentError> {
        let mut s = self.shared.lock();
        s.memory.forget_all();
        s.save_memory();
        vrt::println!("memory erased");
        Ok(())
    }

    fn permissions(&mut self) -> Result<Vec<Permission>, AgentError> {
        let s = self.shared.lock();
        Ok(s.permissions.list().into_iter().map(|(key, label)| Permission { key, label }).collect())
    }

    fn revoke(&mut self, key: String) -> Result<(), AgentError> {
        let mut s = self.shared.lock();
        if !s.permissions.revoke(&key) {
            return Err(AgentError::NotFound);
        }
        s.save_permissions();
        Ok(())
    }
}

/// Plays 24 kHz mono PCM and waits until it has been heard.
fn play(pcm: &[i16]) {
    let Some(client) = vproto::connect(audio::NAME).ok().map(audio::Client::new) else { return };
    let spec = StreamSpec {
        rate: deepgram::OUTPUT_RATE,
        channels: 1,
        buffer_frames: deepgram::OUTPUT_RATE * 2,
        notify_frames: 0,
        name: "Voice preview".into(),
        paused: false,
        volume: 1.0,
    };
    let Ok(stream) = OutputStream::open(&client, spec) else { return };
    let mut pos = 0;
    let end = vrt::time::deadline_after(Duration::from_secs(20));
    while pos < pcm.len() && vrt::time::now_ns() < end {
        stream.wait_writable(2400, vrt::time::deadline_after(Duration::from_millis(200)));
        pos += stream.write(&pcm[pos..]);
    }
    while stream.played_now() < stream.written() && vrt::time::now_ns() < end {
        vrt::time::sleep(Duration::from_millis(50));
    }
    let _ = client.close(stream.id);
}
