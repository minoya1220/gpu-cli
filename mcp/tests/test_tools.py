import asyncio
import json
import os
import time

import pytest
from conftest import audit_lines, job, make_config
from gpu_mcp import gpu
from gpu_mcp.audit import Audit
from gpu_mcp.tools import GpuTools, ToolError

RUN = {"source_repo": "acme/widgets", "ref": "my-branch", "command": ["python", "train.py"],
       "gpu_name": "RTX 4090", "expire": "2h"}


def run(coro):
    return asyncio.run(coro)


def call(tools, tool, caller="owner", **kwargs):
    """Call a tool through the audited wrapper, like the MCP layer does."""
    fn = getattr(tools, tool)
    audit_args = {("gpu" if k == "gpu_name" else k): v for k, v in kwargs.items()}

    async def go():
        return await tools.call(tool, caller, audit_args, lambda: fn(caller, **kwargs))

    return run(go())


def started(fake_gpu, job_id="job-1", rate=0.5):
    fake_gpu.reply("job start", {**job(job_id), "price_per_hour": rate, "phase": "started",
                                 "results_dir": "/x/gpu-runs/" + job_id, "source_repo": "acme/widgets",
                                 "source_ref": "my-branch", "source_commit": "a" * 40, "created": True})


def test_run_job_maps_arguments_and_keeps_secrets_out(tools, fake_gpu, config):
    started(fake_gpu)
    result = call(tools, "run_job", **RUN, files={"scripts/job.sh": "echo hi\n"}, max_price=0.8,
                  setup="pip install -e .", disk=40)
    assert result["job_id"] == "job-1"
    assert result["source_commit"] == "a" * 40
    assert "results_dir" not in result and "outputs_dir" not in result
    [c] = fake_gpu.calls()
    assert c["argv"] == [
        "--json", "job", "start", "--yes", "--gpu=RTX 4090", "--num-gpus=1", "--max-price=0.8",
        "--expire=2h", "--owner=owner", "--source-repo=acme/widgets", "--ref=my-branch",
        "--outputs=outputs", "--disk=40", "--setup=pip install -e .", "--include=scripts/job.sh",
        "--", "python", "train.py",
    ]
    workdir = config.data_dir / "work"
    assert c["cwd"].startswith(str(workdir.resolve())) or c["cwd"].startswith(str(workdir))
    assert (workdir / c["cwd"].rsplit("/", 1)[1] / "scripts/job.sh").read_text() == "echo hi\n"
    assert "GPU_MCP_GITHUB_CLIENT_SECRET" not in c["env_keys"]
    assert "GPU_MCP_GITHUB_CLIENT_ID" not in c["env_keys"]
    assert "GPU_OWNER" not in c["env_keys"]
    # The reservation becomes the rented price times the lifetime.
    assert tools.ledger.spent("owner") == pytest.approx(0.5 * 2)
    [line] = audit_lines(config)
    assert line["tool"] == "run_job" and line["status"] == "ok" and line["caller"] == "owner"
    assert line["job_id"] == "job-1"
    assert line["args"]["files"] == {"scripts/job.sh": 8}
    assert "echo hi" not in json.dumps(line)


def test_run_job_defaults_price_to_cap(tools, fake_gpu):
    started(fake_gpu)
    call(tools, "run_job", **RUN)
    assert "--max-price=1.0" in fake_gpu.calls()[0]["argv"]


