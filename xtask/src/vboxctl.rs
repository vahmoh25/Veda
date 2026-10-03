//! Driving a running VirtualBox machine for automation scripts: keys as
//! PS/2 set-1 scan codes, typed text, screenshots, and the mouse.
//!
//! `VBoxManage` has no mouse command, so a helper PowerShell process holds
//! a session on the machine through VirtualBox's COM API and forwards mouse
//! events. The guest has a PS/2 mouse, which only moves relatively and by at
//! most 255 counts per event: positions are reached by first pushing the
//! pointer into the top-left corner, then stepping towards the target (the
//! compositor moves the pointer one pixel per count, without acceleration).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use crate::util::Result;
use crate::vbox::{VBox, VM};

/// Largest relative step sent at once. PS/2 deltas are 9-bit signed, and
/// VirtualBox adds up the events the guest has not read yet and clamps the
/// sum to that range, so steps stay well below it and are paced.
const STEP: i32 = 100;
/// Time for the guest to read one PS/2 packet before the next event.
const STEP_PAUSE: Duration = Duration::from_millis(25);

/// The make code of a key (QEMU qcode name) and whether it has the `E0`
/// prefix.
fn scancode(qcode: &str) -> Option<(u8, bool)> {
    let letters =
        b"\x1e\x30\x2e\x20\x12\x21\x22\x23\x17\x24\x25\x26\x32\x31\x18\x19\x10\x13\x1f\x14\x16\x2f\x11\x2d\x15\x2c";
    if qcode.len() == 1 {
        let c = qcode.as_bytes()[0];
        return match c {
            b'a'..=b'z' => Some((letters[(c - b'a') as usize], false)),
            b'1'..=b'9' => Some((0x02 + (c - b'1'), false)),
            b'0' => Some((0x0B, false)),
            _ => None,
        };
    }
    if let Some(n) = qcode.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return match n {
            1..=10 => Some((0x3A + n, false)),
            11 => Some((0x57, false)),
            12 => Some((0x58, false)),
            _ => None,
        };
    }
    Some(match qcode {
        "esc" => (0x01, false),
        "minus" => (0x0C, false),
        "equal" => (0x0D, false),
        "backspace" => (0x0E, false),
        "tab" => (0x0F, false),
        "bracket_left" => (0x1A, false),
        "bracket_right" => (0x1B, false),
        "ret" => (0x1C, false),
        "ctrl" | "ctrl_l" => (0x1D, false),
        "semicolon" => (0x27, false),
        "apostrophe" => (0x28, false),
        "grave_accent" => (0x29, false),
        "shift" | "shift_l" => (0x2A, false),
        "backslash" => (0x2B, false),
        "comma" => (0x33, false),
        "dot" => (0x34, false),
        "slash" => (0x35, false),
        "shift_r" => (0x36, false),
        "kp_multiply" => (0x37, false),
        "alt" | "alt_l" => (0x38, false),
        "spc" => (0x39, false),
        "caps_lock" => (0x3A, false),
        "num_lock" => (0x45, false),
        "scroll_lock" => (0x46, false),
        "kp_7" => (0x47, false),
        "kp_8" => (0x48, false),
        "kp_9" => (0x49, false),
        "kp_subtract" => (0x4A, false),
        "kp_4" => (0x4B, false),
        "kp_5" => (0x4C, false),
        "kp_6" => (0x4D, false),
        "kp_add" => (0x4E, false),
        "kp_1" => (0x4F, false),
        "kp_2" => (0x50, false),
        "kp_3" => (0x51, false),
        "kp_0" => (0x52, false),
        "kp_decimal" => (0x53, false),
        "kp_enter" => (0x1C, true),
        "ctrl_r" => (0x1D, true),
        "kp_divide" => (0x35, true),
        "alt_r" => (0x38, true),
        "home" => (0x47, true),
        "up" => (0x48, true),
        "pgup" => (0x49, true),
        "left" => (0x4B, true),
        "right" => (0x4D, true),
        "end" => (0x4F, true),
        "down" => (0x50, true),
        "pgdn" => (0x51, true),
        "insert" => (0x52, true),
        "delete" => (0x53, true),
        "meta_l" => (0x5B, true),
        "meta_r" => (0x5C, true),
        "menu" => (0x5D, true),
        _ => return None,
    })
}

fn key_bytes(qcode: &str, down: bool) -> Result<Vec<u8>> {
    let (code, extended) = scancode(qcode).ok_or_else(|| format!("unknown key '{qcode}'"))?;
    let mut v = Vec::with_capacity(2);
    if extended {
        v.push(0xE0);
    }
    v.push(if down { code } else { code | 0x80 });
    Ok(v)
}

