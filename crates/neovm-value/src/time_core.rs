//! Exact Lisp time conversion core (GNU `timefns.c`).
//!
//! The exact `(TICKS . HZ)` machinery — decode_lisp_time's full input
//! grammar ((TICKS . HZ), (HIGH LOW [USEC [PSEC]]), integer, float, nil),
//! the TicksHz arithmetic and comparisons, the TimeMicros display view,
//! and the current-time-list output encoders — moved below the evaluator
//! with the stage-3 split (C16). The subrs, the calendar/zone layer, and
//! the `current-time-list` selector (which reads a variable through the
//! evaluator) stay in `emacs_core::system::timefns`, which re-exports
//! this module's public surface so historical paths are unchanged.

use crate::error::{Flow, LispCondition, signal};
use crate::value::*;
use malachite::base::num::conversion::traits::RoundingFrom;
use malachite::base::rounding_modes::RoundingMode;
use malachite::integer::Integer;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum TimeConvertForm {
    Integer,
    List,
    InputHz,
    ExplicitHz(Integer),
}
/// Output representation selected by the dynamically bound
/// `current-time-list`.
///
/// Keep this as a semantic enum rather than passing a boolean between modules:
/// callers cannot accidentally invert what `true` means, and all producers of
/// Lisp timestamps share the same GNU-compatible constructor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum LispTimeOutput {
    LegacyList,
    TicksHz,
}
impl LispTimeOutput {
    fn encode(self, time: TimeMicros) -> Value {
        match self {
            Self::LegacyList => time.to_list(),
            Self::TicksHz => time.to_ticks_hz(1_000_000_000),
        }
    }
}
// ---------------------------------------------------------------------------
// Internal time representation
// ---------------------------------------------------------------------------

/// Internal microsecond-precision time (seconds + microseconds since epoch).
/// Allows negative values for times before the epoch.
#[derive(Clone, Copy, Debug)]
#[doc(hidden)]
pub struct TimeMicros {
    /// Total seconds (may be negative).
    pub secs: i64,
    /// Microseconds within the current second, always in [0, 999_999].
    pub usecs: i64,
    /// Picoseconds within the current microsecond, always in [0, 999_999].
    pub psecs: i64,
}
impl TimeMicros {
    fn from_timespec(secs: i64, nanos: i64) -> Self {
        let secs = secs + nanos.div_euclid(1_000_000_000);
        let nanos = nanos.rem_euclid(1_000_000_000);
        Self {
            secs,
            usecs: nanos / 1_000,
            psecs: (nanos % 1_000) * 1_000,
        }
    }

