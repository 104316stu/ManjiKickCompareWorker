"""Expected values from Python/numpy for mk-core's unit tests.

    python -I parity/gen_unit_vectors.py

Writes crates/mk-core/tests/data/vectors.json. Floats are stored as their
IEEE bit patterns (u64 for f64, u32 for f32) so the tests compare exactly.
The audio_match functions are re-stated here (same expressions as the
site's file) so this needs only numpy.
"""

import json
import math
import os
import struct

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "crates", "mk-core", "tests", "data", "vectors.json")


def b64(x):
    return struct.unpack("<Q", struct.pack("<d", float(x)))[0]


def b32(x):
    return struct.unpack("<I", struct.pack("<f", float(x)))[0]


def speed_grid():
    grid, s = [], 0.78
    while s <= 1.46 + 1e-9:
        if abs(s - 1.0) > 0.015:
            grid.append(round(s, 4))
        s *= 1.01
    return sorted(grid, key=lambda v: abs(math.log(v)))


def tempo_grid():
    k_min = math.ceil(math.log(0.78) / math.log(1.01) - 1e-9)
    k_max = math.floor(math.log(1.6) / math.log(1.01) + 1e-9)
    return [round(1.01 ** k, 4) for k in sorted(range(k_min, k_max + 1), key=abs)]


def main():
    rng = np.random.default_rng(20261029)
    v = {}

    # Python round(x, n): ties on the exact binary value, and ordinary values.
    xs = [0.125, 0.375, 0.625, 2.675, 1.005, 0.5, 1.5, 2.5, -0.125, -2.675, 0.0, -0.0,
          1e-7, 123456.785, 3.14159, 0.03125, 0.09375, -0.03125]
    xs += list(rng.uniform(-100, 100, 300)) + list(rng.uniform(0, 1, 200) / 7)
    xs += [k / 31.25 for k in range(-400, 400, 7)] + [k / 8 for k in range(-50, 50)]
    v["py_round"] = [[b64(x), n, b64(round(float(x), n))] for x in xs for n in (0, 1, 2, 3, 4, 5)]

    # np.percentile(.., 90), linear.
    cases = []
    for n in (1, 2, 3, 5, 10, 11, 57, 240, 901):
        a = rng.normal(-30, 12, n)
        if n > 5:
            a[: n // 4] = np.round(a[: n // 4])  # some duplicates
        cases.append([[b64(x) for x in a], b64(np.percentile(a, 90))])
    v["percentile90"] = cases

    v["hanning1024_f32"] = [b32(x) for x in np.hanning(1024).astype(np.float32)]

    # The rms of _loud_run (np.mean pairwise summation), 8000-sample frames.
    x = (rng.standard_normal(8000 * 5) * 0.3).astype(np.float32)
    rms = np.sqrt(np.mean(np.square(x.astype(np.float64)).reshape(5, 8000), axis=1))
    v["rms_input_f32"] = [b32(s) for s in x]
    v["rms"] = [b64(r) for r in rms]

    v["speed_grid"] = [b64(s) for s in speed_grid()]
    v["tempo_grid"] = [b64(s) for s in tempo_grid()]

    # 2.0 ** (-s / 12) as numpy computes it for every semitone value the
    # shift search can use (multiples of 1/32, and those rounded to 4 places).
    semis = sorted({k / 32 for k in range(-160, 161)} | {round(k / 32, 4) for k in range(-160, 161)})
    arr = np.asarray(semis, np.float64)
    v["pitch_factor"] = [[b64(s), b64(f), b64(2.0 ** (-s / 12))] for s, f in zip(semis, 2.0 ** (-arr / 12))]

    # np.interp as _stretch uses it.
    y = rng.standard_normal(1000).astype(np.float32)
    interp = []
    for speed in (0.78, 0.9901, 1.0203, 1.2345, 1.46, 1.00001):
        n_out = int(len(y) * speed)
        pos = np.arange(n_out, dtype=np.float64) / speed
        interp.append([b64(speed), [b32(s) for s in np.interp(pos, np.arange(len(y)), y).astype(np.float32)]])
    v["interp_input_f32"] = [b32(s) for s in y]
    v["interp"] = interp

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as fh:
        json.dump(v, fh, separators=(",", ":"))
    print("wrote", os.path.normpath(OUT))


if __name__ == "__main__":
    main()
