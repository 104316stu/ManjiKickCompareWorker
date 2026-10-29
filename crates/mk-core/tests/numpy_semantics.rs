//! The Python/numpy operations the port relies on, against values Python
//! produced (parity/gen_unit_vectors.py -> tests/data/vectors.json).

use mk_core::internals::{loud_run, speed_grid, tempo_grid};
use mk_core::pyops::{hanning_f32, interp_stretch, pairwise_sum, percentile, py_round};
use serde_json::Value;

/// The expected values come from numpy on Windows. Another libm (glibc, or
/// a different MSVC runtime than numpy's) may round a last bit differently,
/// so those two checks allow one ulp.
const LIBM_ULPS: u64 = 1;

fn ulps32(a: f32, b: f32) -> u64 {
    (a.to_bits() as i64 - b.to_bits() as i64).unsigned_abs()
}

fn ulps64(a: f64, b: f64) -> u64 {
    (a.to_bits() as i64 - b.to_bits() as i64).unsigned_abs()
}

fn vectors() -> Value {
    let text = include_str!("data/vectors.json");
    serde_json::from_str(text).unwrap()
}

fn f64s(v: &Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| f64::from_bits(x.as_u64().unwrap())).collect()
}

fn f32s(v: &Value) -> Vec<f32> {
    v.as_array().unwrap().iter().map(|x| f32::from_bits(x.as_u64().unwrap() as u32)).collect()
}

#[test]
fn py_round_matches_python() {
    let v = vectors();
    let mut bad = Vec::new();
    for case in v["py_round"].as_array().unwrap() {
        let x = f64::from_bits(case[0].as_u64().unwrap());
        let n = case[1].as_u64().unwrap() as usize;
        let want = f64::from_bits(case[2].as_u64().unwrap());
        let got = py_round(x, n);
        if got.to_bits() != want.to_bits() {
            bad.push((x, n, want, got));
        }
    }
    assert!(bad.is_empty(), "{} mismatches, first: {:?}", bad.len(), &bad[..bad.len().min(5)]);
}

#[test]
fn percentile_matches_numpy() {
    let v = vectors();
    for case in v["percentile90"].as_array().unwrap() {
        let a = f64s(&case[0]);
        let want = f64::from_bits(case[1].as_u64().unwrap());
        assert_eq!(percentile(&a, 90.0).to_bits(), want.to_bits(), "n={}", a.len());
    }
}

#[test]
fn hanning_matches_numpy() {
    let v = vectors();
    let want = f32s(&v["hanning1024_f32"]);
    let got = hanning_f32(1024);
    let bad: Vec<usize> = (0..1024).filter(|&i| ulps32(got[i], want[i]) > LIBM_ULPS).collect();
    assert!(bad.is_empty(), "{} window values differ, e.g. index {:?}", bad.len(), bad.first());
}

#[test]
fn mean_is_numpy_pairwise() {
    let v = vectors();
    let x = f32s(&v["rms_input_f32"]);
    let want = f64s(&v["rms"]);
    for (i, w) in want.iter().enumerate() {
        let sq: Vec<f64> = x[i * 8000..(i + 1) * 8000].iter().map(|&s| (s as f64) * (s as f64)).collect();
        let rms = (pairwise_sum(&sq) / 8000.0).sqrt();
        assert_eq!(rms.to_bits(), w.to_bits(), "frame {i}");
    }
}

#[test]
fn grids_match_python() {
    let v = vectors();
    let sg = speed_grid();
    let want = f64s(&v["speed_grid"]);
    assert_eq!(sg.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), want.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    let tg = tempo_grid();
    let want = f64s(&v["tempo_grid"]);
    assert_eq!(tg.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), want.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
}

#[test]
fn pitch_factors_match_python() {
    let v = vectors();
    let mut bad = 0;
    let cases = v["pitch_factor"].as_array().unwrap();
    for case in cases {
        let s = f64::from_bits(case[0].as_u64().unwrap());
        let np_val = f64::from_bits(case[1].as_u64().unwrap());
        let py_val = f64::from_bits(case[2].as_u64().unwrap());
        let got = 2.0f64.powf(-s / 12.0);
        if ulps64(got, np_val) > LIBM_ULPS || ulps64(got, py_val) > LIBM_ULPS {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "{bad} of {} pitch factors differ in the last bit", cases.len());
}

#[test]
fn interp_matches_numpy() {
    let v = vectors();
    let y = f32s(&v["interp_input_f32"]);
    for case in v["interp"].as_array().unwrap() {
        let speed = f64::from_bits(case[0].as_u64().unwrap());
        let want = f32s(&case[1]);
        let n_out = (y.len() as f64 * speed) as usize;
        let got = interp_stretch(&y, n_out, speed);
        assert_eq!(got.len(), want.len());
        let bad = (0..got.len()).filter(|&i| got[i].to_bits() != want[i].to_bits()).count();
        assert_eq!(bad, 0, "speed {speed}: {bad} samples differ");
    }
}

#[test]
fn loud_run_short_input() {
    assert_eq!(loud_run(&[0.0; 8000]), None);
    let mut x = vec![0.0f32; 8000 * 20];
    for s in &mut x[8000 * 5..8000 * 12] {
        *s = 0.5;
    }
    assert_eq!(loud_run(&x), Some((8000 * 5, 8000 * 12)));
}

