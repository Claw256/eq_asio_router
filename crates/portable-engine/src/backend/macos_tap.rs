//! System-audio capture backend (macOS) — [`MacTap`] implements the
//! [`SystemSource`](crate::seams::SystemSource) seam over a Core Audio **input**
//! device, per the A5 DECIDED strategy (docs/plans/cross-platform.md §A5):
//!
//! * **v1 (this module): capture from the user-installed BlackHole loopback
//!   device.** BlackHole is a normal Core Audio input device, so the capture path
//!   is the same plain Core Audio input path as the output in [`super::macos`]:
//!   `AudioDeviceCreateIOProcID` + `AudioDeviceStart` on the chosen input device,
//!   f32 planes pushed into the ring (`router_core::ring::Producer`). We do
//!   **not** bundle or link BlackHole (GPLv3 — redistribution wording is an OPEN
//!   librarian item); users install it themselves and we discover it by UID/name.
//! * **Phase C fallback (documented, not built here): ScreenCaptureKit** (13+)
//!   per-app/system capture for setups without BlackHole — needs Screen Recording
//!   permission and has a weaker latency story (A5 rank 1 con).
//! * **Phase C release path (documented, not built here): a minimal in-house
//!   Audio Server Plug-in loopback** in `~/Library/Audio/Plug-Ins/HAL` (A5 rank 2)
//!   — MIT-licensable and self-contained, but C++/Obj-C + signing + `coreaudiod`
//!   restart work that needs real Mac hardware to develop.
//!
//! Device discovery ([`MacTap::open`], [`MacTap::open_with`]) is shared with the
//! output sink (`super::macos::pick_tap_index`): exact UID → exact name →
//! case-insensitive name-contains "BlackHole" (covers "BlackHole 2ch" /
//! "BlackHole 16ch") → the system default input device, unless that is the
//! default *output* device (never capture our own output). Failing all of that
//! returns [`SourceError::Unavailable`] with BlackHole install guidance
//! ([`BLACKHOLE_INSTALL_HINT`]).
//!
//! Arrival statistics per the seam contract: every IO cycle feeds
//! [`ArrivalTracker::on_frames`] (`router-core/src/arrival.rs:236`) with a
//! `mach_absolute_time`-based timestamp and beats `Heartbeat::beat_capture`
//! (`router-core/src/watchdog.rs:60`); `fill_snapshot()` returns
//! `ArrivalStats::snapshot()`. Device disconnects latch
//! [`DeviceWatch::fault`](super::macos::DeviceWatch) and stop all arrival feeds
//! so the watchdog reports the stall.

use std::ffi::c_void;

use router_core::arrival::{ArrivalSnapshot, ArrivalStats, ArrivalTracker};
use router_core::ring::Producer;
use router_core::watchdog::Heartbeat;

use super::macos::{
    abl_buffer, add_removal_listeners, buffer_plane, configure_f32_streams, default_device_id, discover_devices,
    input_streams, mach_absolute_time, mach_ticks_per_second, negotiate_rate_buffer, pick_tap_index,
    remove_removal_listeners, status_text, ticks_to_seconds, AudioBufferList, AudioDeviceID, AudioDeviceIOProcID,
    DeviceSelector, DeviceWatch, OSStatus,
};
use crate::seams::{SourceError, StreamRequest, SystemSource};

/// Error guidance returned when no loopback input device can be discovered.
pub const BLACKHOLE_INSTALL_HINT: &str =
    "no loopback input device found; install BlackHole (https://github.com/ExistentialAudio/BlackHole) \
     or pass an explicit device UID/name to MacTap::open_with";

/// Heap context the input IOProc gets as `inClientData`. Allocation-free at RT.
pub struct TapContext {
    pub producer: Option<Producer>,
    pub tracker: ArrivalTracker,
    pub heartbeat: Heartbeat,
    pub watch: Arc<DeviceWatch>,
    /// `mach_absolute_time` ticks per second (arrival timestamp conversion).
    pub tps: i64,
}