    pub fn now() -> Self {
        // GNU `current_timespec` has nanosecond resolution; `Ftime_convert`
        // projects it as USEC = ns / 1000 and PSEC = (ns % 1000) * 1000
        // (`src/timefns.c` timespec_to_lisp / decode_lisp_time). Keeping the
        // nanosecond remainder matters observably: timer vectors built by
        // `run-at-time` carry a nonzero PSEC field in GNU.
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(dur) => {
                let nanos = dur.subsec_nanos() as i64;
                TimeMicros {
                    secs: dur.as_secs() as i64,
                    usecs: nanos / 1_000,
                    psecs: (nanos % 1_000) * 1_000,
                }
            }
            Err(e) => {
                let dur = e.duration();
                let nanos = dur.subsec_nanos() as i64;
                TimeMicros {
                    secs: -(dur.as_secs() as i64),
                    usecs: -(nanos / 1_000),
                    psecs: -((nanos % 1_000) * 1_000),
                }
            }
        }
    }

    pub fn to_list(self) -> Value {
        let high = self.secs >> 16;
        let low = self.secs & 0xFFFF;
        Value::list(vec![
            Value::fixnum(high),
            Value::fixnum(low),
            Value::fixnum(self.usecs),
            Value::fixnum(self.psecs),
        ])
    }

    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub fn to_float(self) -> f64 {
        self.secs as f64 + self.usecs as f64 / 1_000_000.0
    }

    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub fn add(self, other: TimeMicros) -> TimeMicros {
        let mut psecs = self.psecs + other.psecs;
        let mut usecs = self.usecs + other.usecs;
        let mut secs = self.secs + other.secs;
        if psecs >= 1_000_000 {
            psecs -= 1_000_000;
            usecs += 1;
        } else if psecs < 0 {
            psecs += 1_000_000;
            usecs -= 1;
        }
        if usecs >= 1_000_000 {
            usecs -= 1_000_000;
            secs += 1;
        } else if usecs < 0 {
            usecs += 1_000_000;
            secs -= 1;
        }
        TimeMicros { secs, usecs, psecs }
    }

    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub fn sub(self, other: TimeMicros) -> TimeMicros {
        let mut psecs = self.psecs - other.psecs;
        let mut usecs = self.usecs - other.usecs;
        let mut secs = self.secs - other.secs;
        if psecs < 0 {
            psecs += 1_000_000;
            usecs -= 1;
        } else if psecs >= 1_000_000 {
            psecs -= 1_000_000;
            usecs += 1;
        }
        if usecs < 0 {
            usecs += 1_000_000;
            secs -= 1;
        } else if usecs >= 1_000_000 {
            usecs -= 1_000_000;
            secs += 1;
        }
        TimeMicros { secs, usecs, psecs }
    }

    fn from_ticks_hz(ticks: i64, hz: i64) -> Result<TimeMicros, Flow> {
        if hz <= 0 {
            return Err(signal(
                "error",
                vec![Value::string("Invalid time specification")],
            ));
        }

        let secs = ticks.div_euclid(hz);
        let rem = ticks.rem_euclid(hz) as i128;
        let hz = hz as i128;
        let micros_total = rem * 1_000_000;
        let usecs = (micros_total / hz) as i64;
        let psecs = (((micros_total % hz) * 1_000_000) / hz) as i64;
        Ok(TimeMicros { secs, usecs, psecs })
    }

    fn from_exact_ticks_hz(ticks: &Integer, hz: &Integer) -> Result<TimeMicros, Flow> {
        if hz <= &Integer::from(0) {
            return Err(signal(
                "error",
                vec![Value::string("Invalid time specification")],
            ));
        }

        let trillion = Integer::from(1_000_000_000_000i64);
        let million = Integer::from(1_000_000i64);
        let total_psecs = integer_div_floor(&(ticks * &trillion), hz);
        let secs = integer_div_floor(&total_psecs, &trillion);
        let rem_psecs = &total_psecs - &secs * &trillion;
        let usecs = &rem_psecs / &million;
        let psecs = &rem_psecs - &usecs * &million;

        let secs = i64::try_from(&secs)
            .map_err(|_| signal("error", vec![Value::string("Time value out of range")]))?;
        let usecs = i64::try_from(&usecs)
            .map_err(|_| signal("error", vec![Value::string("Time value out of range")]))?;
        let psecs = i64::try_from(&psecs)
            .map_err(|_| signal("error", vec![Value::string("Time value out of range")]))?;

        Ok(TimeMicros { secs, usecs, psecs })
    }

    fn to_ticks_hz(self, hz: i64) -> Value {
        self.to_ticks_hz_integer(&Integer::from(hz))
    }

    fn to_ticks_hz_integer(self, hz: &Integer) -> Value {
        let trillion = Integer::from(1_000_000_000_000i64);
        let total_psecs = Integer::from(self.secs) * &trillion
            + Integer::from(self.usecs) * Integer::from(1_000_000i64)
            + Integer::from(self.psecs);
        let ticks = integer_div_floor(&(total_psecs * hz), &trillion);
        Value::cons(Value::make_integer(ticks), Value::make_integer(hz.clone()))
    }

    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub fn less_than(self, other: TimeMicros) -> bool {
        if self.secs != other.secs {
            self.secs < other.secs
        } else if self.usecs != other.usecs {
            self.usecs < other.usecs
        } else {
            self.psecs < other.psecs
        }
    }

    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub fn equal(self, other: TimeMicros) -> bool {
        self.secs == other.secs && self.usecs == other.usecs && self.psecs == other.psecs
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum TimeInputForm {
    Scalar,
    List,
    TicksHz,
}
// ---------------------------------------------------------------------------
// Exact rational time core, mirroring GNU `src/timefns.c`.
//
// GNU represents every decoded timestamp as an exact rational `(TICKS . HZ)`
// (a `struct ticks_hz`, `src/timefns.c:516`) and performs all arithmetic,
// comparison, and float conversion on that exact pair so that arbitrary
// frequencies (HZ) — including the power-of-two HZ produced by decoding a
// float, or a caller-supplied `(TICKS . HZ)` whose HZ is neither 1, 10**6,
// nor 10**12 — round-trip without loss. The previous Neomacs code reduced
// every value to microsecond/picosecond `TimeMicros` first, which silently
// discarded precision (e.g. `(time-add 0.5 '(1 . 3))` could not be 5/6) and
// could not read back bignum TICKS at all. This module reproduces the exact
// path. `TimeMicros` survives only as a derived display view (used by
// `decode-time` / `current-time-string` / `%N`), computed from the exact
// pair.
// ---------------------------------------------------------------------------

/// An exact Lisp timestamp `(TICKS . HZ)`, where `ticks / hz` is the number of
/// seconds since the epoch and `hz > 0`. Mirrors GNU `struct ticks_hz`.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct TicksHz {
    pub ticks: Integer,
    pub hz: Integer,
}
/// A decoded Lisp time: the exact `(TICKS . HZ)` value plus the syntactic form
/// of the input, which GNU's `time_arith` consults when choosing the result
/// representation (`src/timefns.c:1211`). Named distinctly from the calendar
/// `DecodedTime` (broken-down sec/min/hour/...) used by `decode-time`.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct DecodedLispTime {
    pub th: TicksHz,
    form: TimeInputForm,
}
#[doc(hidden)]
pub fn trillion_int() -> Integer {
    Integer::from(1_000_000_000_000i64)
}
#[doc(hidden)]
pub fn million_int() -> Integer {
    Integer::from(1_000_000i64)
}
/// Extract a malachite `Integer` from any Lisp integer (fixnum or bignum).
/// Returns `None` for non-integers, mirroring GNU `INTEGERP`.
#[doc(hidden)]
pub fn value_to_integer(val: &Value) -> Option<Integer> {
    if let Some(n) = val.as_fixnum() {
        Some(Integer::from(n))
    } else {
        val.as_bignum().cloned()
    }
}
/// GNU `time_spec_invalid` (`src/timefns.c`): an ill-formed time value signals
/// `(error "Invalid time specification")`, NOT a `wrong-type-argument`.
#[doc(hidden)]
pub fn time_spec_invalid() -> Flow {
    signal("error", vec![Value::string("Invalid time specification")])
}
#[doc(hidden)]
pub fn time_error_overflow() -> Flow {
    signal(
        LispCondition::Error,
        vec![Value::string("Specified time is not representable")],
    )
}
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct ParsedTime {
    pub(crate) time: TimeMicros,
    pub hz: i64,
    #[allow(dead_code)] // grandfathered when dead_code lint was enabled; delete or wire up
    pub(crate) form: TimeInputForm,
}
/// Parse a time value from a Lisp argument.
///
/// Accepts:
///   - nil            -> current time
///   - integer        -> seconds since epoch
///   - float          -> seconds since epoch (with fractional part)
///   - (TICKS . HZ)   -> modern GNU timestamp cons
///   - (HIGH LOW)     -> high*65536 + low seconds, 0 usecs
///   - (HIGH LOW USEC)       -> with microseconds
///   - (HIGH LOW USEC PSEC)  -> with picoseconds
#[doc(hidden)]
pub fn parse_time(val: &Value) -> Result<TimeMicros, Flow> {
    Ok(parse_time_detailed(val)?.time)
}
/// Decode a Lisp time value into whole `(seconds, nanoseconds)`, using the same
/// full parser as the other time functions so every accepted form ((TICKS .
/// HZ), (HIGH LOW USEC PSEC), integer, float, nil) is handled identically.
/// `nanoseconds` is the subsecond fraction in `[0, 999_999_999]`, matching the
/// `int ns = t.tv_nsec` that GNU's `format_time_string` passes to nstrftime
/// (`src/timefns.c:1391`). This is what the `%N` directive consumes.
#[doc(hidden)]
pub fn time_value_seconds_and_nanos(val: &Value) -> Result<(i64, i64), Flow> {
    // Decode exactly, then floor-divide into whole seconds + nanoseconds the
    // same way GNU `ticks_hz_to_timespec` does (`src/timefns.c:529`): the
    // nanosecond count is floor((ticks * 10**9) / hz) reduced mod 10**9. Doing
    // this on the exact pair (rather than a microsecond-rounded intermediate)
    // keeps `%N`/`%3N`/`%6N` byte-exact for arbitrary HZ.
    if let Some(time) = SmallTime::decode(val)
        && let Some(scaled) = time.ticks.checked_mul(1_000_000_000)
    {
        let total_nanos = scaled.div_euclid(time.hz);
        if let Ok(secs) = i64::try_from(total_nanos.div_euclid(1_000_000_000)) {
            return Ok((secs, total_nanos.rem_euclid(1_000_000_000) as i64));
        }
    }
    let th = decode_lisp_time(val)?.th;
    let timespec_hz = Integer::from(1_000_000_000i64);
    let total_nanos = integer_div_floor(&(&th.ticks * &timespec_hz), &th.hz);
    let (secs, nanos) = integer_fdiv_qr(&total_nanos, &timespec_hz);
    let secs = i64::try_from(&secs).map_err(|_| time_error_overflow())?;
    let nanos = i64::try_from(&nanos).unwrap_or(0);
    Ok((secs, nanos))
}
#[doc(hidden)]
pub fn parse_time_detailed(val: &Value) -> Result<ParsedTime, Flow> {
    // Bignum seconds-since-epoch values get truncated to i64;
    // Emacs's GNU encoding of large times uses (HIGH LOW) cons
    // pairs anyway, so a bignum here usually only occurs for
    // tests that compute (1+ most-positive-fixnum) etc.
    if let ValueKind::Veclike(VecLikeType::Bignum) = val.kind() {
        let f = f64::rounding_from(val.as_bignum().unwrap(), RoundingMode::Nearest).0;
        return Ok(ParsedTime {
            time: TimeMicros {
                secs: f as i64,
                usecs: 0,
                psecs: 0,
            },
            hz: 1,
            form: TimeInputForm::Scalar,
        });
    }
    match val.kind() {
        ValueKind::Nil => Ok(ParsedTime {
            time: TimeMicros::now(),
            hz: 1_000_000_000_000,
            form: TimeInputForm::List,
        }),
        ValueKind::Fixnum(n) => Ok(ParsedTime {
            time: TimeMicros {
                secs: n,
                usecs: 0,
                psecs: 0,
            },
            hz: 1,
            form: TimeInputForm::Scalar,
        }),
        ValueKind::Float => {
            let f = val.xfloat();
            if !f.is_finite() {
                // GNU `decode_lisp_time` (`src/timefns.c:1037`) routes a non-finite
                // float through `time_error (isnan ? EDOM : EOVERFLOW)`: NaN ->
                // "Invalid time specification", ±inf -> "Specified time is not
                // representable".
                return Err(if f.is_nan() {
                    time_spec_invalid()
                } else {
                    time_error_overflow()
                });
            }
            let (ticks, hz) = float_to_exact_ticks_hz(f)?;
            let hz_i64 = i64::try_from(&hz)
                .map_err(|_| signal("error", vec![Value::string("Time value out of range")]))?;
            Ok(ParsedTime {
                time: TimeMicros::from_exact_ticks_hz(&ticks, &hz)?,
                hz: hz_i64,
                form: TimeInputForm::List,
            })
        }
        ValueKind::Cons => {
            let high = val.cons_car();
            let low_or_tail = val.cons_cdr();
            if !low_or_tail.is_cons() {
                let ticks = high.as_int().ok_or_else(|| {
                    signal(
                        LispCondition::WrongTypeArgument,
                        vec![Value::symbol("integerp"), high],
                    )
                })?;
                let hz = low_or_tail.as_int().ok_or_else(|| {
                    signal(
                        LispCondition::WrongTypeArgument,
                        vec![Value::symbol("integerp"), low_or_tail],
                    )
                })?;
                return Ok(ParsedTime {
                    time: TimeMicros::from_ticks_hz(ticks, hz)?,
                    hz,
                    form: TimeInputForm::TicksHz,
                });
            }

            let items = list_to_vec(val).ok_or_else(|| {
                signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("listp"), *val],
                )
            })?;
            if items.len() < 2 {
                return Err(signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("listp"), *val],
                ));
            }
            let high = items[0].as_int().ok_or_else(|| {
                signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("integerp"), items[0]],
                )
            })?;
            let low = items[1].as_int().ok_or_else(|| {
                signal(
                    LispCondition::WrongTypeArgument,
                    vec![Value::symbol("integerp"), items[1]],
                )
            })?;
            let usec = if items.len() > 2 {
                items[2].as_int().unwrap_or(0)
            } else {
                0
            };
            let psec = if items.len() > 3 {
                items[3].as_int().unwrap_or(0)
            } else {
                0
            };
            let secs = i64::try_from(i128::from(high) * 65536 + i128::from(low))
                .map_err(|_| time_error_overflow())?;
            Ok(ParsedTime {
                time: TimeMicros {
                    secs,
                    usecs: usec,
                    psecs: psec,
                },
                hz: time_convert_default_hz(val),
                form: TimeInputForm::List,
            })
        }
        // GNU `decode_lisp_time` (`src/timefns.c:1041`) signals
        // `time_spec_invalid` ("Invalid time specification") for any TIME value
        // that is not nil/cons/integer/float (e.g. a string), NOT a
        // `wrong-type-argument numberp`.
        _ => Err(time_spec_invalid()),
    }
}
#[doc(hidden)]
pub fn float_to_exact_ticks_hz(f: f64) -> Result<(Integer, Integer), Flow> {
    if f == 0.0 {
        return Ok((Integer::from(0), Integer::from(1)));
    }

    let bits = f.to_bits();
    let sign = if bits >> 63 == 0 { 1i128 } else { -1i128 };
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mantissa, unbiased_exponent) = if exponent_bits == 0 {
        (Integer::from(fraction), -1022)
    } else {
        (Integer::from((1u64 << 52) | fraction), exponent_bits - 1023)
    };
    let scale = (52 - unbiased_exponent).max(0);

    let mut ticks = if unbiased_exponent >= 52 {
        let shift = (unbiased_exponent - 52) as u32;
        mantissa << shift
    } else {
        mantissa
    };
    if sign < 0 {
        ticks = -ticks;
    }

    let hz = Integer::from(1) << (scale as u32);
    Ok((ticks, hz))
}
#[doc(hidden)]
pub fn integer_div_floor(n: &Integer, d: &Integer) -> Integer {
    let q = n / d;
    let r = n - &q * d;
    if r != 0 && n < &Integer::from(0) {
        q - Integer::from(1)
    } else {
        q
    }
}
/// Floor-divide `n` by `d` (d > 0), also returning the non-negative remainder.
#[doc(hidden)]
pub fn integer_fdiv_qr(n: &Integer, d: &Integer) -> (Integer, Integer) {
    let q = integer_div_floor(n, d);
    let r = n - &q * d;
    (q, r)
}
#[doc(hidden)]
pub fn integer_gcd(a: &Integer, b: &Integer) -> Integer {
    use malachite::base::num::arithmetic::traits::Gcd;
    // GCD is defined on the magnitudes; `unsigned_abs_ref` borrows the
    // underlying `Natural` without copying.
    Integer::from(a.unsigned_abs_ref().gcd(b.unsigned_abs_ref()))
}
// ---------------------------------------------------------------------------
// Exact (TICKS . HZ) decoding — GNU `decode_lisp_time` and friends.
// ---------------------------------------------------------------------------

