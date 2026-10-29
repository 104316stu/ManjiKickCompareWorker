# ManjiKick compare worker

Runs audio compares for [audios.manjikick.com](https://audios.manjikick.com) on your own machine.
It leases a job, fetches the clip, downloads the video's audio with yt-dlp, compares the two and
sends back the result. yt-dlp, ffmpeg and deno are downloaded on first run (pinned versions,
checked by sha256).

## Download

Get the latest build from [Releases](../../releases):

- Windows: `mkworker-tray.exe` (tray app) and `mkworker.exe` (command line)
- Linux: `mkworker-linux-x86_64` or `mkworker-linux-aarch64` (ARM), static, no dependencies

Connect it to the site by approving a code (below); it updates itself from new releases
(`auto_update = false` in the config turns that off, `mkworker update` updates now).

## Windows

1. Run `mkworker-tray.exe`, click the tray icon, then **Connect to site** and approve the code on the site.
2. Pick a mode. Turn on **Start with Windows** if you want it always on.

Green = working, amber = paused or YouTube cooldown, grey = off, red = token or connection problem.

## Linux

```sh
chmod +x mkworker-linux-x86_64 && mv mkworker-linux-x86_64 ~/mkworker
~/mkworker pair                    # approve the code it shows on the site
~/mkworker run --daemon --mode max --threads 2   # log: ~/.local/state/mkworker/log
~/mkworker status
~/mkworker stop
```

Start on boot without root: `crontab -e`, add

```
@reboot $HOME/mkworker run --daemon --mode max --threads 2
```

Check that downloads work here: `~/mkworker test-download dQw4w9WgXcQ`.

## HidenCloud

For panels that only start a Python file: upload the Linux binary for its CPU with this `FUF.py`
next to it. It registers itself on first start.

```python
import os, platform, subprocess, sys, time
here = os.path.dirname(os.path.abspath(__file__))
arch = "aarch64" if platform.machine() in ("aarch64", "arm64") else "x86_64"
exe = os.path.join(here, f"mkworker-linux-{arch}")
os.environ.setdefault("MKW_HOME", os.path.join(here, "mkworker-data"))
os.chmod(exe, 0o755)
while True:
    code = subprocess.call([exe, "run", "--mode", "max", "--threads", "1", "--duty", "0.2", "--foreground-status"])
    if code == 3:
        sys.exit("worker token rejected: delete mkworker-data/token to register again")
    time.sleep(60)
```

## Modes

| Mode | Threads | Notes |
| --- | --- | --- |
| Light | 1 | lowest priority, pauses for games, battery and a busy PC |
| Balanced | half the cores | lowest priority, pauses for games and battery |
| Max | all cores but one | no automatic pauses |
| Auto (default) | Max when you've been away 5 min, else Light | pauses for games |

Tray: pick the mode in the menu. Command line: `--mode light|balanced|max|auto`
(on Linux, Auto follows the load average), `--threads N` to fix the count,
`--duty 0.2` to use at most a fifth of the CPU time.

Settings live in `%LOCALAPPDATA%\ManjiKickWorker\config` (Windows) or `~/.config/mkworker/config`
(Linux). Set `ytdlp`, `ffmpeg` or `deno` to `system` or a path to use your own copies.
