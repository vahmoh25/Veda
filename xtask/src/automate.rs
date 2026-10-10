//! Scripted, headless runs of Veda in QEMU.
//!
//! Automation scripts drive the virtual machine through QMP and inspect its
//! serial console. They power `cargo xtask shot` and the integration tests.
//! One command per line; `#` starts a comment:
//!
//! ```text
//! wait-serial "desktop ready" 60   # wait for a log line (timeout in s)
//! wait-serial-count "joined" 3 60  # wait until the text has appeared 3 times
//! wait 2                           # sleep
//! shot target/veda/desktop.png     # save a screenshot
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
//! nic e1000e                       # QEMU model of the wired card for this run, applied before boot
//! disk nvme                        # how QEMU attaches the disks for this run (virtio, ahci, nvme), applied before boot
//! acpi-table touchpad              # add a table to the firmware's (xtask/src/acpitest.rs), applied before boot
//! sound hda                        # the sound card for this run (virtio or hda), applied before boot
//! live                             # boot the live system (`xtask iso`) from a USB stick, applied before boot
//! input usb                        # USB keyboard and pointer (see `--input`), applied before boot
//! qmp device_del '{"id":"kbd2"}'   # a QMP command, such as plugging USB devices in and out
//! audio host                       # the host's loudspeakers and microphone instead of a WAV file (echo on real hardware)
//! audio silent                     # QEMU's silent sound system: the output is not recorded, the input records silence
//! expect-audio                     # fail unless the recorded sound output holds more than silence
//! expect-audio-gapless [20]        # fail if it drops out (digital silence over 20 ms between its first and last sound)
//! usb net                          # QEMU's USB network adapter (CDC Ethernet) on the xHCI controller
//! requires c-toolchain             # only with the C test programs (`cargo xtask toolchain`); `test` skips it otherwise
//! requires native-toolchain        # only with GCC in the image (the same)
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
//! agent-hear "Hey Veda, hello" 30  # the simulated recogniser hears this, once the agent streams speech to it
//! agent-hear "Hey Veda. | Hi" 30   # " | " splits it into final transcripts 0.3 s apart (the last ends the utterance)
//! agent-listens 1                  # fail unless the agent opened exactly this many recognition streams
//! agent-listens-at-most 2          # fail if the agent opened more recognition streams than this
//! agent-mic-quiet -60              # fail if the agent sent microphone audio this loud (dBFS) since agent-mark
//! agent-mic-heard -30              # fail unless it sent microphone audio at least this loud since agent-mark
//! ```
//!
//! Scripts that use the microphone commands boot with `testmic`, which
//! connects back to the host (see `mic.rs`). The pointer's and the
//! keyboard's commands wait, the first time in a boot, until the guest's
//! tablet or keyboard is Veda's (Linux drives them in the driver VM, which
//! may come after the desktop).

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::agentsim::AgentSim;
use crate::airsim::AirSim;
use crate::mic::{self, MicServer};
use crate::qemu::{self, DiskBus, InputDevices, NetMode, QemuInstall, VmConfig};
use crate::qmp::Qmp;
use crate::util::{self, Result};

/// The machine a script drives: QEMU, through QMP.
pub struct Machine {
    child: Child,
    qmp: Qmp,
}

impl Machine {
    pub fn screenshot(&mut self, path: &Path) -> Result {
        self.qmp.screenshot(path)
    }

    pub fn move_mouse(&mut self, fx: f64, fy: f64) -> Result {
        self.qmp.move_mouse(fx, fy)
    }

    pub fn mouse_button(&mut self, button: &str, down: bool) -> Result {
        self.qmp.mouse_button(button, down)
    }

    pub fn send_keys(&mut self, combo: &str) -> Result {
        self.qmp.send_keys(combo)
    }

    pub fn key_event(&mut self, key: &str, down: bool) -> Result {
        self.qmp.key_event(key, down)
    }

    pub fn type_text(&mut self, text: &str) -> Result {
        self.qmp.type_text(text)
    }

    /// Plugs in or unplugs the wired card's cable.
    pub fn set_wired_link(&mut self, up: bool) -> Result {
        let name = qemu::WIRED_NIC_ID;
        self.qmp.execute("set_link", &format!("{{\"name\":\"{name}\",\"up\":{up}}}")).map(|_| ())
    }