/// GNU `decode_float_time` (`src/timefns.c:612`): convert a finite double into
/// the exact `(TICKS . HZ)` pair whose value equals it, with HZ the float's
/// frequency (a power of two) or 1, whichever is greater. Reuses the existing
/// bit-exact `float_to_exact_ticks_hz` decomposition.
#[doc(hidden)]
pub fn decode_float_time(t: f64) -> Result<TicksHz, Flow> {
    let (ticks, hz) = float_to_exact_ticks_hz(t)?;
    Ok(TicksHz { ticks, hz })
}
/// GNU `decode_time_components` (`src/timefns.c:855`): combine the HIGH, LOW,
/// USEC and PSEC components at resolution HZ (1, 10**6 or 10**12) into an exact
/// `(TICKS . HZ)`. USEC/PSEC out-of-range values carry into higher-order
/// components exactly as GNU does.
#[doc(hidden)]
pub fn decode_time_components(
    high: &Integer,
    low: &Integer,
    usec: &Integer,
    psec: &Integer,
    hz: &Integer,
) -> TicksHz {
    let million = million_int();
    let trillion = trillion_int();

    // us += ps / 1000000 (floor); ps, us reduced to [0, 1000000).
    let (ps_carry, ps_norm) = integer_fdiv_qr(psec, &million);
    let us_adj = usec + ps_carry;
    let (s_from_us_ps, us_norm) = integer_fdiv_qr(&us_adj, &million);

    // seconds = high * 2**16 + low + s_from_us_ps.
    let lo_time_bits = Integer::from(1i64 << 16);
    let s = high * &lo_time_bits + low + &s_from_us_ps;

    let ticks = if *hz == trillion {
        &s * &trillion + &us_norm * &million + &ps_norm
    } else if *hz == million {
        &s * &million + &us_norm
    } else {
        // hz == 1 (the (A B) / (A B . C-as-list) forms): drop sub-second.
        s
    };

    TicksHz {
        ticks,
        hz: hz.clone(),
    }
}
/// A time value whose every part is a fixnum, as the exact pair
/// `ticks / hz` in machine integers.
///
/// GNU keeps `struct lisp_time` as a fixnum pair whenever the parts fit
/// (`FASTER_TIMEFNS`) and falls back to `mpz` only on overflow.  Every
/// constructor below returns `None` for anything it cannot represent exactly
/// (a bignum or float part, a malformed list, an overflowing product), so the
/// caller takes the arbitrary-precision path and its errors unchanged.
#[derive(Clone, Copy, Debug)]
struct SmallTime {
    ticks: i128,
    hz: i128,
    form: TimeInputForm,
}