@pytest.mark.parametrize(
    "change, message",
    [
        ({"max_price": 1.5}, "over the server cap"),
        ({"expire": "5h"}, "over the server cap of 240 minutes"),
        ({"expire": "10m"}, "must exceed 10 minutes"),
        ({"expire": "6pm"}, "duration"),
        ({"source_repo": "/Users/owner/private"}, "source_repo"),
        ({"source_repo": "file:///etc"}, "source_repo"),
        ({"source_repo": "acme/../x"}, "source_repo"),
        ({"ref": "--upload-pack=x"}, "ref"),
        ({"command": []}, "command"),
        ({"command": "python train.py"}, "command"),
        ({"gpu_name": "--offer=1"}, "gpu"),
        ({"files": {"../evil": "x"}}, "Inline file path"),
        ({"files": {"/abs": "x"}}, "Inline file path"),
        ({"files": {"gpu-runs/x": "x"}}, "Inline file path"),
        ({"files": {"big.txt": "x" * 101}}, "limit is 100"),
        ({"files": {"a": "x" * 80, "b": "x" * 80}}, "total 160 bytes"),
        ({"files": {"a": "", "b": "", "c": "", "d": ""}}, "At most 3"),
        ({"outputs": "../up"}, "outputs"),
        ({"setup": "x", "no_setup": True}, "not both"),
    ],
)
def test_run_job_refusals_start_nothing(tools, fake_gpu, config, change, message):
    started(fake_gpu)
    with pytest.raises(ToolError, match=message):
        call(tools, "run_job", **{**RUN, **change})
    assert fake_gpu.calls() == []
    assert tools.ledger.spent("owner") == 0
    [line] = audit_lines(config)
    assert line["status"] == "refused"


def test_daily_cap_counts_earlier_jobs(tools, fake_gpu, config):
    started(fake_gpu, rate=1.0)
    call(tools, "run_job", **{**RUN, "expire": "4h"})
    assert tools.ledger.spent("owner") == pytest.approx(4.0)
    with pytest.raises(ToolError, match=r"daily spend cap for owner is \$5.00; \$4.00"):
        call(tools, "run_job", **{**RUN, "expire": "2h"})
    assert len(fake_gpu.calls()) == 1
    # A cheaper job that fits under the remaining budget is fine.
    call(tools, "run_job", **{**RUN, "expire": "30m", "max_price": 1.0})
    assert audit_lines(config)[1]["status"] == "refused"


def test_daily_cap_is_per_caller(tmp_path, fake_gpu):
    config = make_config(tmp_path, fake_gpu, allowed_github_users=["owner", "friend"])
    tools = GpuTools(config, Audit(config.data_dir / "audit.jsonl"))
    started(fake_gpu, rate=1.0)
    call(tools, "run_job", **{**RUN, "expire": "4h"})
    call(tools, "run_job", caller="friend", **{**RUN, "expire": "4h"})
    assert "--owner=friend" in fake_gpu.calls()[1]["argv"]


def test_failed_launch_gives_back_only_uncreated_reservations(tools, fake_gpu):
    fake_gpu.reply("job start", {"created": False}, code=10, error="No capacity")
    with pytest.raises(ToolError, match="No capacity"):
        call(tools, "run_job", **RUN)
    assert tools.ledger.spent("owner") == 0
    fake_gpu.reply("job start", {"created": None}, code=14, error="Readiness timeout")
    with pytest.raises(ToolError, match="Readiness timeout"):
        call(tools, "run_job", **RUN)
    assert tools.ledger.spent("owner") == pytest.approx(2.0)
    # A failure before planning (an unknown ref) reports an empty result.
    fake_gpu.reply("job start", {}, code=2, error="Unknown ref main in acme/widgets.")
    with pytest.raises(ToolError, match="Unknown ref"):
        call(tools, "run_job", **RUN)
    assert tools.ledger.spent("owner") == pytest.approx(2.0)


def test_launch_that_dies_without_an_envelope_keeps_its_reservation(tools, fake_gpu, monkeypatch):
    async def died(binary, args, cwd, timeout):
        return gpu.GpuResult(-9, None, "")

    monkeypatch.setattr(gpu, "run", died)
    with pytest.raises(ToolError):
        call(tools, "run_job", **RUN)
    assert tools.ledger.spent("owner") == pytest.approx(2.0)


