//! The fixnum fast path of the time functions answers exactly what the
//! arbitrary-precision path answers, form for form.

use super::*;

/// A deterministic LCG, so a failure names a reproducible input.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 20
    }

    /// A value from the edges and the middle of each component's range.
    fn component(&mut self) -> i64 {
        const EDGES: [i64; 9] = [
            0,
            1,
            -1,
            999_999,
            1_000_000,
            -1_000_000,
            65_535,
            65_536,
            i64::MAX >> 4,
        ];
        match self.next() % 3 {
            0 => EDGES[(self.next() % EDGES.len() as u64) as usize],
            1 => (self.next() % 40_000) as i64 - 20_000,
            _ => (self.next() % 4_000_000_000) as i64 - 2_000_000_000,
        }
    }

    fn time(&mut self) -> Value {
        let (a, b, c, d) = (
            self.component(),
            self.component(),
            self.component(),
            self.component(),
        );
        let fix = Value::fixnum;
        match self.next() % 7 {
            0 => fix(a),
            1 => Value::list_from_slice(&[fix(a % 30_000), fix(b)]),
            2 => Value::list_from_slice(&[fix(a % 30_000), fix(b), fix(c)]),
            3 => Value::cons(fix(a % 30_000), Value::cons(fix(b), fix(c))),
            4 => Value::list_from_slice(&[fix(a % 30_000), fix(b), fix(c), fix(d)]),
            5 => Value::cons(fix(a), fix(self.component().abs() % 5_000_000 + 1)),
            _ => Value::cons(fix(a), fix(1)),
        }
    }
}

/// A canonical text for a Lisp integer, cons or nil result.
fn show(value: Value) -> String {
    if value.is_nil() {
        "nil".into()
    } else if value.is_cons() {
        format!("({} . {})", show(value.cons_car()), show(value.cons_cdr()))
    } else if let Some(n) = value.as_fixnum() {
        n.to_string()
    } else if let Some(n) = value.as_bignum() {
        format!("big{n}")
    } else {
        "?".into()
    }
}

fn show_result(result: Result<Value, Flow>) -> String {
    result.map_or_else(|_| "error".into(), show)
}

#[test]
fn small_decode_matches_the_exact_decode() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..20_000 {
        let time = rng.time();
        let small = SmallTime::decode(&time).unwrap_or_else(|| panic!("decodes {}", show(time)));
        let exact = decode_lisp_time(&time).expect("exact decode").th;
        assert_eq!(Integer::from(small.ticks), exact.ticks, "ticks of {}", show(time));
        assert_eq!(Integer::from(small.hz), exact.hz, "hz of {}", show(time));
    }
}

#[test]
fn small_arithmetic_and_comparison_match_the_exact_path() {
    let mut rng = Lcg(0xfeed);
    for _ in 0..20_000 {
        let (a, b) = (rng.time(), rng.time());
        for subtract in [false, true] {
            assert_eq!(
                show_result(time_arith(&a, &b, subtract)),
                show_result(time_arith_exact(&a, &b, subtract)),
                "{} {} {}",
                show(a),
                if subtract { "-" } else { "+" },
                show(b)
            );
        }
        assert_eq!(
            time_cmp(&a, &b).ok(),
            time_cmp_exact(&a, &b).ok(),
            "{} <=> {}",
            show(a),
            show(b)
        );
    }
}

#[test]
fn small_projections_match_the_exact_path() {
    let mut rng = Lcg(0xbeef);
    for _ in 0..20_000 {
        let time = rng.time();
        let exact = decode_lisp_time(&time).expect("exact decode").th;
        assert_eq!(
            small_time_whole_seconds(&time).map(Integer::from),
            Some(ticks_hz_seconds(&exact)),
            "seconds of {}",
            show(time)
        );
        if let Some(float) = small_time_float(&time) {
            assert_eq!(
                float.to_bits(),
                frac_to_double(&exact.ticks, &exact.hz).to_bits(),
                "float-time of {}",
                show(time)
            );
        }
        let (secs, nanos) = time_value_seconds_and_nanos(&time).expect("seconds and nanos");
        let total = integer_div_floor(&(&exact.ticks * Integer::from(1_000_000_000)), &exact.hz);
        let (want_secs, want_nanos) = integer_fdiv_qr(&total, &Integer::from(1_000_000_000));
        assert_eq!(
            (Integer::from(secs), Integer::from(nanos)),
            (want_secs, want_nanos),
            "seconds and nanos of {}",
            show(time)
        );
    }
}

#[test]
fn forms_outside_the_fixnum_range_take_the_exact_path() {
    let huge = Value::make_integer(Integer::from(1_i128 << 80));
    for time in [
        huge,
        Value::cons(Value::fixnum(1), Value::make_integer(Integer::from(1_i128 << 80))),
        Value::cons(Value::fixnum(1), Value::fixnum(0)),
        Value::list_from_slice(&[Value::fixnum(1), Value::make_float(0.5)]),
        Value::make_float(1.5),
        Value::NIL,
    ] {
        assert!(SmallTime::decode(&time).is_none(), "{}", show(time));
    }
    // A bignum operand still adds exactly.
    let sum = time_arith(&huge, &Value::fixnum(1), false).expect("bignum sum");
    assert_eq!(show(sum), format!("big{}", Integer::from((1_i128 << 80) + 1)));
}