const SMALL_MILLION: i128 = 1_000_000;
const SMALL_TRILLION: i128 = 1_000_000_000_000;

impl SmallTime {
    /// The small form of `val`, for the forms `decode_lisp_time` reads from
    /// fixnums alone: an integer, `(TICKS . HZ)`, `(HIGH LOW)`,
    /// `(HIGH LOW USEC)`, `(HIGH LOW . USEC)` and `(HIGH LOW USEC PSEC)`.
    fn decode(val: &Value) -> Option<Self> {
        if let Some(n) = val.as_fixnum() {
            return Some(Self {
                ticks: i128::from(n),
                hz: 1,
                form: TimeInputForm::Scalar,
            });
        }
        if !val.is_cons() {
            return None;
        }
        let high = val.cons_car();
        let rest = val.cons_cdr();
        if !rest.is_cons() {
            let ticks = high.as_fixnum()?;
            let hz = rest.as_fixnum().filter(|hz| *hz > 0)?;
            return Some(Self {
                ticks: i128::from(ticks),
                hz: i128::from(hz),
                form: TimeInputForm::TicksHz,
            });
        }
        let high = i128::from(high.as_fixnum()?);
        let low = i128::from(rest.cons_car().as_fixnum()?);
        let tail = rest.cons_cdr();
        let (usec, psec, hz) = if tail.is_cons() {
            let usec = i128::from(tail.cons_car().as_fixnum()?);
            let tail2 = tail.cons_cdr();
            if tail2.is_cons() {
                (usec, i128::from(tail2.cons_car().as_fixnum()?), SMALL_TRILLION)
            } else {
                (usec, 0, SMALL_MILLION)
            }
        } else if tail.is_nil() {
            (0, 0, 1)
        } else {
            (i128::from(tail.as_fixnum()?), 0, SMALL_MILLION)
        };
        // `decode_time_components`: USEC and PSEC carry into the seconds.
        let us_adj = usec + psec.div_euclid(SMALL_MILLION);
        let seconds = high * 65536 + low + us_adj.div_euclid(SMALL_MILLION);
        let ticks = match hz {
            SMALL_TRILLION => seconds
                .checked_mul(SMALL_TRILLION)?
                .checked_add(us_adj.rem_euclid(SMALL_MILLION) * SMALL_MILLION)?
                .checked_add(psec.rem_euclid(SMALL_MILLION))?,
            SMALL_MILLION => seconds
                .checked_mul(SMALL_MILLION)?
                .checked_add(us_adj.rem_euclid(SMALL_MILLION))?,
            _ => seconds,
        };
        Some(Self {
            ticks,
            hz,
            form: TimeInputForm::List,
        })
    }

