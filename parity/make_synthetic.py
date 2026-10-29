"""Synthetic pairs for the paths real pairs rarely reach.

    python -I parity/make_synthetic.py 3 7 12 ...   (corpus pairs to derive from)

The shift search (tempo and pitch moved apart) and the speed sweep need a
clip that is the video's audio with its tempo and/or pitch changed. For each
given pair this writes corpus/<1000+k>/ with the same video and a clip made
by ffmpeg: tempo only (atempo), pitch only (asetrate + atempo back), both
apart, and a plain resample (speed and pitch together).
"""

import json
import os
import shutil
import subprocess
import sys

import imageio_ffmpeg

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus")
SR = 44100

VARIANTS = [
    ("tempo 1.12", "atempo=1.12"),
    ("pitch +1.5 st", f"asetrate={SR}*{2 ** (1.5 / 12):.6f},aresample={SR},atempo={1 / 2 ** (1.5 / 12):.6f}"),
    ("tempo 0.9, pitch -2 st", f"asetrate={SR}*{2 ** (-2 / 12):.6f},aresample={SR},atempo={0.9 / 2 ** (-2 / 12):.6f}"),
    ("resampled 1.2x", f"asetrate={SR}*1.2,aresample={SR}"),
]


def main():
    ffmpeg = imageio_ffmpeg.get_ffmpeg_exe()
    k = 1000
    for src in sys.argv[1:]:
        base = os.path.join(CORPUS, src)
        with open(os.path.join(base, "meta.json"), encoding="utf-8") as fh:
            meta = json.load(fh)
        for label, filt in VARIANTS:
            out = os.path.join(CORPUS, str(k))
            shutil.rmtree(out, ignore_errors=True)
            os.makedirs(out)
            shutil.copy(os.path.join(base, meta["video_file"]), os.path.join(out, meta["video_file"]))
            subprocess.run([ffmpeg, "-v", "error", "-nostdin", "-i", os.path.join(base, "clip.bin"),
                            "-vn", "-ac", "1", "-af", f"aresample={SR},{filt}", "-f", "flac",
                            os.path.join(out, "clip.bin")], check=True,
                           creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
            with open(os.path.join(out, "meta.json"), "w", encoding="utf-8") as fh:
                json.dump({"synthetic": label, "from": int(src), "video_file": meta["video_file"]}, fh)
            print(k, "from", src, label, flush=True)
            k += 1


if __name__ == "__main__":
    main()
