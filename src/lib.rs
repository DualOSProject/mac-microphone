//! "Mac Microphone": a Mac's microphone as an ordinary Windows sound input (part of DualOS).
//!
//! No kernel driver of our own: a virtual USB Audio Class 2.0 microphone is served over USB/IP on
//! 127.0.0.1 (usbip-audio, MIT), the Microsoft-certified usbip-win2 client attaches it, and Windows
//! binds its own `usbaudio2.sys`. It is there while DualOS runs (attached at start, detached at
//! quit; if DualOS dies, the connection closes and Windows sees it unplugged), like the virtual
//! displays.
//!
//! The Mac's samples arrive through [`dualos_mic_push`] (48 kHz mono PCM16) into a small buffer
//! that Windows reads at its own pace. [`dualos_mic_in_use`] tells whether a Windows app is
//! recording right now, so the Mac's microphone runs only then.

use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use usbip_audio::uac2::{Uac2Config, Uac2Device, Uac2Streams};
use usbip_audio::{Clock, MonotonicClock, Server};

pub const SAMPLE_RATE: u32 = 48_000;
const BUS_ID: &str = "1-1";
const CAPACITY: usize = SAMPLE_RATE as usize; // 1 s
/// Sound buffered before Windows gets it, so a late block from the Mac does not cut it: starts at
/// 20 ms, 5 ms more after each gap (up to 80 ms), 2.5 ms less after 10 s without one (down to 15 ms).
const START_TARGET: u64 = 960;
const MIN_TARGET: u64 = 720;
const MAX_TARGET: u64 = 3_840;
const CALM_NS: u64 = 10_000_000_000;
/// More than this over the target (the Mac's clock ahead of the PC's, or a burst) is dropped.
const DROP_ABOVE_TARGET: u64 = 2_880; // 60 ms
/// Standing excess kept anyway (the Mac sends in bursts of ~10 ms).
const STANDING_SLACK: u64 = 480; // 10 ms
const WINDOW_NS: u64 = 2_000_000_000;
/// A Windows app counts as recording while it read within this time.
const IN_USE_NS: u64 = 500_000_000;

#[derive(Default)]
struct Ring {
    samples: Vec<i16>,
    read: u64,
    write: u64,
    priming: bool,
    /// Least sound buffered since `window_start_ns`: a standing excess over the target (left by a
    /// burst) is dropped every 2 s, so the delay does not stay high.
    window_min: u64,
    window_start_ns: u64,
    /// Buffered sound waited for before Windows gets it: grows after a gap, shrinks while calm.
    target: u64,
    last_underrun_ns: u64,
    last_shrink_ns: u64,
}

/// Counters for the caller's log line.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct MicStats {
    pub pushed_frames: u64,
    pub read_frames: u64,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub buffered_frames: u64,
    pub target_frames: u64,
    /// Largest single read by Windows, and longest wait between two blocks from the Mac, since
    /// the last stats call.
    pub max_read_frames: u64,
    pub max_push_gap_us: u64,
    /// Reads that did not continue where the last one ended (Windows skipped or repeated time),
    /// and the frames skipped in total.
    pub read_jumps: u64,
    pub read_jump_frames: u64,
}

pub struct Feed {
    ring: Mutex<Ring>,
    clock: Arc<MonotonicClock>,
    last_read_ns: AtomicU64,
    tone: AtomicBool,
    stats: Mutex<MicStats>,
    last_push_ns: AtomicU64,
    next_read_frame: AtomicU64,
}

impl Feed {
    fn new(clock: Arc<MonotonicClock>) -> Self {
        Self {
            ring: Mutex::new(Ring {
                samples: vec![0; CAPACITY],
                priming: true,
                target: START_TARGET,
                ..Default::default()
            }),
            clock,
            last_read_ns: AtomicU64::new(0),
            tone: AtomicBool::new(false),
            stats: Mutex::new(MicStats::default()),
            last_push_ns: AtomicU64::new(0),
            next_read_frame: AtomicU64::new(0),
        }
    }

    pub fn push(&self, samples: &[i16]) {
        let now = self.clock.now_ns();
        let previous = self.last_push_ns.swap(now, Ordering::AcqRel);
        if previous != 0 {
            let gap = now.saturating_sub(previous) / 1_000;
            let mut stats = self.stats.lock().unwrap();
            stats.max_push_gap_us = stats.max_push_gap_us.max(gap);
        }
        let mut ring = self.ring.lock().unwrap();
        for &sample in samples {
            let slot = (ring.write % CAPACITY as u64) as usize;
            ring.samples[slot] = sample;
            ring.write += 1;
        }
        if ring.write - ring.read > CAPACITY as u64 {
            ring.read = ring.write - CAPACITY as u64;
        }
        self.stats.lock().unwrap().pushed_frames += samples.len() as u64;
    }

