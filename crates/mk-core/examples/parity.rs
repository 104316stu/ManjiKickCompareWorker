//! Parity run: compare every corpus pair in Rust and diff against the
//! Python reference (parity/py_reference.py).
//!
//!   cargo run -p mk-core --release --example parity -- parity/corpus [--max-tempos N] [--threads N] [--production]
//!       [--decode <ffmpeg>]   (also re-decode clip.bin / video.<ext> with mk_core::decode
//!                              and check the PCM equals Python's)
//!
//! Reads <pair>/clip.f32, video.f32 and py.json; writes <pair>/rs.json and
//! prints one line per pair plus a summary. Exits 1 when the gate fails:
//! verdicts equal on < 99.5 % of pairs, or any Rust 'same' where Python
//! said 'different'.

use mk_core::{compare_with, decode, Clip, Options, Source, MAX_CLIP_TOTAL_SECONDS, MAX_VIDEO_SECONDS};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let corpus = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("parity/corpus"));
    let production = args.iter().any(|a| a == "--production");
    // --production: the worker's own path (Options::default(): 40 s budget,
    // one thread) against py_prod.json, Python with its real budget.
    let mut opts = if production { Options::default() } else { Options::exhaustive() };
    let (py_name, rs_name) = if production { ("py_prod.json", "rs_prod.json") } else { ("py.json", "rs.json") };
    if let Some(i) = args.iter().position(|a| a == "--max-tempos") {
        opts.max_tempos = Some(args[i + 1].parse().unwrap());
    }
    if let Some(i) = args.iter().position(|a| a == "--threads") {
        opts.threads = args[i + 1].parse().unwrap();
    }
    let ffmpeg = args.iter().position(|a| a == "--decode").map(|i| PathBuf::from(&args[i + 1]));
    let (mut decode_checked, mut decode_diff) = (0, 0);
    let mut shift_cut = 0;
    let mut dirs: Vec<(u64, PathBuf)> = std::fs::read_dir(&corpus)
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            let n = p.file_name()?.to_str()?.parse::<u64>().ok()?;
            p.join(py_name).exists().then_some((n, p))
        })
        .collect();
    dirs.sort();

    let (mut pairs, mut agree, mut false_same, mut errors) = (0, 0, 0, 0);
    let mut numeric_off = Vec::new();
    let mut mismatches = Vec::new();
    let (mut rs_ms_total, mut py_ms_total) = (0.0, 0.0);
    let mut by_verdict = std::collections::BTreeMap::<String, (u32, u32)>::new();
    let mut ratios = Vec::new();
    for (n, dir) in &dirs {
        let py: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(py_name)).unwrap()).unwrap();
        let Some(pr) = py.get("result") else {
            errors += 1;
            println!("{n:>4} python error: {}", py["error"]);
            continue;
        };
        let clip = match Clip::from_pcm(read_f32(&dir.join("clip.f32"))) {
            Ok(c) => c,
            Err(e) => {
                println!("{n:>4} rust clip error: {e}");
                errors += 1;
                continue;
            }
        };
        let video = read_f32(&dir.join("video.f32"));
        if let Some(ffmpeg) = &ffmpeg {
            // mk_core::decode must give the very PCM Python's decode gave.
            let clip_bytes = std::fs::read(dir.join("clip.bin")).unwrap();
            let meta: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
            let a = decode(ffmpeg, Source::Bytes(&clip_bytes), MAX_CLIP_TOTAL_SECONDS).unwrap();
            let b = decode(ffmpeg, Source::Path(&dir.join(meta["video_file"].as_str().unwrap())), MAX_VIDEO_SECONDS)
                .unwrap();
            let same = a == read_f32(&dir.join("clip.f32")) && b == video;
            decode_checked += 1;
            if !same {
                decode_diff += 1;
                println!("{n:>4} DECODE DIFFERS from Python's");
            }
        }
        let t0 = Instant::now();
        let r = compare_with(&clip, &video, &opts);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let rv = serde_json::to_value(&r).unwrap();
        std::fs::write(dir.join(rs_name), serde_json::to_string(&json!({"ms": ms, "result": rv})).unwrap()).unwrap();
        pairs += 1;
        let py_ms = py["ms"].as_f64().unwrap_or(0.0);
        rs_ms_total += ms;
        py_ms_total += py_ms;
        if ms > 0.0 {
            ratios.push(py_ms / ms);
        }
        let pv = pr["verdict"].as_str().unwrap().to_string();
        let e = by_verdict.entry(pv.clone()).or_default();
        e.0 += 1;
        if pv == r.verdict {
            agree += 1;
            e.1 += 1;
        } else {
            mismatches.push(format!("{n}: py {pv} rs {} | py {} | rs {}", r.verdict, pr, rv));
        }
        if pv == "different" && r.verdict == "same" {
            false_same += 1;
        }
        // Numeric tolerances, on pairs where both found a match.
        let mut off = Vec::new();
        if pv == r.verdict {
            let (pa, ra) = (num(pr, "aligned").unwrap(), r.aligned as f64);
            if (pa - ra).abs() > (0.02 * pa).max(3.0) {
                off.push(format!("aligned {pa} vs {ra}"));
            }
            let (pd, rd) = (num(pr, "density").unwrap(), r.density);
            if (pd - rd).abs() > 0.02 * pd.max(1.0) {
                off.push(format!("density {pd} vs {rd}"));
            }
            if let (Some(po), Some(ro)) = (num(pr, "offset_seconds"), r.offset_seconds) {
                if (po - ro).abs() > 0.064 + 1e-9 {
                    off.push(format!("offset {po} vs {ro}"));
                }
            }
            let (ps, rs) = (num(pr, "speed").unwrap(), r.speed);
            if (ps - rs).abs() > 0.0025 + 1e-9 {
                off.push(format!("speed {ps} vs {rs}"));
            }
            if let (Some(pm), Some(rm)) = (num(pr, "matched_segments"), r.matched_segments) {
                if (pm - rm as f64).abs() > 1.0 {
                    off.push(format!("segments {pm} vs {rm}"));
                }
            }
            let (pc, rc) = (num(pr, "coverage").unwrap(), r.coverage);
            if (pc - rc).abs() > 0.05 {
                off.push(format!("coverage {pc} vs {rc}"));
            }
            let (psm, rsm) = (num(pr, "semitones"), r.semitones);
            if psm.is_some() != rsm.is_some() || (psm.unwrap_or(0.0) - rsm.unwrap_or(0.0)).abs() > 0.07 {
                off.push(format!("semitones {psm:?} vs {rsm:?}"));
            }
        }
        // shift_tempos / shift_cut are the worker's own additions.
        let mut rv_cmp = rv.clone();
        if let Some(m) = rv_cmp.as_object_mut() {
            m.remove("shift_tempos");
            m.remove("shift_cut");
        }
        let exact = rv_cmp == *pr;
        if r.shift_cut {
            shift_cut += 1;
        }
        println!(
            "{n:>4} py {pv:<9} rs {:<9} {:>8.1} ms (py {:>8.1} ms){}{}",
            r.verdict,
            ms,
            py_ms,
            if exact { "  identical" } else { "" },
            if off.is_empty() { String::new() } else { format!("  OFF: {}", off.join(", ")) }
        );
        if !off.is_empty() {
            numeric_off.push(format!("{n}: {}", off.join(", ")));
        }
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if ratios.is_empty() { 0.0 } else { ratios[ratios.len() / 2] };
    let share = if pairs > 0 { agree as f64 / pairs as f64 } else { 0.0 };
    println!("\npairs {pairs}, python errors {errors}");
    for (v, (n, a)) in &by_verdict {
        println!("  python {v}: {n}, rust agrees on {a}");
    }
    if ffmpeg.is_some() {
        println!("decode: {decode_checked} pairs re-decoded in Rust, {decode_diff} differ from Python's PCM");
    }
    println!("rust shift searches cut short by the budget: {shift_cut}");
    println!("verdict agreement {agree}/{pairs} = {:.2} %", share * 100.0);
    println!("rust 'same' where python 'different': {false_same}");
    println!("numeric tolerance misses: {}", numeric_off.len());
    for m in &numeric_off {
        println!("  {m}");
    }
    println!("verdict mismatches: {}", mismatches.len());
    for m in &mismatches {
        println!("  {m}");
    }
    println!(
        "time: rust {:.1} s total, python {:.1} s total; speedup total {:.1}x, median per pair {:.1}x",
        rs_ms_total / 1000.0,
        py_ms_total / 1000.0,
        py_ms_total / rs_ms_total.max(1e-9),
        median
    );
    let pass = pairs > 0 && share >= 0.995 && false_same == 0;
    println!("GATE: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
