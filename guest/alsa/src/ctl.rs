//! The sound card's mixer, through ALSA's control interface
//! (`/dev/snd/controlC*`): its output path at unity gain, unmuted. Veda's
//! audio service sets the volume itself, as for Veda's own drivers, whose
//! codecs' outputs are set so too.
//!
//! Only output controls are touched (`Master`, `PCM`, the outputs' own):
//! the loopbacks a mixer may have (a microphone or a line input played
//! back) stay as they are. Of the inputs, an audio DSP's digital
//! microphones are unmuted at unity gain (SOF's `Dmic0`, whose switch
//! starts off); a codec's input is as its driver sets it.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use guest_sys::{ioc, ioctl};

/// The controls of the output path, by the first word(s) of their names.
const OUTPUTS: [&str; 10] =
    ["Master", "PCM", "Front", "Surround", "Center", "LFE", "Side", "Speaker", "Headphone", "Line Out"];
/// The controls of the inputs set, likewise: SOF's digital microphones'
/// (its `DMIC` device's).
const INPUTS: [&str; 1] = ["Dmic0"];

const IFACE_MIXER: i32 = 2;
const TYPE_BOOLEAN: i32 = 1;
const TYPE_INTEGER: i32 = 2;
const ACCESS_WRITE: u32 = 1 << 1;
const ACCESS_TLV_READ: u32 = 1 << 4;

/// TLV types of dB scales.
const TLV_DB_SCALE: u32 = 1;
const TLV_DB_MINMAX: u32 = 4;
const TLV_DB_MINMAX_MUTE: u32 = 5;

/// `snd_ctl_elem_id`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ElemId {
    numid: u32,
    iface: i32,
    device: u32,
    subdevice: u32,
    name: [u8; 44],
    index: u32,
}

/// `snd_ctl_elem_list`.
#[repr(C)]
struct ElemList {
    offset: u32,
    space: u32,
    used: u32,
    count: u32,
    pids: usize,
    reserved: [u8; 50],
}

/// `snd_ctl_elem_info`, its value an integer's range.
#[repr(C)]
struct ElemInfo {
    id: ElemId,
    kind: i32,
    access: u32,
    count: u32,
    owner: i32,
    min: i64,
    max: i64,
    step: i64,
    value_rest: [u8; 104],
    reserved: [u8; 64],
}

/// `snd_ctl_elem_value`, its values integers.
#[repr(C)]
struct ElemValue {
    id: ElemId,
    indirect: u32,
    values: [i64; 128],
    reserved: [u8; 128],
}

/// `snd_ctl_tlv`, with room for its data.
#[repr(C)]
struct Tlv {
    numid: u32,
    length: u32,
    data: [u32; 64],
}

const _: () = assert!(size_of::<ElemId>() == 64);
const _: () = assert!(size_of::<ElemList>() == 80);
const _: () = assert!(size_of::<ElemInfo>() == 272);
const _: () = assert!(size_of::<ElemValue>() == 1224);

const ELEM_LIST: u32 = ioc(3, b'U', 0x10, size_of::<ElemList>());
const ELEM_INFO: u32 = ioc(3, b'U', 0x11, size_of::<ElemInfo>());
const ELEM_WRITE: u32 = ioc(3, b'U', 0x13, size_of::<ElemValue>());
const TLV_READ: u32 = ioc(3, b'U', 0x1A, 8);

fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-data structures above.
    unsafe { core::mem::zeroed() }
}

fn name(id: &ElemId) -> String {
    let end = id.name.iter().position(|&b| b == 0).unwrap_or(id.name.len());
    String::from_utf8_lossy(&id.name[..end]).into_owned()
}