    pub fn in_use(&self) -> bool {
        let last = self.last_read_ns.load(Ordering::Acquire);
        last != 0 && self.clock.now_ns().saturating_sub(last) < IN_USE_NS
    }

    pub fn stats(&self) -> MicStats {
        let mut guard = self.stats.lock().unwrap();
        let mut stats = *guard;
        guard.max_read_frames = 0;
        guard.max_push_gap_us = 0;
        drop(guard);
        let ring = self.ring.lock().unwrap();
        stats.buffered_frames = ring.write - ring.read;
        stats.target_frames = ring.target;
        stats
    }
}

impl Uac2Streams for Feed {
    fn playback(&self, _frame: u64, _samples: &[i32]) {}

    fn capture(&self, frame: u64, samples: &mut [i32]) {
        let now = self.clock.now_ns().max(1);
        self.last_read_ns.store(now, Ordering::Release);
        let expected = self.next_read_frame.swap(frame + samples.len() as u64, Ordering::AcqRel);
        if expected != 0 && expected != frame {
            let mut stats = self.stats.lock().unwrap();
            stats.read_jumps += 1;
            stats.read_jump_frames += frame.abs_diff(expected);
        }
        if self.tone.load(Ordering::Relaxed) {
            // Test tone: 440 Hz at -20 dBFS.
            for (index, sample) in samples.iter_mut().enumerate() {
                let t = (frame + index as u64) as f64 / SAMPLE_RATE as f64;
                *sample = ((t * 440.0 * std::f64::consts::TAU).sin() * 0.1 * i32::MAX as f64) as i32 & !0xff;
            }
            self.stats.lock().unwrap().read_frames += samples.len() as u64;
            return;
        }
        let mut ring = self.ring.lock().unwrap();
        let mut stats = self.stats.lock().unwrap();
        let buffered = ring.write - ring.read;
        let target = ring.target;
        if ring.priming && buffered >= target {
            ring.priming = false;
        }
        if !ring.priming && buffered > target + DROP_ABOVE_TARGET {
            let drop = buffered - target;
            ring.read += drop;
            stats.dropped_frames += drop;
        }
        if !ring.priming {
            let buffered = ring.write - ring.read;
            if ring.window_start_ns == 0 {
                ring.window_start_ns = now;
                ring.window_min = buffered;
            }
            ring.window_min = ring.window_min.min(buffered);
            if now - ring.window_start_ns >= WINDOW_NS {
                if ring.window_min > target + STANDING_SLACK {
                    let drop = ring.window_min - target;
                    ring.read += drop;
                    stats.dropped_frames += drop;
                }
                ring.window_start_ns = now;
                ring.window_min = u64::MAX;
            }
        } else {
            ring.window_start_ns = 0;
        }
        let mut ran_dry = false;
        for sample in samples.iter_mut() {
            if !ring.priming && ring.read < ring.write {
                let slot = (ring.read % CAPACITY as u64) as usize;
                *sample = (ring.samples[slot] as i32) << 16;
                ring.read += 1;
            } else {
                *sample = 0;
                ran_dry = !ring.priming;
            }
        }
        if ran_dry {
            ring.priming = true;
            if ring.write > 0 {
                stats.underruns += 1;
                ring.target = (ring.target + 240).min(MAX_TARGET);
                ring.last_underrun_ns = now;
            }
        } else if now.saturating_sub(ring.last_underrun_ns) > CALM_NS
            && now.saturating_sub(ring.last_shrink_ns) > CALM_NS
            && ring.target > MIN_TARGET
        {
            ring.target = ring.target.saturating_sub(120).max(MIN_TARGET);
            ring.last_shrink_ns = now;
        }
        stats.read_frames += samples.len() as u64;
        stats.max_read_frames = stats.max_read_frames.max(samples.len() as u64);
    }
}

pub struct Mic {
    server: Option<Server>,
    feed: Arc<Feed>,
    usbip: PathBuf,
    port: Option<u32>,
}

