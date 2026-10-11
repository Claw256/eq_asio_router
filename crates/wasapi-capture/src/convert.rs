//! Pure (COM-free) conversion and accounting code.

/// Sample container formats we can decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Int16,
    /// 24-bit packed in 3 bytes.
    Int24,
    /// 24 valid bits in a 32-bit container (left justified).
    Int24In32,
    Int32,
    Float32,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::Int16 => 2,
            SampleFormat::Int24 => 3,
            _ => 4,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::Int16 => "PCM16",
            SampleFormat::Int24 => "PCM24 (packed)",
            SampleFormat::Int24In32 => "PCM24-in-32",
            SampleFormat::Int32 => "PCM32",
            SampleFormat::Float32 => "FLOAT32",
        }
    }
}

/// Decoded stream format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits: u16,
    pub channel_mask: u32,
    pub format: SampleFormat,
}

impl FormatInfo {
    pub fn block_align(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }
}

pub const WAVE_FORMAT_PCM: u16 = 1;
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
pub const SUBTYPE_PCM: u128 = 0x00000001_0000_0010_8000_00aa00389b71;
pub const SUBTYPE_FLOAT: u128 = 0x00000003_0000_0010_8000_00aa00389b71;

/// Classify a format given raw WAVEFORMAT fields. `subformat` is the extensible
/// sub-format GUID (as u128) when tag == EXTENSIBLE.
pub fn classify(
    tag: u16,
    subformat: Option<u128>,
    bits: u16,
    valid_bits: u16,
) -> Option<SampleFormat> {
    let float = match tag {
        WAVE_FORMAT_PCM => false,
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => match subformat? {
            SUBTYPE_PCM => false,
            SUBTYPE_FLOAT => true,
            _ => return None,
        },
        _ => return None,
    };
    match (float, bits) {
        (true, 32) => Some(SampleFormat::Float32),
        (false, 16) => Some(SampleFormat::Int16),
        (false, 24) => Some(SampleFormat::Int24),
        (false, 32) if valid_bits != 0 && valid_bits <= 24 => Some(SampleFormat::Int24In32),
        (false, 32) => Some(SampleFormat::Int32),
        _ => None,
    }
}

#[inline]
fn read_sample(fmt: SampleFormat, b: &[u8]) -> f32 {
    match fmt {
        SampleFormat::Int16 => i16::from_le_bytes([b[0], b[1]]) as f32 * (1.0 / 32768.0),
        SampleFormat::Int24 => {
            // sign-extend by placing into the top of an i32
            let v = i32::from_le_bytes([0, b[0], b[1], b[2]]);
            v as f32 * (1.0 / 2_147_483_648.0)
        }
        SampleFormat::Int24In32 => {
            let v = i32::from_le_bytes([b[0], b[1], b[2], b[3]]) & !0xFF;
            v as f32 * (1.0 / 2_147_483_648.0)
        }
        SampleFormat::Int32 => {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 * (1.0 / 2_147_483_648.0)
        }
        SampleFormat::Float32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
    }
}

/// Encode one sample (used by the tone generator). `out.len() >= bytes_per_sample`.
pub fn write_sample(fmt: SampleFormat, v: f32, out: &mut [u8]) {
    let v = v.clamp(-1.0, 1.0);
    match fmt {
        SampleFormat::Int16 => {
            out[..2].copy_from_slice(&((v * 32767.0).round() as i16).to_le_bytes())
        }
        SampleFormat::Int24 => {
            let i = ((v as f64 * 2_147_483_647.0).round() as i32).to_le_bytes();
            out[..3].copy_from_slice(&i[1..4]);
        }
        SampleFormat::Int24In32 | SampleFormat::Int32 => {
            let mut i = (v as f64 * 2_147_483_647.0).round() as i32;
            if fmt == SampleFormat::Int24In32 {
                i &= !0xFF;
            }
            out[..4].copy_from_slice(&i.to_le_bytes());
        }
        SampleFormat::Float32 => out[..4].copy_from_slice(&v.to_le_bytes()),
    }
}

/// Convert interleaved `data` (`frames` frames of `fmt`) into planar stereo.
/// mono -> duplicated, >2 channels -> first two. Returns frames converted
/// (limited by data length and output slice lengths).
pub fn convert_to_stereo(
    fmt: &FormatInfo,
    data: &[u8],
    frames: usize,
    left: &mut [f32],
    right: &mut [f32],
) -> usize {
    let ch = fmt.channels as usize;
    let bps = fmt.format.bytes_per_sample();
    let stride = bps * ch;
    if ch == 0 || stride == 0 {
        return 0;
    }
    let n = frames.min(data.len() / stride).min(left.len()).min(right.len());
    let f = fmt.format;
    for i in 0..n {
        let base = i * stride;
        let l = read_sample(f, &data[base..base + bps]);
        let r = if ch >= 2 {
            read_sample(f, &data[base + bps..base + 2 * bps])
        } else {
            l
        };
        left[i] = l;
        right[i] = r;
    }
    n
}

/// Flags of a captured packet (subset of AUDCLNT_BUFFERFLAGS_*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketFlags {
    pub data_discontinuity: bool,
    pub silent: bool,
    pub timestamp_error: bool,
}

impl PacketFlags {
    pub fn from_raw(raw: u32) -> Self {
        Self {
            data_discontinuity: raw & 0x1 != 0,
            silent: raw & 0x2 != 0,
            timestamp_error: raw & 0x4 != 0,
        }
    }
}

/// Pure accounting of packets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    pub packets: u64,
    pub frames: u64,
    pub discontinuities: u64,
    pub silent_packets: u64,
    pub timestamp_errors: u64,
}

