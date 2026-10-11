//! Parser and writer for AutoEQ `ParametricEQ.txt` files.

use crate::dsp::{FilterKind, FilterSpec};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// A complete EQ profile: preamp plus parametric bands.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EqProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default)]
    pub filters: Vec<FilterSpec>,
}

/// Errors from [`parse_parametric_eq`]. Line numbers are 1-based.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AutoEqError {
    #[error("line {line}: unrecognised line {text:?}")]
    UnknownLine { line: usize, text: String },
    #[error("line {line}: unsupported filter type {kind:?}")]
    UnknownFilterType { line: usize, kind: String },
    #[error("line {line}: bandwidth (BW Oct) filters are not supported; use a Q-based profile")]
    BandwidthUnsupported { line: usize },
    #[error("line {line}: missing {field}")]
    MissingField { line: usize, field: &'static str },
    #[error("line {line}: invalid number {text:?}")]
    InvalidNumber { line: usize, text: String },
    #[error("line {line}: unexpected token {token:?}")]
    UnexpectedToken { line: usize, token: String },
    #[error("line {line}: {msg}")]
    OutOfRange { line: usize, msg: &'static str },
}

fn parse_num(line: usize, tok: &str, unit: &str) -> Result<f64, AutoEqError> {
    let t = tok.strip_suffix(unit).unwrap_or(tok);
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(AutoEqError::InvalidNumber { line, text: tok.to_string() }),
    }
}

fn parse_kind(line: usize, tok: &str) -> Result<FilterKind, AutoEqError> {
    Ok(match tok {
        "pk" | "peak" | "peaking" => FilterKind::Peaking,
        "lsc" | "ls" => FilterKind::LowShelf,
        "hsc" | "hs" => FilterKind::HighShelf,
        "lp" | "lpq" => FilterKind::LowPass,
        "hp" | "hpq" => FilterKind::HighPass,
        _ => return Err(AutoEqError::UnknownFilterType { line, kind: tok.to_uppercase() }),
    })
}

/// Parse the body of `Filter N:` (everything after the colon, lowercased).
fn parse_filter(line: usize, rest: &str) -> Result<FilterSpec, AutoEqError> {
    let mut toks = rest.split_whitespace().peekable();
    let enabled = match toks.next() {
        Some("on") => true,
        Some("off") => false,
        Some(t) => return Err(AutoEqError::UnexpectedToken { line, token: t.to_string() }),
        None => return Err(AutoEqError::MissingField { line, field: "ON/OFF" }),
    };
    let kind_tok = toks.next().ok_or(AutoEqError::MissingField { line, field: "filter type" })?;
    let kind = parse_kind(line, kind_tok)?;
    let (mut fc, mut gain, mut q) = (None, None, None);
    while let Some(tok) = toks.next() {
        // Accept both "Fc 31 Hz" and "Fc 31Hz".
        match tok {
            "fc" | "gain" | "q" => {
                let v = toks.next().ok_or(AutoEqError::MissingField {
                    line,
                    field: match tok {
                        "fc" => "Fc value",
                        "gain" => "Gain value",
                        _ => "Q value",
                    },
                })?;
                let unit = match tok {
                    "fc" => "hz",
                    "gain" => "db",
                    _ => "",
                };
                let n = parse_num(line, v, unit)?;
                match tok {
                    "fc" => fc = Some(n),
                    "gain" => gain = Some(n),
                    _ => q = Some(n),
                }
                if !unit.is_empty() && toks.peek() == Some(&unit) {
                    toks.next();
                }
            }
            "bw" | "oct" | "bandwidth" => return Err(AutoEqError::BandwidthUnsupported { line }),
            other => return Err(AutoEqError::UnexpectedToken { line, token: other.to_string() }),
        }
    }
    let fc_hz = fc.ok_or(AutoEqError::MissingField { line, field: "Fc" })?;
    let shaped = matches!(kind, FilterKind::Peaking | FilterKind::LowShelf | FilterKind::HighShelf);
    let gain_db = match gain {
        Some(g) => g,
        None if shaped => return Err(AutoEqError::MissingField { line, field: "Gain" }),
        None => 0.0,
    };
    let q = match q {
        Some(q) => q,
        None if kind == FilterKind::Peaking => {
            return Err(AutoEqError::MissingField { line, field: "Q" })
        }
        None => std::f64::consts::FRAC_1_SQRT_2,
    };
    if fc_hz <= 0.0 {
        return Err(AutoEqError::OutOfRange { line, msg: "Fc must be positive" });
    }
    if q <= 0.0 {
        return Err(AutoEqError::OutOfRange { line, msg: "Q must be positive" });
    }
    Ok(FilterSpec { kind, fc_hz, gain_db, q, enabled })
}

