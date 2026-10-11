//! Optional quiet test-signal renderer (shared or exclusive WASAPI render), for probing the cable.

use crate::convert::{write_sample, FormatInfo};
use crate::device::{activate_client, find_device, open_device, parse_format, ComGuard, Flow, MixFormat};
use crate::session::make_ext;
use crate::signal::{SignalGen, SignalKind};
use crate::{CaptureError, Result};
use std::time::{Duration, Instant};

pub use crate::impulse::{
    click_samples, render_impulses, ImpulseRenderOptions, ImpulseRenderer, ImpulseShape, ImpulseTimes, ImpulseTrain,
    MAX_IMPULSE_DBFS,
};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioRenderClient, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    WAVEFORMATEX,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Sample container for exclusive rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderBits {
    B16,
    /// 24-bit packed in 3 bytes.
    B24,
    /// 24 valid bits in a 32-bit container.
    B24In32,
    B32,
    Float,
}

impl RenderBits {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "16" => Some(Self::B16),
            "24" => Some(Self::B24),
            "24in32" | "24-32" => Some(Self::B24In32),
            "32" => Some(Self::B32),
            "float" | "f32" => Some(Self::Float),
            _ => None,
        }
    }
    /// (container bits, valid bits, float)
    pub fn layout(self) -> (u16, u16, bool) {
        match self {
            Self::B16 => (16, 16, false),
            Self::B24 => (24, 24, false),
            Self::B24In32 => (32, 24, false),
            Self::B32 => (32, 32, false),
            Self::Float => (32, 32, true),
        }
    }
}

/// Exclusive render format request.
#[derive(Clone, Copy, Debug)]
pub struct ExclusiveFmt {
    pub rate: u32,
    pub bits: RenderBits,
}

/// What to render.
#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub name: String,
    pub seconds: f32,
    pub signal: SignalKind,
    pub dbfs: f32,
    /// `None` = shared mode at the endpoint mix format.
    pub exclusive: Option<ExclusiveFmt>,
    /// Shared mode with this stream format (Windows converts: AUTOCONVERTPCM + SRC), as a normal
    /// non-exclusive player at its native rate would do. Ignored when `exclusive` is set.
    pub shared_fmt: Option<ExclusiveFmt>,
}

/// Measured outcome of a render run.
#[derive(Clone, Debug)]
pub struct RenderReport {
    pub endpoint: String,
    pub exclusive: bool,
    pub format: FormatInfo,
    pub period_frames: u32,
    pub frames_written: u64,
    pub wall_s: f64,
    pub late_wakes: u32,
    pub max_gap_ms: f64,
}

impl std::fmt::Display for RenderReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} {} Hz {} ch {} ({} valid bits), period {} fr ({:.2} ms), wrote {} frames in {:.2} s wall ({:.0} fr/s), late wakes {}, max wake gap {:.2} ms",
            self.endpoint,
            if self.exclusive { "EXCLUSIVE" } else { "shared" },
            self.format.sample_rate,
            self.format.channels,
            self.format.format.name(),
            self.format.valid_bits,
            self.period_frames,
            self.period_frames as f64 * 1000.0 / self.format.sample_rate as f64,
            self.frames_written,
            self.wall_s,
            self.frames_written as f64 / self.wall_s.max(1e-9),
            self.late_wakes,
            self.max_gap_ms
        )
    }
}

/// Render a sine of `freq` Hz at `dbfs` into the render endpoint whose name contains
/// `name_substring`, for `seconds` (shared mode). Blocks the calling thread.
pub fn play_tone(name_substring: &str, seconds: f32, freq: f32, dbfs: f32) -> Result<String> {
    let opts = RenderOptions {
        name: name_substring.to_string(),
        seconds,
        signal: SignalKind::Tone(f64::from(freq)),
        dbfs,
        exclusive: None,
        shared_fmt: None,
    };
    let r = play_signal(&opts)?;
    Ok(format!("{r} ({:.1} dBFS {} Hz tone)", dbfs, freq))
}

/// Fade gain for frame `i` of `total` (20 ms linear in/out so the run does not click).
fn fade(i: u64, total: u64, rate: u32) -> f32 {
    let n = (rate as u64 / 50).max(1);
    let a = (i as f32 / n as f32).min(1.0);
    let b = (total.saturating_sub(i) as f32 / n as f32).min(1.0);
    a.min(b)
}

fn fill(buf: &mut [u8], frames: usize, fmt: &FormatInfo, gen: &mut SignalGen, pos: &mut u64, total: u64) {
    let block = fmt.block_align();
    let bps = fmt.format.bytes_per_sample();
    for i in 0..frames {
        let v = if *pos < total { gen.next_sample() * fade(*pos, total, fmt.sample_rate) } else { 0.0 };
        *pos += 1;
        for c in 0..fmt.channels as usize {
            let o = i * block + c * bps;
            write_sample(fmt.format, v, &mut buf[o..o + bps]);
        }
    }
}