    fn reset(&mut self) -> Result {
        self.qmp.execute("system_reset", "{}").map(|_| ())
    }

    /// Why the machine stopped, if it did.
    fn stopped(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("QEMU exited ({status})")),
            _ => None,
        }
    }

    fn finish(&mut self) {
        self.qmp.quit();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
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
    /// Where this boot's output starts, and whether its pointer and its
    /// keyboard are up.
    boot: usize,
    pointer: bool,
    keyboard: bool,
    /// The Wi-Fi simulator, when the machine has the virtual radio.
    pub sim: Option<AirSim>,
}

impl Session {
    pub fn start(install: &QemuInstall, disk: &Path, mut vm: VmConfig) -> Result<Self> {
        let out = util::out_dir();
        let serial_log = out.join("serial.log");
        let _ = std::fs::remove_file(&serial_log);
        // Kernel panics print "PANIC", user-space panics "panicked at".
        let fail_patterns = vec!["PANIC".into(), "panicked at".into()];
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
        Ok(Session {
            m: Machine { child, qmp },
            serial_log,
            fail_patterns,
            since: 0,
            boot: 0,
            pointer: false,
            keyboard: false,
            sim,
        })
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
        self.boot = self.since;
        self.pointer = false;
        self.keyboard = false;
        self.m.reset()
    }

    /// Waits until the serial log contains `needle`.
    pub fn wait_serial(&mut self, needle: &str, timeout: Duration) -> Result {
        self.wait_from(self.since, needle, timeout)
    }

    /// The first pointer action of a boot waits until the guest's tablet is
    /// Veda's: Linux drives it, in the driver VM, which may not have done so
    /// yet when the desktop is ready.
    fn pointer(&mut self) -> Result {
        if !self.pointer {
            self.wait_from(self.boot, "): tablet", Duration::from_secs(60))?;
            self.pointer = true;
        }
        Ok(())
    }

    /// The first key of a boot waits until the guest's keyboard is Veda's,
    /// as the pointer does for its tablet.
    fn keyboard(&mut self) -> Result {
        if !self.keyboard {
            self.wait_from(self.boot, "): keyboard", Duration::from_secs(60))?;
            self.keyboard = true;
        }
        Ok(())
    }

