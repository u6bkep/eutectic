//! Decimal-exact quantity parse/format — the crate's first boundary parser, and the
//! one the simulation tier inherits (Decision 14). No floating point: a value is an
//! `i64` mantissa times a power of ten (`mant × 10^exp`), so every authored spelling
//! round-trips through arithmetic without rounding drift.
//!
//! # Accepted input forms
//!
//! [`parse`] accepts, after trimming and stripping an optional trailing unit token:
//!
//!   - **plain decimals** — `10000`, `0.1`, `4.7`, `0.47`;
//!   - **SI multiplier suffix** — a trailing scale letter `p n u µ μ m k K M G`
//!     (`10k`, `100n`, `4.7u`); micro accepts both the micro sign `µ` (U+00B5) and Greek
//!     mu `μ` (U+03BC); `k` and `K` are both kilo, lowercase `m` is milli and uppercase
//!     `M` is mega;
//!   - **IEC 60062 letter notation** — a scale letter used *as the decimal point*
//!     (`2R6` = 2.6, `4k7` = 4700, `1M2` = 1_200_000, `R47` = 0.47, `4m7` = 0.0047);
//!     `R` denotes the ones place and is IEC-only.
//!
//! A trailing **unit token** — `Ω`, `ohm`, `F`, `H`, `V`, `A`, `Hz` — is stripped and
//! discarded (`10kΩ`, `4.7uF`, `100nF`): the *formatter's* unit always comes from the
//! caller's format spec, never from the parsed string. Anything else — garbage, an
//! unknown suffix, two scale letters, an overflowing mantissa — is a parse failure
//! (`None`); callers degrade gracefully rather than erroring.
//!
//! # Formatting
//!
//! [`Quantity::format_si`] renders SI-symbol engineering notation (exponent a multiple
//! of three, coefficient in `[1, 1000)`, no trailing zeros) with a caller-supplied
//! unit. [`Quantity::format_iec`] renders the IEC letter-as-decimal-point form.

/// A decimal-exact quantity: `mant × 10^exp`. Kept as public fields so the simulation
/// tier can consume the parsed value without re-parsing the string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quantity {
    pub mant: i64,
    pub exp: i32,
}

/// Trailing unit tokens stripped before parsing, longest first so `ohm`/`Hz` win over
/// a shorter prefix of themselves.
const UNITS: &[&str] = &["ohm", "Hz", "Ω", "F", "H", "V", "A"];

/// Map a scale letter to its power-of-ten exponent. `R` is the IEC ones marker (`10^0`).
fn scale_exp(c: char) -> Option<i32> {
    Some(match c {
        'p' => -12,
        'n' => -9,
        'u' | 'µ' | 'μ' => -6,
        'm' => -3,
        'R' => 0,
        'k' | 'K' => 3,
        'M' => 6,
        'G' => 9,
        _ => return None,
    })
}

/// The SI prefix symbol for an engineering exponent (multiple of three).
fn si_prefix(exp: i32) -> Option<&'static str> {
    Some(match exp {
        -12 => "p",
        -9 => "n",
        -6 => "µ",
        -3 => "m",
        0 => "",
        3 => "k",
        6 => "M",
        9 => "G",
        _ => return None,
    })
}

/// The IEC letter for an engineering exponent (multiple of three).
fn iec_letter(exp: i32) -> Option<char> {
    Some(match exp {
        -12 => 'p',
        -9 => 'n',
        -6 => 'µ',
        -3 => 'm',
        0 => 'R',
        3 => 'k',
        6 => 'M',
        9 => 'G',
        _ => return None,
    })
}

/// Parse a `-?` decimal string (`"4.7"`, `"0.047"`, `"10000"`, `".5"`) exactly into a
/// mantissa + exponent. `None` on any non-digit content or an overflowing mantissa.
fn parse_decimal(s: &str) -> Option<(i64, i32)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // Reject a stray sign, exponent, or unit that slipped through.
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{int_part}{frac_part}");
    if digits.is_empty() {
        return None;
    }
    let mut mant: i64 = digits.parse().ok()?;
    if neg {
        mant = -mant;
    }
    Some((mant, -(frac_part.len() as i32)))
}