/// Renders `opts` and returns the measured outcome. Blocks the calling thread.
pub fn play_signal(opts: &RenderOptions) -> Result<RenderReport> {
    let _com = ComGuard::init()?;
    let ep = find_device(Flow::Render, &opts.name)?;
    let dev = open_device(&ep.id)?;
    match opts.exclusive {
        None => play_shared(opts, &ep.name, &dev),
        Some(x) => play_exclusive(opts, &ep.name, &dev, x),
    }
}

fn play_shared(opts: &RenderOptions, ep_name: &str, dev: &windows::Win32::Media::Audio::IMMDevice) -> Result<RenderReport> {
    let client = activate_client(dev)?;
    let mix = MixFormat::from_client(&client)?;
    let custom = opts.shared_fmt.map(|x| {
        let (c, v, f) = x.bits.layout();
        make_ext(x.rate, 2, c, v, f)
    });
    let (fmt, fptr, flags) = match &custom {
        // SAFETY: `custom` outlives every use of the pointer in this function.
        Some(e) => (
            unsafe { parse_format(e as *const _ as *const WAVEFORMATEX)? },
            e as *const _ as *const WAVEFORMATEX,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        ),
        None => (mix.info()?, mix.ptr(), 0),
    };
    // SAFETY: shared-mode init with a 200 ms buffer; format pointer valid.
    let render: IAudioRenderClient = unsafe {
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, fptr, None)
            .map_err(CaptureError::com("Initialize(render)"))?;
        client.GetService().map_err(CaptureError::com("GetService(IAudioRenderClient)"))?
    };
    // SAFETY: valid initialised client.
    let total_buf = unsafe { client.GetBufferSize() }.map_err(CaptureError::com("GetBufferSize"))?;
    let mut gen = SignalGen::new(opts.signal, fmt.sample_rate, opts.dbfs);
    let block = fmt.block_align();
    let total = (opts.seconds as f64 * fmt.sample_rate as f64) as u64;
    let mut pos = 0u64;

    // SAFETY: Start/Stop on a valid client.
    unsafe { client.Start() }.map_err(CaptureError::com("Start(render)"))?;
    let t0 = Instant::now();
    let dur = Duration::from_secs_f32(opts.seconds);
    while t0.elapsed() < dur {
        // SAFETY: GetBuffer/ReleaseBuffer pairing with the frame count we request;
        // the buffer holds `avail * block` writable bytes.
        unsafe {
            let pad = client.GetCurrentPadding().map_err(CaptureError::com("GetCurrentPadding"))?;
            let avail = total_buf - pad;
            if avail > 0 {
                let p = render.GetBuffer(avail).map_err(CaptureError::com("GetBuffer(render)"))?;
                let buf = std::slice::from_raw_parts_mut(p, avail as usize * block);
                fill(buf, avail as usize, &fmt, &mut gen, &mut pos, total);
                render.ReleaseBuffer(avail, 0).map_err(CaptureError::com("ReleaseBuffer(render)"))?;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let wall_s = t0.elapsed().as_secs_f64();
    // SAFETY: valid client.
    unsafe {
        let _ = client.Stop();
    }
    Ok(RenderReport {
        endpoint: ep_name.to_string(),
        exclusive: false,
        format: fmt,
        period_frames: total_buf,
        frames_written: pos.min(total),
        wall_s,
        late_wakes: 0,
        max_gap_ms: 0.0,
    })
}

fn play_exclusive(
    opts: &RenderOptions,
    ep_name: &str,
    dev: &windows::Win32::Media::Audio::IMMDevice,
    x: ExclusiveFmt,
) -> Result<RenderReport> {
    let (cont, valid, float) = x.bits.layout();
    let ext = make_ext(x.rate, 2, cont, valid, float);
    let ptr = &ext as *const _ as *const WAVEFORMATEX;
    // SAFETY: `ext` is a live local for the whole function.
    let fmt = unsafe { parse_format(ptr)? };

    let probe = activate_client(dev)?;
    // SAFETY: valid client and format pointer.
    let hr = unsafe { probe.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, ptr, None) };
    if hr != windows::core::HRESULT(0) {
        return Err(CaptureError::Com {
            context: "IsFormatSupported(exclusive render)",
            source: windows::core::Error::from(hr),
        });
    }
    let mut min = 0i64;
    // SAFETY: valid out pointer.
    unsafe { probe.GetDevicePeriod(None, Some(&mut min)) }.map_err(CaptureError::com("GetDevicePeriod"))?;

    let init = |c: &windows::Win32::Media::Audio::IAudioClient, hns: i64| unsafe {
        // SAFETY: exclusive event-driven init; period == buffer duration.
        c.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, hns, hns, ptr, None)
    };
    let mut client = activate_client(dev)?;
    if let Err(e) = init(&client, min) {
        if e.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            return Err(CaptureError::Com { context: "Initialize(exclusive render)", source: e });
        }
        // SAFETY: documented recovery: query aligned size, recreate client, retry.
        let frames = unsafe { client.GetBufferSize() }.map_err(CaptureError::com("GetBufferSize"))?;
        let hns = (1e7 / x.rate as f64 * frames as f64 + 0.5) as i64;
        client = activate_client(dev)?;
        init(&client, hns).map_err(CaptureError::com("Initialize(exclusive render, realigned)"))?;
    }
    // SAFETY: auto-reset event signalled by WASAPI; closed below.
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(CaptureError::com("CreateEventW"))?;
    // SAFETY: client initialised for event callback.
    let (render, buf_frames): (IAudioRenderClient, u32) = unsafe {
        client.SetEventHandle(event).map_err(CaptureError::com("SetEventHandle"))?;
        (
            client.GetService().map_err(CaptureError::com("GetService(IAudioRenderClient)"))?,
            client.GetBufferSize().map_err(CaptureError::com("GetBufferSize"))?,
        )
    };
    let mut gen = SignalGen::new(opts.signal, x.rate, opts.dbfs);
    let block = fmt.block_align();
    let total = (opts.seconds as f64 * x.rate as f64) as u64;
    let mut pos = 0u64;
    let write_buf = |gen: &mut SignalGen, pos: &mut u64| -> Result<()> {
        // SAFETY: GetBuffer returns `buf_frames * block` writable bytes until ReleaseBuffer.
        unsafe {
            let p = render.GetBuffer(buf_frames).map_err(CaptureError::com("GetBuffer(exclusive render)"))?;
            let buf = std::slice::from_raw_parts_mut(p, buf_frames as usize * block);
            fill(buf, buf_frames as usize, &fmt, gen, pos, total);
            render.ReleaseBuffer(buf_frames, 0).map_err(CaptureError::com("ReleaseBuffer(exclusive render)"))
        }
    };
    write_buf(&mut gen, &mut pos)?;
    // SAFETY: valid client.
    unsafe { client.Start() }.map_err(CaptureError::com("Start(exclusive render)"))?;
    let t0 = Instant::now();
    let period_ms = buf_frames as f64 * 1000.0 / x.rate as f64;
    let (mut late, mut max_gap) = (0u32, 0.0f64);
    let mut last = Instant::now();
    let mut result = Ok(());
    // run until the signal plus two silent buffers have been delivered
    while pos < total + 2 * buf_frames as u64 {
        // SAFETY: valid event handle.
        let w = unsafe { WaitForSingleObject(event, 2000) };
        if w != WAIT_OBJECT_0 {
            result = Err(CaptureError::Thread("exclusive render: event wait timed out".into()));
            break;
        }
        let gap = last.elapsed().as_secs_f64() * 1000.0;
        last = Instant::now();
        max_gap = max_gap.max(gap);
        if gap > period_ms * 1.5 + 1.0 {
            late += 1;
        }
        if let Err(e) = write_buf(&mut gen, &mut pos) {
            result = Err(e);
            break;
        }
    }
    let wall_s = t0.elapsed().as_secs_f64();
    // SAFETY: valid client / event.
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    result?;
    Ok(RenderReport {
        endpoint: ep_name.to_string(),
        exclusive: true,
        format: fmt,
        period_frames: buf_frames,
        frames_written: pos.min(total),
        wall_s,
        late_wakes: late,
        max_gap_ms: max_gap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_shape() {
        assert_eq!(fade(0, 48000, 48000), 0.0);
        assert_eq!(fade(24000, 48000, 48000), 1.0);
        assert!(fade(47999, 48000, 48000) < 0.01);
        assert!((fade(480, 48000, 48000) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn bits_parse_and_layout() {
        assert_eq!(RenderBits::parse("24"), Some(RenderBits::B24));
        assert_eq!(RenderBits::parse("float").unwrap().layout(), (32, 32, true));
        assert_eq!(RenderBits::parse("24in32").unwrap().layout(), (32, 24, false));
        assert_eq!(RenderBits::parse("8"), None);
    }

    #[test]
    fn fill_writes_each_format() {
        for b in [RenderBits::B16, RenderBits::B24, RenderBits::B24In32, RenderBits::B32, RenderBits::Float] {
            let (c, v, f) = b.layout();
            let ext = make_ext(48000, 2, c, v, f);
            // SAFETY: local struct.
            let fmt = unsafe { parse_format(&ext as *const _ as *const WAVEFORMATEX) }.unwrap();
            let mut gen = SignalGen::new(SignalKind::Tone(1000.0), 48000, -6.0);
            let mut buf = vec![0u8; 960 * fmt.block_align()];
            let mut pos = 0;
            fill(&mut buf, 960, &fmt, &mut gen, &mut pos, 48000);
            assert_eq!(pos, 960);
            assert!(buf.iter().any(|&b| b != 0), "{b:?}");
        }
    }
}