    /// Whole seconds, `floor (ticks / hz)` (`ticks_hz_seconds`).
    fn whole_seconds(self) -> i128 {
        self.ticks.div_euclid(self.hz)
    }
}

/// A Lisp integer from a machine integer, bignum only past the fixnum range.
fn small_integer_value(n: i128) -> Value {
    match i64::try_from(n) {
        Ok(n)
            if (crate::tagged::value::TaggedValue::MOST_NEGATIVE_FIXNUM
                ..=crate::tagged::value::TaggedValue::MOST_POSITIVE_FIXNUM)
                .contains(&n) =>
        {
            Value::fixnum(n)
        }
        _ => Value::make_integer(Integer::from(n)),
    }
}

/// Whole seconds of the fixnum-only time forms, or `None` for any value the
/// arbitrary-precision decoder must read (floats, nil, bignums, bad input).
#[doc(hidden)]
pub fn small_time_whole_seconds(val: &Value) -> Option<i64> {
    i64::try_from(SmallTime::decode(val)?.whole_seconds()).ok()
}

/// `float-time` of a fixnum-only time form when the quotient is exact in
/// binary64: both operands below 2**53 make the one IEEE division the
/// correctly rounded rational, which is `frac_to_double`'s answer.
#[doc(hidden)]
pub fn small_time_float(val: &Value) -> Option<f64> {
    const EXACT: i128 = 1 << 53;
    let time = SmallTime::decode(val)?;
    (time.ticks.abs() < EXACT && time.hz < EXACT).then(|| time.ticks as f64 / time.hz as f64)
}