/// The key combination for an ASCII character (US layout), as QEMU typing
/// uses it.
fn char_combo(c: char) -> Option<String> {
    Some(match c {
        'a'..='z' | '0'..='9' => c.to_string(),
        'A'..='Z' => format!("shift-{}", c.to_ascii_lowercase()),
        ' ' => "spc".into(),
        '\n' => "ret".into(),
        '\t' => "tab".into(),
        '.' => "dot".into(),
        ',' => "comma".into(),
        '-' => "minus".into(),
        '/' => "slash".into(),
        ';' => "semicolon".into(),
        '\'' => "apostrophe".into(),
        '=' => "equal".into(),
        '[' => "bracket_left".into(),
        ']' => "bracket_right".into(),
        '\\' => "backslash".into(),
        '`' => "grave_accent".into(),
        '!' => "shift-1".into(),
        '@' => "shift-2".into(),
        '#' => "shift-3".into(),
        '$' => "shift-4".into(),
        '%' => "shift-5".into(),
        '^' => "shift-6".into(),
        '&' => "shift-7".into(),
        '*' => "shift-8".into(),
        '(' => "shift-9".into(),
        ')' => "shift-0".into(),
        '_' => "shift-minus".into(),
        '+' => "shift-equal".into(),
        '{' => "shift-bracket_left".into(),
        '}' => "shift-bracket_right".into(),
        '|' => "shift-backslash".into(),
        ':' => "shift-semicolon".into(),
        '"' => "shift-apostrophe".into(),
        '<' => "shift-comma".into(),
        '>' => "shift-dot".into(),
        '?' => "shift-slash".into(),
        '~' => "shift-grave_accent".into(),
        _ => return None,
    })
}

/// The scan codes that press and release a combination such as
/// `ctrl-alt-t` (keys pressed in order, released in reverse).
fn combo_bytes(combo: &str) -> Result<Vec<u8>> {
    let keys: Vec<&str> = combo.split('-').collect();
    let mut v = Vec::new();
    for k in &keys {
        v.extend(key_bytes(k, true)?);
    }
    for k in keys.iter().rev() {
        v.extend(key_bytes(k, false)?);
    }
    Ok(v)
}

const MOUSE_HELPER: &str = r#"
$ErrorActionPreference = 'Stop'
$client = New-Object -ComObject VirtualBox.VirtualBoxClient
$machine = $client.VirtualBox.FindMachine('__VM__')
$session = $client.Session
$machine.LockMachine($session, 1)
$mouse = $session.Console.Mouse
[Console]::Out.WriteLine('ready'); [Console]::Out.Flush()
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($line -eq $null -or $line -eq 'quit') { break }
  $p = $line.Split(' ')
  try {
    $mouse.PutMouseEvent([int]$p[0], [int]$p[1], [int]$p[2], 0, [int]$p[3])
    [Console]::Out.WriteLine('ok')
  } catch {
    [Console]::Out.WriteLine('error ' + $_.Exception.Message)
  }
  [Console]::Out.Flush()
}
$session.UnlockMachine()
"#;

/// The helper process forwarding mouse events.
struct MouseHelper {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl MouseHelper {
    fn start() -> Result<MouseHelper> {
        let script = MOUSE_HELPER.replace("__VM__", VM);
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("starting the mouse helper: {e}"))?;
        let input = child.stdin.take().ok_or("no helper stdin")?;
        let mut output = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
        let mut line = String::new();
        output.read_line(&mut line).map_err(|e| e.to_string())?;
        if line.trim() != "ready" {
            let _ = child.kill();
            return Err(format!("the mouse helper did not start ({})", line.trim()));
        }
        Ok(MouseHelper { child, input, output })
    }

    fn event(&mut self, dx: i32, dy: i32, dz: i32, buttons: u32) -> Result {
        writeln!(self.input, "{dx} {dy} {dz} {buttons}").map_err(|e| format!("mouse helper: {e}"))?;
        self.input.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        self.output.read_line(&mut line).map_err(|e| e.to_string())?;
        match line.trim() {
            "ok" => Ok(()),
            other => Err(format!("mouse helper: {other}")),
        }
    }
}

impl Drop for MouseHelper {
    fn drop(&mut self) {
        let _ = writeln!(self.input, "quit");
        let _ = self.input.flush();
        std::thread::sleep(Duration::from_millis(200));
        let _ = self.child.kill();
    }
}

/// Control of the running machine.
pub struct Control {
    vbox: VBox,
    mouse: Option<MouseHelper>,
    /// Where the pointer is, once known.
    pos: Option<(i32, i32)>,
    screen: (i32, i32),
    buttons: u32,
}