/// Parse a quantity in any of the documented forms. See the module docs.
pub fn parse(raw: &str) -> Option<Quantity> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Strip a trailing unit token (longest match first).
    for u in UNITS {
        if let Some(rest) = s.strip_suffix(u) {
            s = rest;
            break;
        }
    }
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Locate the single scale letter, if any.
    let scale_positions: Vec<(usize, char)> = s
        .char_indices()
        .filter(|&(_, c)| scale_exp(c).is_some())
        .collect();

    let (mant, exp) = match scale_positions.as_slice() {
        [] => parse_decimal(s)?,
        [(idx, c)] => {
            let sexp = scale_exp(*c).expect("filtered to scale letters");
            let before = &s[..*idx];
            let after = &s[idx + c.len_utf8()..];
            if after.is_empty() {
                // SI multiplier suffix: `10k`, `4.7u`. The letter scales the decimal.
                let (m, e) = parse_decimal(before)?;
                (m, e + sexp)
            } else {
                // IEC letter-as-decimal-point: `4k7`, `R47`, `2R6`. `before`/`after`
                // are the integer/fraction digits around the point.
                if !after.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let int_digits = if before.is_empty() { "0" } else { before };
                if !int_digits.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let (m, e) = parse_decimal(&format!("{int_digits}.{after}"))?;
                (m, e + sexp)
            }
        }
        _ => return None, // more than one scale letter is not a quantity
    };
    Some(Quantity { mant, exp })
}

impl Quantity {
    /// Choose an engineering exponent (multiple of three) for the value and return it
    /// with the coefficient's `(mantissa, exponent)` (`coeff = mant × 10^(exp − E)`).
    ///
    /// `toward_zero` selects the rounding of the value's leading-digit place to a
    /// multiple of three: **floor** for SI (the coefficient stays in `[1, 1000)`, so
    /// `0.47` → `470m`); **toward zero** for IEC 60062, which keeps sub-unit values on
    /// the ones (`R`) letter until there are ≥3 leading fractional zeros (`0.47` → `R47`
    /// but `0.0047` → `4m7`).
    fn engineering(&self, toward_zero: bool) -> (i32, i64, i32) {
        let ndigits = {
            let mut n = self.mant.unsigned_abs();
            let mut d = 0i32;
            while n > 0 {
                n /= 10;
                d += 1;
            }
            d.max(1)
        };
        // Place (base-10) of the leading digit of the value. Saturating: `mant`/`exp`
        // are public, so a caller can construct an extreme `exp` (e.g. `i32::MAX`) that
        // would overflow this add in debug; parse-derived values are nowhere near the
        // bound. When `eng` lands in the formatter tables `exp` is small and `cshift`
        // reduces to `≈ -(ndigits-1)`; the out-of-table case degrades to `scientific`.
        let leading = self.exp.saturating_add(ndigits).saturating_sub(1);
        // `saturating_mul`: floor division (`div_euclid`) near `i32::MIN` yields a
        // quotient whose ×3 would overflow below the bound. Both branches saturate;
        // an out-of-table `eng` just routes to the `scientific` fallback anyway.
        let eng = if toward_zero {
            (leading / 3).saturating_mul(3)
        } else {
            leading.div_euclid(3).saturating_mul(3)
        };
        (eng, self.mant, self.exp.saturating_sub(eng))
    }

    /// A sane fallback for exponents beyond the prefix/letter tables — plain
    /// `<mant>e<exp>` scientific, which never allocates a giant zero-run the way an
    /// unscaled decimal of an extreme exponent would.
    fn scientific(&self) -> String {
        format!("{}e{}", self.mant, self.exp)
    }

