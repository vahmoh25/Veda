//! Variables in OVMF's variable store, as the firmware keeps them, for the
//! tests of what Veda does with a PC's firmware variables: a script's
//! `firmware-variable NAME GUID ATTRIBUTES DATA` adds one to the store its
//! run starts with.
//!
//! The store (EDK II's, in the flash image OVMF's `OVMF_VARS` file is) is a
//! firmware volume whose header is followed by the variable store's, then
//! the variables, each a header, its name (UTF-16, NUL-terminated) and its
//! data, the next header 4-byte aligned; the free space after them is all
//! ones. Its headers are authenticated variables' or plain ones', as the
//! store's GUID says.

use crate::util::Result;

/// A variable to add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// Its vendor, as UEFI lays a GUID out.
    pub guid: [u8; 16],
    pub attributes: u32,
    pub data: Vec<u8>,
}

/// The firmware volume of the variables (`EFI_SYSTEM_NV_DATA_FV_GUID`).
const NV_DATA_FV: &str = "fff12b8d-7696-4c8b-a985-2747075b4f50";
/// The store of authenticated variables' headers (`gEfiAuthenticatedVariableGuid`).
const AUTHENTICATED_STORE: &str = "aaf32c78-947b-439a-a180-2e144ec37792";
/// The store of plain headers (`gEfiVariableGuid`).
const PLAIN_STORE: &str = "ddcf3616-3275-4164-98b6-fe85707ffe7d";
/// What starts a variable's header.
const START_ID: u16 = 0x55AA;
/// A variable's state when it is there (`VAR_ADDED`).
const ADDED: u8 = 0x3F;
/// The store header: its GUID, size, format, state and two reserved fields.
const STORE_HEADER: usize = 28;

