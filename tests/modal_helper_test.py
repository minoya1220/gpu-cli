"""Offline contract tests: the real helper with a synthetic modal package."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

STUB = r'''
import os
from types import SimpleNamespace
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
class App:
    pass
class AsyncCall:
    def __init__(self, value): self.value = value
    async def aio(self, *args, **kw): return self.value
class Reader:
    def __init__(self, data): self.data = data
    def __aiter__(self): return self
    async def __anext__(self):
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
class Sandbox:
    object_id = "sb-test"
    def __init__(self): self.exec = AsyncCall(Process())
    @staticmethod
    async def allocate(*args, **kw):
        assert kw["image"].has_uv
        assert kw["cpu"] == (4, 4)
        assert kw["memory"] == (16384, 16384)
        assert kw["timeout"] == 1800
        assert "secrets" not in kw and "env" not in kw
        assert kw["tags"]["gpu-cli"] == "true"
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
    def get_tags(self): return {"gpu-cli": "true", "run_id": "test"}
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
        self.env = {"PATH": os.environ.get("PATH", ""), "PYTHONPATH": self.temp.name,
                    "PYTHONDONTWRITEBYTECODE": "1"}

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