    /// Render `|mant| × 10^shift` as a minimal decimal string (no trailing zeros, an
    /// integer part of at least one digit). Sign is handled by the caller.
    fn render_scaled(mant_abs: u64, shift: i32) -> String {
        let digits = mant_abs.to_string();
        if shift >= 0 {
            return format!("{digits}{}", "0".repeat(shift as usize));
        }
        let k = (-shift) as usize;
        let (int_part, frac_part) = if digits.len() > k {
            let cut = digits.len() - k;
            (digits[..cut].to_string(), digits[cut..].to_string())
        } else {
            (
                "0".to_string(),
                format!("{}{digits}", "0".repeat(k - digits.len())),
            )
        };
        let frac = frac_part.trim_end_matches('0');
        if frac.is_empty() {
            int_part
        } else {
            format!("{int_part}.{frac}")
        }
    }

    /// SI engineering notation with a caller-supplied `unit` (`2.6kΩ`, `470mF`, `10`).
    /// An exponent beyond the prefix table falls back to `<mant>e<exp>` scientific.
    pub fn format_si(&self, unit: &str) -> String {
        if self.mant == 0 {
            return format!("0{unit}");
        }
        let sign = if self.mant < 0 { "-" } else { "" };
        let (eng, cm, cshift) = self.engineering(false);
        match si_prefix(eng) {
            Some(p) => format!(
                "{sign}{}{p}{unit}",
                Self::render_scaled(cm.unsigned_abs(), cshift)
            ),
            None => format!("{}{unit}", self.scientific()),
        }
    }

    /// IEC 60062 letter-as-decimal-point notation (`2k6`, `R47`, `4m7`, `1M2`). An
    /// exponent beyond the letter table falls back to `<mant>e<exp>` scientific.
    pub fn format_iec(&self) -> String {
        if self.mant == 0 {
            return "0".to_string();
        }
        let sign = if self.mant < 0 { "-" } else { "" };
        let (eng, cm, cshift) = self.engineering(true);
        let Some(letter) = iec_letter(eng) else {
            return self.scientific();
        };
        let coeff = Self::render_scaled(cm.unsigned_abs(), cshift);
        let body = match coeff.split_once('.') {
            // Fractional coefficient: the letter *is* the decimal point. A `0` integer
            // part (a sub-unit value) is dropped so `0.47` → `R47`.
            Some((int, frac)) => {
                if int == "0" {
                    format!("{letter}{frac}")
                } else {
                    format!("{int}{letter}{frac}")
                }
            }
            // Integer coefficient: letter trails (`1k`, `100R`).
            None => format!("{coeff}{letter}"),
        };
        format!("{sign}{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(mant: i64, exp: i32) -> Quantity {
        Quantity { mant, exp }
    }

    #[test]
    fn parses_plain_decimals() {
        assert_eq!(parse("10000"), Some(q(10000, 0)));
        assert_eq!(parse("0.1"), Some(q(1, -1)));
        assert_eq!(parse("4.7"), Some(q(47, -1)));
        assert_eq!(parse("0.47"), Some(q(47, -2)));
        assert_eq!(parse(".5"), Some(q(5, -1)));
        assert_eq!(parse("-4.7"), Some(q(-47, -1)));
    }

    #[test]
    fn parses_si_multiplier_suffix() {
        // value(10k) == value(10000): 10 * 10^3.
        assert_eq!(parse("10k"), Some(q(10, 3)));
        assert_eq!(parse("100n"), Some(q(100, -9)));
        assert_eq!(parse("4.7u"), Some(q(47, -7)));
        assert_eq!(parse("4.7µ"), Some(q(47, -7)));
        // k and K both kilo; lowercase m milli, uppercase M mega.
        assert_eq!(parse("2K"), parse("2k"));
        assert_eq!(parse("5m"), Some(q(5, -3)));
        assert_eq!(parse("5M"), Some(q(5, 6)));
    }

    #[test]
    fn parses_iec_letter_notation() {
        assert_eq!(parse("2R6"), Some(q(26, -1))); // 2.6
        assert_eq!(parse("4k7"), Some(q(47, 2))); // 4700
        assert_eq!(parse("1M2"), Some(q(12, 5))); // 1_200_000
        assert_eq!(parse("R47"), Some(q(47, -2))); // 0.47
        assert_eq!(parse("4m7"), Some(q(47, -4))); // 0.0047
    }

    #[test]
    fn strips_trailing_unit_token() {
        assert_eq!(parse("10kΩ"), parse("10k"));
        assert_eq!(parse("4.7uF"), parse("4.7u"));
        assert_eq!(parse("100nF"), parse("100n"));
        assert_eq!(parse("4700Ω"), parse("4700"));
        assert_eq!(parse("3.3V"), parse("3.3"));
        assert_eq!(parse("16MHz"), parse("16M"));
        assert_eq!(parse("10ohm"), parse("10"));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse("10x"), None);
        assert_eq!(parse("1k2k"), None); // two scale letters
        assert_eq!(parse("F"), None); // bare unit, no number
        assert_eq!(parse("1.2.3"), None);
    }

