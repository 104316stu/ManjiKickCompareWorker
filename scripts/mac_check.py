"""Runs the real app on a Mac against a stand-in for the site: install, connect,
panel, crash, quit, reopen, self-update, uninstall. For CI.

    mac_check.py <folder with mkworker-macos + SHA256SUMS> <folder with the 9.9.9 build>
"""
import hashlib
import http.server
import json
import os
import pathlib
import subprocess
import sys
import threading
import time

PORT = 8765
HOME = pathlib.Path.home()
DATA = HOME / "Library/Application Support/ManjiKickWorker"
LOG = HOME / "Library/Logs/ManjiKickWorker/mkworker.log"
APP = HOME / "Applications/ManjiKick Worker.app"
EXE = APP / "Contents/MacOS/mkworker"
AGENT = HOME / "Library/LaunchAgents/com.manjikick.worker.plist"
LABEL = f"gui/{os.getuid()}/com.manjikick.worker"
SHOTS = pathlib.Path("mac-shots")

dist, newer = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
seen = []
offer_update = threading.Event()


class Site(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, body, code=200, kind="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        seen.append(self.path)
        if self.path == "/latest":
            if not offer_update.is_set():
                return self.reply({}, 404)
            url = f"http://127.0.0.1:{PORT}/new/"
            return self.reply({"tag_name": "v9.9.9", "assets": [
                {"name": "mkworker-macos", "browser_download_url": url + "mkworker-macos"},
                {"name": "SHA256SUMS", "browser_download_url": url + "SHA256SUMS"}]})
        if self.path.startswith("/new/"):
            return self.reply((newer / self.path[5:]).read_bytes(), kind="application/octet-stream")
        self.reply({"ok": True})

    def do_POST(self):
        seen.append(self.path)
        size = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(size) or b"{}")
        if self.path.endswith("/register"):
            return self.reply({"ok": True, "token": "mkw_" + "a" * 40, "name": body.get("name"), "trust": "shadow"})
        if self.path.endswith("/hello"):
            return self.reply({"ok": True, "name": "ci-mac", "trust": "shadow", "algo": body.get("algo", ""),
                               "limits": {}, "lease_seconds": 300, "cookie_version": None})
        if self.path.endswith("/lease"):
            return self.reply({"ok": True, "jobs": [], "poll_after_seconds": 2, "cookie_version": None})
        self.reply({"ok": True})


def wait(what, check, seconds=60):
    until = time.time() + seconds
    while time.time() < until:
        got = check()
        if got:
            print(f"ok: {what}", flush=True)
            return got
        time.sleep(1)
    print(f"FAILED: {what}", flush=True)
    dump()
    sys.exit(1)


def dump():
    print("--- log ---")
    print(LOG.read_text(errors="replace")[-6000:] if LOG.exists() else "(no log)")
    print("--- launchctl ---")
    print(subprocess.run(["launchctl", "print", LABEL], capture_output=True, text=True).stdout[:3000])
    print("--- requests ---", seen[-20:])


def pid():
    """The app's pid according to launchd."""
    out = subprocess.run(["launchctl", "print", LABEL], capture_output=True, text=True).stdout
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("pid = "):
            return int(line[6:])
    return None


def status():
    try:
        return json.loads((DATA / "status.json").read_text())
    except (OSError, ValueError):
        return {}


def shot(name):
    SHOTS.mkdir(exist_ok=True)
    subprocess.run(["screencapture", "-x", str(SHOTS / name)], check=False)


def main():
    threading.Thread(target=http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Site).serve_forever,
                     daemon=True).start()
    DATA.mkdir(parents=True, exist_ok=True)
    (DATA / "config").write_text(f"server = http://127.0.0.1:{PORT}\nmode = max\n")
    subprocess.run(["launchctl", "setenv", "MKW_UPDATE_URL", f"http://127.0.0.1:{PORT}/latest"], check=True)

    subprocess.run(["sh", "scripts/install-mac.sh"], check=True,
                   env={**os.environ, "MKW_BASE": "file://" + str(dist.resolve())})
    wait("the app is installed", EXE.exists)
    wait("launchd runs it", pid)
    wait("it registered", lambda: any(p.endswith("/register") for p in seen))
    wait("it says hello", lambda: any(p.endswith("/hello") for p in seen))
    wait("the token is saved", (DATA / "token").exists)
    wait("it leases", lambda: any(p.endswith("/lease") for p in seen), 300)

    (DATA / "open-panel").write_text("")
    wait("the panel request is taken", lambda: not (DATA / "open-panel").exists())
    time.sleep(3)
    shot("panel.png")

    first = pid()
    (DATA / "crash-test").write_text("")
    wait("launchd restarts it after a crash", lambda: pid() not in (None, first), 120)
    wait("the crash is in the log", lambda: "crash test requested" in LOG.read_text(errors="replace"))

    subprocess.run([str(EXE), "stop"], check=False)
    wait("stop ends it", lambda: pid() is None)
    time.sleep(70)
    if pid() is not None:
        print("FAILED: a clean stop was restarted")
        dump()
        sys.exit(1)
    print("ok: a clean stop stays stopped", flush=True)

    subprocess.run(["open", str(APP)], check=True)
    wait("opening the app starts it under launchd", pid)

    offer_update.set()
    subprocess.run(["launchctl", "kickstart", "-k", LABEL], check=True)
    wait("it updates itself to 9.9.9", lambda: "9.9.9" in str(status().get("version")), 180)
    plist = APP / "Contents/Info.plist"
    wait("the bundle says 9.9.9", lambda: "9.9.9" in plist.read_text())
    wait("still run by launchd after the update", pid)
    wait("still working after the update", lambda: status().get("at", 0) > time.time() - 30)
    shot("updated.png")

    subprocess.run([str(EXE), "uninstall", "--everything"], check=True)
    wait("the app is removed", lambda: not APP.exists())
    wait("the agent is gone", lambda: not AGENT.exists())
    wait("nothing is left running", lambda: pid() is None)
    wait("the data is removed", lambda: not DATA.exists())
    print("all good")


if __name__ == "__main__":
    main()
