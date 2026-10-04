//! Parser for `\VEDA\BOOT.CFG`, a tiny `key=value` file:
//!
//! ```text
//! # comment
//! resolution=1280x800
//! cmdline=log=debug
//! ```

use bootinfo::CMDLINE_MAX;

pub struct Config {
    pub resolution: Option<(u32, u32)>,
    pub cmdline: [u8; CMDLINE_MAX],
    pub cmdline_len: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { resolution: Some((1280, 800)), cmdline: [0; CMDLINE_MAX], cmdline_len: 0 }
    }
}

fn parse_resolution(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w >= 640 && h >= 480).then_some((w, h))
}

impl Config {
    pub fn parse(text: &str) -> Config {
        let mut cfg = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            match key.trim() {
                "resolution" => cfg.resolution = parse_resolution(value),
                "cmdline" => {
                    let v = value.trim().as_bytes();
                    let n = v.len().min(CMDLINE_MAX);
                    cfg.cmdline[..n].copy_from_slice(&v[..n]);
                    cfg.cmdline_len = n;
                }
                _ => {}
            }
        }
        cfg
    }
}