    #[test]
    fn formats_si_engineering() {
        assert_eq!(q(2600, 0).format_si("Ω"), "2.6kΩ");
        assert_eq!(q(26, 2).format_si("Ω"), "2.6kΩ"); // representation-independent
        assert_eq!(q(4700, 0).format_si(""), "4.7k");
        assert_eq!(q(470, 0).format_si(""), "470");
        assert_eq!(q(47, -2).format_si(""), "470m"); // 0.47 → 470m (floor)
        assert_eq!(q(1, 6).format_si(""), "1M");
        assert_eq!(q(0, 0).format_si("Ω"), "0Ω");
        // no trailing zeros
        assert_eq!(q(2600, 0).format_si("Ω"), "2.6kΩ");
        assert_eq!(q(100, 3).format_si("F"), "100kF");
    }

    #[test]
    fn formats_iec() {
        assert_eq!(q(26, 2).format_iec(), "2k6"); // 2.6k
        assert_eq!(q(26, -1).format_iec(), "2R6"); // 2.6
        assert_eq!(q(4700, 0).format_iec(), "4k7");
        assert_eq!(q(12, 5).format_iec(), "1M2"); // 1_200_000
        assert_eq!(q(47, -2).format_iec(), "R47"); // 0.47
        assert_eq!(q(47, -4).format_iec(), "4m7"); // 0.0047
        assert_eq!(q(47, 0).format_iec(), "47R");
        assert_eq!(q(1, 3).format_iec(), "1k");
    }

    #[test]
    fn parse_then_format_round_families() {
        // Common resistor/cap spellings survive parse→format at the intended value.
        assert_eq!(parse("4.7k").unwrap().format_iec(), "4k7");
        assert_eq!(parse("4k7").unwrap().format_si("Ω"), "4.7kΩ");
        assert_eq!(parse("2.6k").unwrap().format_iec(), "2k6");
        assert_eq!(parse("100nF").unwrap().format_si("F"), "100nF");
    }

    #[test]
    fn extreme_caller_exponent_degrades_without_panic() {
        // `mant`/`exp` are public, so a caller can construct an exponent that would
        // overflow the engineering arithmetic in debug. Formatting must not panic (and
        // must not allocate a giant zero-run); it degrades to `<mant>e<exp>` scientific.
        let hi = q(1, i32::MAX);
        assert_eq!(hi.format_si("Ω"), format!("1e{}Ω", i32::MAX));
        assert_eq!(hi.format_iec(), format!("1e{}", i32::MAX));
        let lo = q(-5, i32::MIN);
        assert_eq!(lo.format_si("F"), format!("-5e{}F", i32::MIN));
        assert_eq!(lo.format_iec(), format!("-5e{}", i32::MIN));
    }
}