/// `time_arith_to_lisp` for machine integers; `None` when the result needs the
/// arbitrary-precision constructors.
fn small_arith_to_lisp(ticks: i128, hz: i128, a: TimeInputForm, b: TimeInputForm) -> Option<Value> {
    if hz == 1 {
        return Some(small_integer_value(ticks));
    }
    if a == TimeInputForm::TicksHz || b == TimeInputForm::TicksHz || SMALL_TRILLION % hz != 0 {
        return Some(Value::cons(small_integer_value(ticks), small_integer_value(hz)));
    }
    // `ticks_hz_list4`: floor (ticks * 10**12 / hz), split into seconds and a
    // 12-digit fraction, then seconds into 16-bit HIGH and LOW.
    let scaled = ticks.checked_mul(SMALL_TRILLION)?.div_euclid(hz);
    let seconds = scaled.div_euclid(SMALL_TRILLION);
    let fraction = scaled.rem_euclid(SMALL_TRILLION);
    Some(Value::list_from_slice(&[
        small_integer_value(seconds.div_euclid(65536)),
        small_integer_value(seconds.rem_euclid(65536)),
        small_integer_value(fraction / SMALL_MILLION),
        small_integer_value(fraction % SMALL_MILLION),
    ]))
}

/// GNU `decode_lisp_time` (`src/timefns.c:959`), returning the exact
/// `(TICKS . HZ)` and the syntactic form. Accepts the same four canonical and
/// three compatibility forms GNU does:
///   nil                -> current time
///   integer            -> SEC . 1
///   float              -> exact power-of-two pair
///   (TICKS . HZ)       -> as-is (TICKS/HZ may be bignums)
///   (HIGH LOW)         -> hz 1
///   (HIGH LOW . USEC)  -> hz 10**6
///   (HIGH LOW USEC)    -> hz 10**6
///   (HIGH LOW USEC PSEC) -> hz 10**12
#[doc(hidden)]
pub fn decode_lisp_time(val: &Value) -> Result<DecodedLispTime, Flow> {
    match val.kind() {
        ValueKind::Nil => {
            let now = TimeMicros::now();
            // current_time uses a 10**9 (nanosecond) clock; mirror GNU's
            // timespec-derived (TICKS . timespec_hz) for the current time.
            // `psecs` holds the sub-microsecond part as (ns % 1000) * 1000, so
            // its nanosecond contribution is psecs / 1000 — dropping it made
            // `(time-add nil N)` (and thus timer.el's timer vectors) lose the
            // PSEC field GNU carries.
            let hz = Integer::from(1_000_000_000i64);
            let ticks = Integer::from(now.secs) * &hz
                + Integer::from(now.usecs) * Integer::from(1000i64)
                + Integer::from(now.psecs / 1000);
            Ok(DecodedLispTime {
                th: TicksHz { ticks, hz },
                form: TimeInputForm::List,
            })
        }
        ValueKind::Fixnum(n) => Ok(DecodedLispTime {
            th: TicksHz {
                ticks: Integer::from(n),
                hz: Integer::from(1),
            },
            form: TimeInputForm::Scalar,
        }),
        ValueKind::Veclike(VecLikeType::Bignum) => Ok(DecodedLispTime {
            th: TicksHz {
                ticks: val.as_bignum().expect("bignum payload").clone(),
                hz: Integer::from(1),
            },
            form: TimeInputForm::Scalar,
        }),
        ValueKind::Float => {
            let f = val.xfloat();
            if !f.is_finite() {
                // GNU `decode_lisp_time` (`src/timefns.c:1037`):
                // `time_error (isnan (d) ? EDOM : EOVERFLOW)`. EDOM (NaN) maps to
                // `time_spec_invalid` ("Invalid time specification"); EOVERFLOW
                // (±inf) maps to `time_overflow` ("Specified time is not
                // representable").
                return Err(if f.is_nan() {
                    time_spec_invalid()
                } else {
                    time_error_overflow()
                });
            }
            Ok(DecodedLispTime {
                th: decode_float_time(f)?,
                form: TimeInputForm::List,
            })
        }
        ValueKind::Cons => {
            let high = val.cons_car();
            let low = val.cons_cdr();
            if low.is_cons() {
                // (HIGH LOW ...) old-format list.
                let mut usec = Integer::from(0);
                let mut psec = Integer::from(0);
                let mut hz = Integer::from(1);
                let low_car = low.cons_car();
                let low_tail = low.cons_cdr();
                if low_tail.is_cons() {
                    usec = value_to_integer(&low_tail.cons_car()).ok_or_else(time_spec_invalid)?;
                    let tail2 = low_tail.cons_cdr();
                    if tail2.is_cons() {
                        psec = value_to_integer(&tail2.cons_car()).ok_or_else(time_spec_invalid)?;
                        hz = trillion_int();
                    } else {
                        hz = million_int();
                    }
                } else if !low_tail.is_nil() {
                    // (HIGH LOW . USEC) dotted form.
                    usec = value_to_integer(&low_tail).ok_or_else(time_spec_invalid)?;
                    hz = million_int();
                }
                let high_i = value_to_integer(&high).ok_or_else(time_spec_invalid)?;
                let low_i = value_to_integer(&low_car).ok_or_else(time_spec_invalid)?;
                Ok(DecodedLispTime {
                    th: decode_time_components(&high_i, &low_i, &usec, &psec, &hz),
                    form: TimeInputForm::List,
                })
            } else {
                // (TICKS . HZ): TICKS integer, HZ positive integer.
                let ticks = value_to_integer(&high).ok_or_else(time_spec_invalid)?;
                let hz = value_to_integer(&low).ok_or_else(time_spec_invalid)?;
                if hz <= 0 {
                    return Err(time_spec_invalid());
                }
                Ok(DecodedLispTime {
                    th: TicksHz { ticks, hz },
                    form: TimeInputForm::TicksHz,
                })
            }
        }
        _ => Err(time_spec_invalid()),
    }
}
/// GNU `frac_to_double` (`src/timefns.c:408`): convert the exact rational
/// `numerator / denominator` to the nearest double (round to even). malachite's
/// `Rational` -> `f64` rounding-from gives the correctly-rounded result.
#[doc(hidden)]
pub fn frac_to_double(numerator: &Integer, denominator: &Integer) -> f64 {
    use malachite::base::num::conversion::traits::RoundingFrom;
    use malachite::rational::Rational;
    let q = Rational::from_integers(numerator.clone(), denominator.clone());
    f64::rounding_from(&q, RoundingMode::Nearest).0
}
/// GNU `ticks_hz_list4` (`src/timefns.c:664`): render an exact `(TICKS . HZ)`
/// as the backward-compatible `(HI LO US PS)` list, dropping any excess
/// precision below 10**-12 s (floor).
#[doc(hidden)]
pub fn ticks_hz_list4(ticks: &Integer, hz: &Integer) -> Value {
    let trillion = trillion_int();
    let million = million_int();
    // floor((ticks * trillion) / hz).
    let scaled = integer_div_floor(&(ticks * &trillion), hz);
    // Split into seconds and the 12-digit sub-second remainder.
    let (secs, rem) = integer_fdiv_qr(&scaled, &trillion);
    let us = &rem / &million;
    let ps = &rem - &us * &million;
    // Split seconds into HI/LO at 16 bits (LO non-negative).
    let lo_time_bits = Integer::from(1i64 << 16);
    let (hi, lo) = integer_fdiv_qr(&secs, &lo_time_bits);
    Value::list(vec![
        Value::make_integer(hi),
        Value::make_integer(lo),
        Value::make_integer(us),
        Value::make_integer(ps),
    ])
}
/// True if the positive integer HZ divides evenly into a trillion
/// (GNU `trillion_factor`, `src/timefns.c:98`).
#[doc(hidden)]
pub fn trillion_factor(hz: &Integer) -> bool {
    &trillion_int() % hz == 0
}
/// GNU `ticks_hz_seconds` (`src/timefns.c:800`): floor(ticks / hz) — the whole
/// seconds of an exact `(TICKS . HZ)` value.
#[doc(hidden)]
pub fn ticks_hz_seconds(t: &TicksHz) -> Integer {
    integer_div_floor(&t.ticks, &t.hz)
}
/// GNU `ticks_hz_hz_ticks` (`src/timefns.c:747`): convert T to a count of
/// `hz_out` ticks, taking the floor — floor((t.ticks * hz_out) / t.hz). HZ_OUT
/// must be a positive integer.
#[doc(hidden)]
pub fn ticks_hz_hz_ticks(t: &TicksHz, hz_out: &Integer) -> Result<Integer, Flow> {
    if hz_out <= &Integer::from(0) {
        return Err(signal(
            "error",
            vec![Value::string("Invalid time frequency")],
        ));
    }
    if t.hz == *hz_out {
        return Ok(t.ticks.clone());
    }
    Ok(integer_div_floor(&(&t.ticks * hz_out), &t.hz))
}
/// GNU `time_arith` (`src/timefns.c:1127`): add (or subtract) two Lisp time
/// values exactly and choose the GNU result representation.
#[doc(hidden)]
pub fn time_arith(a: &Value, b: &Value, subtract: bool) -> Result<Value, Flow> {
    // Equal frequencies (two integers, two `(HIGH LOW)` lists, two
    // `(HIGH LOW USEC PSEC)` lists) need only one machine-integer add.
    if let (Some(sa), Some(sb)) = (SmallTime::decode(a), SmallTime::decode(b))
        && sa.hz == sb.hz
        && let Some(ticks) = if subtract {
            sa.ticks.checked_sub(sb.ticks)
        } else {
            sa.ticks.checked_add(sb.ticks)
        }
        && let Some(value) = small_arith_to_lisp(ticks, sa.hz, sa.form, sb.form)
    {
        return Ok(value);
    }
    time_arith_exact(a, b, subtract)
}

