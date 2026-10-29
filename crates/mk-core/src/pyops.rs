//! The handful of Python / numpy operations whose exact semantics decide
//! results: Python's round(), numpy's percentile, hanning window, pairwise
//! summation and np.interp.

use std::f64::consts::PI;

/// Python's `round(x, ndigits)`: the double nearest to x rounded to
/// `ndigits` decimals, ties (on the exact binary value) to even.
///
/// Rust's fixed-precision formatting is exact and rounds ties to even
/// (since 1.67), and parsing is correctly rounded, which is exactly how
/// CPython implements it (dtoa mode 3, then strtod).
pub fn py_round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let s = format!("{:.*}", ndigits, x);
    s.parse::<f64>().unwrap_or(x)
}

/// Python's `round(x)` (no ndigits) on a float: an int, ties to even.
pub fn py_round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `np.round(x)` (decimals=0): rint, ties to even, stays float.
#[inline]
pub fn np_round(x: f64) -> f64 {
    x.round_ties_even()
}

/// `np.hanning(m).astype(np.float32)`: 0.5 + 0.5*cos(pi*n/(m-1)) for
/// n = 1-m, 3-m, ..., m-1.
pub fn hanning_f32(m: usize) -> Vec<f32> {
    if m == 0 {
        return Vec::new();
    }
    if m == 1 {
        return vec![1.0];
    }
    let denom = (m - 1) as f64;
    (0..m)
        .map(|i| {
            let n = (1 - m as i64 + 2 * i as i64) as f64;
            (0.5 + 0.5 * (PI * n / denom).cos()) as f32
        })
        .collect()
}

/// numpy's pairwise summation (DOUBLE_pairwise_sum), as used by a
/// reduction over a contiguous axis, starting from the identity 0.0.
pub fn pairwise_sum(a: &[f64]) -> f64 {
    0.0 + pairwise_inner(a)
}

fn pairwise_inner(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut res = 0.0;
        for &v in a {
            res += v;
        }
        res
    } else if n <= 128 {
        let mut r = [0.0f64; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        pairwise_inner(&a[..n2]) + pairwise_inner(&a[n2..])
    }
}

/// `np.percentile(values, q)` with the default 'linear' method.
pub fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() || values.iter().any(|v| v.is_nan()) {
        return f64::NAN; // numpy: NaN in, NaN out
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    let quantile = q / 100.0;
    let virtual_index = (n as f64 - 1.0) * quantile;
    let prev = virtual_index.floor();
    let gamma = virtual_index - prev;
    let lo = (prev.max(0.0) as usize).min(n - 1);
    let hi = (lo + 1).min(n - 1);
    lerp(v[lo], v[hi], gamma)
}

/// numpy's _lerp: a + (b-a)*t, computed from the far end when t >= 0.5.
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    let diff = b - a;
    if t >= 0.5 {
        b - diff * (1.0 - t)
    } else {
        a + diff * t
    }
}

/// `np.interp(np.arange(n_out) / speed, np.arange(len(x)), x).astype(f32)`
/// for the uniform grid _stretch uses.
pub fn interp_stretch(x: &[f32], n_out: usize, speed: f64) -> Vec<f32> {
    let mut out = Vec::with_capacity(n_out);
    if x.is_empty() {
        out.resize(n_out, f32::NAN);
        return out;
    }
    let len = x.len();
    let last = (len - 1) as f64;
    for i in 0..n_out {
        let p = i as f64 / speed;
        let v = if p > last {
            x[len - 1] as f64
        } else if p < 0.0 {
            x[0] as f64
        } else {
            let j = p.floor() as usize;
            if j >= len - 1 {
                x[len - 1] as f64
            } else if (j as f64) == p {
                x[j] as f64
            } else {
                let slope = (x[j + 1] as f64 - x[j] as f64) / 1.0;
                slope * (p - j as f64) + x[j] as f64
            }
        };
        out.push(v as f32);
    }
    out
}
