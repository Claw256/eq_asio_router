//! Windows 10 2004+ **process loopback** capture (`VAD\Process_Loopback` via
//! `ActivateAudioInterfaceAsync`): the pre-mix audio of one process tree.
//!
//! Read-only measurement helper used by `capture-probe proc-loopback <pid|name> <secs>`: it records
//! the packet size and arrival time of every packet so the cadence can be judged with
//! [`crate::loopback::arrival_stats`]. It renders nothing and changes no system state.

use crate::device::ComGuard;
use crate::loopback::{arrival_stats, ArrivalStats};
use crate::{CaptureError, Result};
use std::mem::ManuallyDrop;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, HRESULT};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Handler(Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Handler_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// Result of one process-loopback run.
#[derive(Debug)]
pub struct ProcLoopbackReport {
    pub pid: u32,
    pub rate: u32,
    pub packets: usize,
    pub frames: u64,
    pub silent_packets: usize,
    pub discontinuities: usize,
    /// Distinct packet sizes (frames) with counts, descending by count.
    pub sizes: Vec<(u32, usize)>,
    pub arrival: Option<ArrivalStats>,
    pub peak_dbfs: f32,
    pub first_packet_ms: f64,
    pub notes: Vec<String>,
}

/// Resolve a numeric pid or a process image name (case-insensitive, with or without `.exe`).
pub fn resolve_pid(spec: &str) -> Result<u32> {
    if let Ok(p) = spec.parse::<u32>() {
        return Ok(p);
    }
    let want = spec.to_ascii_lowercase();
    let want = want.strip_suffix(".exe").unwrap_or(&want).to_string();
    // SAFETY: standard ToolHelp enumeration with a properly sized PROCESSENTRY32W.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(CaptureError::com("CreateToolhelp32Snapshot"))?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
                if name.strip_suffix(".exe").unwrap_or(&name) == want {
                    found = Some(e.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found.ok_or_else(|| CaptureError::NotFound(spec.to_string()))
    }
}

fn now_us() -> f64 {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e6
}

/// Capture the process tree of `pid` for `seconds` and report packet cadence.
pub fn run(pid: u32, seconds: f32, rate: u32, exclude: bool) -> Result<ProcLoopbackReport> {
    let _com = ComGuard::init()?;
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        ..Default::default()
    };
    params.Anonymous.ProcessLoopbackParams.TargetProcessId = pid;
    params.Anonymous.ProcessLoopbackParams.ProcessLoopbackMode = if exclude {
        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
    } else {
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
    };
    let mut pv = PROPVARIANT::default();
    pv.Anonymous = PROPVARIANT_0 {
        Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
            vt: VT_BLOB,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: PROPVARIANT_0_0_0 {
                blob: windows::Win32::System::Com::BLOB {
                    cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                    pBlobData: &mut params as *mut _ as *mut u8,
                },
            },
        }),
    };
    // Never let PROPVARIANT::drop run: it would CoTaskMemFree the stack-allocated blob.
    let pv = ManuallyDrop::new(pv);
    let (tx, rx) = channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Handler(tx).into();
    // SAFETY: `pv` points at `params`; both stay alive until activation completes (we wait below).
    let op = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)
    }
    .map_err(CaptureError::com("ActivateAudioInterfaceAsync"))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| CaptureError::Thread("process loopback activation timed out".into()))?;
    let mut hr = HRESULT(0);
    let mut unk = None;
    // SAFETY: completed operation.
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }.map_err(CaptureError::com("GetActivateResult"))?;
    if hr.is_err() {
        return Err(CaptureError::Com { context: "process loopback activation result", source: windows::core::Error::from(hr) });
    }
    let client: IAudioClient = unk
        .ok_or_else(|| CaptureError::Thread("no interface".into()))?
        .cast()
        .map_err(CaptureError::com("cast IAudioClient"))?;

    let fmt = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let mut notes = Vec::new();
    // SAFETY: valid client and format.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &fmt, None) }
        .map_err(CaptureError::com("Initialize(process loopback)"))?;
    // SAFETY: event handle closed below.
    let ev = unsafe { CreateEventW(None, false, false, None) }.map_err(CaptureError::com("CreateEventW"))?;
    unsafe { client.SetEventHandle(ev) }.map_err(CaptureError::com("SetEventHandle"))?;
    if let Ok(n) = unsafe { client.GetBufferSize() } {
        notes.push(format!("client buffer {} frames ({:.2} ms)", n, n as f64 * 1000.0 / rate as f64));
    }
    let cap: IAudioCaptureClient = unsafe { client.GetService() }.map_err(CaptureError::com("GetService(capture)"))?;
    unsafe { client.Start() }.map_err(CaptureError::com("Start"))?;

    let t_start = now_us();
    let mut times = Vec::new();
    let mut sizes: std::collections::BTreeMap<u32, usize> = Default::default();
    let (mut frames, mut silent, mut disc, mut peak) = (0u64, 0usize, 0usize, 0f32);
    let mut first = f64::NAN;
    let end = Instant::now() + Duration::from_secs_f32(seconds);
    while Instant::now() < end {
        // SAFETY: valid event.
        if unsafe { WaitForSingleObject(ev, 100) } != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let n = unsafe { cap.GetNextPacketSize() }.map_err(CaptureError::com("GetNextPacketSize"))?;
            if n == 0 {
                break;
            }
            let (mut data, mut got, mut fl) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
            // SAFETY: out-pointers valid; buffer released below.
            unsafe { cap.GetBuffer(&mut data, &mut got, &mut fl, None, None) }.map_err(CaptureError::com("GetBuffer"))?;
            let now = now_us();
            if first.is_nan() {
                first = (now - t_start) / 1000.0;
            }
            times.push(now);
            *sizes.entry(got).or_default() += 1;
            frames += u64::from(got);
            if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                silent += 1;
            } else if !data.is_null() {
                // SAFETY: float32 stereo, `got` frames.
                let s = unsafe { std::slice::from_raw_parts(data as *const f32, got as usize * 2) };
                for v in s {
                    peak = peak.max(v.abs());
                }
            }
            if fl & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                disc += 1;
            }
            unsafe { cap.ReleaseBuffer(got) }.map_err(CaptureError::com("ReleaseBuffer"))?;
        }
    }
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(ev);
    }
    let mut sz: Vec<(u32, usize)> = sizes.into_iter().collect();
    sz.sort_by_key(|x| std::cmp::Reverse(x.1));
    let nominal = sz.first().map(|s| s.0 as f64 * 1e6 / rate as f64).unwrap_or(10_000.0);
    if times.is_empty() {
        notes.push("no packets delivered (target silent / not rendering?)".to_string());
    }
    Ok(ProcLoopbackReport {
        pid,
        rate,
        packets: times.len(),
        frames,
        silent_packets: silent,
        discontinuities: disc,
        sizes: sz,
        arrival: arrival_stats(&times, nominal),
        peak_dbfs: if peak > 0.0 { 20.0 * peak.log10() } else { f32::NEG_INFINITY },
        first_packet_ms: first,
        notes,
    })
}
