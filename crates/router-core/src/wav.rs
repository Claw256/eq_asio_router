//! Minimal WAV reader / writer (32-bit float and 16/24/32-bit PCM), used by the
//! measurement tap and the offline analyzer. No dependencies.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Streaming writer of 32-bit float WAV files. The header is patched on [`finish`](Self::finish).
pub struct WavWriter {
    out: BufWriter<File>,
    channels: u16,
    frames: u64,
}

impl WavWriter {
    /// Creates `path` (truncating) with `channels` interleaved channels at `rate` Hz.
    pub fn create(path: &Path, rate: u32, channels: u16) -> io::Result<WavWriter> {
        let mut out = BufWriter::with_capacity(1 << 16, File::create(path)?);
        write_header(&mut out, rate, channels, 0)?;
        Ok(WavWriter { out, channels, frames: 0 })
    }

    /// Appends planar stereo data as interleaved frames (`channels` must be 2).
    pub fn write_stereo(&mut self, l: &[f32], r: &[f32]) -> io::Result<()> {
        debug_assert_eq!(self.channels, 2);
        let n = l.len().min(r.len());
        for i in 0..n {
            self.out.write_all(&l[i].to_le_bytes())?;
            self.out.write_all(&r[i].to_le_bytes())?;
        }
        self.frames += n as u64;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Flushes and patches the RIFF / data sizes.
    pub fn finish(mut self) -> io::Result<u64> {
        self.out.flush()?;
        let bytes = self.frames * u64::from(self.channels) * 4;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&((36 + bytes).min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&(bytes.min(u64::from(u32::MAX)) as u32).to_le_bytes())?;
        f.flush()?;
        Ok(self.frames)
    }
}

fn write_header<W: Write>(w: &mut W, rate: u32, channels: u16, data_bytes: u32) -> io::Result<()> {
    let block = channels * 4;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * u32::from(block)).to_le_bytes())?;
    w.write_all(&block.to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

/// Decoded WAV: de-interleaved channels.
pub struct Wav {
    pub rate: u32,
    pub channels: Vec<Vec<f32>>,
}

/// Reads a WAV file (PCM 16/24/32 or float32).
pub fn read(path: &Path) -> io::Result<Wav> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    parse(&bytes)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Parses an in-memory WAV image.
pub fn parse(b: &[u8]) -> io::Result<Wav> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let (mut tag, mut ch, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut pos = 12;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body = pos + 8;
        let end = if id == b"data" { (body + size).min(b.len()) } else { body.saturating_add(size).min(b.len()) };
        if id == b"fmt " && end - body >= 16 {
            tag = u16::from_le_bytes([b[body], b[body + 1]]);
            ch = u16::from_le_bytes([b[body + 2], b[body + 3]]);
            rate = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            if tag == 0xFFFE && end - body >= 26 {
                tag = u16::from_le_bytes([b[body + 24], b[body + 25]]);
            }
        } else if id == b"data" {
            data = Some(&b[body..end]);
            break;
        }
        pos = end + (size & 1);
    }
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    if ch == 0 || rate == 0 {
        return Err(bad("missing fmt chunk"));
    }
    let bps = usize::from(bits / 8);
    if !matches!((tag, bits), (3, 32) | (1, 16) | (1, 24) | (1, 32)) {
        return Err(bad("unsupported WAV sample format"));
    }
    let frames = data.len() / (bps * usize::from(ch));
    let mut chans = vec![Vec::with_capacity(frames); usize::from(ch)];
    for f in 0..frames {
        for (c, v) in chans.iter_mut().enumerate() {
            let o = (f * usize::from(ch) + c) * bps;
            let s = &data[o..o + bps];
            v.push(match (tag, bits) {
                (3, 32) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                (1, 16) => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
                (1, 24) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            });
        }
    }
    Ok(Wav { rate, channels: chans })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join("router_core_wav_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let mut w = WavWriter::create(&p, 44100, 2).unwrap();
        let l: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
        let r: Vec<f32> = l.iter().map(|x| -x).collect();
        w.write_stereo(&l[..500], &r[..500]).unwrap();
        w.write_stereo(&l[500..], &r[500..]).unwrap();
        assert_eq!(w.finish().unwrap(), 1000);
        let back = read(&p).unwrap();
        assert_eq!(back.rate, 44100);
        assert_eq!(back.channels.len(), 2);
        assert_eq!(back.channels[0], l);
        assert_eq!(back.channels[1], r);
        let _ = std::fs::remove_file(&p);
    }
}
