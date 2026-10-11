//! Sample-format conversion between ASIO buffers and `f32` / `i32` (Int32 LSB).
//! Everything here is allocation-free and safe to call from the audio callback.

use asio_abi::*;

/// True if [`read_i32`]/[`write_f32`] can handle this sample type.
pub fn is_supported(t: AsioSampleType) -> bool {
    matches!(
        t,
        ASIOST_INT16_LSB
            | ASIOST_INT24_LSB
            | ASIOST_INT32_LSB
            | ASIOST_FLOAT32_LSB
            | ASIOST_FLOAT64_LSB
            | ASIOST_INT32_LSB16
            | ASIOST_INT32_LSB18
            | ASIOST_INT32_LSB20
            | ASIOST_INT32_LSB24
    )
}

/// Valid bit count of the `Int32 LSBn` family (left-align shift = 32 - bits).
fn lsbn_bits(t: AsioSampleType) -> Option<u32> {
    match t {
        ASIOST_INT32_LSB16 => Some(16),
        ASIOST_INT32_LSB18 => Some(18),
        ASIOST_INT32_LSB20 => Some(20),
        ASIOST_INT32_LSB24 => Some(24),
        _ => None,
    }
}

#[inline]
pub fn f32_to_i32(x: f32) -> i32 {
    // `as` saturates and maps NaN to 0.
    (x * 2147483648.0) as i32
}

#[inline]
pub fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / 2147483648.0)
}

/// Reads `dst.len()` samples of type `t` from `src` into left-aligned `i32`.
/// Unsupported types produce silence.
///
/// # Safety
/// `src` must be valid for `dst.len() * bytes_per_sample(t)` bytes.
pub unsafe fn read_i32(src: *const u8, t: AsioSampleType, dst: &mut [i32]) {
    let n = dst.len();
    match t {
        ASIOST_INT32_LSB => {
            // Little-endian host: LSB layout == native.
            core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr() as *mut u8, n * 4);
        }
        ASIOST_INT16_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                let v = core::ptr::read_unaligned(src.add(i * 2) as *const i16);
                *d = (v as i32) << 16;
            }
        }
        ASIOST_INT24_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                let p = src.add(i * 3);
                let v = (*p as u32) | ((*p.add(1) as u32) << 8) | ((*p.add(2) as u32) << 16);
                *d = (v << 8) as i32; // left-align, sign preserved
            }
        }
        ASIOST_FLOAT32_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                *d = f32_to_i32(core::ptr::read_unaligned(src.add(i * 4) as *const f32));
            }
        }
        ASIOST_FLOAT64_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                let v = core::ptr::read_unaligned(src.add(i * 8) as *const f64);
                *d = (v * 2147483648.0) as i32;
            }
        }
        _ => match lsbn_bits(t) {
            Some(bits) => {
                let sh = 32 - bits;
                for (i, d) in dst.iter_mut().enumerate() {
                    *d = core::ptr::read_unaligned(src.add(i * 4) as *const i32) << sh;
                }
            }
            None => dst.fill(0),
        },
    }
}

/// Left-aligned `i32` samples to normalised `f32`.
pub fn i32_slice_to_f32(src: &[i32], dst: &mut [f32]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = i32_to_f32(s);
    }
}

/// Reads samples as normalised `f32`.
///
/// # Safety
/// `src` must be valid for `dst.len() * bytes_per_sample(t)` bytes.
pub unsafe fn read_f32(src: *const u8, t: AsioSampleType, dst: &mut [f32]) {
    match t {
        ASIOST_FLOAT32_LSB => core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr() as *mut u8, dst.len() * 4),
        ASIOST_INT32_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                *d = i32_to_f32(core::ptr::read_unaligned(src.add(i * 4) as *const i32));
            }
        }
        ASIOST_INT16_LSB => {
            for (i, d) in dst.iter_mut().enumerate() {
                *d = core::ptr::read_unaligned(src.add(i * 2) as *const i16) as f32 * (1.0 / 32768.0);
            }
        }
        _ => {
            let bps = bytes_per_sample(t);
            for (i, d) in dst.iter_mut().enumerate() {
                let mut one = [0i32; 1];
                read_i32(src.add(i * bps), t, &mut one);
                *d = i32_to_f32(one[0]);
            }
        }
    }
}