use std::sync::Arc;

unsafe extern "C" fn input_io_proc(
    _device: AudioDeviceID,
    _now: *const c_void,
    in_data: *const AudioBufferList,
    _in_time: *const c_void,
    _out: *mut AudioBufferList,
    _out_time: *const c_void,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() || in_data.is_null() {
        return 0;
    }
    let ctx = &mut *(client as *mut TapContext);
    let now_ticks = mach_absolute_time() as i64;

    if ctx.watch.faulted() {
        // Device removed: stop feeding ring/arrival/heartbeat so the watchdog
        // sees the stall instead of phantom captures.
        return 0;
    }

    let buffers = (*in_data).mNumberBuffers;
    if buffers == 0 {
        return 0;
    }
    // Frames in this packet, derived from the first buffer's layout.
    let first = abl_buffer(in_data, 0);
    let frames = if first.mNumberChannels <= 1 {
        (first.mDataByteSize as usize) / 4
    } else {
        (first.mDataByteSize as usize) / (4 * first.mNumberChannels as usize)
    };
    if frames == 0 || first.mData.is_null() {
        return 0;
    }

    // The first two planes become the stereo pair (mono duplicates its plane).
    {
        let p0 = buffer_plane(abl_buffer(in_data, 0), frames);
        if buffers >= 2 {
            let p1 = buffer_plane(abl_buffer(in_data, 1), frames);
            if let Some(p) = ctx.producer.as_mut() {
                p.push_frames(p0, p1);
            }
        } else if let Some(p) = ctx.producer.as_mut() {
            p.push_frames(p0, p0);
        }
    }

    ctx.tracker.on_frames(ticks_to_seconds(now_ticks, ctx.tps), frames as u32);
    ctx.heartbeat.beat_capture();
    0
}

/// System-audio tap ([`SystemSource`]) over a BlackHole-style Core Audio input
/// device. `open` → `attach` → `start` → `stop`.
pub struct MacTap {
    pub device: AudioDeviceID,
    pub uid: String,
    pub name: String,
    pub rate: u32,
    pub buffer_frames: u32,
    pub ctx: Box<TapContext>,
    pub proc_id: AudioDeviceIOProcID,
    pub started: bool,
    pub watch: Arc<DeviceWatch>,
}

impl MacTap {
    /// Open the loopback input device with default BlackHole discovery (see the
    /// module doc) and negotiate `want.rate_hz` / `want.buffer_frames` strictly,
    /// exactly like [`super::macos::MacSink`].
    pub fn open(want: &StreamRequest) -> Result<MacTap, SourceError> {
        Self::open_with(want, &DeviceSelector::default())
    }