    /// Waits until the serial log from byte `from` on contains `needle`.
    fn wait_from(&mut self, from: usize, needle: &str, timeout: Duration) -> Result {
        let start = Instant::now();
        loop {
            let b = self.serial_bytes();
            if String::from_utf8_lossy(&b[from.min(b.len())..]).contains(needle) {
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

/// A screenshot's path as a script gives it: relative to the workspace, with
/// `target/veda/` standing for the output directory (`$VEDA_OUT`).
fn shot_path(w: &[String], i: usize) -> Result<PathBuf> {
    let given = w.get(i).ok_or("missing file name")?;
    if let Some(rest) = given.strip_prefix("target/veda/") {
        return Ok(util::out_dir().join(rest));
    }
    let path = PathBuf::from(given);
    Ok(if path.is_absolute() { path } else { util::workspace_root().join(path) })
}

/// Fails unless the screenshots `a` and `b` are alike, pixel for pixel, in
/// `region` (fractions of the screen: left, top, right, bottom).
fn same_pictures(a: &Path, b: &Path, region: [f64; 4]) -> Result {
    let load = |p: &Path| -> Result<vimage::Image> {
        let data = std::fs::read(p).map_err(|e| format!("reading {}: {e}", p.display()))?;
        vimage::decode(&data).map_err(|e| format!("decoding {}: {e:?}", p.display()))
    };
    let (first, second) = (load(a)?, load(b)?);
    if (first.width, first.height) != (second.width, second.height) {
        return Err(format!("{} and {} differ in size", a.display(), b.display()));
    }
    let (w, h) = (first.width as f64, first.height as f64);
    let x = (region[0] * w) as u32..((region[2] * w) as u32).min(first.width);
    let y = (region[1] * h) as u32..((region[3] * h) as u32).min(first.height);
    let mut differing = y
        .flat_map(|y| x.clone().map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let i = (y * first.width + x) as usize;
            first.pixels[i] != second.pixels[i]
        })
        .peekable();
    match differing.peek().copied() {
        None => Ok(()),
        Some((x, y)) => Err(format!(
            "{} and {} differ in {} pixels, the first at ({x}, {y})",
            a.display(),
            b.display(),
            differing.count()
        )),
    }
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

/// The keyboard and pointing devices a script asks for with `input`.
pub fn input_devices(script: &str) -> Result<Option<InputDevices>> {
    let mut input = None;
    for w in script.lines().map(words) {
        if w.first().is_some_and(|c| c == "input") {
            let v = w.get(1).ok_or("input: missing devices")?;
            input = Some(InputDevices::parse(v).ok_or(format!("input: unknown devices '{v}' (standard, usb)"))?);
        }
    }
    Ok(input)
}

/// Whether a script asks for QEMU's 3D GPU (`gpu on`) or plain VGA (`gpu
/// off`) as the display; `None` if it does not say.
pub fn gpu(script: &str) -> Result<Option<bool>> {
    let mut gpu = None;
    for w in script.lines().map(words) {
        if w.first().is_some_and(|c| c == "gpu") {
            gpu = Some(match w.get(1).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                other => return Err(format!("gpu: expected on or off, not {}", other.unwrap_or("nothing"))),
            });
        }
    }
    Ok(gpu)
}

/// Whether a script starts with the agent asleep (`agent-asleep`) rather
/// than in a conversation.
pub fn agent_asleep(script: &str) -> bool {
    script.lines().map(words).any(|w| w.first().is_some_and(|c| c == "agent-asleep"))
}

/// Whether a script boots the live system from a USB stick (`live`).
pub fn live(script: &str) -> bool {
    script.lines().map(words).any(|w| w.first().is_some_and(|c| c == "live"))
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

/// The part of the C toolchain a script needs (`requires c-toolchain`: the
/// C test programs the cross compiler builds; `requires native-toolchain`:
/// GCC in the image), as [`crate::toolchain::built`] names it.
pub fn needs_toolchain(script: &str) -> Option<&'static str> {
    script.lines().map(words).find_map(|w| match w.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["requires", "c-toolchain", ..] => Some("cross toolchain"),
        ["requires", "native-toolchain", ..] => Some("native toolchain"),
        _ => None,
    })
}

/// The sound card a script asks for with `sound` (`virtio` or `hda`).
pub fn sound_card(script: &str) -> Option<String> {
    script.lines().map(words).filter(|w| w.first().is_some_and(|c| c == "sound")).find_map(|w| w.get(1).cloned())
}

/// Whether a script plays through the host's loudspeakers and records its
/// microphone (`audio host`) instead of recording the output to a file.
pub fn host_audio(script: &str) -> bool {
    script.lines().map(words).any(|w| w.len() == 2 && w[0] == "audio" && w[1] == "host")
}

/// Whether a script's sound card has QEMU's silent sound system (`audio
/// silent`): an input that records silence at the card's pace, and an
/// output nothing records.
pub fn silent_audio(script: &str) -> bool {
    script.lines().map(words).any(|w| w.len() == 2 && w[0] == "audio" && w[1] == "silent")
}

/// Whether a script asks for QEMU's USB network adapter (`usb net`).
pub fn usb_net(script: &str) -> bool {
    script.lines().map(words).any(|w| w.len() == 2 && w[0] == "usb" && w[1] == "net")
}

/// How QEMU attaches the disks, as a script asks with `disk`.
pub fn disk_bus(script: &str) -> Result<Option<DiskBus>> {
    let mut bus = None;
    for w in script.lines().map(words) {
        if w.first().is_some_and(|c| c == "disk") {
            let v = w.get(1).ok_or("disk: missing bus")?;
            bus = Some(DiskBus::parse(v).ok_or(format!("disk: unknown bus '{v}' (virtio, ahci, nvme)"))?);
        }
    }
    Ok(bus)
}

/// The ACPI tables a script adds to QEMU's with `acpi-table NAME`
/// (`acpitest`), written out for QEMU.
pub fn acpi_tables(script: &str) -> Result<Vec<PathBuf>> {
    let mut tables = Vec::new();
    for w in script.lines().map(words).filter(|w| w.first().is_some_and(|c| c == "acpi-table")) {
        let n = w.get(1).ok_or("acpi-table: missing name")?;
        let table = crate::acpitest::table(n).ok_or(format!("acpi-table: no table '{n}' (touchpad)"))?;
        let path = util::out_dir().join(format!("acpi-{n}.aml"));
        std::fs::write(&path, table).map_err(|e| format!("{}: {e}", path.display()))?;
        tables.push(path);
    }
    Ok(tables)
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
    install: &QemuInstall,
    disk: &Path,
    vm: VmConfig,
    script: &str,
    mic: Option<MicServer>,
    mut agent: Option<AgentSim>,
) -> Result<String> {
    let mut s = Session::start(install, disk, vm)?;
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
                    let path = shot_path(&w, 1)?;
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    s.m.screenshot(&path).map_err(ctx)?;
                    util::status("Screenshot", path.display());
                }
                // Two screenshots show the same in a region (fractions of
                // the screen; all of it if none is given): what came and
                // went between them left nothing behind.
                "expect-same" => {
                    let region = if w.len() >= 7 {
                        [num(&w, 3)?, num(&w, 4)?, num(&w, 5)?, num(&w, 6)?]
                    } else {
                        [0.0, 0.0, 1.0, 1.0]
                    };
                    same_pictures(&shot_path(&w, 1)?, &shot_path(&w, 2)?, region).map_err(ctx)?;
                }
                "move" => {
                    s.pointer().map_err(ctx)?;
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?
                }
                "click" => {
                    s.pointer().map_err(ctx)?;
                    let button = w.get(3).map(String::as_str).unwrap_or("left");
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button(button, true).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button(button, false).map_err(ctx)?;
                }
                "drag" => {
                    s.pointer().map_err(ctx)?;
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
                "boot-cmdline" | "net" | "nic" | "disk" | "usb" | "sound" | "audio" | "requires" | "live" | "input"
                | "gpu" | "acpi-table" => {}
                "qmp" => {
                    let command = w.get(1).ok_or("missing command")?;
                    let arguments = w.get(2).map_or("{}", String::as_str);
                    s.m.qmp.execute(command, arguments).map(|_| ()).map_err(ctx)?;
                }
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
                    s.pointer().map_err(ctx)?;
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.m.mouse_button("left", true).map_err(ctx)?;
                }
                "mouse-up" => s.m.mouse_button("left", false).map_err(ctx)?,
                "double-click" => {
                    s.pointer().map_err(ctx)?;
                    s.m.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    for _ in 0..2 {
                        std::thread::sleep(Duration::from_millis(60));
                        s.m.mouse_button("left", true).map_err(ctx)?;
                        std::thread::sleep(Duration::from_millis(60));
                        s.m.mouse_button("left", false).map_err(ctx)?;
                    }
                }
                "key" => {
                    s.keyboard().map_err(ctx)?;
                    s.m.send_keys(w.get(1).ok_or("missing key")?).map_err(ctx)?
                }
                "key-down" => {
                    s.keyboard().map_err(ctx)?;
                    s.m.key_event(w.get(1).ok_or("missing key")?, true).map_err(ctx)?
                }
                "key-up" => s.m.key_event(w.get(1).ok_or("missing key")?, false).map_err(ctx)?,
                "type" => {
                    s.keyboard().map_err(ctx)?;
                    s.m.type_text(w.get(1).ok_or("missing text")?).map_err(ctx)?
                }
                // Types a secret (an API key) from the environment, so it
                // appears in neither the script nor the logs.
                "type-env" => {
                    let name = w.get(1).ok_or("missing variable name")?;
                    let value = std::env::var(name).map_err(|_| ctx(format!("${name} is not set")))?;
                    s.keyboard().map_err(ctx)?;
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
                // While it plays, the sound output never drops out: no
                // stretch of digital silence longer than the limit (20 ms by
                // default) between the first sound and the last.
                "expect-audio-gapless" => {
                    let limit = if w.len() > 1 { num(&w, 1)? } else { 20.0 };
                    let wav =
                        std::fs::read(util::out_dir().join("audio.wav")).map_err(|e| ctx(format!("audio.wav: {e}")))?;
                    let gaps = silent_gaps(&wav, limit).map_err(ctx)?;
                    if !gaps.is_empty() {
                        let list: Vec<String> =
                            gaps.iter().take(5).map(|(at, ms)| format!("{ms:.0} ms at {at:.2} s")).collect();
                        return Err(ctx(format!(
                            "the sound output drops out {} time(s): {}",
                            gaps.len(),
                            list.join(", ")
                        )));
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
                        "agent-listens-at-most" => {
                            let n = num(&w, 1).map_err(ctx)? as u32;
                            if a.listen_connections() > n {
                                return Err(ctx(format!(
                                    "the agent opened {} recognition streams, more than {n}",
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

/// The stretches of digital silence (every channel exactly zero) longer
/// than `limit_ms` in a 16-bit PCM WAV file, between its first and last
/// sound: where (s) and how long (ms). QEMU leaves the data chunk's length
/// at zero while it records; that means "to the end".
pub fn silent_gaps(wav: &[u8], limit_ms: f64) -> std::result::Result<Vec<(f64, f64)>, String> {
    let u16_at = |at: usize| wav.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |at: usize| wav.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if wav.get(0..4) != Some(b"RIFF") || wav.get(8..12) != Some(b"WAVE") {
        return Err("audio.wav is not a WAV file".into());
    }
    let (mut at, mut format) = (12, None);
    let data = loop {
        let (Some(id), Some(len)) = (wav.get(at..at + 4), u32_at(at + 4)) else {
            return Err("audio.wav has no data".into());
        };
        if id == b"fmt " {
            format = Some((u16_at(at + 10).unwrap_or(0), u32_at(at + 12).unwrap_or(0), u16_at(at + 22).unwrap_or(0)));
        } else if id == b"data" {
            let end = if len == 0 { wav.len() } else { (at + 8 + len).min(wav.len()) };
            break &wav[at + 8..end];
        }
        at += 8 + len + (len & 1);
    };
    let Some((channels @ 1.., rate @ 1.., 16)) = format else { return Err("audio.wav is not 16-bit PCM".into()) };
    let frames: Vec<&[u8]> = data.chunks_exact(2 * channels).collect();
    let silent = |f: &[u8]| f.iter().all(|&b| b == 0);
    let loud = |f: &[u8]| f.as_chunks::<2>().0.iter().any(|s| i16::from_le_bytes(*s).unsigned_abs() > 64);
    let (Some(first), Some(last)) = (frames.iter().position(|f| loud(f)), frames.iter().rposition(|f| loud(f))) else {
        return Ok(Vec::new());
    };
    let mut gaps = Vec::new();
    let mut run = 0usize;
    for (i, f) in frames.iter().enumerate().take(last + 1).skip(first) {
        if silent(f) {
            run += 1;
            continue;
        }
        let ms = run as f64 * 1000.0 / rate as f64;
        if ms > limit_ms {
            gaps.push(((i - run) as f64 / rate as f64, ms));
        }
        run = 0;
    }
    Ok(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(samples: &[i16], data_len: Option<u32>) -> Vec<u8> {
        let mut w = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        w.extend_from_slice(&16u32.to_le_bytes());
        // PCM, 2 channels, 1000 Hz, 4000 bytes/s, 4 bytes/frame, 16 bits.
        for v in [1u16, 2] {
            w.extend_from_slice(&v.to_le_bytes());
        }
        w.extend_from_slice(&1000u32.to_le_bytes());
        w.extend_from_slice(&4000u32.to_le_bytes());
        for v in [4u16, 16] {
            w.extend_from_slice(&v.to_le_bytes());
        }
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.unwrap_or(samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    }

    #[test]
    fn silent_gaps_inside_the_sound() {
        // 1000 frames per second, stereo: silence, sound, a 30 ms
        // dropout, sound, a 10 ms one, sound, silence.
        let mut s = vec![0i16; 2 * 50];
        s.extend(std::iter::repeat_n(3000, 2 * 100));
        s.extend(std::iter::repeat_n(0, 2 * 30));
        s.extend(std::iter::repeat_n(-3000, 2 * 100));
        s.extend(std::iter::repeat_n(0, 2 * 10));
        s.extend(std::iter::repeat_n(2000, 2 * 100));
        s.extend(std::iter::repeat_n(0, 2 * 500));
        let gaps = silent_gaps(&wav(&s, None), 20.0).unwrap();
        assert_eq!(gaps, [(0.15, 30.0)]);
        // QEMU's header while recording: no data length.
        assert_eq!(silent_gaps(&wav(&s, Some(0)), 5.0).unwrap(), [(0.15, 30.0), (0.28, 10.0)]);
        assert!(silent_gaps(b"RIFF", 20.0).is_err());
    }
}
