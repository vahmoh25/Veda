//! Scripted, headless runs of Vindows in QEMU.
//!
//! Automation scripts drive the virtual machine through QMP and inspect its
//! serial console. They power `cargo xtask shot` and the integration tests.
//! One command per line; `#` starts a comment:
//!
//! ```text
//! wait-serial "desktop ready" 60   # wait for a log line (timeout in s)
//! wait-serial-count "joined" 3 60  # wait until the text has appeared 3 times
//! wait 2                           # sleep
//! shot target/vindows/desktop.png  # save a screenshot
//! move 0.5 0.5                     # move the pointer (fractions of screen)
//! click 0.1 0.97 [left|right]      # move + press + release
//! drag 0.3 0.3 0.6 0.6             # press at A, move to B, release
//! mouse-down 0.3 0.3 / mouse-up      # press (at a point) and release separately
//! key ctrl-alt-t                   # press a key combination (QEMU qcodes)
//! key-down alt / key-up alt         # hold or release a single key
//! type "hello world"               # type ASCII text
//! type-env DEEPGRAM_API_KEY        # type an environment variable (a secret: in neither script nor log)
//! expect-serial "PASS"             # fail unless the log contains the text
//! reject-serial "panic"            # fail if the log contains the text
//! fail-on "PANIC"                  # abort later waits as soon as the text appears
//! reset                            # reboot (disks are kept); later waits see only new output
//! checkpoint                       # later waits and expects see only output from now on
//! link wired off                   # unplug (off) or plug in (on) the wired card's cable
//! boot-cmdline "run=about"        # extra kernel command line, applied before boot
//! double-click 0.05 0.44           # two quick clicks
//! net wifi                         # network for this run (wifi, both, ethernet, none), applied before boot
//! nic e1000                        # QEMU model of the wired card for this run, applied before boot
//! sound ac97                       # QEMU's sound card for this run (virtio or ac97), applied before boot
//! audio host                       # the host's loudspeakers and microphone instead of a WAV file (echo on real hardware)
//! expect-audio                     # fail unless the recorded sound output holds more than silence
//! requires qemu                    # only for QEMU (or `virtualbox`); `test` skips it elsewhere
//! air "ap home off"                # send a command to the Wi-Fi simulator (fails on an error)
//! air-expect "list" "1 joined"     # fail unless the simulator's answer contains the text
//! air-wait "list" "1 joined" 60    # wait until it does (timeout in s)
//! say "open the text editor"      # speak into the test microphone (Deepgram TTS, cached) and wait until said
//! say-async "stop"                 # the same without waiting (to talk over the agent)
//! mic-wav tests/audio/hello.wav    # play a WAV file into the microphone and wait
//! mic-silence 2                    # queue seconds of silence
//! mic-tone 1.5 [6000]              # queue seconds of a voice-like buzz (peak amplitude), for voice detection
//! mic-echo -6 250                  # the machine's sound output comes back into the microphone (gain dB, delay ms)
//! mic-wait 30                      # wait until everything queued has been heard
//! agent-connected 120              # wait until the agent opened a conversation with the simulator
//! agent-call open_app '{"app":"editor"}'  # the simulated model calls a function; waits for the result
//! agent-result "opened"            # fail unless the last function result contains the text
//! agent-mark                       # later agent-expect looks only at messages from now on
//! agent-expect "InjectUserMessage" 30  # wait until the agent sends a message containing the text
//! agent-speak 3                    # the simulated agent talks for 3 s (a tone)
//! agent-interrupt                  # the simulated recogniser hears the user start speaking
//! agent-send '{"type":"..."}'       # any message from the simulated service
//! agent-audio 3200                 # fail unless the agent streamed at least this many bytes of microphone audio
//! agent-asleep                     # boot with the agent asleep (no conversation at start)
//! agent-hear "Hey Vera, hello" 30  # the simulated recogniser hears this, once the agent streams speech to it
//! agent-listens 1                  # fail unless the agent opened exactly this many recognition streams
//! agent-mic-quiet -60              # fail if the agent sent microphone audio this loud (dBFS) since agent-mark
//! agent-mic-heard -30              # fail unless it sent microphone audio at least this loud since agent-mark
//! ```
//!
//! Scripts that use the microphone commands boot with `testmic`, which
//! connects back to the host (see `mic.rs`).

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::agentsim::AgentSim;
use crate::airsim::AirSim;
use crate::mic::{self, MicServer};
use crate::qemu::{self, NetMode, QemuInstall, VmConfig};
use crate::qmp::Qmp;
use crate::util::{self, Result};
use crate::vboxctl;