def test_old_launch_folders_of_ended_jobs_are_removed(tools, fake_gpu, config):
    def folder(name, job_id, age_days):
        root = config.data_dir / "work" / name / "gpu-runs" / job_id
        root.mkdir(parents=True)
        stamp = time.time() - age_days * 86400
        for path in (root, root.parent, root.parent.parent):
            os.utime(path, (stamp, stamp))
        return root.parent.parent

    ended = folder("launch-" + "1" * 16, "job-ended", 10)
    running = folder("launch-" + "2" * 16, "job-running", 10)
    recent = folder("launch-" + "3" * 16, "job-recent", 1)
    fake_gpu.reply("job ls", {"jobs": [job("job-ended", state="succeeded"), job("job-running", state="running"),
                                       job("job-recent", state="succeeded")]})
    started(fake_gpu)
    call(tools, "run_job", **RUN)
    assert not ended.exists()
    assert running.exists() and recent.exists()


def test_slow_launch_returns_a_launch_id_to_poll(tools, fake_gpu, config):
    started(fake_gpu)
    fake_gpu.responses["job start"]["sleep"] = 3
    fake_gpu._save()
    fake_gpu.reply("job status job-1", job())

    async def go():
        first = await tools.run_job("owner", **RUN)
        assert first["state"] == "starting"
        launch = first["launch_id"]
        polled = await tools.job_status("owner", launch)
        assert polled["state"] == "starting"
        with pytest.raises(ToolError, match="not found"):
            await tools.job_status("intruder", launch)
        listed_ops = [op for op in tools.ops.values() if op.result is None]
        assert len(listed_ops) == 1
        await listed_ops[0].task
        done = await tools.job_status("owner", launch)
        assert done["job_id"] == "job-1" and done["state"] == "running"

    run(go())


def test_slow_launch_failure_is_reported_on_poll(tools, fake_gpu):
    fake_gpu.reply("job start", {"created": False}, code=11, error="Over --max-price", sleep=3)

    async def go():
        first = await tools.run_job("owner", **RUN)
        await tools.ops[first["launch_id"]].task
        polled = await tools.job_status("owner", first["launch_id"])
        assert polled == {"launch_id": first["launch_id"], "state": "start_failed", "error": "Over --max-price"}

    run(go())


def test_jobs_of_other_owners_are_not_found(tools, fake_gpu, config):
    fake_gpu.reply("job status job-2", job("job-2", owner="alice"))
    fake_gpu.reply("job status job-3", {}, code=16, error="Job job-3 not found on this machine.")
    for tool in ("job_status", "job_logs", "job_outputs", "cancel_job"):
        with pytest.raises(ToolError, match="Job job-2 not found"):
            call(tools, tool, job_id="job-2")
    with pytest.raises(ToolError, match="Job job-3 not found"):
        call(tools, "job_status", job_id="job-3")
    # Only status lookups ran: nothing was cancelled, collected or read.
    assert {tuple(c["argv"][1:3]) for c in fake_gpu.calls()} == {("job", "status")}
    with pytest.raises(ToolError, match="job_id must be"):
        call(tools, "job_status", job_id="--all")


def test_status_view(tools, fake_gpu):
    fake_gpu.reply("job status job-1", job(log_bytes=10))
    result = call(tools, "job_status", job_id="job-1")
    assert result["state"] == "running" and result["instance_id"] == "12345"
    assert "results_dir" not in result
    assert fake_gpu.calls()[0]["argv"] == ["--json", "job", "status", "job-1"]


def test_logs_tail_and_offsets(tools, fake_gpu):
    fake_gpu.reply("job status job-1", job(log_bytes=100))
    fake_gpu.reply("job logs job-1", {"log": "x" * 36 + "y" * 64, "next_offset": 100, "log_bytes": 100})
    tail = call(tools, "job_logs", job_id="job-1")
    # The tail starts log_bytes - limit (64) in, and is cut to the limit.
    assert fake_gpu.calls()[-1]["argv"] == ["--json", "job", "logs", "job-1", "--offset=36"]
    assert tail["log"] == "y" * 64 and tail["truncated"] and tail["next_offset"] == 100
    forward = call(tools, "job_logs", job_id="job-1", offset=0, max_bytes=10)
    assert fake_gpu.calls()[-1]["argv"][-1] == "--offset=0"
    assert forward["log"] == "x" * 10 and forward["next_offset"] == 10 and forward["truncated"]
    with pytest.raises(ToolError, match="max_bytes"):
        call(tools, "job_logs", job_id="job-1", max_bytes=10_000)