impl Control {
    pub fn new(vbox: VBox, screen: (u32, u32)) -> Control {
        Control { vbox, mouse: None, pos: None, screen: (screen.0 as i32, screen.1 as i32), buttons: 0 }
    }

    pub fn vbox(&self) -> &VBox {
        &self.vbox
    }

    fn mouse(&mut self) -> Result<&mut MouseHelper> {
        if self.mouse.is_none() {
            self.mouse = Some(MouseHelper::start()?);
        }
        Ok(self.mouse.as_mut().unwrap())
    }

    fn step(&mut self, dx: i32, dy: i32) -> Result {
        let buttons = self.buttons;
        self.mouse()?.event(dx, dy, 0, buttons)?;
        std::thread::sleep(STEP_PAUSE);
        Ok(())
    }

    /// Moves the pointer to a position given as a fraction of the screen.
    pub fn move_mouse(&mut self, fx: f64, fy: f64) -> Result {
        let (w, h) = self.screen;
        let tx = ((fx.clamp(0.0, 1.0) * w as f64).round() as i32).min(w - 1);
        let ty = ((fy.clamp(0.0, 1.0) * h as f64).round() as i32).min(h - 1);
        let (mut x, mut y) = match self.pos {
            Some(p) => p,
            None => {
                // Into the top-left corner.
                for _ in 0..(w.max(h) / 255 + 2) {
                    self.step(-255, -255)?;
                }
                (0, 0)
            }
        };
        while (x, y) != (tx, ty) {
            let dx = (tx - x).clamp(-STEP, STEP);
            let dy = (ty - y).clamp(-STEP, STEP);
            self.step(dx, dy)?;
            x += dx;
            y += dy;
        }
        self.pos = Some((x, y));
        Ok(())
    }

    /// Presses or releases `left`, `right`, `middle`; `wheel-up`/`wheel-down`
    /// scroll once when pressed.
    pub fn mouse_button(&mut self, button: &str, down: bool) -> Result {
        let dz = match button {
            "wheel-up" => -1,
            "wheel-down" => 1,
            _ => 0,
        };
        if dz != 0 {
            if down {
                let buttons = self.buttons;
                self.mouse()?.event(0, 0, dz, buttons)?;
            }
            return Ok(());
        }
        let bit = match button {
            "left" => 1,
            "right" => 2,
            "middle" => 4,
            other => return Err(format!("unknown mouse button '{other}'")),
        };
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        let buttons = self.buttons;
        self.mouse()?.event(0, 0, 0, buttons)
    }

    pub fn send_keys(&mut self, combo: &str) -> Result {
        self.vbox.scancodes(&combo_bytes(combo)?)
    }

    pub fn key_event(&mut self, qcode: &str, down: bool) -> Result {
        self.vbox.scancodes(&key_bytes(qcode, down)?)
    }

    /// Types ASCII text (US layout), in batches of characters.
    pub fn type_text(&mut self, text: &str) -> Result {
        let mut batch = Vec::new();
        for c in text.chars() {
            if let Some(combo) = char_combo(c) {
                batch.extend(combo_bytes(&combo)?);
            }
            // keyboardputscancode takes the codes as arguments: keep the
            // command line short.
            if batch.len() > 96 {
                self.vbox.scancodes(&batch)?;
                batch.clear();
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        self.vbox.scancodes(&batch)
    }

    pub fn screenshot(&mut self, path: &std::path::Path) -> Result {
        self.vbox.screenshot(path)
    }

    pub fn reset(&mut self) -> Result {
        self.pos = None;
        self.vbox.reset()
    }

    /// Ends the session and powers the machine off.
    pub fn finish(&mut self) {
        self.mouse = None;
        self.vbox.poweroff();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations_press_in_order_and_release_in_reverse() {
        assert_eq!(combo_bytes("ctrl-alt-t").unwrap(), vec![0x1D, 0x38, 0x14, 0x94, 0xB8, 0x9D]);
        assert_eq!(combo_bytes("meta_l-up").unwrap(), vec![0xE0, 0x5B, 0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0xDB]);
        assert_eq!(combo_bytes("ret").unwrap(), vec![0x1C, 0x9C]);
        assert_eq!(combo_bytes("f4").unwrap(), vec![0x3E, 0xBE]);
        assert_eq!(combo_bytes("shift-a").unwrap(), vec![0x2A, 0x1E, 0x9E, 0xAA]);
        assert!(combo_bytes("ctrl-nosuchkey").is_err());
        assert_eq!(char_combo('A').as_deref(), Some("shift-a"));
        assert_eq!(char_combo('|').as_deref(), Some("shift-backslash"));
    }
}