impl Mic {
    /// Serves the microphone and attaches it to Windows.
    pub fn start(name: &str, serial: &str) -> Result<Self, String> {
        let usbip = find_usbip().ok_or("usbip-win2 is not installed")?;
        let clock = Arc::new(MonotonicClock::new());
        let feed = Arc::new(Feed::new(clock.clone()));
        let config = Uac2Config {
            // pid.codes: open-source VID 0x1209, PID 0xD05C (Mac Microphone).
            vendor_id: 0x1209,
            product_id: 0xD05C,
            bcd_device: 0x0100,
            manufacturer: "DualOS".into(),
            product: name.into(),
            serial: serial.into(),
            sample_rate: SAMPLE_RATE,
            playback_channels: 0,
            capture_channels: 1,
            playback_name: None,
            capture_name: Some(name.into()),
            capture_connector: false,
        };
        let device = Uac2Device::new(config, feed.clone())?;
        let server = Server::start(0, clock).map_err(|e| format!("USB/IP server: {e}"))?;
        server.add(BUS_ID, Arc::new(device));
        let port = attach(&usbip, server.port())?;
        Ok(Self { server: Some(server), feed, usbip, port: Some(port) })
    }

    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    pub fn set_tone(&self, on: bool) {
        self.feed.tone.store(on, Ordering::Relaxed);
    }

    /// Unplugs it from Windows and stops serving.
    pub fn stop(&mut self) {
        if let Some(server) = &self.server {
            server.remove(BUS_ID);
        }
        if let Some(port) = self.port.take() {
            let _ = run(&self.usbip, &["detach", "--port", &port.to_string()]);
        }
        self.server = None;
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop();
    }
}

fn find_usbip() -> Option<PathBuf> {
    ["ProgramW6432", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| PathBuf::from(base).join("USBip").join("usbip.exe"))
        .find(|path| path.is_file())
}

fn run(usbip: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(usbip);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().map_err(|e| format!("could not run usbip: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if output.status.success() { Ok(text) } else { Err(format!("usbip {}: {}", args.join(" "), text.trim())) }
}

fn attach(usbip: &Path, tcp_port: u16) -> Result<u32, String> {
    let port = tcp_port.to_string();
    let text = run(usbip, &["--tcp-port", &port, "attach", "--remote", "127.0.0.1", "--bus-id", BUS_ID, "--terse"])?;
    text.trim().lines().last().unwrap_or("").trim().parse().map_err(|_| format!("usbip attach returned {text:?}"))
}

// ---- C interface for the Windows app ----

/// Starts "Mac Microphone". Returns null on failure, with the reason in `error` (UTF-8, cut to fit).
#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_start(
    name: *const c_char,
    serial: *const c_char,
    error: *mut u8,
    error_capacity: usize,
) -> *mut Mic {
    let text = |pointer: *const c_char, fallback: &str| {
        if pointer.is_null() {
            fallback.to_owned()
        } else {
            unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
        }
    };
    match Mic::start(&text(name, "Mac Microphone"), &text(serial, "0001")) {
        Ok(mic) => Box::into_raw(Box::new(mic)),
        Err(message) => {
            if !error.is_null() && error_capacity > 0 {
                let bytes = message.as_bytes();
                let length = bytes.len().min(error_capacity - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), error, length);
                    *error.add(length) = 0;
                }
            }
            std::ptr::null_mut()
        }
    }
}

/// 48 kHz mono PCM16 from the Mac.
#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_push(mic: *mut Mic, samples: *const i16, count: usize) {
    if mic.is_null() || samples.is_null() || count == 0 {
        return;
    }
    let mic = unsafe { &*mic };
    mic.feed.push(unsafe { std::slice::from_raw_parts(samples, count) });
}

/// 1 while a Windows app records from it.
#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_in_use(mic: *mut Mic) -> i32 {
    if mic.is_null() {
        return 0;
    }
    i32::from(unsafe { &*mic }.feed.in_use())
}

#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_stats(mic: *mut Mic, out: *mut MicStats) {
    if mic.is_null() || out.is_null() {
        return;
    }
    unsafe { *out = (*mic).feed.stats() };
}

#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_set_tone(mic: *mut Mic, on: i32) {
    if !mic.is_null() {
        unsafe { &*mic }.set_tone(on != 0);
    }
}

/// Unplugs it and frees it.
#[unsafe(no_mangle)]
pub extern "C" fn dualos_mic_stop(mic: *mut Mic) {
    if !mic.is_null() {
        drop(unsafe { Box::from_raw(mic) });
    }
}
