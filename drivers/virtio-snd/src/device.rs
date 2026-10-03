//! The virtio-snd device (virtio 1.2, section 5.14): configuration space,
//! the control queue and PCM stream management.
//!
//! Control requests are rare (set-up, start and stop), so the control
//! queue is polled: a request is a device-readable header followed by a
//! device-writable response whose first word is a status code.

use alloc::vec::Vec;
use core::fmt;

use vproto::pci::pcidev;
use vrt::time::{Duration, deadline_after, now_ns, sleep};
use vvirtio::{DmaBuffer, Segment, VirtioError, Virtqueue};

/// Queue indices.
pub const CONTROLQ: u16 = 0;
pub const EVENTQ: u16 = 1;
pub const TXQ: u16 = 2;
pub const RXQ: u16 = 3;

/// Request codes.
pub const R_JACK_INFO: u32 = 1;
pub const R_PCM_INFO: u32 = 0x0100;
pub const R_PCM_SET_PARAMS: u32 = 0x0101;
pub const R_PCM_PREPARE: u32 = 0x0102;
pub const R_PCM_RELEASE: u32 = 0x0103;
pub const R_PCM_START: u32 = 0x0104;
pub const R_PCM_STOP: u32 = 0x0105;
pub const R_CHMAP_INFO: u32 = 0x0200;

/// Event codes (event queue).
pub const EVT_JACK_CONNECTED: u32 = 0x1000;
pub const EVT_JACK_DISCONNECTED: u32 = 0x1001;
pub const EVT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
pub const EVT_PCM_XRUN: u32 = 0x1101;

/// Status codes.
pub const S_OK: u32 = 0x8000;
pub const S_BAD_MSG: u32 = 0x8001;
pub const S_NOT_SUPP: u32 = 0x8002;
pub const S_IO_ERR: u32 = 0x8003;

/// Stream directions.
pub const D_OUTPUT: u8 = 0;
pub const D_INPUT: u8 = 1;
/// Sample format: signed 16-bit little-endian.
pub const FMT_S16: u8 = 5;
/// Frame rates by their index in the `rates` bitmap.
pub const RATES: [u32; 14] =
    [5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000, 384000];

const CONTROL_QUEUE_SIZE: u16 = 16;
const REQUEST_AREA: usize = 2048;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Driver failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverError {
    Virtio(VirtioError),
    /// The device did not answer a control request in time.
    Timeout(u32),
    /// The device answered a control request with an error status.
    Status(u32, u32),
    /// No output stream supports 16-bit PCM.
    NoOutputStream,
    /// The device config space is missing or too small.
    NoConfig,
}

impl From<VirtioError> for DriverError {
    fn from(e: VirtioError) -> Self {
        DriverError::Virtio(e)
    }
}

fn status_name(s: u32) -> &'static str {
    match s {
        S_OK => "OK",
        S_BAD_MSG => "BAD_MSG",
        S_NOT_SUPP => "NOT_SUPP",
        S_IO_ERR => "IO_ERR",
        _ => "unknown status",
    }
}

fn request_name(code: u32) -> &'static str {
    match code {
        R_JACK_INFO => "JACK_INFO",
        R_PCM_INFO => "PCM_INFO",
        R_PCM_SET_PARAMS => "PCM_SET_PARAMS",
        R_PCM_PREPARE => "PCM_PREPARE",
        R_PCM_RELEASE => "PCM_RELEASE",
        R_PCM_START => "PCM_START",
        R_PCM_STOP => "PCM_STOP",
        R_CHMAP_INFO => "CHMAP_INFO",
        _ => "request",
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Virtio(e) => write!(f, "virtio transport error: {e:?}"),
            DriverError::Timeout(c) => write!(f, "{} timed out", request_name(*c)),
            DriverError::Status(c, s) => write!(f, "{} failed: {}", request_name(*c), status_name(*s)),
            DriverError::NoOutputStream => f.write_str("no output stream supports S16 PCM"),
            DriverError::NoConfig => f.write_str("device configuration is missing"),
        }
    }
}

/// `struct virtio_snd_pcm_info`.
#[derive(Debug, Clone, Copy)]
pub struct PcmInfo {
    pub id: u32,
    pub features: u32,
    pub formats: u64,
    pub rates: u64,
    pub direction: u8,
    pub channels_min: u8,
    pub channels_max: u8,
}

/// The configuration chosen for the output stream.
#[derive(Debug, Clone, Copy)]
pub struct PcmConfig {
    pub stream: u32,
    pub rate: u32,
    pub rate_index: u8,
    pub channels: u8,
    pub period_frames: u32,
    /// Periods the device buffer holds (we keep up to this many queued).
    pub periods: u32,
}