/// `time_arith` on the arbitrary-precision pair, for every input the fixnum
/// fast path declines.
fn time_arith_exact(a: &Value, b: &Value, subtract: bool) -> Result<Value, Flow> {
    let da = decode_lisp_time(a)?;
    let db = decode_lisp_time(b)?;

    let (ticks, hz) = if da.th.hz == db.th.hz {
        let ticks = if subtract {
            &da.th.ticks - &db.th.ticks
        } else {
            &da.th.ticks + &db.th.ticks
        };
        (ticks, da.th.hz.clone())
    } else {
        // Compute (na*(db/g) OP nb*(da/g)) / lcm(da,db), then normalize by the
        // gcd of numerator and denominator, rescaling up so the denominator is
        // never coarser than the finer of the two inputs (GNU hzmin rule).
        let g = integer_gcd(&da.th.hz, &db.th.hz);
        let fa = &da.th.hz / &g; // da/g
        let fb = &db.th.hz / &g; // db/g
        let mut ihz = &fa * &db.th.hz; // lcm(da, db)
        let mut iticks = &fb * &da.th.ticks;
        if subtract {
            iticks -= &fa * &db.th.ticks;
        } else {
            iticks += &fa * &db.th.ticks;
        }

        let ig = integer_gcd(&iticks, &ihz);
        if ig > 1 {
            iticks /= &ig;
            ihz /= &ig;
            let hzmin = if da.th.hz < db.th.hz {
                &da.th.hz
            } else {
                &db.th.hz
            };
            if ihz < *hzmin {
                // rescale = ceil(hzmin / ihz).
                let rescale = {
                    let (q, r) = integer_fdiv_qr(hzmin, &ihz);
                    if r == 0 { q } else { q + Integer::from(1) }
                };
                iticks *= &rescale;
                ihz *= &rescale;
            }
        }
        (iticks, ihz)
    };

    Ok(time_arith_to_lisp(ticks, hz, &da, &db))
}
/// Select the result form, mirroring the final `return` of GNU `time_arith`
/// (`src/timefns.c:1211`). `current-time-list` defaults to t in Neomacs, so an
/// integer HZ != 1 yields the list form unless an input used `(TICKS . HZ)` or
/// HZ does not divide a trillion.
#[doc(hidden)]
pub fn time_arith_to_lisp(
    ticks: Integer,
    hz: Integer,
    da: &DecodedLispTime,
    db: &DecodedLispTime,
) -> Value {
    if hz == 1 {
        return Value::make_integer(ticks);
    }
    let a_is_ticks_hz = da.form == TimeInputForm::TicksHz;
    let b_is_ticks_hz = db.form == TimeInputForm::TicksHz;
    if a_is_ticks_hz || b_is_ticks_hz || !trillion_factor(&hz) {
        Value::cons(Value::make_integer(ticks), Value::make_integer(hz))
    } else {
        ticks_hz_list4(&ticks, &hz)
    }
}
/// GNU `time_cmp` (`src/timefns.c:1250`): compare two exact time values by
/// cross-multiplying ATICKS*BHZ vs BTICKS*AHZ.
#[doc(hidden)]
pub fn time_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering, Flow> {
    if let (Some(sa), Some(sb)) = (SmallTime::decode(a), SmallTime::decode(b))
        && let (Some(lhs), Some(rhs)) = (sa.ticks.checked_mul(sb.hz), sb.ticks.checked_mul(sa.hz))
    {
        return Ok(lhs.cmp(&rhs));
    }
    time_cmp_exact(a, b)
}

