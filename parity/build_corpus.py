"""Build the parity corpus from real pairs, acting as a SHADOW worker.

Leases shadow jobs from the live worker API, fetches each job's Roblox clip,
releases every lease at once (no result is ever posted), then downloads each
pair's YouTube audio with yt_dlp, one at a time.

    MKW_TOKEN_FILE=... MKW_API=... python -I parity/build_corpus.py [target_pairs]

Writes parity/corpus/<n>/{clip.bin, video.<ext>, meta.json}. Resumable: it
counts what is already there. If the API ever hands out a non-shadow ("live")
job, every leased job is released at once and the script stops.
"""

import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "corpus")
# MKW_TOKEN_FILE: a file holding a worker token in shadow trust.
# MKW_API: the worker API base URL (https://<site>/api/worker/v1).
TOKEN_FILE = os.environ.get("MKW_TOKEN_FILE", "")
API = os.environ.get("MKW_API", "").rstrip("/")
UA = "ManjiKickWorker/parity"
YT_FORMAT = "bestaudio[ext=webm][protocol=https]/bestaudio[ext=m4a][protocol=https]/bestaudio"
MAX_VIDEO_SECONDS = 1800


def _token():
    if not TOKEN_FILE or not API:
        sys.exit("set MKW_TOKEN_FILE and MKW_API")
    with open(TOKEN_FILE, encoding="utf-8") as fh:
        return fh.read().strip()


TOKEN = _token()


def call(method, path, body=None, headers=None, raw=False, timeout=60):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(API + path, data=data, method=method, headers={
        "Authorization": f"Bearer {TOKEN}", "User-Agent": UA,
        **({"Content-Type": "application/json"} if data is not None else {}), **(headers or {})})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = resp.read()
    return payload if raw else json.loads(payload or b"{}")


def log(msg):
    print(time.strftime("%H:%M:%S"), msg, flush=True)


def release(jobs):
    if not jobs:
        return
    try:
        call("POST", "/release", {"jobs": [{"job_id": j["job_id"], "lease": j["lease"]} for j in jobs]})
    except Exception as exc:  # noqa: BLE001
        log(f"release failed: {exc}")


def download(video_id, folder):
    import yt_dlp

    class Quiet:
        def __init__(self):
            self.errors = []

        def debug(self, msg):
            pass

        def warning(self, msg):
            pass

        def error(self, msg):
            self.errors.append(str(msg))

    quiet = Quiet()
    opts = {"format": YT_FORMAT, "outtmpl": os.path.join(folder, "video.%(ext)s"), "noplaylist": True,
            "quiet": True, "no_warnings": True, "logger": quiet, "nopart": True, "retries": 2,
            "socket_timeout": 20, "max_filesize": 80 * 1024 * 1024,
            "match_filter": lambda info, *a, **k: (
                "live" if info.get("is_live") else
                "too long" if (info.get("duration") or 0) > MAX_VIDEO_SECONDS else None)}
    try:
        with yt_dlp.YoutubeDL(opts) as ydl:
            info = ydl.extract_info(f"https://www.youtube.com/watch?v={video_id}", download=True)
    except Exception as exc:  # noqa: BLE001
        raise RuntimeError((quiet.errors[-1] if quiet.errors else str(exc))[:200]) from exc
    if not info:
        raise RuntimeError("no video information")
    path = info.get("requested_downloads", [{}])[0].get("filepath") or ydl.prepare_filename(info)
    if not os.path.exists(path):
        raise RuntimeError("video skipped (live or too long)")
    return path


def existing():
    os.makedirs(CORPUS, exist_ok=True)
    done, seen = [], set()
    for name in os.listdir(CORPUS):
        meta = os.path.join(CORPUS, name, "meta.json")
        if os.path.exists(meta):
            with open(meta, encoding="utf-8") as fh:
                m = json.load(fh)
            done.append(int(name))
            seen.add((m["audio_id"], m["video_id"]))
    return done, seen


def main():
    target = int(sys.argv[1]) if len(sys.argv) > 1 else 150
    hello = call("POST", "/hello", {"version": "parity", "platform": sys.platform, "threads": 1})
    log(f"hello: trust={hello.get('trust')} algo={hello.get('algo')}")
    if hello.get("trust") != "shadow":
        log("worker is not in shadow trust: not leasing anything")
        return 2
    done, seen = existing()
    n = max(done) + 1 if done else 0
    failures = 0
    while len(done) < target:
        try:
            jobs = call("POST", "/lease", {"want": 8}).get("jobs") or []
        except urllib.error.HTTPError as exc:
            if exc.code != 429:
                raise
            log("rate limited; waiting a minute")
            time.sleep(65)
            continue
        if any(j.get("kind") != "shadow" for j in jobs):
            release(jobs)
            log("got LIVE jobs: released them all and stopped")
            return 3
        if not jobs:
            log("no shadow jobs offered; stopping")
            break
        clips = []
        for j in jobs:
            if (j["audio_id"], j["video_id"]) in seen:
                continue
            try:
                clip = call("GET", f"/clip/{j['job_id']}", headers={"X-Lease": j["lease"]}, raw=True, timeout=90)
            except urllib.error.HTTPError as exc:
                log(f"clip {j['audio_id']}: HTTP {exc.code}")
                continue
            except Exception as exc:  # noqa: BLE001
                log(f"clip {j['audio_id']}: {exc}")
                continue
            clips.append((j, clip))
        release(jobs)
        for j, clip in clips:
            if len(done) >= target:
                break
            folder = os.path.join(CORPUS, str(n))
            shutil.rmtree(folder, ignore_errors=True)
            os.makedirs(folder)
            try:
                path = download(j["video_id"], folder)
            except Exception as exc:  # noqa: BLE001
                log(f"video {j['video_id']}: {exc}")
                shutil.rmtree(folder, ignore_errors=True)
                failures += 1
                if failures > 40:
                    log("too many download failures; stopping")
                    return 1
                time.sleep(3)
                continue
            with open(os.path.join(folder, "clip.bin"), "wb") as fh:
                fh.write(clip)
            meta = {"audio_id": j["audio_id"], "video_id": j["video_id"], "job_id": j["job_id"],
                    "video_file": os.path.basename(path), "clip_bytes": len(clip),
                    "video_seconds": j.get("video_seconds")}
            with open(os.path.join(folder, "meta.json"), "w", encoding="utf-8") as fh:
                json.dump(meta, fh)
            seen.add((j["audio_id"], j["video_id"]))
            done.append(n)
            log(f"#{n}: {j['audio_id']} / {j['video_id']} ({len(done)}/{target})")
            n += 1
            time.sleep(2)
    log(f"corpus has {len(done)} pairs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