/// Parse AutoEQ ParametricEQ text. `name` is stored in the profile.
pub fn parse_parametric_eq(text: &str, name: &str) -> Result<EqProfile, AutoEqError> {
    let mut profile = EqProfile { name: name.to_string(), preamp_db: 0.0, filters: Vec::new() };
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let s = raw.trim().trim_start_matches('\u{feff}').trim();
        if s.is_empty() || s.starts_with('#') || s.starts_with("//") {
            continue;
        }
        let lower = s.to_ascii_lowercase();
        let unknown = || AutoEqError::UnknownLine { line, text: s.to_string() };
        if let Some(rest) = lower.strip_prefix("preamp") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(':').ok_or_else(unknown)?;
            let mut it = rest.split_whitespace();
            let v = it.next().ok_or(AutoEqError::MissingField { line, field: "preamp value" })?;
            profile.preamp_db = parse_num(line, v, "db")?;
            match (it.next(), it.next()) {
                (None, _) | (Some("db"), None) => {}
                (Some(t), _) => {
                    return Err(AutoEqError::UnexpectedToken { line, token: t.to_string() })
                }
            }
        } else if lower.starts_with("filter") {
            let (head, rest) = lower.split_once(':').ok_or_else(unknown)?;
            let idx = head["filter".len()..].trim();
            if !idx.is_empty() && !idx.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unknown());
            }
            profile.filters.push(parse_filter(line, rest)?);
        } else {
            return Err(unknown());
        }
    }
    Ok(profile)
}

