"""Standalone Linux/Python 3 controller, sent over SSH stdin; stdlib only.

No account key, CLI configuration, or API response body is read or logged.
State is ~/.local/state/gpu-expire/deadline.json. Detached workers survive
SSH disconnects, not instance shutdown/reboot. All mutations and the destroy
request share a lock, so replacing/cancelling cannot race an old worker.
"""
import contextlib
import fcntl
import json
import os
import select
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


class ExpiryError(Exception):
    pass


def credentials(instance_id):
    # SSH non-login shells don't always inherit Vast's injected environment.
    values = {}
    try:
        for entry in Path("/proc/1/environ").read_bytes().split(b"\0"):
            key, _, value = entry.decode(errors="replace").partition("=")
            if key in {"CONTAINER_ID", "CONTAINER_API_KEY"}:
                values[key] = value
    except OSError:
        pass
    try:
        for line in Path("/etc/environment").read_text().splitlines():
            parts = shlex.split(line, comments=True)
            if parts and parts[0] == "export":
                parts = parts[1:]
            if len(parts) == 1:
                key, _, value = parts[0].partition("=")
                if key in {"CONTAINER_ID", "CONTAINER_API_KEY"}:
                    values[key] = value
    except (OSError, ValueError):
        pass
    values.update({key: os.environ[key] for key in ("CONTAINER_ID", "CONTAINER_API_KEY")
                   if os.environ.get(key)})
    key = values.get("CONTAINER_API_KEY", "")
    if values.get("CONTAINER_ID") != instance_id or not key or any(c.isspace() for c in key):
        raise ExpiryError("credentials")
    return key


@contextlib.contextmanager
def locked(root):
    with (root / "lock").open("a") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def load_state(root):
    path = root / "deadline.json"
    return json.loads(path.read_text()) if path.exists() else None


def save_state(root, state):
    path = root / "deadline.tmp"
    path.write_text(json.dumps(state))
    os.chmod(path, 0o600)
    path.replace(root / "deadline.json")


def running(state):
    # Verify worker identity, not just kill(pid, 0): PIDs can be recycled.
    try:
        parts = Path("/proc/{}/cmdline".format(state["pid"])).read_bytes().split(b"\0")
        return b"worker" in parts and state["token"].encode() in parts
    except (OSError, KeyError):
        return False


def public_state(state):
    if not state:
        return {"status": "none"}
    status = state["status"]
    if status != "destroy-request-accepted" and not running(state):
        status = "inactive"
    return {"status": status, "deadline": state["deadline"]}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # never forward an Authorization header to another host


def destroy(instance_id, key):
    request = urllib.request.Request(
        f"https://console.vast.ai/api/v0/instances/{instance_id}/",
        data=b"{}", method="DELETE",
        headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
    )
    # No proxies from the shell environment; no response/error body is logged.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=15) as response:
        result = json.loads(response.read(65536))
        return isinstance(result, dict) and result.get("success") is True


def tick(root, token, instance_id, key, now=None):
    """One locked worker iteration. Return None to exit, seconds until retry otherwise."""
    with locked(root):
        state = load_state(root)
        if not state or state["token"] != token:
            return None
        remaining = state["deadline"] - (time.time() if now is None else now)
        if remaining > 0:
            return min(remaining, 1)
        try:
            accepted = destroy(instance_id, key)
        except (OSError, ValueError, urllib.error.HTTPError):
            accepted = False
        state["status"] = "destroy-request-accepted" if accepted else "retrying"
        save_state(root, state)
        return None if accepted else 30


def worker(root, token, instance_id, ready_fd):
    key = credentials(instance_id)
    os.write(ready_fd, b"ready")
    os.close(ready_fd)
    while True:
        delay = tick(root, token, instance_id, key)
        if delay is None:
            return
        time.sleep(delay)


def start_worker(root, token, instance_id, key):
    reader, writer = os.pipe()
    process = None
    try:
        # Give the detached worker only the scoped credentials, never ambient keys.
        process = subprocess.Popen(
            [sys.executable, str(root / "runner.py"), "worker", str(root), token, instance_id, str(writer)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, pass_fds=(writer,),
            env={"HOME": str(Path.home()), "PATH": "/usr/local/bin:/usr/bin:/bin",
                 "CONTAINER_ID": instance_id, "CONTAINER_API_KEY": key},
        )
        os.close(writer)
        writer = None
        ready, _, _ = select.select([reader], [], [], 5)
        if not ready or os.read(reader, 5) != b"ready" or process.poll() is not None:
            raise ExpiryError("verification")
        state = {"pid": process.pid, "token": token}
        if not running(state):
            raise ExpiryError("verification")
        return process
    except BaseException:
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        raise
    finally:
        os.close(reader)
        if writer is not None:
            os.close(writer)


def control(root, action, instance_id, deadline=None, source=None):
    os.umask(0o077)
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    with locked(root):
        state = load_state(root)
        if action == "show":
            return public_state(state)
        if action == "cancel":
            (root / "deadline.json").unlink(missing_ok=True)
            return {"status": "cancelled" if state else "none"}
        key = credentials(instance_id)
        if deadline <= time.time():
            raise ExpiryError("past")
        # Atomic replacement means an old worker never reads a half-written script.
        script = root / "runner.tmp"
        script.write_text(source)
        script.replace(root / "runner.py")
        token = uuid.uuid4().hex
        process = start_worker(root, token, instance_id, key)
        previous = state
        try:
            if deadline <= time.time():
                raise ExpiryError("past")
            state = {"deadline": deadline, "token": token, "pid": process.pid, "status": "armed"}
            save_state(root, state)
            if not running(state):
                if previous:
                    save_state(root, previous)
                else:
                    (root / "deadline.json").unlink()
                raise ExpiryError("verification")
        except BaseException:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            raise
        return {"status": "armed", "deadline": deadline}


def main():
    try:
        if sys.argv[1] == "worker":
            worker(Path(sys.argv[2]), sys.argv[3], sys.argv[4], int(sys.argv[5]))
            return 0
        # When executing 'python3 -', its own source is already consumed. The
        # transport appends a repr of the standalone source as SOURCE below.
        action, instance_id = sys.argv[1:3]
        if action not in {"arm", "show", "cancel"} or not instance_id.isascii() or not instance_id.isdigit():
            raise ExpiryError("remote")
        deadline = int(sys.argv[3]) if action == "arm" else None
        result = control(Path.home() / ".local/state/gpu-expire", action, instance_id,
                         deadline, globals().get("SOURCE"))
        print(json.dumps(result))
        return 0
    except (ExpiryError, OSError, ValueError, TypeError, KeyError, IndexError, subprocess.SubprocessError) as exc:
        code = str(exc) if isinstance(exc, ExpiryError) else "remote"
        print(json.dumps({"error": code}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