/// Where a script runs.
pub enum Hypervisor<'a> {
    Qemu(&'a QemuInstall),
    /// VirtualBox, with the screen resolution the image boots with.
    VirtualBox {
        resolution: &'a str,
    },
}

/// The machine a script drives.
pub enum Machine {
    Qemu { child: Child, qmp: Qmp },
    VirtualBox(vboxctl::Control),
}

impl Machine {
    pub fn screenshot(&mut self, path: &Path) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.screenshot(path),
            Machine::VirtualBox(c) => c.screenshot(path),
        }
    }

    pub fn move_mouse(&mut self, fx: f64, fy: f64) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.move_mouse(fx, fy),
            Machine::VirtualBox(c) => c.move_mouse(fx, fy),
        }
    }

    pub fn mouse_button(&mut self, button: &str, down: bool) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.mouse_button(button, down),
            Machine::VirtualBox(c) => c.mouse_button(button, down),
        }
    }

    pub fn send_keys(&mut self, combo: &str) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.send_keys(combo),
            Machine::VirtualBox(c) => c.send_keys(combo),
        }
    }

    pub fn key_event(&mut self, key: &str, down: bool) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.key_event(key, down),
            Machine::VirtualBox(c) => c.key_event(key, down),
        }
    }

    pub fn type_text(&mut self, text: &str) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.type_text(text),
            Machine::VirtualBox(c) => c.type_text(text),
        }
    }

    /// Plugs in or unplugs the wired card's cable.
    pub fn set_wired_link(&mut self, up: bool) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => {
                let name = qemu::WIRED_NIC_ID;
                qmp.execute("set_link", &format!("{{\"name\":\"{name}\",\"up\":{up}}}")).map(|_| ())
            }
            Machine::VirtualBox(c) => c.vbox().set_link(up),
        }
    }

    fn reset(&mut self) -> Result {
        match self {
            Machine::Qemu { qmp, .. } => qmp.execute("system_reset", "{}").map(|_| ()),
            Machine::VirtualBox(c) => c.reset(),
        }
    }

    /// Why the machine stopped, if it did.
    fn stopped(&mut self) -> Option<String> {
        match self {
            Machine::Qemu { child, .. } => match child.try_wait() {
                Ok(Some(status)) => Some(format!("QEMU exited ({status})")),
                _ => None,
            },
            Machine::VirtualBox(c) => (!c.vbox().is_running()).then(|| "the VirtualBox machine stopped".into()),
        }
    }

    fn finish(&mut self) {
        match self {
            Machine::Qemu { child, qmp } => {
                qmp.quit();
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(5) {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
            }
            Machine::VirtualBox(c) => c.finish(),
        }
    }
}

/// A running headless VM under script control.
pub struct Session {
    pub m: Machine,
    pub serial_log: PathBuf,
    /// Serial log patterns that abort any wait immediately.
    pub fail_patterns: Vec<String>,
    /// Byte offset in the serial log where `wait-serial` and `expect-serial`
    /// start looking (moved past the output of earlier boots by `reset`).
    since: usize,
    /// The Wi-Fi simulator, when the machine has the virtual radio.
    pub sim: Option<AirSim>,
}