/// Serialise a profile to AutoEQ ParametricEQ text.
pub fn to_parametric_eq_text(profile: &EqProfile) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Preamp: {} dB", profile.preamp_db);
    for (i, f) in profile.filters.iter().enumerate() {
        let state = if f.enabled { "ON" } else { "OFF" };
        let _ = match f.kind {
            FilterKind::Peaking | FilterKind::LowShelf | FilterKind::HighShelf => {
                let t = match f.kind {
                    FilterKind::Peaking => "PK",
                    FilterKind::LowShelf => "LSC",
                    _ => "HSC",
                };
                writeln!(
                    out,
                    "Filter {}: {} {} Fc {} Hz Gain {} dB Q {}",
                    i + 1,
                    state,
                    t,
                    f.fc_hz,
                    f.gain_db,
                    f.q
                )
            }
            FilterKind::LowPass | FilterKind::HighPass => {
                let t = if f.kind == FilterKind::LowPass { "LP" } else { "HP" };
                writeln!(out, "Filter {}: {} {} Fc {} Hz Q {}", i + 1, state, t, f.fc_hz, f.q)
            }
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD650: &str = "Preamp: -6.4 dB\n\
Filter 1: ON PK Fc 31 Hz Gain 1.5 dB Q 1.41\n\
Filter 2: ON PK Fc 62 Hz Gain -0.5 dB Q 1.0\n\
Filter 3: ON PK Fc 125 Hz Gain 2.2 dB Q 0.88\n\
Filter 4: ON PK Fc 250 Hz Gain -1.4 dB Q 1.12\n\
Filter 5: ON PK Fc 500 Hz Gain 0.6 dB Q 1.5\n\
Filter 6: ON PK Fc 1000 Hz Gain -2.1 dB Q 1.3\n\
Filter 7: ON PK Fc 2000 Hz Gain 3.4 dB Q 2.2\n\
Filter 8: ON PK Fc 4000 Hz Gain -4.8 dB Q 3.1\n\
Filter 9: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.7\n\
Filter 10: ON HSC Fc 10000 Hz Gain -3.2 dB Q 0.7\n";

    const MESSY: &str = "\u{feff}# AutoEQ export\r\n\r\n  preamp:   -3.5db \r\n\
FILTER 1:  on  pk  fc 105hz  gain 4.1db  q 0.71\r\n\
Filter 2: OFF PK Fc 2500 Hz Gain -2 dB Q 2\r\n\
Filter 3: ON LS Fc 80 Hz Gain 3 dB Q 0.5\r\n\
Filter 4: ON HS Fc 9000 Hz Gain -2 dB Q 0.6\r\n\
Filter 5: ON HP Fc 20 Hz\r\n\
Filter 6: ON LP Fc 18000 Hz Q 0.9\r\n";

    #[test]
    fn parses_realistic_headphone_profile() {
        let p = parse_parametric_eq(HD650, "HD 650").unwrap();
        assert_eq!(p.name, "HD 650");
        assert_eq!(p.preamp_db, -6.4);
        assert_eq!(p.filters.len(), 10);
        assert_eq!(
            p.filters[0],
            FilterSpec { kind: FilterKind::Peaking, fc_hz: 31.0, gain_db: 1.5, q: 1.41, enabled: true }
        );
        assert_eq!(p.filters[8].kind, FilterKind::LowShelf);
        assert_eq!(p.filters[8].fc_hz, 105.0);
        assert_eq!(p.filters[9].kind, FilterKind::HighShelf);
        assert_eq!(p.filters[9].gain_db, -3.2);
    }

    #[test]
    fn tolerates_case_whitespace_crlf_and_variants() {
        let p = parse_parametric_eq(MESSY, "m").unwrap();
        assert_eq!(p.preamp_db, -3.5);
        assert_eq!(p.filters.len(), 6);
        assert_eq!(p.filters[0].fc_hz, 105.0);
        assert_eq!(p.filters[0].gain_db, 4.1);
        assert!(!p.filters[1].enabled);
        assert_eq!(p.filters[2].kind, FilterKind::LowShelf);
        assert_eq!(p.filters[3].kind, FilterKind::HighShelf);
        assert_eq!(p.filters[4].kind, FilterKind::HighPass);
        assert_eq!(p.filters[4].gain_db, 0.0);
        assert_eq!(p.filters[5].kind, FilterKind::LowPass);
        assert_eq!(p.filters[5].q, 0.9);
    }

    #[test]
    fn second_realistic_profile() {
        let txt = "Preamp: -4.2 dB\nFilter 1: ON LSC Fc 105 Hz Gain 7.0 dB Q 0.70\n\
Filter 2: ON PK Fc 210 Hz Gain -3.4 dB Q 0.93\nFilter 3: ON PK Fc 5600 Hz Gain 4.5 dB Q 2.50\n\
Filter 4: ON HSC Fc 10000 Hz Gain -6.0 dB Q 0.70\n";
        let p = parse_parametric_eq(txt, "x").unwrap();
        assert_eq!(p.filters.len(), 4);
        assert_eq!(p.preamp_db, -4.2);
    }

    #[test]
    fn empty_input_is_flat() {
        let p = parse_parametric_eq("\n# nothing\n", "e").unwrap();
        assert_eq!(p.preamp_db, 0.0);
        assert!(p.filters.is_empty());
    }

    #[test]
    fn rejects_bandwidth_variant() {
        let e = parse_parametric_eq("Filter 1: ON PK Fc 1000 Hz Gain 3 dB BW Oct 0.5", "x").unwrap_err();
        assert_eq!(e, AutoEqError::BandwidthUnsupported { line: 1 });
        assert!(e.to_string().contains("BW Oct"));
    }

    #[test]
    fn rejects_bad_input() {
        let err = |t: &str| parse_parametric_eq(t, "x").unwrap_err();
        assert!(matches!(err("Hello"), AutoEqError::UnknownLine { line: 1, .. }));
        assert!(matches!(
            err("Preamp: -3 dB\nFilter 1: ON XX Fc 100 Hz Gain 1 dB Q 1"),
            AutoEqError::UnknownFilterType { line: 2, .. }
        ));
        assert!(matches!(err("Preamp: abc dB"), AutoEqError::InvalidNumber { .. }));
        assert!(matches!(err("Filter 1: ON PK Gain 1 dB Q 1"), AutoEqError::MissingField { field: "Fc", .. }));
        assert!(matches!(err("Filter 1: ON PK Fc 100 Hz Q 1"), AutoEqError::MissingField { field: "Gain", .. }));
        assert!(matches!(err("Filter 1: ON PK Fc 100 Hz Gain 1 dB"), AutoEqError::MissingField { field: "Q", .. }));
        assert!(matches!(err("Filter 1: MAYBE PK Fc 1 Hz"), AutoEqError::UnexpectedToken { .. }));
        assert!(matches!(err("Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 0"), AutoEqError::OutOfRange { .. }));
        assert!(matches!(err("Filter 1: ON PK Fc -5 Hz Gain 1 dB Q 1"), AutoEqError::OutOfRange { .. }));
        assert!(matches!(err("Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1 foo"), AutoEqError::UnexpectedToken { .. }));
        assert!(matches!(err("Filter 1 ON PK"), AutoEqError::UnknownLine { .. }));
        assert!(matches!(err("Filter 1: ON PK Fc"), AutoEqError::MissingField { .. }));
    }

    #[test]
    fn writer_round_trips() {
        for src in [HD650, MESSY] {
            let p = parse_parametric_eq(src, "n").unwrap();
            let text = to_parametric_eq_text(&p);
            let p2 = parse_parametric_eq(&text, "n").unwrap();
            assert_eq!(p, p2);
        }
        let text = to_parametric_eq_text(&parse_parametric_eq(HD650, "n").unwrap());
        assert!(text.starts_with("Preamp: -6.4 dB\nFilter 1: ON PK Fc 31 Hz Gain 1.5 dB Q 1.41\n"));
    }
}
