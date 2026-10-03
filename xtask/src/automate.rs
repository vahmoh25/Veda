//! Scripted, headless runs of Vindows in QEMU.
//!
//! Automation scripts drive the virtual machine through QMP and inspect its
//! serial console. They power `cargo xtask shot` and the integration tests.
//! One command per line; `#` starts a comment:
//!
//! ```text
//! wait-serial "desktop ready" 60   # wait for a log line (timeout in s)
//! wait 2                           # sleep
//! shot target/vindows/desktop.png  # save a screenshot
//! move 0.5 0.5                     # move the pointer (fractions of screen)
//! click 0.1 0.97 [left|right]      # move + press + release
//! drag 0.3 0.3 0.6 0.6             # press at A, move to B, release
//! key ctrl-alt-t                   # press a key combination (QEMU qcodes)
//! type "hello world"               # type ASCII text
//! expect-serial "PASS"             # fail unless the log contains the text
//! reject-serial "panic"            # fail if the log contains the text
//! fail-on "PANIC"                  # abort later waits as soon as the text appears
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::qemu::{self, QemuInstall, VmConfig};
use crate::qmp::Qmp;
use crate::util::{self, Result};

/// A running headless VM under QMP control.
pub struct Session {
    child: Child,
    pub qmp: Qmp,
    pub serial_log: PathBuf,
    /// Serial log patterns that abort any wait immediately.
    pub fail_patterns: Vec<String>,
}

impl Session {
    pub fn start(install: &QemuInstall, disk: &Path, mut vm: VmConfig) -> Result<Self> {
        let out = util::out_dir();
        let serial_log = out.join("serial.log");
        let _ = std::fs::remove_file(&serial_log);
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            l.local_addr().map_err(|e| e.to_string())?.port()
        };
        vm.display = false;
        vm.serial_file = Some(serial_log.clone());
        vm.qmp_port = Some(port);
        vm.debug_exit = true;
        let vars = qemu::vars_file(install)?;
        let mut cmd = qemu::command(install, disk, &vars, &vm);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
        let child = cmd.spawn().map_err(|e| format!("starting QEMU: {e}"))?;
        let qmp = Qmp::connect(port, Duration::from_secs(20))?;
        Ok(Session { child, qmp, serial_log, fail_patterns: Vec::new() })
    }

    pub fn serial(&self) -> String {
        std::fs::read(&self.serial_log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    }

    /// Waits until the serial log contains `needle`.
    pub fn wait_serial(&mut self, needle: &str, timeout: Duration) -> Result {
        let start = Instant::now();
        loop {
            let log = self.serial();
            if log.contains(needle) {
                return Ok(());
            }
            if let Some(p) = self.fail_patterns.iter().find(|p| log.contains(p.as_str())) {
                return Err(format!("serial log contains failure pattern \"{p}\""));
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("QEMU exited ({status}) before \"{needle}\" appeared"));
            }
            if start.elapsed() > timeout {
                return Err(format!("timed out after {}s waiting for \"{needle}\"", timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn finish(mut self) {
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

/// Splits a script line into words, honouring double quotes.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
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

/// Runs an automation script against a fresh VM booted from `disk`.
pub fn run_script(install: &QemuInstall, disk: &Path, vm: VmConfig, script: &str) -> Result<String> {
    let mut s = Session::start(install, disk, vm)?;
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
                "shot" => {
                    let path = PathBuf::from(w.get(1).ok_or("missing file name")?);
                    let path = if path.is_absolute() { path } else { util::workspace_root().join(path) };
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    s.qmp.screenshot(&path).map_err(ctx)?;
                    util::status("Screenshot", path.display());
                }
                "move" => s.qmp.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?,
                "click" => {
                    let button = w.get(3).map(String::as_str).unwrap_or("left");
                    s.qmp.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.qmp.mouse_button(button, true).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.qmp.mouse_button(button, false).map_err(ctx)?;
                }
                "drag" => {
                    s.qmp.move_mouse(num(&w, 1)?, num(&w, 2)?).map_err(ctx)?;
                    std::thread::sleep(Duration::from_millis(80));
                    s.qmp.mouse_button("left", true).map_err(ctx)?;
                    for step in 1..=10 {
                        let t = step as f64 / 10.0;
                        let x = num(&w, 1)? + (num(&w, 3)? - num(&w, 1)?) * t;
                        let y = num(&w, 2)? + (num(&w, 4)? - num(&w, 2)?) * t;
                        s.qmp.move_mouse(x, y).map_err(ctx)?;
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    s.qmp.mouse_button("left", false).map_err(ctx)?;
                }
                "fail-on" => s.fail_patterns.push(w.get(1).ok_or("missing text")?.clone()),
                "key" => s.qmp.send_keys(w.get(1).ok_or("missing key")?).map_err(ctx)?,
                "type" => s.qmp.type_text(w.get(1).ok_or("missing text")?).map_err(ctx)?,
                "expect-serial" => {
                    let needle = w.get(1).ok_or("missing text")?;
                    if !s.serial().contains(needle.as_str()) {
                        return Err(ctx(format!("serial log does not contain \"{needle}\"")));
                    }
                }
                "reject-serial" => {
                    let needle = w.get(1).ok_or("missing text")?;
                    if s.serial().contains(needle.as_str()) {
                        return Err(ctx(format!("serial log contains \"{needle}\"")));
                    }
                }
                other => return Err(ctx(format!("unknown command '{other}'"))),
            }
        }
        Ok(())
    })();
    let log = s.serial();
    s.finish();
    result.map(|_| log.clone()).map_err(|e| format!("{e}\n--- serial log ---\n{}", tail(&log, 60)))
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