impl Session {
    pub fn start(hv: &Hypervisor, disk: &Path, mut vm: VmConfig) -> Result<Self> {
        let out = util::out_dir();
        let serial_log = out.join("serial.log");
        let _ = std::fs::remove_file(&serial_log);
        // Kernel panics print "PANIC", user-space panics "panicked at".
        let fail_patterns = vec!["PANIC".into(), "panicked at".into()];
        let install = match hv {
            Hypervisor::Qemu(install) => *install,
            Hypervisor::VirtualBox { resolution } => {
                let vbox = crate::vbox::VBox::locate()?;
                vbox.configure(&vm, disk, vm.home_disk.as_deref(), &serial_log, resolution, None)?;
                vbox.start(true)?;
                let (w, h) = resolution
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .unwrap_or((1280, 800));
                let m = Machine::VirtualBox(vboxctl::Control::new(vbox, (w, h)));
                return Ok(Session { m, serial_log, fail_patterns, since: 0, sim: None });
            }
        };
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            l.local_addr().map_err(|e| e.to_string())?.port()
        };
        vm.display = false;
        vm.serial_file = Some(serial_log.clone());
        vm.qmp_port = Some(port);
        vm.debug_exit = true;
        let sim = if vm.net.wireless() {
            let sim = AirSim::start(&crate::airsim::build()?, &out.join("airsim.log"))?;
            vm.wifi = Some(sim.ports);
            Some(sim)
        } else {
            None
        };
        let vars = qemu::vars_file(install)?;
        let mut cmd = qemu::command(install, disk, &vars, &vm);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
        let child = cmd.spawn().map_err(|e| format!("starting QEMU: {e}"))?;
        let qmp = Qmp::connect(port, Duration::from_secs(20))?;
        Ok(Session { m: Machine::Qemu { child, qmp }, serial_log, fail_patterns, since: 0, sim })
    }

    fn serial_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.serial_log).unwrap_or_default()
    }

    /// The whole serial log.
    pub fn serial(&self) -> String {
        String::from_utf8_lossy(&self.serial_bytes()).into_owned()
    }

    /// The serial log since the last `reset`.
    pub fn recent(&self) -> String {
        let b = self.serial_bytes();
        String::from_utf8_lossy(&b[self.since.min(b.len())..]).into_owned()
    }

    /// Makes later waits and expectations look only at output from now on.
    pub fn checkpoint(&mut self) {
        self.since = self.serial_bytes().len();
    }

    /// Resets the machine (a reboot that keeps the disks).
    pub fn reset(&mut self) -> Result {
        self.since = self.serial_bytes().len();
        self.m.reset()
    }

    /// Waits until the serial log contains `needle`.
    pub fn wait_serial(&mut self, needle: &str, timeout: Duration) -> Result {
        let start = Instant::now();
        loop {
            if self.recent().contains(needle) {
                return Ok(());
            }
            let log = self.serial();
            if let Some(p) = self.fail_patterns.iter().find(|p| log.contains(p.as_str())) {
                return Err(format!("serial log contains failure pattern \"{p}\""));
            }
            if let Some(why) = self.m.stopped() {
                return Err(format!("{why} before \"{needle}\" appeared"));
            }
            if start.elapsed() > timeout {
                return Err(format!("timed out after {}s waiting for \"{needle}\"", timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn finish(mut self) {
        self.m.finish();
    }
}

/// Splits a script line into words, honouring double quotes.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    // Single quotes keep double quotes inside (JSON arguments).
    let mut single = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '\'' if !quoted => {
                single = !single;
                any = true;
            }
            c if single => cur.push(c),
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '#' if !quoted => break,
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

fn num(w: &[String], i: usize) -> Result<f64> {
    w.get(i).ok_or("missing argument")?.parse::<f64>().map_err(|_| format!("'{}' is not a number", w[i]))
}

/// The `boot-cmdline` arguments of a script (they must be known before the
/// disk image is built, as the command line is part of it).
pub fn boot_cmdline(script: &str) -> Vec<String> {
    script
        .lines()
        .map(words)
        .filter(|w| w.first().is_some_and(|c| c == "boot-cmdline"))
        .filter_map(|w| w.get(1).cloned())
        .collect()
}

/// The network a script asks for with `net` (it must be known before the
/// machine starts).
pub fn net_mode(script: &str) -> Result<Option<NetMode>> {
    let mut mode = None;
    for w in script.lines().map(words) {
        if w.first().is_some_and(|c| c == "net") {
            let m = w.get(1).ok_or("net: missing mode")?;
            mode = Some(NetMode::parse(m).ok_or(format!("net: unknown mode '{m}'"))?);
        }
    }
    Ok(mode)
}

/// Why a script can only run under QEMU, if it can: the simulated Wi-Fi
/// (virtio-serial and airsim), QEMU's 82574L card, or `requires qemu`.
pub fn needs_qemu(script: &str) -> Option<&'static str> {
    for w in script.lines().map(words) {
        match w.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["net", "wifi" | "both", ..] => return Some("simulated Wi-Fi"),
            ["nic", "e1000e", ..] => return Some("the 82574L card"),
            ["air" | "air-expect" | "air-wait", ..] => return Some("the Wi-Fi simulator"),
            ["requires", "qemu", ..] => return Some("marked as QEMU only"),
            _ => {}
        }
    }
    None
}

/// Whether a script starts with the agent asleep (`agent-asleep`) rather
/// than in a conversation.
pub fn agent_asleep(script: &str) -> bool {
    script.lines().map(words).any(|w| w.first().is_some_and(|c| c == "agent-asleep"))
}

/// Whether a script talks to the simulated Voice Agent service.
pub fn needs_agentsim(script: &str) -> bool {
    script.lines().map(words).any(|w| w.first().is_some_and(|c| c.starts_with("agent-")))
}

/// Whether a script uses the test microphone.
pub fn needs_mic(script: &str) -> bool {
    script.lines().map(words).any(|w| {
        w.first().is_some_and(|c| {
            matches!(c.as_str(), "say" | "say-async" | "mic-wav" | "mic-silence" | "mic-tone" | "mic-echo" | "mic-wait")
        })
    })
}

/// Whether a script says `requires virtualbox`.
pub fn needs_virtualbox(script: &str) -> bool {
    script.lines().map(words).any(|w| w.len() >= 2 && w[0] == "requires" && w[1] == "virtualbox")
}

/// The sound card a script asks for with `sound` (`virtio` or `ac97`).
pub fn sound_card(script: &str) -> Option<String> {
    script.lines().map(words).filter(|w| w.first().is_some_and(|c| c == "sound")).find_map(|w| w.get(1).cloned())
}

/// Whether a script plays through the host's loudspeakers and records its
/// microphone (`audio host`) instead of recording the output to a file.
pub fn host_audio(script: &str) -> bool {
    script.lines().map(words).any(|w| w.len() == 2 && w[0] == "audio" && w[1] == "host")
}

/// The wired card model a script asks for with `nic`.
pub fn nic_model(script: &str) -> Option<String> {
    script.lines().map(words).filter(|w| w.first().is_some_and(|c| c == "nic")).find_map(|w| w.get(1).cloned())
}

impl Session {
    fn sim(&self) -> Result<&AirSim> {
        self.sim.as_ref().ok_or_else(|| "this run has no Wi-Fi simulator (add `net wifi`)".to_string())
    }
}

/// Runs an automation script against a fresh VM booted from `disk`.
pub fn run_script(
    hv: &Hypervisor,
    disk: &Path,
    vm: VmConfig,
    script: &str,
    mic: Option<MicServer>,
    mut agent: Option<AgentSim>,
) -> Result<String> {
    let mut s = Session::start(hv, disk, vm)?;
    let mic_ref = || mic.as_ref().ok_or_else(|| "this run has no test microphone".to_string());
    let mut agent_result = String::new();
    let mut agent_mark = 0usize;
    let mut mic_mark = 0usize;
    let result = (|| -> Result {
        for (lineno, line) in script.lines().enumerate() {
            let w = words(line);
            let Some(cmd) = w.first() else { continue };
            let ctx = |e: String| format!("script line {}: {e}", lineno + 1);
            match cmd.as_str() {
                "wait" => std::thread::sleep(Duration::from_secs_f64(num(&w, 1).map_err(ctx)?)),
                "wait-serial" => {
                    let timeout = num(&w, 2).unwrap_or(60.0);
                    s.wait_serial(w.get(1).ok_or("missing text")?, Duration::from_secs_f64(timeout)).map_err(ctx)?
                }
                "wait-serial-count" => {
                    let needle = w.get(1).ok_or("missing text")?.clone();
                    let count = num(&w, 2).map_err(ctx)? as usize;
                    let timeout = Duration::from_secs_f64(num(&w, 3).unwrap_or(60.0));
                    let start = Instant::now();
                    loop {
                        let seen = s.recent().matches(needle.as_str()).count();
                        if seen >= count {
                            break;
                        }
                        let log = s.serial();
                        if let Some(p) = s.fail_patterns.iter().find(|p| log.contains(p.as_str())) {
                            return Err(ctx(format!("serial log contains failure pattern \"{p}\"")));
                        }
                        if start.elapsed() > timeout {
                            return Err(ctx(format!(
                                "timed out: \"{needle}\" appeared {seen} of {count} times in {}s",
                                timeout.as_secs()
                            )));
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                "shot" => {
                    let path = PathBuf::from(w.get(1).ok_or("missing file name")?);
                    let path = if path.is_absolute() { path } else { util::workspace_root().join(path) };
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    s.m.screenshot(&path).map_err(ctx)?;
                    util::status("Screenshot", path.display());
                }
                "move" => s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?,
                "click" => {
                    let button = w.get(3).map(String::as_str).unwrap_or("left");
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button(button, true).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button(button, false).map_err(ctx)?;
                }
                "drag" => {
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button("left", true).map_err(ctx)?;
                    for step in 1..=10 {
                        let t = step as f64 / 10.0;
                        let x = num(&w, 1)? + (num(&w, 3)? - num(&w, 1)?) * t;
                        let y = num(&w, 2)? + (num(&w, 4)? - num(&w, 2)?) * t;
                        s.m.move_mouse(x, y).map_err(ctx)?;
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    s.m.mouse_button("left", false).map_err(ctx)?;
                }
                "fail-on" => s.fail_patterns.push(w.get(1).ok_or("missing text")?.clone()),
                "boot-cmdline" | "net" | "nic" | "sound" | "audio" | "requires" => {}
                "air" => {
                    let line = w.get(1).ok_or("missing command")?;
                    s.sim().and_then(|sim| sim.command(line)).map_err(ctx)?;
                }
                "air-expect" => {
                    let (line, needle) = (w.get(1).ok_or("missing command")?, w.get(2).ok_or("missing text")?);
                    let answer = s.sim().and_then(|sim| sim.command(line)).map_err(ctx)?;
                    if !answer.contains(needle.as_str()) {
                        return Err(ctx(format!("airsim's answer to \"{line}\" lacks \"{needle}\":\n{answer}")));
                    }
                }
                "air-wait" => {
                    let (line, needle) = (w.get(1).ok_or("missing command")?, w.get(2).ok_or("missing text")?);
                    let timeout = Duration::from_secs_f64(num(&w, 3).unwrap_or(60.0));
                    let start = Instant::now();
                    loop {
                        let answer = s.sim().and_then(|sim| sim.command(line)).map_err(ctx)?;
                        if answer.contains(needle.as_str()) {
                            break;
                        }
                        let log = s.serial();
                        if let Some(p) = s.fail_patterns.iter().find(|p| log.contains(p.as_str())) {
                            return Err(ctx(format!("serial log contains failure pattern \"{p}\"")));
                        }
                        if start.elapsed() > timeout {
                            return Err(ctx(format!(
                                "timed out waiting for \"{needle}\" in airsim's answer to \"{line}\":\n{answer}"
                            )));
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                "reset" => s.reset().map_err(ctx)?,
                "checkpoint" => s.checkpoint(),
                "link" => {
                    let up = match w.get(2).map(String::as_str) {
                        Some("on" | "up") => true,
                        Some("off" | "down") => false,
                        _ => return Err(ctx("usage: link wired on|off".into())),
                    };
                    let name = match w.get(1).map(String::as_str) {
                        Some("wired") => qemu::WIRED_NIC_ID,
                        _ => return Err(ctx("usage: link wired on|off".into())),
                    };
                    let _ = name;
                    s.m.set_wired_link(up).map_err(ctx)?;
                }
                "mouse-down" => {
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button("left", true).map_err(ctx)?;
                }
                "mouse-up" => s.m.mouse_button("left", false).map_err(ctx)?,
                "double-click" => {
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    for _ in 0..2 {
                        std::thread::sleep(Duration::from_millis(60));
                        s.m.mouse_button("left", true).map_err(ctx)?;
                        std::thread::sleep(Duration::from_millis(60));
                        s.m.mouse_button("left", false).map_err(ctx)?;
                    }
                }
                "key" => s.m.send_keys(w.get(1).ok_or("missing key")?).map_err(ctx)?,
                "key-down" => s.m.key_event(w.get(1).ok_or("missing key")?, true).map_err(ctx)?,
                "key-up" => s.m.key_event(w.get(1).ok_or("missing key")?, false).map_err(ctx)?,
                "type" => s.m.type_text(w.get(1).ok_or("missing text")?).map_err(ctx)?,
                // Types a secret (an API key) from the environment, so it
                // appears in neither the script nor the logs.
                "type-env" => {
                    let name = w.get(1).ok_or("missing variable name")?;
                    let value = std::env::var(name).map_err(|_| ctx(format!("${name} is not set")))?;
                    s.m.type_text(value.trim()).map_err(|_| ctx(format!("cannot type ${name}")))?;
                }
                "expect-serial" => {
                    let needle = w.get(1).ok_or("missing text")?;
                    if !s.recent().contains(needle.as_str()) {
                        return Err(ctx(format!("serial log does not contain \"{needle}\"")));
                    }
                }
                // The guest's sound output (QEMU records it to a WAV file in
                // scripts) holds more than silence.
                "expect-audio" => {
                    let wav =
                        std::fs::read(util::out_dir().join("audio.wav")).map_err(|e| ctx(format!("audio.wav: {e}")))?;
                    let peak = wav
                        .get(44..)
                        .unwrap_or(&[])
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b).unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    if peak < 1000 {
                        return Err(ctx(format!("the sound output is silent (peak {peak})")));
                    }
                }
                "reject-serial" => {
                    let needle = w.get(1).ok_or("missing text")?;
                    if s.recent().contains(needle.as_str()) {
                        return Err(ctx(format!("serial log contains \"{needle}\"")));
                    }
                }
                "say" | "say-async" => {
                    let text = w.get(1).ok_or("missing text")?;
                    let m = mic_ref().map_err(ctx)?;
                    let speech = mic::synthesize(text, mic::USER_VOICE).map_err(ctx)?;
                    m.enqueue(&speech);
                    m.silence(0.6);
                    if cmd == "say" {
                        let secs = speech.len() as f64 / mic::RATE as f64 + 30.0;
                        m.wait_drained(Duration::from_secs_f64(secs)).map_err(ctx)?;
                    }
                }
                "mic-wav" => {
                    let path = w.get(1).ok_or("missing file")?;
                    let m = mic_ref().map_err(ctx)?;
                    let audio = mic::read_wav(&util::workspace_root().join(path)).map_err(ctx)?;
                    m.enqueue(&audio);
                    let secs = audio.len() as f64 / mic::RATE as f64 + 30.0;
                    m.wait_drained(Duration::from_secs_f64(secs)).map_err(ctx)?;
                }
                "mic-silence" => mic_ref().map_err(ctx)?.silence(num(&w, 1).map_err(ctx)?),
                "mic-tone" => mic_ref().map_err(ctx)?.tone(num(&w, 1).map_err(ctx)?, num(&w, 2).unwrap_or(6000.0)),
                "mic-echo" => mic_ref().map_err(ctx)?.echo(
                    util::out_dir().join("audio.wav"),
                    num(&w, 1).map_err(ctx)?,
                    num(&w, 2).map_err(ctx)?,
                ),
                c if c.starts_with("agent-") => {
                    let a = agent.as_mut().ok_or_else(|| ctx("this run has no agent simulator".into()))?;
                    match c {
                        "agent-connected" => {
                            a.wait_connected(Duration::from_secs_f64(num(&w, 1).unwrap_or(120.0))).map_err(ctx)?
                        }
                        "agent-call" => {
                            let name = w.get(1).ok_or("missing function name")?;
                            let args = w.get(2).map(String::as_str).unwrap_or("{}");
                            let timeout = Duration::from_secs_f64(num(&w, 3).unwrap_or(60.0));
                            agent_result = a.call(name, args, timeout).map_err(ctx)?;
                            println!("agent-call {name}: {}", agent_result.chars().take(300).collect::<String>());
                        }
                        "agent-result" => {
                            let needle = w.get(1).ok_or("missing text")?;
                            if !agent_result.contains(needle.as_str()) {
                                return Err(ctx(format!(
                                    "the last result does not contain \"{needle}\": {agent_result}"
                                )));
                            }
                        }
                        "agent-mark" => {
                            agent_mark = a.texts().len();
                            mic_mark = a.mic_levels_len();
                        }
                        // What the agent sent of the microphone since the
                        // mark: never louder than (quiet) or at least (heard)
                        // this level in dBFS.
                        "agent-mic-quiet" | "agent-mic-heard" => {
                            let limit = num(&w, 1).map_err(ctx)? as f32;
                            let peak = a.mic_peak_since(mic_mark);
                            let quiet = c == "agent-mic-quiet";
                            if quiet && peak >= limit {
                                return Err(ctx(format!(
                                    "the agent sent microphone audio at {peak:.1} dBFS (limit {limit})"
                                )));
                            }
                            if !quiet && peak < limit {
                                return Err(ctx(format!(
                                    "the agent sent no microphone audio louder than {peak:.1} dBFS (wanted {limit})"
                                )));
                            }
                        }
                        "agent-expect" => {
                            let needle = w.get(1).ok_or("missing text")?;
                            let timeout = Duration::from_secs_f64(num(&w, 2).unwrap_or(60.0));
                            let t = a.wait_text(needle, agent_mark, timeout).map_err(ctx)?;
                            println!("agent-expect: {}", t.chars().take(300).collect::<String>());
                        }
                        "agent-speak" => a.speak(num(&w, 1).map_err(ctx)?).map_err(ctx)?,
                        "agent-interrupt" => a.send_json(r#"{"type":"UserStartedSpeaking"}"#).map_err(ctx)?,
                        "agent-hear" => {
                            let text = w.get(1).ok_or("missing text")?;
                            a.hear(text, Duration::from_secs_f64(num(&w, 2).unwrap_or(30.0))).map_err(ctx)?
                        }
                        "agent-asleep" => {}
                        "agent-listens" => {
                            let n = num(&w, 1).map_err(ctx)? as u32;
                            if a.listen_connections() != n {
                                return Err(ctx(format!(
                                    "the agent opened {} recognition streams, not {n}",
                                    a.listen_connections()
                                )));
                            }
                        }
                        "agent-send" => a.send_json(w.get(1).ok_or("missing message")?).map_err(ctx)?,
                        "agent-audio" => {
                            let want = num(&w, 1).map_err(ctx)? as usize;
                            let got = a.audio_bytes();
                            if got < want {
                                return Err(ctx(format!("the agent streamed only {got} bytes of microphone audio")));
                            }
                        }
                        other => return Err(ctx(format!("unknown command '{other}'"))),
                    }
                }
                "mic-wait" => {
                    let timeout = num(&w, 1).unwrap_or(60.0);
                    mic_ref().map_err(ctx)?.wait_drained(Duration::from_secs_f64(timeout)).map_err(ctx)?;
                }
                other => return Err(ctx(format!("unknown command '{other}'"))),
            }
        }
        Ok(())
    })();
    let log = s.serial();
    // Something may also have failed after the last wait.
    let result = result.and_then(|()| match s.fail_patterns.iter().find(|p| log.contains(p.as_str())) {
        Some(p) => Err(format!("serial log contains failure pattern \"{p}\"")),
        None => Ok(()),
    });
    let sim_log = s.sim.as_ref().map(|sim| sim.log.clone());
    s.finish();
    result.map(|_| log.clone()).map_err(|e| {
        let mut msg = format!("{e}\n--- serial log ---\n{}", tail(&log, 60));
        if let Some(text) = sim_log.and_then(|p| std::fs::read_to_string(p).ok()) {
            msg.push_str(&format!("\n--- airsim log ---\n{}", tail(&text, 30)));
        }
        msg
    })
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
