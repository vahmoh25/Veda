//! The `name` table: font family, style, copyright and license strings.
//!
//! Records are chosen in this order of preference: Windows Unicode English (US), any Windows
//! Unicode record, Unicode platform, Macintosh Roman English, any Macintosh Roman record.
//! Windows/Unicode strings are UTF-16BE; Macintosh strings are Mac OS Roman.

use alloc::string::String;

use crate::parse::u16_at;

/// Well-known name ids.
pub mod name_id {
    /// Copyright notice.
    pub const COPYRIGHT: u16 = 0;
    /// Font family name (legacy, at most four styles per family).
    pub const FAMILY: u16 = 1;
    /// Font subfamily (style) name (legacy).
    pub const SUBFAMILY: u16 = 2;
    /// Unique font identifier.
    pub const UNIQUE_ID: u16 = 3;
    /// Full font name.
    pub const FULL_NAME: u16 = 4;
    /// Version string.
    pub const VERSION: u16 = 5;
    /// PostScript name.
    pub const POSTSCRIPT_NAME: u16 = 6;
    /// Trademark notice.
    pub const TRADEMARK: u16 = 7;
    /// Manufacturer name.
    pub const MANUFACTURER: u16 = 8;
    /// Designer name.
    pub const DESIGNER: u16 = 9;
    /// License description.
    pub const LICENSE: u16 = 13;
    /// License information URL.
    pub const LICENSE_URL: u16 = 14;
    /// Typographic family name.
    pub const TYPOGRAPHIC_FAMILY: u16 = 16;
    /// Typographic subfamily name.
    pub const TYPOGRAPHIC_SUBFAMILY: u16 = 17;
}

/// Mac OS Roman code points 0x80..=0xFF.
const MAC_ROMAN: [u16; 128] = [
    0x00C4, 0x00C5, 0x00C7, 0x00C9, 0x00D1, 0x00D6, 0x00DC, 0x00E1, 0x00E0, 0x00E2, 0x00E4, 0x00E3, 0x00E5, 0x00E7, 0x00E9,
    0x00E8, 0x00EA, 0x00EB, 0x00ED, 0x00EC, 0x00EE, 0x00EF, 0x00F1, 0x00F3, 0x00F2, 0x00F4, 0x00F6, 0x00F5, 0x00FA, 0x00F9,
    0x00FB, 0x00FC, 0x2020, 0x00B0, 0x00A2, 0x00A3, 0x00A7, 0x2022, 0x00B6, 0x00DF, 0x00AE, 0x00A9, 0x2122, 0x00B4, 0x00A8,
    0x2260, 0x00C6, 0x00D8, 0x221E, 0x00B1, 0x2264, 0x2265, 0x00A5, 0x00B5, 0x2202, 0x2211, 0x220F, 0x03C0, 0x222B, 0x00AA,
    0x00BA, 0x03A9, 0x00E6, 0x00F8, 0x00BF, 0x00A1, 0x00AC, 0x221A, 0x0192, 0x2248, 0x2206, 0x00AB, 0x00BB, 0x2026, 0x00A0,
    0x00C0, 0x00C3, 0x00D5, 0x0152, 0x0153, 0x2013, 0x2014, 0x201C, 0x201D, 0x2018, 0x2019, 0x00F7, 0x25CA, 0x00FF, 0x0178,
    0x2044, 0x20AC, 0x2039, 0x203A, 0xFB01, 0xFB02, 0x2021, 0x00B7, 0x201A, 0x201E, 0x2030, 0x00C2, 0x00CA, 0x00C1, 0x00CB,
    0x00C8, 0x00CD, 0x00CE, 0x00CF, 0x00CC, 0x00D3, 0x00D4, 0xF8FF, 0x00D2, 0x00DA, 0x00DB, 0x00D9, 0x0131, 0x02C6, 0x02DC,
    0x00AF, 0x02D8, 0x02D9, 0x02DA, 0x00B8, 0x02DD, 0x02DB, 0x02C7,
];

/// Preference score of a name record (lower is better), `None` if unsupported.
fn score(platform: u16, encoding: u16, language: u16) -> Option<u8> {
    match (platform, encoding) {
        (3, 1) | (3, 10) => Some(if language == 0x0409 { 0 } else { 1 }),
        (0, _) => Some(2),
        (1, 0) => Some(if language == 0 { 3 } else { 4 }),
        _ => None,
    }
}

/// Looks up and decodes the best record with the given name id.
pub(crate) fn lookup(name: &[u8], id: u16) -> Option<String> {
    let count = u16_at(name, 2)? as usize;
    let storage = u16_at(name, 4)? as usize;
    let mut best: Option<(u8, u16, &[u8])> = None;
    for i in 0..count {
        let rec = 6 + i * 12;
        let Some(name_id) = u16_at(name, rec + 6) else { break };
        if name_id != id {
            continue;
        }
        let (Some(p), Some(e), Some(l), Some(len), Some(off)) = (
            u16_at(name, rec),
            u16_at(name, rec + 2),
            u16_at(name, rec + 4),
            u16_at(name, rec + 8),
            u16_at(name, rec + 10),
        ) else {
            break;
        };
        let Some(s) = score(p, e, l) else { continue };
        if best.is_some_and(|(b, _, _)| b <= s) {
            continue;
        }
        let start = storage + off as usize;
        let Some(bytes) = name.get(start..start + len as usize) else { continue };
        best = Some((s, p, bytes));
    }
    let (_, platform, bytes) = best?;
    let mut out = String::new();
    if platform == 1 {
        for &b in bytes {
            let c = if b < 0x80 { b as u32 } else { MAC_ROMAN[(b - 0x80) as usize] as u32 };
            out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
        }
    } else {
        let units = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
        for c in core::char::decode_utf16(units) {
            out.push(c.unwrap_or('\u{FFFD}'));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn decodes_and_prefers_windows_english() {
        // Two records for name id 1: Mac Roman "Caf\x8e" and Windows "Win" (preferred).
        let mac = [b'C', b'a', b'f', 0x8E];
        let win: Vec<u8> = "Wïn".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut d = Vec::new();
        for v in [0u16, 2, 6 + 24] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        for v in [1u16, 0, 0, 1, mac.len() as u16, 0] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        for v in [3u16, 1, 0x409, 1, win.len() as u16, mac.len() as u16] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&mac);
        d.extend_from_slice(&win);
        assert_eq!(lookup(&d, 1).as_deref(), Some("Wïn"));
        assert_eq!(lookup(&d, 2), None);
        // Mac-only lookup through the Roman table.
        let mut m = d.clone();
        m[6 + 12 + 6] = 0;
        m[6 + 12 + 7] = 9; // second record becomes name id 9
        assert_eq!(lookup(&m, 1).as_deref(), Some("Café"));
        for n in 0..d.len() {
            let _ = lookup(&d[..n], 1);
        }
    }
}