def collected(config, job_id="job-1", files=None):
    root = config.data_dir / "work" / "launch-0000000000000000" / "gpu-runs" / job_id
    for rel, content in (files or {}).items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    return job(job_id, state="succeeded", exit_code=0, outputs_collected=True, destroyed=True,
               results_dir=str(root), outputs_dir=str(root / "outputs"))


def test_outputs_collects_then_lists_small_files_inline(tools, fake_gpu, config):
    done = collected(config, files={"result.json": b'{"exit": 0}', "outputs/small.txt": b"hello",
                                    "outputs/big.txt": b"z" * 40, "outputs/img.bin": b"\xff\xfe",
                                    "outputs/second.txt": b"q" * 30, "outputs/third.txt": b"t" * 20})
    fake_gpu.reply("job status job-1", job(state="succeeded", exit_code=0))
    fake_gpu.reply("job outputs job-1", done)
    result = call(tools, "job_outputs", job_id="job-1")
    assert fake_gpu.calls()[-1]["argv"] == ["--json", "job", "outputs", "job-1"]
    files = {f["path"]: f for f in result["files"]}
    assert files["outputs/small.txt"]["content"] == "hello"
    assert "content" not in files["outputs/big.txt"] and files["outputs/big.txt"]["bytes"] == 40
    assert files["outputs/img.bin"]["binary"] is True
    # In path order, second (30) and small (5) leave 13 of the 48-byte inline budget:
    # third (20) is only listed, result.json (11) still fits.
    assert files["outputs/second.txt"]["content"] == "q" * 30
    assert "content" not in files["outputs/third.txt"] and files["outputs/third.txt"]["bytes"] == 20
    assert files["result.json"]["content"] == '{"exit": 0}'
    assert "results_dir" not in result
    # Already collected: list without downloading again; read a big file in chunks.
    fake_gpu.reply("job status job-1", done)
    chunk = call(tools, "job_outputs", job_id="job-1", path="outputs/big.txt")
    assert chunk["content"] == "z" * 32 and chunk["next_offset"] == 32
    rest = call(tools, "job_outputs", job_id="job-1", path="outputs/big.txt", offset=32)
    assert rest["content"] == "z" * 8 and rest["next_offset"] is None
    assert [c["argv"][2] for c in fake_gpu.calls()].count("outputs") == 1


def test_outputs_paths_stay_inside_results(tools, fake_gpu, config, tmp_path):
    done = collected(config, files={"outputs/a.txt": b"a"})
    secret = tmp_path / "secret.txt"
    secret.write_text("top secret")
    link = config.data_dir / "work" / "launch-0000000000000000" / "gpu-runs" / "job-1" / "outputs" / "link"
    link.symlink_to(secret)
    fake_gpu.reply("job status job-1", done)
    for path in ("../../../../../secret.txt", "/etc/passwd", "outputs/link"):
        with pytest.raises(ToolError):
            call(tools, "job_outputs", job_id="job-1", path=path)
    listing = call(tools, "job_outputs", job_id="job-1")
    assert [f["path"] for f in listing["files"]] == ["outputs/a.txt"]


def test_outputs_of_jobs_started_elsewhere_are_not_served(tools, fake_gpu):
    fake_gpu.reply("job status job-1", job(state="succeeded", outputs_collected=True, results_dir="/tmp/x"))
    with pytest.raises(ToolError, match="not started by this server"):
        call(tools, "job_outputs", job_id="job-1")