/// The value at which a volume control is at 0 dB, from its dB scale, or
/// its highest value if that is lower or the scale is not known.
fn unity(fd: RawFd, info: &ElemInfo) -> i64 {
    let (min, max) = (info.min, info.max);
    if info.access & ACCESS_TLV_READ == 0 || max <= min {
        return max;
    }
    let mut tlv = Tlv { numid: info.id.numid, length: size_of::<[u32; 64]>() as u32, data: [0; 64] };
    // SAFETY: the kernel writes at most `length` bytes of the scale.
    if unsafe { ioctl(fd, TLV_READ, &mut tlv as *mut Tlv as usize) }.is_err() {
        return max;
    }
    let [kind, _len, a, b, ..] = tlv.data;
    let value = match kind {
        // The lowest value's dB, and the dB of a step (in hundredths).
        TLV_DB_SCALE => {
            let step = (b & 0xFFFF) as i64;
            (step > 0).then(|| min + (-(a as i32) as i64) / step)
        }
        // The lowest and highest values' dB, the steps between even.
        TLV_DB_MINMAX | TLV_DB_MINMAX_MUTE => {
            let (low, high) = (a as i32 as i64, b as i32 as i64);
            (high > low).then(|| min + (-low) * (max - min) / (high - low))
        }
        _ => None,
    };
    value.map_or(max, |v| v.clamp(min, max))
}

/// Sets the card's output controls (`/dev/snd/controlC{card}`): switches
/// on, volumes at 0 dB. Returns the names of the controls it set.
pub fn open_outputs(card: &str) -> io::Result<Vec<String>> {
    open(card, &OUTPUTS, "Playback")
}

/// Sets the card's input controls of [`INPUTS`] so too.
pub fn open_inputs(card: &str) -> io::Result<Vec<String>> {
    open(card, &INPUTS, "Capture")
}

/// Sets the controls of card `card` named `PATH STREAM Switch` and
/// `PATH STREAM Volume`, a path one of `paths` (its first word(s)):
/// switches on, volumes at 0 dB.
fn open(card: &str, paths: &[&str], stream: &str) -> io::Result<Vec<String>> {
    let file = OpenOptions::new().read(true).write(true).open(format!("/dev/snd/controlC{card}"))?;
    let fd = file.as_raw_fd();
    let mut list: ElemList = zeroed();
    // SAFETY: with no room for ids, the kernel only counts them.
    unsafe { ioctl(fd, ELEM_LIST, &mut list as *mut ElemList as usize)? };
    let mut ids: Vec<ElemId> = (0..list.count).map(|_| zeroed()).collect();
    list.space = list.count;
    list.pids = ids.as_mut_ptr() as usize;
    // SAFETY: room for `space` ids, which the kernel writes.
    unsafe { ioctl(fd, ELEM_LIST, &mut list as *mut ElemList as usize)? };
    ids.truncate(list.used as usize);

    let mut set = Vec::new();
    for id in ids {
        let n = name(&id);
        let on_path = paths.iter().any(|p| n.starts_with(p) && n[p.len()..].starts_with(' '));
        let (switch, volume) = (n.ends_with(&format!("{stream} Switch")), n.ends_with(&format!("{stream} Volume")));
        if id.iface != IFACE_MIXER || !on_path || !(switch || volume) {
            continue;
        }
        let mut info: ElemInfo = zeroed();
        info.id = id;
        // SAFETY: the kernel fills in the element's information.
        if unsafe { ioctl(fd, ELEM_INFO, &mut info as *mut ElemInfo as usize) }.is_err()
            || info.access & ACCESS_WRITE == 0
        {
            continue;
        }
        let value = match info.kind {
            TYPE_BOOLEAN => 1,
            TYPE_INTEGER => unity(fd, &info),
            _ => continue,
        };
        let mut v: ElemValue = zeroed();
        v.id = id;
        let count = (info.count as usize).min(v.values.len());
        v.values[..count].fill(value);
        // SAFETY: the element's values, which the kernel reads.
        if unsafe { ioctl(fd, ELEM_WRITE, &mut v as *mut ElemValue as usize) }.is_ok() {
            set.push(n);
        }
    }
    Ok(set)
}
