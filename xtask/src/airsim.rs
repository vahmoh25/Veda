//! Building, starting and controlling `airsim`, the simulated Wi-Fi
//! environment that `--net wifi` runs next to QEMU (see `tools/airsim`).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::qemu::WifiPorts;
use crate::util::{self, Result};

/// The password of the simulated secured networks (see `tools/airsim`).
pub const PASSWORD: &str = "vindows-wifi";

/// Builds the simulator for the host; returns the executable.
pub fn build() -> Result<PathBuf> {
    util::run(util::cargo().args(["build", "--quiet", "--package", "airsim"]))?;
    let exe =
        util::workspace_root().join("target").join("debug").join(format!("airsim{}", std::env::consts::EXE_SUFFIX));
    if exe.is_file() { Ok(exe) } else { Err(format!("{} was not built", exe.display())) }
}

fn free_tcp_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok(l.local_addr().map_err(|e| e.to_string())?.port())
}

fn free_udp_port() -> Result<u16> {
    let s = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok(s.local_addr().map_err(|e| e.to_string())?.port())
}

/// A running simulator. It is stopped when dropped (and exits by itself if
/// this process dies, as it watches its standard input).
pub struct AirSim {
    child: Child,
    pub ports: WifiPorts,
    pub control: u16,
    pub log: PathBuf,
}

impl AirSim {
    /// Picks free local ports and starts the simulator, logging to `log`.
    pub fn start(exe: &Path, log: &Path) -> Result<AirSim> {
        let ports = WifiPorts { radio: free_tcp_port()?, qemu_udp: free_udp_port()?, sim_udp: free_udp_port()? };
        let control = free_tcp_port()?;
        let out = std::fs::File::create(log).map_err(|e| format!("creating {}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let child = Command::new(exe)
            .args(["--radio", &format!("127.0.0.1:{}", ports.radio)])
            .args(["--wired-local", &format!("127.0.0.1:{}", ports.sim_udp)])
            .args(["--wired-remote", &format!("127.0.0.1:{}", ports.qemu_udp)])
            .args(["--control", &format!("127.0.0.1:{control}")])
            .args(["--password", PASSWORD])
            .arg("--exit-with-stdin")
            .stdin(Stdio::piped())
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("starting airsim: {e}"))?;
        let mut sim = AirSim { child, ports, control, log: log.to_path_buf() };
        // Wait until it accepts control connections.
        let start = std::time::Instant::now();
        while sim.command("status").is_err() {
            if let Ok(Some(status)) = sim.child.try_wait() {
                return Err(format!("airsim exited ({status}); see {}", log.display()));
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err("airsim did not start".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(sim)
    }

    /// Sends a control command; returns its answer (without the final
    /// `ok`), or the simulator's error.
    pub fn command(&self, line: &str) -> Result<String> {
        let mut s = TcpStream::connect(("127.0.0.1", self.control)).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        s.write_all(format!("{}\n", line.trim()).as_bytes()).map_err(|e| e.to_string())?;
        let mut answer = String::new();
        for l in BufReader::new(s).lines() {
            let l = l.map_err(|e| e.to_string())?;
            if l == "ok" {
                return Ok(answer);
            }
            if let Some(e) = l.strip_prefix("error: ") {
                return Err(format!("airsim: {e}"));
            }
            answer.push_str(&l);
            answer.push('\n');
        }
        Err("airsim closed the control connection".into())
    }
}

impl Drop for AirSim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
