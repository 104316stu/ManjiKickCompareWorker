# ManjiKick compare worker

Runs audio compares for [audios.manjikick.com](https://audios.manjikick.com) on your own machine.
It leases a job, fetches the clip, downloads the video's audio with yt-dlp, compares the two and
sends back the result. yt-dlp, ffmpeg and deno are downloaded on first run (pinned versions,
checked by sha256).

## Download

Get the latest build from [Releases](../../releases):

- Windows: `mkworker-tray.exe` (tray app) and `mkworker.exe` (command line)
- Linux: `mkworker-linux-x86_64` (static, no dependencies)

You need a worker token (`mkw_...`) from the site.

## Windows

1. Copy the token.
2. Run `mkworker-tray.exe`, right-click the tray icon, **Set token from clipboard**.
3. Pick a mode. Tick **Start with Windows** if you want it always on.

Green = working, amber = paused or YouTube cooldown, grey = off, red = token or connection problem.

## Linux

```sh
chmod +x mkworker-linux-x86_64 && mv mkworker-linux-x86_64 ~/mkworker
echo 'mkw_...' | ~/mkworker token set
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

For panels that only start a Python file: upload `mkworker-linux-x86_64` with this `FUF.py`
next to it and put your token in `mkworker-data/token`.

```python
import os, subprocess, sys, time
here = os.path.dirname(os.path.abspath(__file__))
exe = os.path.join(here, "mkworker-linux-x86_64")
os.environ.setdefault("MKW_HOME", os.path.join(here, "mkworker-data"))
os.chmod(exe, 0o755)
while True:
    code = subprocess.call([exe, "run", "--mode", "max", "--threads", "1", "--duty", "0.2", "--foreground-status"])
    if code == 3:
        sys.exit("worker token rejected: put a new one in mkworker-data/token")
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
