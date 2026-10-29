"""Reference results from the site's own audio_match.py, for the parity check.

    AUDIO_MATCH_PY=/path/to/frontend/audio_match.py \
        python -I parity/py_reference.py [--jobs N] [--max-tempos N] [--force]

For every parity/corpus/<n>/ (clip.bin + video.<ext>) it decodes both sides
with audio_match.decode (imageio-ffmpeg's ffmpeg), writes the PCM the Rust
side reads (clip.f32: the clip decoded to MAX_CLIP_TOTAL_SECONDS, video.f32:
the video to MAX_VIDEO_SECONDS, raw f32le) and py.json: compare_pcm's result
and how long it took.

The shift search's wall-clock budget makes a result depend on the machine,
so it is lifted here (every tempo is searched, as the Rust parity run does);
--max-tempos N instead limits it to the first N tempos of the grid.
--production keeps the site's 40 s budget and writes py_prod.json (run it
with --jobs 1 so the clock means what it does on the server).
"""

import argparse
import importlib.util
import json
import os
import sys
import time
from concurrent.futures import ProcessPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus")


def load_module(path, max_tempos, production=False):
    os.environ.pop("AUDIO_MATCH_SHIFT_BUDGET_SECONDS", None)
    spec = importlib.util.spec_from_file_location("audio_match", path)
    am = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(am)
    if not production:
        am.SHIFT_BUDGET_SECONDS = float("inf")
    if max_tempos is not None:
        grid = am._tempo_grid
        am._tempo_grid = lambda: grid()[:max_tempos]
    return am


_am = None
_out_name = "py.json"


def _init(path, max_tempos, production):
    global _am, _out_name
    _am = load_module(path, max_tempos, production)
    _out_name = "py_prod.json" if production else "py.json"


def run_pair(folder, force):
    am = _am
    out = os.path.join(folder, _out_name)
    if os.path.exists(out) and not force:
        return folder, "cached"
    with open(os.path.join(folder, "meta.json"), encoding="utf-8") as fh:
        meta = json.load(fh)
    np = am._np()
    rec = {"algo": am.ALGO_VERSION}
    try:
        with open(os.path.join(folder, "clip.bin"), "rb") as fh:
            clip_bytes = fh.read()
        x_full = am.decode(clip_bytes, am.MAX_CLIP_TOTAL_SECONDS)
        x_full.astype("<f4").tofile(os.path.join(folder, "clip.f32"))
        x_video = am.decode(os.path.join(folder, meta["video_file"]), am.MAX_VIDEO_SECONDS)
        x_video.astype("<f4").tofile(os.path.join(folder, "video.f32"))
        if len(x_full) < am.FS * 2:
            raise am.MatchUnavailable("clip shorter than two seconds")
        clip = am.Clip(x_full[:int(am.MAX_CLIP_SECONDS * am.FS)], round(len(x_full) / am.FS, 2))
        t0 = time.perf_counter()
        result = am.compare_pcm(clip.pcm, x_video, clip.total_seconds)
        rec["ms"] = round((time.perf_counter() - t0) * 1000, 1)
        rec["result"] = result
    except am.MatchUnavailable as exc:
        rec["error"] = str(exc)
    del np
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(rec, fh)
    return folder, rec.get("result", {}).get("verdict") or rec.get("error")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--audio-match", default=os.environ.get("AUDIO_MATCH_PY"))
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--max-tempos", type=int, default=None)
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--production", action="store_true",
                    help="keep the site's 40 s shift budget; write py_prod.json")
    args = ap.parse_args()
    if not args.audio_match or not os.path.exists(args.audio_match):
        sys.exit("pass --audio-match or set AUDIO_MATCH_PY to the site's audio_match.py")
    folders = sorted((os.path.join(CORPUS, d) for d in os.listdir(CORPUS)
                      if os.path.exists(os.path.join(CORPUS, d, "meta.json"))),
                     key=lambda p: int(os.path.basename(p)))
    with ProcessPoolExecutor(args.jobs, initializer=_init,
                             initargs=(args.audio_match, args.max_tempos, args.production)) as pool:
        futures = [pool.submit(run_pair, f, args.force) for f in folders]
        for fut in futures:
            folder, what = fut.result()
            print(time.strftime("%H:%M:%S"), os.path.basename(folder), what, flush=True)


if __name__ == "__main__":
    main()