/// Writes normalised `f32` samples into a buffer of type `t` (saturating).
/// Unsupported types are left untouched.
///
/// # Safety
/// `dst` must be valid for `src.len() * bytes_per_sample(t)` bytes.
pub unsafe fn write_f32(src: &[f32], dst: *mut u8, t: AsioSampleType) {
    match t {
        ASIOST_FLOAT32_LSB => core::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, src.len() * 4),
        ASIOST_FLOAT64_LSB => {
            for (i, &s) in src.iter().enumerate() {
                core::ptr::write_unaligned(dst.add(i * 8) as *mut f64, s as f64);
            }
        }
        ASIOST_INT32_LSB => {
            for (i, &s) in src.iter().enumerate() {
                core::ptr::write_unaligned(dst.add(i * 4) as *mut i32, f32_to_i32(s));
            }
        }
        ASIOST_INT16_LSB => {
            for (i, &s) in src.iter().enumerate() {
                let v = (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                core::ptr::write_unaligned(dst.add(i * 2) as *mut i16, v);
            }
        }
        ASIOST_INT24_LSB => {
            for (i, &s) in src.iter().enumerate() {
                let v = ((s * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32) as u32;
                let p = dst.add(i * 3);
                *p = v as u8;
                *p.add(1) = (v >> 8) as u8;
                *p.add(2) = (v >> 16) as u8;
            }
        }
        _ => {
            if let Some(bits) = lsbn_bits(t) {
                let full = (1i64 << (bits - 1)) as f32;
                let (lo, hi) = (-full, full - 1.0);
                for (i, &s) in src.iter().enumerate() {
                    let v = (s * full).round().clamp(lo, hi) as i32;
                    core::ptr::write_unaligned(dst.add(i * 4) as *mut i32, v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(t: AsioSampleType, vals: &[f32], tol: f32) {
        let n = vals.len();
        let mut buf = vec![0u8; n * bytes_per_sample(t)];
        let mut out = vec![0f32; n];
        unsafe {
            write_f32(vals, buf.as_mut_ptr(), t);
            read_f32(buf.as_ptr(), t, &mut out);
        }
        for (a, b) in vals.iter().zip(&out) {
            assert!((a - b).abs() <= tol, "type {t}: {a} vs {b}");
        }
    }

    #[test]
    fn roundtrips() {
        let v = [0.0, 0.5, -0.5, 0.25, -1.0, 0.999];
        roundtrip(ASIOST_INT32_LSB, &v, 1e-6);
        roundtrip(ASIOST_FLOAT32_LSB, &v, 0.0);
        roundtrip(ASIOST_FLOAT64_LSB, &v, 1e-6);
        roundtrip(ASIOST_INT24_LSB, &v, 1.0 / 8388608.0);
        roundtrip(ASIOST_INT16_LSB, &v, 1.0 / 32768.0);
        roundtrip(ASIOST_INT32_LSB24, &v, 1.0 / 8388608.0);
        roundtrip(ASIOST_INT32_LSB16, &v, 1.0 / 32768.0);
    }

    #[test]
    fn saturates() {
        assert_eq!(f32_to_i32(2.0), i32::MAX);
        assert_eq!(f32_to_i32(-2.0), i32::MIN);
        assert_eq!(f32_to_i32(f32::NAN), 0);
        let mut b = [0u8; 2];
        unsafe { write_f32(&[1.5], b.as_mut_ptr(), ASIOST_INT16_LSB) };
        assert_eq!(i16::from_le_bytes(b), i16::MAX);
        let mut b = [0u8; 3];
        unsafe { write_f32(&[-3.0], b.as_mut_ptr(), ASIOST_INT24_LSB) };
        assert_eq!(b, [0x00, 0x00, 0x80]);
    }

    #[test]
    fn int24_sign_extension() {
        let b = [0xff, 0xff, 0xff]; // -1 in 24 bit
        let mut d = [0i32; 1];
        unsafe { read_i32(b.as_ptr(), ASIOST_INT24_LSB, &mut d) };
        assert_eq!(d[0], -256);
        let b = [0x00, 0x00, 0x40]; // +0.5
        unsafe { read_i32(b.as_ptr(), ASIOST_INT24_LSB, &mut d) };
        assert_eq!(d[0], 0x4000_0000);
    }

    #[test]
    fn int16_to_i32_and_float() {
        let b = 0x4000i16.to_le_bytes();
        let mut d = [0i32; 1];
        unsafe { read_i32(b.as_ptr(), ASIOST_INT16_LSB, &mut d) };
        assert_eq!(d[0], 0x4000_0000);
        let mut f = [0f32; 1];
        unsafe { read_f32(b.as_ptr(), ASIOST_INT16_LSB, &mut f) };
        assert_eq!(f[0], 0.5);
    }

    #[test]
    fn unsupported_is_silent() {
        let b = [1u8; 4];
        let mut d = [7i32; 1];
        unsafe { read_i32(b.as_ptr(), ASIOST_INT32_MSB, &mut d) };
        assert_eq!(d[0], 0);
        assert!(!is_supported(ASIOST_INT32_MSB));
        assert!(is_supported(ASIOST_INT32_LSB));
    }
}