impl Accounting {
    pub fn record(&mut self, frames: u32, flags: PacketFlags) {
        self.packets += 1;
        self.frames += frames as u64;
        self.discontinuities += flags.data_discontinuity as u64;
        self.silent_packets += flags.silent as u64;
        self.timestamp_errors += flags.timestamp_error as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fi(format: SampleFormat, channels: u16) -> FormatInfo {
        FormatInfo {
            sample_rate: 48000,
            channels,
            bits_per_sample: (format.bytes_per_sample() * 8) as u16,
            valid_bits: 0,
            channel_mask: 0,
            format,
        }
    }

    #[test]
    fn int16_stereo() {
        let mut d = Vec::new();
        for v in [16384i16, -32768, 0, 32767] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let (mut l, mut r) = ([0.0; 2], [0.0; 2]);
        assert_eq!(convert_to_stereo(&fi(SampleFormat::Int16, 2), &d, 2, &mut l, &mut r), 2);
        assert_eq!(l, [0.5, 0.0]);
        assert_eq!(r[0], -1.0);
        assert!((r[1] - 32767.0 / 32768.0).abs() < 1e-7);
    }

    #[test]
    fn int24_packed_sign() {
        let neg = (-0x400000i32).to_le_bytes();
        let pos = 0x400000i32.to_le_bytes();
        let d = [neg[0], neg[1], neg[2], pos[0], pos[1], pos[2]];
        let (mut l, mut r) = ([0.0; 1], [0.0; 1]);
        convert_to_stereo(&fi(SampleFormat::Int24, 2), &d, 1, &mut l, &mut r);
        assert_eq!(l[0], -0.5);
        assert_eq!(r[0], 0.5);
    }

    #[test]
    fn int24_in_32_and_int32() {
        let v = (0x400000i32 << 8).to_le_bytes();
        let (mut l, mut r) = ([0.0; 1], [0.0; 1]);
        convert_to_stereo(&fi(SampleFormat::Int24In32, 1), &v, 1, &mut l, &mut r);
        assert_eq!((l[0], r[0]), (0.5, 0.5));
        let g = ((0x400000i32 << 8) | 0x7F).to_le_bytes();
        convert_to_stereo(&fi(SampleFormat::Int24In32, 1), &g, 1, &mut l, &mut r);
        assert_eq!(l[0], 0.5);
        let m = i32::MIN.to_le_bytes();
        convert_to_stereo(&fi(SampleFormat::Int32, 1), &m, 1, &mut l, &mut r);
        assert_eq!(l[0], -1.0);
    }

    #[test]
    fn float_and_channel_mapping() {
        let mut d = Vec::new();
        for v in [0.1f32, 0.2, 0.9, -0.3, -0.4, 0.9] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let (mut l, mut r) = ([0.0; 2], [0.0; 2]);
        convert_to_stereo(&fi(SampleFormat::Float32, 3), &d, 2, &mut l, &mut r);
        assert_eq!(l, [0.1, -0.3]);
        assert_eq!(r, [0.2, -0.4]);
        let m = 0.25f32.to_le_bytes();
        let (mut l, mut r) = ([0.0; 1], [0.0; 1]);
        convert_to_stereo(&fi(SampleFormat::Float32, 1), &m, 1, &mut l, &mut r);
        assert_eq!((l[0], r[0]), (0.25, 0.25));
    }

    #[test]
    fn short_data_is_clamped() {
        let d = [0u8; 6];
        let (mut l, mut r) = ([1.0; 4], [1.0; 4]);
        assert_eq!(convert_to_stereo(&fi(SampleFormat::Int16, 2), &d, 4, &mut l, &mut r), 1);
    }

    #[test]
    fn encode_decode_roundtrip() {
        for f in [
            SampleFormat::Int16,
            SampleFormat::Int24,
            SampleFormat::Int24In32,
            SampleFormat::Int32,
            SampleFormat::Float32,
        ] {
            let mut b = [0u8; 4];
            write_sample(f, 0.3, &mut b);
            let (mut l, mut r) = ([0.0; 1], [0.0; 1]);
            convert_to_stereo(&fi(f, 1), &b[..f.bytes_per_sample()], 1, &mut l, &mut r);
            assert!((l[0] - 0.3).abs() < 1e-4, "{f:?} {}", l[0]);
        }
    }

    #[test]
    fn classify_formats() {
        assert_eq!(classify(1, None, 16, 16), Some(SampleFormat::Int16));
        assert_eq!(classify(3, None, 32, 32), Some(SampleFormat::Float32));
        assert_eq!(classify(0xFFFE, Some(SUBTYPE_PCM), 24, 24), Some(SampleFormat::Int24));
        assert_eq!(classify(0xFFFE, Some(SUBTYPE_PCM), 32, 24), Some(SampleFormat::Int24In32));
        assert_eq!(classify(0xFFFE, Some(SUBTYPE_PCM), 32, 32), Some(SampleFormat::Int32));
        assert_eq!(classify(0xFFFE, Some(SUBTYPE_FLOAT), 32, 32), Some(SampleFormat::Float32));
        assert_eq!(classify(0xFFFE, None, 32, 32), None);
        assert_eq!(classify(2, None, 4, 4), None);
    }

    #[test]
    fn accounting() {
        let mut a = Accounting::default();
        a.record(480, PacketFlags::from_raw(0));
        a.record(480, PacketFlags::from_raw(1));
        a.record(480, PacketFlags::from_raw(2 | 4));
        assert_eq!(a.packets, 3);
        assert_eq!(a.frames, 1440);
        assert_eq!(a.discontinuities, 1);
        assert_eq!(a.silent_packets, 1);
        assert_eq!(a.timestamp_errors, 1);
    }
}
