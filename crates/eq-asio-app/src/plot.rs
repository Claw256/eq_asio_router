//! Frequency-response plot helpers and the egui_plot widget.

use egui_plot::{GridInput, GridMark, HLine, HoverPosition, Line, Plot, PlotPoints};
use eq_core::{magnitude_response_db, EqProfile};

pub const F_MIN: f64 = 20.0;
pub const F_MAX: f64 = 20_000.0;
pub const POINTS: usize = 512;

/// `n` log-spaced frequencies from `lo` to `hi` inclusive.
pub fn log_freqs(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    if n < 2 {
        return vec![lo];
    }
    let (a, b) = (lo.log10(), hi.log10());
    (0..n)
        .map(|i| 10f64.powf(a + (b - a) * i as f64 / (n - 1) as f64))
        .collect()
}

/// Response curve as `[log10(f), dB]` points.
pub fn response_points(profile: &EqProfile, fs: f64) -> Vec<[f64; 2]> {
    let freqs = log_freqs(F_MIN, F_MAX, POINTS);
    let db = magnitude_response_db(profile, fs, &freqs);
    freqs.iter().zip(db).map(|(f, d)| [f.log10(), d]).collect()
}

/// Symmetric y extent: at least 15 dB, grown (rounded up to 5 dB) to fit data.
pub fn y_extent(points: &[[f64; 2]]) -> f64 {
    let peak = points.iter().map(|p| p[1].abs()).filter(|v| v.is_finite()).fold(0.0_f64, f64::max);
    ((peak / 5.0).ceil() * 5.0).max(15.0)
}

/// Label for a log10(frequency) tick.
pub fn freq_label(log_f: f64) -> String {
    let f = 10f64.powf(log_f);
    if f >= 1000.0 {
        let k = f / 1000.0;
        if (k - k.round()).abs() < 1e-6 {
            format!("{}k", k.round() as u32)
        } else {
            format!("{k:.1}k")
        }
    } else {
        format!("{}", f.round() as u32)
    }
}

fn grid_spacer(input: GridInput) -> Vec<GridMark> {
    let (lo, hi) = input.bounds;
    let mut marks = Vec::new();
    let first = lo.floor() as i32 - 1;
    let last = hi.ceil() as i32 + 1;
    for decade in first..=last {
        for m in 1..=9 {
            let v = (m as f64).log10() + decade as f64;
            if v < lo || v > hi {
                continue;
            }
            let step_size = if m == 1 { 1.0 } else { 0.3 };
            marks.push(GridMark { value: v, step_size });
        }
    }
    marks
}

pub fn show(ui: &mut egui::Ui, points: &[[f64; 2]]) {
    let ext = y_extent(points);
    Plot::new("eq_response")
        .height(ui.available_height().max(180.0))
        .allow_scroll(false)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .include_x(F_MIN.log10())
        .include_x(F_MAX.log10())
        .include_y(ext)
        .include_y(-ext)
        .x_axis_label("Frequency (Hz)")
        .y_axis_label("Gain (dB)")
        .x_grid_spacer(grid_spacer)
        .x_axis_formatter(|mark, _| freq_label(mark.value))
        .label_formatter(|pos| {
            let p = match pos {
                HoverPosition::NearDataPoint { position, .. } => position,
                HoverPosition::Elsewhere { position } => position,
            };
            Some(format!("{:.0} Hz\n{:+.2} dB", 10f64.powf(p.x), p.y))
        })
        .show(ui, |plot_ui| {
            plot_ui.hline(HLine::new("0 dB", 0.0).color(egui::Color32::from_gray(110)));
            plot_ui.line(
                Line::new("Response", PlotPoints::from(points.to_vec()))
                    .color(egui::Color32::from_rgb(90, 190, 255))
                    .width(2.0),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_freqs_endpoints_and_ratio() {
        let f = log_freqs(20.0, 20_000.0, 512);
        assert_eq!(f.len(), 512);
        assert!((f[0] - 20.0).abs() < 1e-9);
        assert!((f[511] - 20_000.0).abs() < 1e-6);
        assert!((f[1] / f[0] - f[300] / f[299]).abs() < 1e-9);
    }

    #[test]
    fn labels() {
        assert_eq!(freq_label(2.0), "100");
        assert_eq!(freq_label(3.0), "1k");
        assert_eq!(freq_label(4.0), "10k");
        assert_eq!(freq_label(1.30103), "20");
    }

    #[test]
    fn flat_profile_is_flat_and_extent_default() {
        let p = response_points(&EqProfile::default(), 48_000.0);
        assert_eq!(p.len(), POINTS);
        assert!(p.iter().all(|q| q[1].abs() < 1e-9));
        assert_eq!(y_extent(&p), 15.0);
    }

    #[test]
    fn extent_grows() {
        assert_eq!(y_extent(&[[0.0, 16.0]]), 20.0);
    }
}
