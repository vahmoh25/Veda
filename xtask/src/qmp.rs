//! A tiny client for the QEMU Machine Protocol (QMP), used to automate a
//! running virtual machine: screenshots, keyboard/mouse input, shutdown.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::util::Result;

pub struct Qmp {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Qmp {
    /// Connects to QEMU's QMP server, retrying until `timeout` expires (QEMU
    /// needs a moment to open the socket after starting).
    pub fn connect(port: u16, timeout: Duration) -> Result<Self> {
        let start = Instant::now();
        let stream = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => break s,
                Err(e) if start.elapsed() > timeout => return Err(format!("cannot connect to QMP: {e}")),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let mut qmp = Qmp { reader: BufReader::new(stream.try_clone().map_err(|e| e.to_string())?), writer: stream };
        qmp.read_line()?; // greeting
        qmp.execute("qmp_capabilities", "{}")?;
        Ok(qmp)
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        self.reader.read_line(&mut line).map_err(|e| format!("QMP read: {e}"))?;
        if line.is_empty() {
            return Err("QMP connection closed".into());
        }
        Ok(line)
    }

    /// Executes a command and waits for its result, skipping async events.
    pub fn execute(&mut self, command: &str, arguments: &str) -> Result<String> {
        let msg = format!("{{\"execute\":\"{command}\",\"arguments\":{arguments}}}\n");
        self.writer.write_all(msg.as_bytes()).map_err(|e| format!("QMP write: {e}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains("\"return\"") {
                return Ok(line);
            }
            if line.contains("\"error\"") {
                return Err(format!("QMP {command} failed: {}", line.trim()));
            }
        }
    }

    pub fn screenshot(&mut self, path: &Path) -> Result {
        let path = path.to_string_lossy().replace('\\', "/");
        self.execute("screendump", &format!("{{\"filename\":\"{path}\",\"format\":\"png\"}}")).map(|_| ())
    }

    /// Presses and releases a key combination given in QEMU's `qcode` names
    /// joined by `-` (e.g. `ctrl-alt-t`, `ret`, `a`).
    pub fn send_keys(&mut self, combo: &str) -> Result {
        let keys: Vec<String> = combo.split('-').map(|k| format!("{{\"type\":\"qcode\",\"data\":\"{k}\"}}")).collect();
        self.execute("send-key", &format!("{{\"keys\":[{}]}}", keys.join(","))).map(|_| ())
    }

    /// Types ASCII text by sending one key press per character.
    pub fn type_text(&mut self, text: &str) -> Result {
        for c in text.chars() {
            let combo = match c {
                'a'..='z' | '0'..='9' => c.to_string(),
                'A'..='Z' => format!("shift-{}", c.to_ascii_lowercase()),
                ' ' => "spc".into(),
                '\n' => "ret".into(),
                '.' => "dot".into(),
                ',' => "comma".into(),
                '-' => "minus".into(),
                '/' => "slash".into(),
                '!' => "shift-1".into(),
                '?' => "shift-slash".into(),
                ':' => "shift-semicolon".into(),
                ';' => "semicolon".into(),
                '\'' => "apostrophe".into(),
                '(' => "shift-9".into(),
                ')' => "shift-0".into(),
                '\t' => "tab".into(),
                '=' => "equal".into(),
                '+' => "shift-equal".into(),
                '_' => "shift-minus".into(),
                '[' => "bracket_left".into(),
                ']' => "bracket_right".into(),
                '{' => "shift-bracket_left".into(),
                '}' => "shift-bracket_right".into(),
                '\\' => "backslash".into(),
                '|' => "shift-backslash".into(),
                '"' => "shift-apostrophe".into(),
                '<' => "shift-comma".into(),
                '>' => "shift-dot".into(),
                '`' => "grave_accent".into(),
                '~' => "shift-grave_accent".into(),
                '@' => "shift-2".into(),
                '#' => "shift-3".into(),
                '$' => "shift-4".into(),
                '%' => "shift-5".into(),
                '^' => "shift-6".into(),
                '&' => "shift-7".into(),
                '*' => "shift-8".into(),
                _ => continue,
            };
            self.send_keys(&combo)?;
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(())
    }

    /// Moves the absolute pointer (virtio-tablet) to a position given as a
    /// fraction of the screen (0.0 ..= 1.0).
    pub fn move_mouse(&mut self, fx: f64, fy: f64) -> Result {
        let (x, y) = ((fx.clamp(0.0, 1.0) * 32767.0) as u32, (fy.clamp(0.0, 1.0) * 32767.0) as u32);
        let events = format!(
            "{{\"events\":[{{\"type\":\"abs\",\"data\":{{\"axis\":\"x\",\"value\":{x}}}}},\
             {{\"type\":\"abs\",\"data\":{{\"axis\":\"y\",\"value\":{y}}}}}]}}"
        );
        self.execute("input-send-event", &events).map(|_| ())
    }

    /// Presses or releases a mouse button (`left`, `right`, `middle`).
    pub fn mouse_button(&mut self, button: &str, down: bool) -> Result {
        let events =
            format!("{{\"events\":[{{\"type\":\"btn\",\"data\":{{\"button\":\"{button}\",\"down\":{down}}}}}]}}");
        self.execute("input-send-event", &events).map(|_| ())
    }

    /// Presses or releases one key (QEMU qcode), e.g. to hold a modifier.
    pub fn key_event(&mut self, qcode: &str, down: bool) -> Result {
        let events = format!(
            "{{\"events\":[{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":{{\"type\":\"qcode\",\"data\":\"{qcode}\"}}}}}}]}}"
        );
        self.execute("input-send-event", &events).map(|_| ())
    }

    pub fn quit(&mut self) {
        let _ = self.execute("quit", "{}");
    }
}