/// `time_cmp` on the arbitrary-precision pair.
fn time_cmp_exact(a: &Value, b: &Value) -> Result<std::cmp::Ordering, Flow> {
    let da = decode_lisp_time(a)?;
    let db = decode_lisp_time(b)?;
    let lhs = &da.th.ticks * &db.th.hz;
    let rhs = &db.th.ticks * &da.th.hz;
    Ok(lhs.cmp(&rhs))
}
#[doc(hidden)]
pub fn time_convert_default_hz(value: &Value) -> i64 {
    match value.kind() {
        ValueKind::Fixnum(_) => 1,
        ValueKind::Float => 1_000_000,
        ValueKind::Cons => {
            let tail = value.cons_cdr();
            if !tail.is_cons() {
                return tail.as_int().filter(|hz| *hz > 0).unwrap_or(1);
            }
            if let Some(items) = list_to_vec(value) {
                match items.len() {
                    0..=2 => 1,
                    3 => 1_000_000,
                    _ => 1_000_000_000_000,
                }
            } else {
                1
            }
        }
        _ => 1,
    }
}
#[doc(hidden)]
pub fn current_time_value(output: LispTimeOutput) -> Value {
    output.encode(TimeMicros::now())
}
/// Convert a system `(seconds, nanoseconds)` timestamp to the representation
/// selected by `current-time-list`, matching GNU `timefns.c:make_lisp_time`.
#[doc(hidden)]
pub fn make_lisp_time(secs: i64, nanos: i64, output: LispTimeOutput) -> Value {
    output.encode(TimeMicros::from_timespec(secs, nanos))
}

#[cfg(test)]
#[path = "time_core/tests/small_time_test.rs"]
mod small_time_tests;