def test_slow_collection_returns_collecting(tools, fake_gpu, config):
    done = collected(config, files={"outputs/a.txt": b"a"})
    fake_gpu.reply("job status job-1", job(state="succeeded", exit_code=0))
    fake_gpu.reply("job outputs job-1", done, sleep=3)

    async def go():
        first = await tools.job_outputs("owner", "job-1")
        assert first["state"] == "collecting"
        assert (await tools.job_status("owner", "job-1"))["pending"] == "outputs"
        second = await tools.job_outputs("owner", "job-1")
        third = second if "files" in second else await tools.job_outputs("owner", "job-1")
        assert [f["path"] for f in third["files"]] == ["outputs/a.txt"]

    run(go())
    assert [c["argv"][2] for c in fake_gpu.calls()].count("outputs") == 1


def test_cancel_maps_to_job_cancel(tools, fake_gpu):
    fake_gpu.reply("job status job-1", job())
    fake_gpu.reply("job cancel job-1", job(state="cancelled", destroyed=True))
    result = call(tools, "cancel_job", job_id="job-1")
    assert result["state"] == "cancelled" and result["destroyed"]
    assert fake_gpu.calls()[-1]["argv"] == ["--json", "job", "cancel", "job-1"]


def test_list_jobs_is_the_callers_own(tools, fake_gpu):
    fake_gpu.reply("job ls", {"jobs": [job("job-1"), job("job-9", owner="alice")], "unreachable": 0})
    result = call(tools, "list_jobs")
    assert fake_gpu.calls()[0]["argv"] == ["--json", "job", "ls", "--owner=owner"]
    assert [j["job_id"] for j in result["jobs"]] == ["job-1"]
    assert result["server_caps"]["daily_spend_per_caller"] == 5.0


def test_search_offers_is_a_dry_run(tools, fake_gpu, config):
    fake_gpu.reply("job start dry", {"gpu": "RTX 4090", "price_per_hour": 0.35, "estimated_spend": 0.35,
                                     "request": ["vastai", "create", "instance", "987", "--image", "x"],
                                     "dry_run": True})
    result = call(tools, "search_offers", gpu_name="RTX 4090", max_price=0.5, expire="1h")
    [c] = fake_gpu.calls()
    assert c["argv"] == ["--json", "job", "start", "--dry-run", "--yes", "--gpu=RTX 4090", "--num-gpus=1",
                         "--max-price=0.5", "--expire=1h", "--owner=owner", "--", "true"]
    assert c["cwd"].endswith("work/offers")
    assert result["offer_id"] == "987" and result["price_per_hour"] == 0.35
    assert result["daily_remaining"] == 5.0
    assert "request" not in result


def test_login_without_valid_owner_label_is_refused(tmp_path, fake_gpu):
    long_login = "a" * 39
    config = make_config(tmp_path, fake_gpu, allowed_github_users=[long_login])
    tools = GpuTools(config, Audit(config.data_dir / "audit.jsonl"))
    with pytest.raises(ToolError, match="owner_labels"):
        call(tools, "list_jobs", caller=long_login)
    config = make_config(tmp_path, fake_gpu, allowed_github_users=[long_login],
                         owner_labels={long_login: "me"})
    tools = GpuTools(config, Audit(config.data_dir / "audit.jsonl"))
    fake_gpu.reply("job ls", {"jobs": []})
    call(tools, "list_jobs", caller=long_login)
    assert fake_gpu.calls()[-1]["argv"][-1] == "--owner=me"


def test_unreadable_ledger_refuses_launches(tools, fake_gpu, config):
    config.data_dir.mkdir(parents=True, exist_ok=True)
    (config.data_dir / "spend.json").write_text("{")
    started(fake_gpu)
    with pytest.raises(ToolError, match="unreadable"):
        call(tools, "run_job", **RUN)
    assert fake_gpu.calls() == []


def test_ledger_window_drops_old_entries(tools):
    key = tools.ledger.reserve("owner", 4.0, 5.0)
    data = json.loads(tools.ledger.path.read_text())
    data["owner"][0]["at"] = time.time() - 90000
    tools.ledger.path.write_text(json.dumps(data))
    assert tools.ledger.spent("owner") == 0
    tools.ledger.settle("owner", key, 1.0)
    tools.ledger.reserve("owner", 4.9, 5.0)