impl PcmConfig {
    pub fn frame_bytes(&self) -> usize {
        self.channels as usize * 2
    }

    pub fn period_bytes(&self) -> usize {
        self.period_frames as usize * self.frame_bytes()
    }
}

/// Picks the rate (48 kHz preferred, then 44.1 kHz, then the closest
/// supported rate), the channel count (stereo if possible) and the period
/// size (about 10 ms, a power of two) for an output stream.
pub fn choose_config(info: &PcmInfo, periods: u32) -> Option<PcmConfig> {
    if info.direction != D_OUTPUT || info.formats & (1 << FMT_S16) == 0 || info.rates == 0 {
        return None;
    }
    let supported = |i: usize| info.rates & (1 << i) != 0;
    let rate_index = if supported(7) {
        7
    } else if supported(6) {
        6
    } else {
        // Closest to 48 kHz, preferring higher rates.
        (0..RATES.len())
            .filter(|&i| supported(i))
            .min_by_key(|&i| (RATES[i] as i64 - 48_000).abs() * 2 - (RATES[i] >= 48_000) as i64)?
    };
    let rate = RATES[rate_index];
    let (lo, hi) = (info.channels_min.max(1), info.channels_max.max(1));
    let channels = 2u8.clamp(lo, hi.max(lo));
    if channels > 2 {
        return None; // we only produce mono or stereo
    }
    let period_frames = (rate / 100).next_power_of_two().clamp(64, 4096);
    Some(PcmConfig { stream: info.id, rate, rate_index: rate_index as u8, channels, period_frames, periods })
}

/// Picks the configuration of an input (capture) stream: 48 kHz preferred
/// (what host audio systems record natively), then 44.1 or 16 kHz, mono if
/// possible, periods of about 10 ms.
pub fn choose_input_config(info: &PcmInfo) -> Option<PcmConfig> {
    if info.direction != D_INPUT || info.formats & (1 << FMT_S16) == 0 || info.rates == 0 {
        return None;
    }
    let supported = |i: usize| info.rates & (1 << i) != 0;
    let rate_index = [7usize, 6, 3]
        .into_iter()
        .find(|&i| supported(i))
        .or_else(|| (0..RATES.len()).filter(|&i| supported(i) && RATES[i] <= 48_000).max_by_key(|&i| RATES[i]))?;
    let rate = RATES[rate_index];
    let (lo, hi) = (info.channels_min.max(1), info.channels_max.max(1));
    if lo > 2 {
        return None;
    }
    let channels = 1u8.clamp(lo, hi.max(lo));
    let period_frames = (rate / 100).next_power_of_two().clamp(64, 4096);
    Some(PcmConfig { stream: info.id, rate, rate_index: rate_index as u8, channels, period_frames, periods: 4 })
}

/// The device and its control queue.
pub struct SndDevice {
    pub dev: vvirtio::Device,
    control: Virtqueue,
    ctl: DmaBuffer,
    pub jacks: u32,
    pub streams: u32,
    pub chmaps: u32,
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le64(b: &[u8], off: usize) -> u64 {
    le32(b, off) as u64 | (le32(b, off + 4) as u64) << 32
}

impl SndDevice {
    /// Resets the device, negotiates features (none beyond VERSION_1) and
    /// sets up the control queue. The caller sets up the other queues and
    /// then calls `driver_ok`.
    pub fn new(pci: pcidev::Client) -> Result<SndDevice, DriverError> {
        let dev = vvirtio::Device::new(pci)?;
        dev.initialize(0)?;
        let jacks = dev.cfg_read32(0);
        let streams = dev.cfg_read32(4);
        let chmaps = dev.cfg_read32(8);
        let control = dev.setup_queue(CONTROLQ, CONTROL_QUEUE_SIZE)?;
        let ctl = DmaBuffer::new(dev.dma(), 2 * REQUEST_AREA)?;
        Ok(SndDevice { dev, control, ctl, jacks, streams, chmaps })
    }

    /// Sends one control request and returns the response payload (after
    /// the status word).
    pub fn request(&mut self, req: &[u8], payload_len: usize) -> Result<Vec<u8>, DriverError> {
        let code = le32(req, 0);
        let resp_len = 4 + payload_len;
        assert!(req.len() <= REQUEST_AREA && resp_len <= REQUEST_AREA);
        self.ctl.write(0, req);
        self.ctl.write(REQUEST_AREA, &[0xFF; 4]);
        let chain = [
            Segment { phys: self.ctl.phys(), len: req.len() as u32, device_writes: false },
            Segment { phys: self.ctl.phys() + REQUEST_AREA as u64, len: resp_len as u32, device_writes: true },
        ];
        // Collect stale completions of earlier (timed out) requests.
        while self.control.pop_used().is_some() {}
        let head = self.control.push(&chain).ok_or(DriverError::Virtio(VirtioError::BadQueue))?;
        self.control.notify();
        let deadline = deadline_after(CONTROL_TIMEOUT);
        loop {
            if let Some((h, _len)) = self.control.pop_used() {
                if h == head {
                    break;
                }
                continue;
            }
            if now_ns() >= deadline {
                return Err(DriverError::Timeout(code));
            }
            sleep(Duration::from_micros(200));
        }
        // SAFETY: the device has completed the request.
        let resp = unsafe { self.ctl.bytes(REQUEST_AREA, resp_len) };
        let status = le32(resp, 0);
        if status != S_OK {
            return Err(DriverError::Status(code, status));
        }
        Ok(resp[4..].to_vec())
    }