    /// Open with an explicit device selector (exact UID / name first, then the
    /// BlackHole + default-input fallbacks).
    pub fn open_with(want: &StreamRequest, selector: &DeviceSelector) -> Result<MacTap, SourceError> {
        let devices = discover_devices().map_err(SourceError::Unavailable)?;
        let default_input_uid = default_device_id(false)
            .ok()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.uid.clone()));
        let default_output_uid = default_device_id(true)
            .ok()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.uid.clone()));
        let idx = pick_tap_index(
            &devices,
            selector.uid.as_deref(),
            selector.name.as_deref(),
            default_input_uid.as_deref(),
            default_output_uid.as_deref(),
        )
        .ok_or_else(|| SourceError::Unavailable(BLACKHOLE_INSTALL_HINT.to_string()))?;
        let info = &devices[idx];

        let (rate, buffer_frames) =
            negotiate_rate_buffer(info.id, want.rate_hz, want.buffer_frames).map_err(SourceError::Unavailable)?;
        let streams = input_streams(info.id).map_err(SourceError::Unavailable)?;
        if streams.is_empty() {
            return Err(SourceError::Unavailable(format!(
                "device '{}' reports no input streams",
                info.name
            )));
        }
        configure_f32_streams(&streams).map_err(SourceError::Unavailable)?;

        let watch = Arc::new(DeviceWatch::new(info.id));
        let stats = ArrivalStats::new(f64::from(rate));
        Ok(MacTap {
            device: info.id,
            uid: info.uid.clone(),
            name: info.name.clone(),
            rate,
            buffer_frames,
            ctx: Box::new(TapContext {
                producer: None,
                tracker: ArrivalTracker::new(stats),
                heartbeat: Heartbeat::new(),
                watch: watch.clone(),
                tps: mach_ticks_per_second(),
            }),
            proc_id: std::ptr::null_mut(),
            started: false,
            watch,
        })
    }

    /// Install the input IOProc and start capture.
    pub fn start(&mut self) -> Result<(), SourceError> {
        if self.started {
            return Err(SourceError::Attach("tap already streaming".into()));
        }
        if self.watch.faulted() {
            return Err(SourceError::Unavailable(format!("device '{}' is gone", self.name)));
        }
        let client = &mut *self.ctx as *mut TapContext as *mut c_void;
        let mut proc_id: AudioDeviceIOProcID = std::ptr::null_mut();
        let s = unsafe { super::macos::AudioDeviceCreateIOProcID(self.device, Some(input_io_proc), client, &mut proc_id) };
        if s != 0 {
            return Err(SourceError::Unavailable(format!(
                "AudioDeviceCreateIOProcID: OSStatus {}",
                status_text(s)
            )));
        }
        self.proc_id = proc_id;
        if let Err(e) = add_removal_listeners(&self.watch) {
            unsafe { super::macos::AudioDeviceDestroyIOProcID(self.device, self.proc_id) };
            self.proc_id = std::ptr::null_mut();
            return Err(SourceError::Unavailable(e));
        }
        let s = unsafe { super::macos::AudioDeviceStart(self.device, self.proc_id) };
        if s != 0 {
            remove_removal_listeners(&self.watch);
            unsafe { super::macos::AudioDeviceDestroyIOProcID(self.device, self.proc_id) };
            self.proc_id = std::ptr::null_mut();
            return Err(SourceError::Unavailable(format!(
                "AudioDeviceStart: OSStatus {}",
                status_text(s)
            )));
        }
        self.started = true;
        Ok(())
    }

    /// Stop capture and tear the IOProc down. Reports device removal as
    /// [`SourceError::Unavailable`] so supervision can react.
    pub fn stop(&mut self) -> Result<(), SourceError> {
        if !self.started {
            return Ok(());
        }
        let s = unsafe { super::macos::AudioDeviceStop(self.device, self.proc_id) };
        remove_removal_listeners(&self.watch);
        unsafe { super::macos::AudioDeviceDestroyIOProcID(self.device, self.proc_id) };
        self.proc_id = std::ptr::null_mut();
        self.started = false;
        if self.watch.faulted() {
            return Err(SourceError::Unavailable(format!(
                "device '{}' was removed or died during capture",
                self.name
            )));
        }
        if s != 0 {
            return Err(SourceError::Unavailable(format!("AudioDeviceStop: OSStatus {}", status_text(s))));
        }
        Ok(())
    }

    /// Did the loopback device disappear? (latched by the removal listeners)
    pub fn faulted(&self) -> bool {
        self.watch.faulted()
    }
}

impl SystemSource for MacTap {
    fn negotiated_rate(&self) -> u32 {
        self.rate
    }

    fn attach(&mut self, ring: Producer) -> Result<(), SourceError> {
        if self.started {
            return Err(SourceError::Attach("cannot attach while the tap is streaming".into()));
        }
        self.ctx.producer = Some(ring);
        Ok(())
    }

    fn arrival(&mut self) -> &mut ArrivalTracker {
        &mut self.ctx.tracker
    }

    fn fill_snapshot(&mut self) -> ArrivalSnapshot {
        self.ctx.tracker.stats().snapshot()
    }

    fn heartbeat(&self) -> &Heartbeat {
        &self.ctx.heartbeat
    }
}