/// A GUID as it is written (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`), as
/// UEFI lays it out (its first three fields little-endian).
pub fn guid(s: &str) -> Option<[u8; 16]> {
    let parts: Vec<&str> = s.split('-').collect();
    let [a, b, c, d, e] = parts[..] else { return None };
    if [a.len(), b.len(), c.len(), d.len(), e.len()] != [8, 4, 4, 4, 12] {
        return None;
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&u32::from_str_radix(a, 16).ok()?.to_le_bytes());
    out[4..6].copy_from_slice(&u16::from_str_radix(b, 16).ok()?.to_le_bytes());
    out[6..8].copy_from_slice(&u16::from_str_radix(c, 16).ok()?.to_le_bytes());
    for (i, byte) in out[8..].iter_mut().enumerate() {
        let digits = if i < 2 { &d[i * 2..i * 2 + 2] } else { &e[(i - 2) * 2..(i - 2) * 2 + 2] };
        *byte = u8::from_str_radix(digits, 16).ok()?;
    }
    Some(out)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Adds `variables` to the variable store of `image` (an `OVMF_VARS`
/// file's contents), after those it has.
pub fn add(image: &mut [u8], variables: &[Variable]) -> Result<()> {
    let bad = || "not OVMF's variable store".to_string();
    if image.get(0x10..0x20) != Some(&guid(NV_DATA_FV).unwrap()[..]) || image.get(0x28..0x2C) != Some(b"_FVH") {
        return Err(bad());
    }
    let store = u16_at(image, 0x30).ok_or_else(bad)? as usize;
    let kind = image.get(store..store + 16).ok_or_else(bad)?;
    let header = if kind == guid(AUTHENTICATED_STORE).unwrap() {
        60
    } else if kind == guid(PLAIN_STORE).unwrap() {
        32
    } else {
        return Err(bad());
    };
    let end = store + u32_at(image, store + 16).ok_or_else(bad)? as usize;
    if end > image.len() {
        return Err(bad());
    }
    // Past the variables the store has.
    let mut at = (store + STORE_HEADER).next_multiple_of(4);
    while u16_at(image, at) == Some(START_ID) {
        let sizes = at + header - 24;
        let (name, data) = (u32_at(image, sizes).ok_or_else(bad)?, u32_at(image, sizes + 4).ok_or_else(bad)?);
        at = (at + header + name as usize + data as usize).next_multiple_of(4);
    }
    for v in variables {
        let name: Vec<u8> = v.name.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let size = header + name.len() + v.data.len();
        let room = image.get_mut(at..at + size).filter(|_| at + size <= end);
        let room = room.ok_or_else(|| format!("no room in the variable store for {}", v.name))?;
        if room.iter().any(|&b| b != 0xFF) {
            return Err(format!("the variable store's free space is not free where {} goes", v.name));
        }
        room.fill(0);
        room[..2].copy_from_slice(&START_ID.to_le_bytes());
        room[2] = ADDED;
        room[4..8].copy_from_slice(&v.attributes.to_le_bytes());
        // The monotonic count, time stamp and public key index of an
        // authenticated variable's header stay zero.
        let sizes = header - 24;
        room[sizes..sizes + 4].copy_from_slice(&(name.len() as u32).to_le_bytes());
        room[sizes + 4..sizes + 8].copy_from_slice(&(v.data.len() as u32).to_le_bytes());
        room[sizes + 8..header].copy_from_slice(&v.guid);
        room[header..header + name.len()].copy_from_slice(&name);
        room[header + name.len()..].copy_from_slice(&v.data);
        at = (at + size).next_multiple_of(4);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty store as OVMF's template has it: the firmware volume's
    /// header (72 bytes, with its block map), the store's, free space.
    fn template(store: &str) -> Vec<u8> {
        let mut image = vec![0xFF; 0x2000];
        image[..0x48].fill(0);
        image[0x10..0x20].copy_from_slice(&guid(NV_DATA_FV).unwrap());
        image[0x20..0x28].copy_from_slice(&0x2000u64.to_le_bytes());
        image[0x28..0x2C].copy_from_slice(b"_FVH");
        image[0x30..0x32].copy_from_slice(&0x48u16.to_le_bytes());
        image[0x48..0x58].copy_from_slice(&guid(store).unwrap());
        image[0x58..0x5C].copy_from_slice(&0x1000u32.to_le_bytes());
        image[0x5C] = 0x5A;
        image[0x5D] = 0xFE;
        image[0x5E..0x64].fill(0);
        image
    }

    fn variable(name: &str, data: &[u8]) -> Variable {
        Variable {
            name: name.into(),
            guid: guid("02f9af02-7734-4233-b43d-93fe5aa35db3").unwrap(),
            attributes: 7,
            data: data.to_vec(),
        }
    }

    #[test]
    fn guids_are_laid_out_as_uefi_does() {
        assert_eq!(
            guid("02f9af02-7734-4233-b43d-93fe5aa35db3").unwrap(),
            [0x02, 0xaf, 0xf9, 0x02, 0x34, 0x77, 0x33, 0x42, 0xb4, 0x3d, 0x93, 0xfe, 0x5a, 0xa3, 0x5d, 0xb3]
        );
        assert_eq!(guid("02f9af02-7734-4233-b43d"), None);
        assert_eq!(guid("02f9af0-27734-4233-b43d-93fe5aa35db3"), None);
        assert_eq!(guid("02f9af02-7734-4233-b43d-93fe5aa35dbx"), None);
    }

    #[test]
    fn variables_follow_each_other_after_the_store_header() {
        let mut image = template(AUTHENTICATED_STORE);
        add(&mut image, &[variable("Ab", &[1, 2, 3])]).unwrap();
        // The first at 0x64: its header, "Ab\0" in UTF-16, the data.
        let v = &image[0x64..];
        assert_eq!(&v[..4], &[0xAA, 0x55, ADDED, 0]);
        assert_eq!(u32_at(v, 4), Some(7));
        assert_eq!(u32_at(v, 36), Some(6));
        assert_eq!(u32_at(v, 40), Some(3));
        assert_eq!(&v[44..60], &variable("", &[]).guid);
        assert_eq!(&v[60..69], &[b'A', 0, b'b', 0, 0, 0, 1, 2, 3]);
        assert!(v[69..].iter().all(|&b| b == 0xFF));
        // The next after it, 4-byte aligned, past those it finds.
        add(&mut image, &[variable("C", &[9])]).unwrap();
        let next = 0x64 + (60 + 9usize).next_multiple_of(4);
        assert_eq!(&image[next..next + 3], &[0xAA, 0x55, ADDED]);
        assert_eq!(&image[next + 60..next + 65], &[b'C', 0, 0, 0, 9]);
    }

    #[test]
    fn plain_stores_have_shorter_headers() {
        let mut image = template(PLAIN_STORE);
        add(&mut image, &[variable("A", &[5]), variable("B", &[6])]).unwrap();
        let v = &image[0x64..];
        assert_eq!(u32_at(v, 8), Some(4));
        assert_eq!(u32_at(v, 12), Some(1));
        assert_eq!(&v[32..37], &[b'A', 0, 0, 0, 5]);
        assert_eq!(&v[40..43], &[0xAA, 0x55, ADDED]);
    }

    #[test]
    fn what_is_not_a_store_or_does_not_fit_is_refused() {
        let mut image = template(AUTHENTICATED_STORE);
        image[0x28] = b'X';
        assert!(add(&mut image, &[]).is_err());
        let mut image = template("00000000-0000-0000-0000-000000000000");
        assert!(add(&mut image, &[]).is_err());
        let mut image = template(AUTHENTICATED_STORE);
        assert!(add(&mut image, &[variable("Big", &[0; 0x1000])]).is_err());
    }
}