    fn simple(&mut self, code: u32, stream: u32) -> Result<(), DriverError> {
        let mut req = [0u8; 8];
        req[0..4].copy_from_slice(&code.to_le_bytes());
        req[4..8].copy_from_slice(&stream.to_le_bytes());
        self.request(&req, 0).map(|_| ())
    }

    fn query(&mut self, code: u32, count: u32, size: u32) -> Result<Vec<u8>, DriverError> {
        let mut req = [0u8; 16];
        req[0..4].copy_from_slice(&code.to_le_bytes());
        req[8..12].copy_from_slice(&count.to_le_bytes());
        req[12..16].copy_from_slice(&size.to_le_bytes());
        self.request(&req, (count * size) as usize)
    }

    /// Information about every PCM stream.
    pub fn pcm_info(&mut self) -> Result<Vec<PcmInfo>, DriverError> {
        const SIZE: u32 = 32;
        let count = self.streams.min((REQUEST_AREA as u32 - 4) / SIZE);
        if count == 0 {
            return Ok(Vec::new());
        }
        let data = self.query(R_PCM_INFO, count, SIZE)?;
        Ok(data
            .as_chunks::<{ SIZE as usize }>()
            .0
            .iter()
            .enumerate()
            .map(|(i, c)| PcmInfo {
                id: i as u32,
                features: le32(c, 4),
                formats: le64(c, 8),
                rates: le64(c, 16),
                direction: c[24],
                channels_min: c[25],
                channels_max: c[26],
            })
            .collect())
    }

    /// `(hda_fn_nid, connected)` of every jack.
    pub fn jack_info(&mut self) -> Result<Vec<(u32, bool)>, DriverError> {
        const SIZE: u32 = 24;
        let count = self.jacks.min((REQUEST_AREA as u32 - 4) / SIZE);
        if count == 0 {
            return Ok(Vec::new());
        }
        let data = self.query(R_JACK_INFO, count, SIZE)?;
        Ok(data.as_chunks::<{ SIZE as usize }>().0.iter().map(|c| (le32(c, 0), c[16] != 0)).collect())
    }

    /// `(direction, channels)` of every channel map.
    pub fn chmap_info(&mut self) -> Result<Vec<(u8, u8)>, DriverError> {
        const SIZE: u32 = 24;
        let count = self.chmaps.min((REQUEST_AREA as u32 - 4) / SIZE);
        if count == 0 {
            return Ok(Vec::new());
        }
        let data = self.query(R_CHMAP_INFO, count, SIZE)?;
        Ok(data.as_chunks::<{ SIZE as usize }>().0.iter().map(|c| (c[4], c[5])).collect())
    }

    pub fn set_params(&mut self, cfg: &PcmConfig) -> Result<(), DriverError> {
        let mut req = [0u8; 24];
        req[0..4].copy_from_slice(&R_PCM_SET_PARAMS.to_le_bytes());
        req[4..8].copy_from_slice(&cfg.stream.to_le_bytes());
        let period = cfg.period_bytes() as u32;
        req[8..12].copy_from_slice(&(period * cfg.periods).to_le_bytes());
        req[12..16].copy_from_slice(&period.to_le_bytes());
        // features (16..20) = 0
        req[20] = cfg.channels;
        req[21] = FMT_S16;
        req[22] = cfg.rate_index;
        self.request(&req, 0).map(|_| ())
    }

    pub fn prepare(&mut self, stream: u32) -> Result<(), DriverError> {
        self.simple(R_PCM_PREPARE, stream)
    }

    pub fn start(&mut self, stream: u32) -> Result<(), DriverError> {
        self.simple(R_PCM_START, stream)
    }

    pub fn stop(&mut self, stream: u32) -> Result<(), DriverError> {
        self.simple(R_PCM_STOP, stream)
    }

    pub fn release(&mut self, stream: u32) -> Result<(), DriverError> {
        self.simple(R_PCM_RELEASE, stream)
    }
}
