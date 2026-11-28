import json
import stat
import sys
from pathlib import Path

import pytest
from gpu_mcp.audit import Audit
from gpu_mcp.config import from_dict
from gpu_mcp.tools import GpuTools

HERE = Path(__file__).parent

# Placeholder credentials: the tests never talk to GitHub.
TEST_ENV = {"GPU_MCP_GITHUB_CLIENT_ID": "test-client-id", "GPU_MCP_GITHUB_CLIENT_SECRET": "test-client-secret"}


class FakeGpu:
    def __init__(self, root: Path):
        self.root = root
        self.responses: dict = {}
        self.binary = root / "gpu"
        self.binary.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(HERE / 'fake_gpu.py')!r})\n")
        self.binary.chmod(self.binary.stat().st_mode | stat.S_IEXEC)
        self._save()

    def reply(self, key: str, result: dict | None = None, *, code: int = 0, error: str | None = None,
              sleep: float = 0, command: str | None = None) -> None:
        envelope = {"command": command or " ".join(key.split()[:2]), "ok": code == 0, "result": result or {}}
        if error is not None:
            # gpu's error envelope shape (src/output.rs).
            envelope["error"] = {"code": "error", "message": error, "exit_code": code}
        self.responses[key] = {"code": code, "envelope": envelope, "sleep": sleep}
        self._save()

    def _save(self) -> None:
        (self.root / "responses.json").write_text(json.dumps(self.responses))

    def calls(self) -> list[dict]:
        path = self.root / "calls.jsonl"
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines()]


@pytest.fixture
def fake_gpu(tmp_path, monkeypatch):
    root = tmp_path / "fake"
    root.mkdir()
    monkeypatch.setenv("FAKE_GPU_DIR", str(root))
    for key, value in TEST_ENV.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setenv("GPU_OWNER", "someone-else")
    return FakeGpu(root)


def make_config(tmp_path, fake_gpu, **overrides):
    raw = {
        "public_url": "https://gpu.example.ts.net",
        "allowed_github_users": ["Owner"],
        "gpu_binary": str(fake_gpu.binary),
        "data_dir": str(tmp_path / "data"),
        "caps": {"max_price_per_hour": 1.0, "max_expire": "4h", "daily_spend_per_caller": 5.0},
        "limits": {"tool_wait_seconds": 2.0, "log_max_bytes": 64, "output_file_max_bytes": 32,
                   "outputs_inline_total_bytes": 48, "inline_file_max_bytes": 100,
                   "inline_files_max_total_bytes": 150, "inline_files_max_count": 3},
    }
    for key, value in overrides.items():
        if isinstance(value, dict) and isinstance(raw.get(key), dict):
            raw[key] = {**raw[key], **value}
        else:
            raw[key] = value
    return from_dict(raw, TEST_ENV)


@pytest.fixture
def config(tmp_path, fake_gpu):
    return make_config(tmp_path, fake_gpu)


@pytest.fixture
def tools(config):
    return GpuTools(config, Audit(config.data_dir / "audit.jsonl"))


def audit_lines(config) -> list[dict]:
    path = config.data_dir / "audit.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def job(job_id="job-1", owner="owner", state="running", **extra):
    base = {
        "job_id": job_id, "state": state, "exit_code": None, "timed_out": False, "elapsed_seconds": 5,
        "cost_usd": 0.01, "price_per_hour": 0.4, "provider": "vast", "id": "12345", "owner": owner,
        "gpu": "RTX 4090", "expires_at": "2026-11-28T18:00:00Z", "outputs_collected": False,
        "destroyed": False, "outputs_dir": None, "results_dir": "/elsewhere", "command": ["python", "t.py"],
    }
    return {**base, **extra}


