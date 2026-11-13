"""Offline contract tests: the real helper with a synthetic modal package."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import unittest

STUB = r'''
import os
from types import SimpleNamespace
from pathlib import Path
__version__ = "1.6.3"
class AuthError(Exception): pass
class ResourceExhaustedError(Exception): pass
class ImageBuildError(Exception): pass
class InvalidError(Exception): pass
class NotFoundError(Exception): pass
class Billing:
    def rates(self):
        if os.getenv("FAIL_KIND"):
            raise globals()[os.environ["FAIL_KIND"]]("token_secret=private-canary")
        return {"gpu_hour_cost_t4": "0.59"}
class Workspace:
    @staticmethod
    def from_context(): return SimpleNamespace(name="offline", billing=Billing())
class Image:
    def __init__(self): self.has_uv = False
    @staticmethod
    def from_registry(name): return Image()
    @staticmethod
    def debian_slim(**kw): return Image()
    def pip_install(self, *args):
        raise AssertionError("bootstrap must work without Python/pip")
    def dockerfile_commands(self, *commands):
        # Consume the emitted image recipe, not helper source. Model the COPY
        # of pinned, standalone executables without requiring a Python layer.
        import shlex
        assert len(commands) == 1
        instruction, source, *paths = shlex.split(commands[0])
        assert instruction.upper() == "COPY"
        assert source == "--from=ghcr.io/astral-sh/uv:0.10.4"
        assert set(paths[:-1]) == {"/uv", "/uvx"}
        assert paths[-1].rstrip("/") == "/usr/local/bin"
        self.has_uv = True
        return self
class Call:
    def __init__(self, fn): self.fn = fn
    def __call__(self, *a, **kw): return self.fn(*a, **kw)
    async def aio(self, *a, **kw): return self.fn(*a, **kw)
class Volume:
    def __init__(self, name, create=False, version=None):
        self.name, self.create, self.version = name, create, version
        self.hydrate = Call(self.load)
    def load(self):
        path = Path(os.environ['STATE']) / self.name
        if self.create: path.touch()
        if not path.exists(): raise NotFoundError('private-canary')
        return self
    @staticmethod
    def from_name(name, create_if_missing=False, version=None): return Volume(name, create_if_missing, version)
    @staticmethod
    def delete(name, allow_missing=False):
        if not os.getenv('STUCK_VOLUME'):
            (Path(os.environ['STATE']) / name).unlink(missing_ok=allow_missing)
    def iterdir(self, path, recursive):
        self.load()
        return [SimpleNamespace(path=os.getenv('VOLUME_PATH', 'checkpoint.bin'), type=1)]
    def read_file(self, path): yield b'checkpoint\x00\xff'
Volume.objects = SimpleNamespace(delete=Volume.delete, list=lambda created_before: [
    Volume(p.name) for p in Path(os.environ['STATE']).iterdir() if p.stat().st_mtime < created_before.timestamp()])
exception = SimpleNamespace(NotFoundError=NotFoundError)
class App:
    pass
class AsyncCall:
    def __init__(self, value): self.value = value
    async def aio(self, *args, **kw): return self.value
class Reader:
    def __init__(self, data): self.data = data
    def __aiter__(self): return self
    async def __anext__(self):
        if os.getenv('SLOW_EXEC'):
            import asyncio
            await asyncio.sleep(120)
        if self.data is None: raise StopAsyncIteration
        data, self.data = self.data, None
        return data
class Writer:
    def __init__(self): self.drain = AsyncCall(None)
    def write(self, data): assert isinstance(data, bytes)
    def write_eof(self): pass
class Process:
    def __init__(self):
        self.stdin = Writer()
        self.stdout = Reader(b"\x00\xff\r\nraw")
        self.stderr = Reader(b"stderr\r\n")
        self.wait = AsyncCall(7)
class ExecCall:
    async def aio(self, *command, **kw):
        assert kw['text'] is False
        if os.getenv('PTY_EXPECTED'):
            assert kw['pty'] is True
        if not os.getenv('EXEC_LOCAL'): return Process()
        import asyncio
        if os.getenv('REPLY_ARCHIVE'): command = ('cat', os.environ['REPLY_ARCHIVE'])
        proc = await asyncio.create_subprocess_exec(*command, stdin=asyncio.subprocess.PIPE,
                                                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        class LocalWriter:
            def __init__(self): self.drain = SimpleNamespace(aio=proc.stdin.drain)
            def write(self, data): proc.stdin.write(data)
            def write_eof(self): proc.stdin.write_eof()
        return SimpleNamespace(stdin=LocalWriter(), stdout=proc.stdout, stderr=proc.stderr,
                               wait=SimpleNamespace(aio=proc.wait))
class Sandbox:
    object_id = "sb-test"
    def __init__(self): self.exec = ExecCall()
    @staticmethod
    async def allocate(*args, **kw):
        assert kw["image"].has_uv
        assert kw["cpu"] == (4, 4)
        assert kw["memory"] == (16384, 16384)
        assert kw["timeout"] == 1800
        assert "secrets" not in kw and "env" not in kw
        assert kw["tags"]["gpu-cli"] == "true"
        if kw['volumes']:
            volume = kw['volumes']['/gpu-outputs']
            assert volume.load() is volume
            assert volume.version == 2
            assert kw['tags']['volume'] == volume.name
        if os.getenv("SLOW_CREATE"):
            import asyncio
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                print("pending-terminated", file=__import__("sys").stderr)
                raise
        if os.getenv("FAIL_KIND"):
            raise globals()[os.environ["FAIL_KIND"]]("private-canary")
        return Sandbox()
    @staticmethod
    def from_id(id):
        if id == "missing": raise NotFoundError("private-canary")
        return Sandbox()
    @staticmethod
    def list(**kw):
        assert kw["tags"]["gpu-cli"] == "true"
        return [Sandbox()]
    def poll(self): return 137 if os.getenv("FINISHED") else None
    def terminate(self, **kw):
        assert kw["wait"] is True
        os.environ["FINISHED"] = "1"
    def get_tags(self): return {"gpu-cli": "true", "run_id": "test", **({'volume': os.environ['RUN_VOLUME']} if os.getenv('RUN_VOLUME') else {})}
Sandbox.create = SimpleNamespace(aio=Sandbox.allocate)
App.lookup = AsyncCall(object())
'''


class HelperTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        package = Path(self.temp.name) / "modal"
        package.mkdir()
        # PEP 723 metadata is the uv dependency contract: consume it as TOML
        # and make the stub report that version, so runtime checks cannot drift.
        source = Path(os.environ["HELPER_PATH"]).read_text()
        metadata = source.split("# /// script\n", 1)[1].split("# ///", 1)[0]
        dependencies = tomllib.loads("\n".join(line.removeprefix("# ") for line in metadata.splitlines()))["dependencies"]
        self.version = dependencies[0].removeprefix("modal==")
        (package / "__init__.py").write_text(STUB.replace('__version__ = "1.6.3"', f'__version__ = {self.version!r}'))
        (package / "config.py").write_text('import os\nclass Config:\n def get(self, key): return None if os.getenv("NO_TOKEN") else {"token_id":"test-id", "token_secret":"private-canary"}.get(key)\nconfig=Config()\n')
        self.state = Path(self.temp.name) / 'state'
        self.state.mkdir()
        self.env = {"PATH": os.environ.get("PATH", ""), "PYTHONPATH": self.temp.name,
                    "PYTHONDONTWRITEBYTECODE": "1", "STATE": str(self.state)}

    def tearDown(self):
        self.temp.cleanup()

    def call(self, *args, data=b"", **env):
        import sys
        return subprocess.run([sys.executable, "-B", os.environ["HELPER_PATH"], *args],
                              input=data, capture_output=True, env={**self.env, **env}, timeout=10)

    def test_check_and_rates(self):
        self.assertEqual(json.loads(self.call("check").stdout),
                         {"workspace": "offline", "environment": "main", "client_version": self.version})
        self.assertEqual(json.loads(self.call("rates").stdout)["gpu_hour_cost_t4"], "0.59")

    def test_errors_are_typed_and_never_echo_secrets(self):
        for kind in ["AuthError", "ResourceExhaustedError", "ImageBuildError", "InvalidError"]:
            out = self.call("rates", FAIL_KIND=kind)
            self.assertEqual(out.returncode, 1)
            self.assertEqual(json.loads(out.stdout)["error"]["kind"], kind)
            self.assertNotIn(b"private-canary", out.stdout + out.stderr)
        self.assertEqual(json.loads(self.call("check", NO_TOKEN="1").stdout)["error"]["kind"], "MissingToken")

    def test_create_exact_limits_and_no_secrets(self):
        spec = {"gpu": "T4", "image": None, "cpu": 4, "memory": 16384,
                "timeout": 1800, "tags": {"run_id": "test"}, "command": ["true"], "env_names": ["TOKEN"]}
        out = self.call("create", data=json.dumps(spec).encode())
        self.assertEqual(out.returncode, 0, out.stdout)
        self.assertEqual(json.loads(out.stdout)["id"], "sb-test")
        out = self.call("create", "0.01", data=json.dumps(spec).encode(), SLOW_CREATE="1")
        self.assertEqual(json.loads(out.stdout)["error"]["kind"], "SchedulingTimeout")
        self.assertIn(b"pending-terminated", out.stderr)
        self.assertNotIn(b"private-canary", out.stdout + out.stderr)
        spec["env"] = {"TOKEN": "private-canary"}
        out = self.call("create", data=json.dumps(spec).encode())
        self.assertEqual(out.returncode, 1)
        self.assertNotIn(b"private-canary", out.stdout + out.stderr)

    def test_pythonless_registry_bootstrap_and_create_diagnostics(self):
        spec = {"gpu": "T4", "image": "nvidia/cuda:12.8.1-devel-ubuntu24.04",
                "cpu": 4, "memory": 16384, "timeout": 1800,
                "tags": {"run_id": "test"}, "command": ["true"]}
        data = json.dumps(spec).encode()
        out = self.call("create", data=data)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(json.loads(out.stdout)["id"], "sb-test")
        for kind, hint in [("InvalidError", "account/billing"),
                           ("ImageBuildError", "build logs in the Modal dashboard")]:
            out = self.call("create", data=data, FAIL_KIND=kind)
            self.assertEqual(out.returncode, 1)
            error = json.loads(out.stdout)["error"]
            self.assertEqual(error["kind"], kind)
            self.assertIn(hint, error["message"])
            self.assertNotIn(b"private-canary", out.stdout + out.stderr)
            self.assertNotIn("helper failed", error["message"])
            self.assertLess(len(error["message"]), 256)

    def test_byte_exact_exec_and_remote_exit(self):
        out = self.call("exec", "sb-test", "--", "cat", data=b"\x00\xff\r\n")
        self.assertEqual(out.returncode, 7, out.stderr)
        self.assertEqual(out.stdout, b"\x00\xff\r\nraw")
        self.assertEqual(out.stderr, b"stderr\r\n")
        out = self.call("exec", "missing", "--", "true")
        self.assertEqual(out.returncode, 255)
        self.assertEqual(out.stdout, b"")
        self.assertEqual(json.loads(out.stderr)["error"]["kind"], "NotFoundError")

    def test_volume_create_download_delete_and_verification(self):
        import io
        import tarfile
        name = 'gpu-cli-run-test'
        spec = {"gpu": "T4", "cpu": 4, "memory": 16384, "timeout": 1800,
                "tags": {"run_id": "test", "volume": name}, "command": ["true"], "volume": name}
        out = self.call('create', data=json.dumps(spec).encode())
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertTrue((self.state / name).exists())
        (self.state / 'unrelated').touch()
        self.assertEqual(json.loads(self.call('list').stdout)['volumes'], [])
        old = time.time() - 26 * 3600
        for path in self.state.iterdir():
            os.utime(path, (old, old))
        rows = json.loads(self.call('list').stdout)['volumes']
        self.assertEqual(rows, [{'name': name}])
        self.assertEqual(json.loads(self.call('list', RUN_VOLUME=name).stdout)['volumes'], [])
        out = self.call('volume-get', name, 'results/checkpoints')
        self.assertEqual(out.returncode, 0, out.stderr)
        with tarfile.open(fileobj=io.BytesIO(out.stdout)) as archive:
            self.assertEqual(archive.extractfile('results/checkpoints/checkpoint.bin').read(), b'checkpoint\x00\xff')
        out = self.call('volume-get', name, 'outputs', VOLUME_PATH='../escape')
        self.assertEqual(out.returncode, 255)
        self.assertEqual(json.loads(out.stderr)['error']['kind'], 'ValueError')
        out = self.call('volume-delete', name, STUCK_VOLUME='1')
        self.assertEqual(json.loads(out.stdout)['error']['kind'], 'CleanupUnverified')
        out = self.call('terminate', 'sb-test', RUN_VOLUME=name)
        self.assertTrue(json.loads(out.stdout)['finished'])
        self.assertFalse((self.state / name).exists())
        self.assertTrue(json.loads(self.call('volume-delete', name).stdout)['finished'])
        self.assertTrue((self.state / 'unrelated').exists())

    def test_exec_cat_pty_and_real_file_copy(self):
        data = b'hello\x00\xff\r\n'
        out = self.call('exec', 'sb-test', '--', 'cat', data=data, EXEC_LOCAL='1')
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout, data)
        out = self.call('exec', 'sb-test', '--pty', '--', 'true', PTY_EXPECTED='1')
        self.assertEqual(out.returncode, 7)
        local = Path(self.temp.name) / 'input file'
        remote = Path(self.temp.name) / 'remote file'
        local.write_bytes(data)
        out = self.call('exec', 'sb-test', '--copy', 'file', str(local), ':' + str(remote), EXEC_LOCAL='1')
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(remote.read_bytes(), data)
        target = Path(self.temp.name) / 'recovered file'
        out = self.call('exec', 'sb-test', '--copy', 'file', ':' + str(remote), str(target), EXEC_LOCAL='1')
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(target.read_bytes(), data)
        directory = Path(self.temp.name) / 'folder'
        directory.mkdir()
        (directory / 'child').write_bytes(data)
        remote_dir = Path(self.temp.name) / 'remote-folder'
        out = self.call('exec', 'sb-test', '--copy', 'recursive', str(directory), ':' + str(remote_dir), EXEC_LOCAL='1')
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual((remote_dir / 'child').read_bytes(), data)
        out = self.call('exec', 'sb-test', '--copy', 'file', ':' + str(remote_dir), str(target), EXEC_LOCAL='1')
        self.assertNotEqual(out.returncode, 0)
        out = self.call('exec', 'sb-test', '--copy', 'recursive', ':' + str(remote_dir), str(Path(self.temp.name) / 'tree'), EXEC_LOCAL='1')
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual((Path(self.temp.name) / 'tree/child').read_bytes(), data)

    def test_untrusted_copy_archives_and_existing_destination_links(self):
        import io
        import tarfile
        reply = Path(self.temp.name) / 'reply.tar'
        target = Path(self.temp.name) / 'target'
        for name, kind in [('../escape', tarfile.REGTYPE), ('/absolute', tarfile.REGTYPE),
                           ('root/link', tarfile.SYMTYPE), ('root/fifo', tarfile.FIFOTYPE)]:
            with tarfile.open(reply, 'w') as archive:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.linkname = '../../escape'
                archive.addfile(member, io.BytesIO())
            out = self.call('exec', 'sb-test', '--copy', 'recursive', ':/remote', str(target),
                            EXEC_LOCAL='1', REPLY_ARCHIVE=str(reply))
            self.assertNotEqual(out.returncode, 0)
            self.assertFalse(target.exists())
            self.assertFalse((Path(self.temp.name) / 'escape').exists())
        with tarfile.open(reply, 'w') as archive:
            member = tarfile.TarInfo('remote/x')
            member.size = 4
            archive.addfile(member, io.BytesIO(b'evil'))
        (target / 'remote').mkdir(parents=True)
        outside = Path(self.temp.name) / 'outside'
        outside.write_bytes(b'original')
        (target / 'remote/x').symlink_to(outside)
        out = self.call('exec', 'sb-test', '--copy', 'recursive', ':/remote', str(target),
                        EXEC_LOCAL='1', REPLY_ARCHIVE=str(reply))
        self.assertNotEqual(out.returncode, 0)
        self.assertEqual(outside.read_bytes(), b'original')

    def test_exec_with_devnull_and_regular_file_stdin(self):
        import sys
        command = [sys.executable, '-B', os.environ['HELPER_PATH'], 'exec', 'sb-test', '--', 'cat']
        env = {**self.env, 'EXEC_LOCAL': '1'}
        out = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, env=env, timeout=5)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout, b'')
        path = Path(self.temp.name) / 'stdin'
        path.write_bytes(b'file\x00\xff')
        with path.open('rb') as source:
            out = subprocess.run(command, stdin=source, capture_output=True, env=env, timeout=5)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout, path.read_bytes())

    def test_pty_stdin_can_stay_open_after_exit_and_terminal_is_restored(self):
        import signal
        import sys
        import termios
        import time
        for interrupt in (False, True):
            master, slave = os.openpty()
            saved = termios.tcgetattr(slave)
            try:
                process = subprocess.Popen([sys.executable, '-B', os.environ['HELPER_PATH'],
                                            'exec', 'sb-test', '--pty', '--', 'bash', '-l'],
                                           stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           env={**self.env, 'PTY_EXPECTED': '1', **({'SLOW_EXEC': '1'} if interrupt else {})})
                if interrupt:
                    until = time.monotonic() + 5
                    while termios.tcgetattr(slave) == saved and time.monotonic() < until:
                        time.sleep(0.01)
                    self.assertNotEqual(termios.tcgetattr(slave), saved)
                    process.send_signal(signal.SIGTERM)
                process.communicate(timeout=5)
                self.assertEqual(process.returncode, 143 if interrupt else 7)
                restored = termios.tcgetattr(slave)
                # macOS can set the transient PENDIN bit when returning to
                # canonical mode; it is kernel pending-input state, not a mode.
                restored[3] &= ~getattr(termios, 'PENDIN', 0)
                saved[3] &= ~getattr(termios, 'PENDIN', 0)
                self.assertEqual(restored, saved)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                os.close(master)
                os.close(slave)

    def test_status_list_terminate(self):
        self.assertTrue(json.loads(self.call("status", "sb-test").stdout)["running"])
        self.assertEqual(json.loads(self.call("status", "sb-test", FINISHED="1").stdout)["returncode"], 137)
        self.assertEqual(len(json.loads(self.call("list").stdout)["sandboxes"]), 1)
        self.assertEqual(json.loads(self.call("list", FINISHED="1").stdout)["sandboxes"], [])
        for args in [("terminate", "sb-test"), ("terminate", "--tag", "run_id=test")]:
            self.assertTrue(json.loads(self.call(*args).stdout)["finished"])
        out = self.call("status", "missing")
        self.assertEqual(json.loads(out.stdout)["error"]["kind"], "NotFoundError")


if __name__ == "__main__":
    unittest.main()
