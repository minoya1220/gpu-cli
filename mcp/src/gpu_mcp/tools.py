"""Tool logic: argument checks, server-side caps, ownership, size limits, audit.

Every tool takes the authenticated caller's GitHub login. Jobs are labeled
with the caller's owner label, and a job owned by anyone else is reported as
not found. Calls that can outlast an MCP tool-call timeout (starting a job,
downloading its outputs, cancelling) run in the background: the tool waits a
few seconds and otherwise returns a handle to poll with job_status or the same
tool again.
"""

from __future__ import annotations

import asyncio
import re
import secrets
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

from mcp.server.mcpserver.exceptions import ToolError as SdkToolError

from . import gpu
from .audit import Audit
from .config import Config, parse_duration
from .ledger import Ledger, LedgerError

JOB_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,79}")
LAUNCH_ID = re.compile(r"launch-[0-9a-f]{16}")
REF = re.compile(r"[A-Za-z0-9_./@{}^~+-]{1,200}")
# owner/name, a bare name, an https URL or git@host:path; never a local path.
REPO = re.compile(
    r"[A-Za-z0-9][A-Za-z0-9_.-]{0,99}(?:/[A-Za-z0-9_.-]{1,100})?"
    r"|https://[A-Za-z0-9.-]+(?::[0-9]+)?/[A-Za-z0-9_./~-]{1,250}"
    r"|git@[A-Za-z0-9.-]+:[A-Za-z0-9_./~-]{1,250}"
)
GPU_NAME = re.compile(r"[A-Za-z0-9 _.+-]{1,64}")
OUTPUTS = re.compile(r"[A-Za-z0-9_.-][A-Za-z0-9_./-]{0,199}")
MIN_EXPIRE = 600  # gpu reserves 10 minutes of the lifetime for collecting outputs
NOT_FOUND = "Job {} not found."
RESERVED_TOP = {"gpu-runs", ".git"}


class ToolError(SdkToolError):
    """A refusal or failure reported to the caller as a tool error (its text is shown)."""

    def __init__(self, message: str, status: str = "error"):
        super().__init__(message)
        self.status = status


def refuse(message: str) -> ToolError:
    return ToolError(message, "refused")


@dataclass
class Op:
    """A gpu call running in the background on behalf of one caller."""

    id: str
    kind: str
    caller: str
    job_id: str | None
    task: asyncio.Task | None = None
    result: dict[str, Any] | None = None
    started: float = field(default_factory=time.time)


class GpuTools:
    def __init__(self, config: Config, audit: Audit, ledger: Ledger | None = None):
        self.config = config
        self.audit = audit
        self.ledger = ledger or Ledger(config.data_dir / "spend.json")
        self.ops: dict[str, Op] = {}
        self.work = config.data_dir / "work"

    # Plumbing --------------------------------------------------------------

    async def call(self, tool: str, caller: str, args: dict[str, Any], fn) -> dict[str, Any]:
        """Run one tool call and write its audit line, whatever happens."""
        job_id = args.get("job_id")
        try:
            result = await fn()
        except ToolError as exc:
            self.audit.write(caller=caller, tool=tool, status=exc.status, args=_audit_args(args),
                             job_id=job_id, error=str(exc))
            raise
        except Exception as exc:
            self.audit.write(caller=caller, tool=tool, status="error", args=_audit_args(args),
                             job_id=job_id, error=f"internal error: {type(exc).__name__}")
            raise ToolError("Internal server error; see the server log.") from exc
        self.audit.write(caller=caller, tool=tool, status="ok", args=_audit_args(args),
                         job_id=result.get("job_id") or job_id)
        return result

    async def _gpu(self, args: list[str], cwd: Path | None = None, timeout: float | None = -1.0) -> gpu.GpuResult:
        if timeout == -1.0:
            timeout = self.config.limits.read_timeout_seconds
        cwd = cwd or self.config.data_dir
        cwd.mkdir(parents=True, exist_ok=True, mode=0o700)
        return await gpu.run(self.config.gpu_binary, args, cwd, timeout)

    def _owner(self, caller: str) -> str:
        try:
            return self.config.owner_label(caller)
        except ValueError as exc:
            raise refuse(str(exc)) from None

    async def _background(self, op: Op, coro) -> dict[str, Any] | None:
        """Start `coro` for `op`; return its result if it ends within the wait."""
        self.ops[op.id] = op
        op.task = asyncio.create_task(self._finish(op, coro))
        done, _ = await asyncio.wait({op.task}, timeout=self.config.limits.tool_wait_seconds)
        return op.result if done else None

    async def _finish(self, op: Op, coro) -> None:
        try:
            op.result = await coro
        except ToolError as exc:
            op.result = {"ok": False, "error": str(exc)}
        except Exception as exc:  # noqa: BLE001 - reported to the caller on its next poll
            op.result = {"ok": False, "error": f"internal error: {type(exc).__name__}"}
        self._prune_ops()

    def _prune_ops(self) -> None:
        cutoff = time.time() - 86400
        self.ops = {k: v for k, v in self.ops.items() if v.result is None or v.started > cutoff}

    def _running_op(self, caller: str, kind: str, job_id: str) -> Op | None:
        for op in self.ops.values():
            if op.caller == caller and op.kind == kind and op.job_id == job_id and op.result is None:
                return op
        return None

    async def _owned(self, caller: str, job_id: str) -> dict[str, Any]:
        """The job's status, if the caller owns it; otherwise not found."""
        if not isinstance(job_id, str) or not JOB_ID.fullmatch(job_id):
            raise refuse("job_id must be a job ID from run_job or list_jobs.")
        owner = self._owner(caller)
        res = await self._gpu(["job", "status", job_id])
        if res.code == 16 or (res.ok and res.result.get("owner") != owner):
            raise ToolError(NOT_FOUND.format(job_id))
        if not res.ok:
            raise ToolError(res.error)
        return res.result

    # search_offers ---------------------------------------------------------

    async def search_offers(self, caller: str, gpu_name: str, num_gpus: int = 1,
                            max_price: float | None = None, expire: str = "1h",
                            disk: int | None = None) -> dict[str, Any]:
        owner = self._owner(caller)
        price, seconds = self._caps(max_price, expire)
        args = ["job", "start", "--dry-run", "--yes", f"--gpu={_gpu_name(gpu_name)}",
                f"--num-gpus={_int(num_gpus, 'num_gpus', 1, 16)}", f"--max-price={price!r}",
                f"--expire={expire}", f"--owner={owner}"]
        if disk is not None:
            args.append(f"--disk={_int(disk, 'disk', 1, 2000)}")
        probe = self.work / "offers"
        probe.mkdir(parents=True, exist_ok=True, mode=0o700)
        res = await self._gpu([*args, "--", "true"], cwd=probe)
        if not res.ok:
            raise ToolError(res.error)
        r = res.result
        request = r.get("request")
        offer_id = request[3] if isinstance(request, list) and len(request) > 3 else None
        spent = self._spent(caller)
        return {
            "note": "The cheapest matching Vast offer that run_job would rent now; offers change quickly.",
            "gpu": r.get("gpu"),
            "offer_id": offer_id,
            "price_per_hour": r.get("price_per_hour"),
            "estimated_spend": r.get("estimated_spend"),
            "expire_seconds": seconds,
            "max_price_per_hour": price,
            "server_caps": self._cap_view(),
            "daily_spent_estimate": spent,
            "daily_remaining": max(0.0, self.config.caps.daily_spend_per_caller - spent),
        }

    def _spent(self, caller: str) -> float:
        try:
            return round(self.ledger.spent(caller), 4)
        except LedgerError as exc:
            raise ToolError(str(exc)) from None

    def _cap_view(self) -> dict[str, Any]:
        caps = self.config.caps
        return {
            "max_price_per_hour": caps.max_price_per_hour,
            "max_expire_seconds": caps.max_expire_seconds,
            "daily_spend_per_caller": caps.daily_spend_per_caller,
        }

    def _caps(self, max_price: float | None, expire: str) -> tuple[float, int]:
        caps = self.config.caps
        if max_price is None:
            price = caps.max_price_per_hour
        else:
            if isinstance(max_price, bool) or not isinstance(max_price, (int, float)) or not max_price > 0:
                raise refuse("max_price must be a positive number of USD per hour.")
            if max_price > caps.max_price_per_hour:
                raise refuse(f"Refused: max_price ${max_price}/hr is over the server cap of "
                             f"${caps.max_price_per_hour}/hr.")
            price = float(max_price)
        try:
            seconds = parse_duration(expire)
        except ValueError as exc:
            raise refuse(f"expire: {exc}") from None
        if seconds > caps.max_expire_seconds:
            raise refuse(f"Refused: expire {expire} is over the server cap of "
                         f"{caps.max_expire_seconds // 60} minutes.")
        if seconds <= MIN_EXPIRE:
            raise refuse("expire must exceed 10 minutes, which gpu reserves for collecting outputs.")
        return price, seconds

    # run_job ---------------------------------------------------------------

    async def run_job(self, caller: str, source_repo: str, ref: str, command: list[str],
                      gpu_name: str, expire: str, num_gpus: int = 1,
                      max_price: float | None = None, outputs: str = "outputs",
                      files: dict[str, str] | None = None, setup: str | None = None,
                      no_setup: bool = False, image: str | None = None,
                      disk: int | None = None) -> dict[str, Any]:
        owner = self._owner(caller)
        price, seconds = self._caps(max_price, expire)
        if (not isinstance(source_repo, str) or not REPO.fullmatch(source_repo)
                or ".." in source_repo.split("/")):
            raise refuse("source_repo must be a GitHub owner/name, an https:// URL or a git@host:path URL.")
        if not isinstance(ref, str) or not REF.fullmatch(ref) or ref.startswith("-"):
            raise refuse("ref must be a commit SHA, branch or tag.")
        argv = _command(command, self.config.limits.command_max_bytes)
        if not isinstance(outputs, str) or not OUTPUTS.fullmatch(outputs) or ".." in outputs.split("/"):
            raise refuse("outputs must be a relative folder such as outputs.")
        inline = _inline_files(files or {}, self.config.limits)
        args = ["job", "start", "--yes", f"--gpu={_gpu_name(gpu_name)}",
                f"--num-gpus={_int(num_gpus, 'num_gpus', 1, 16)}", f"--max-price={price!r}",
                f"--expire={expire}", f"--owner={owner}", f"--source-repo={source_repo}",
                f"--ref={ref}", f"--outputs={outputs}"]
        if disk is not None:
            args.append(f"--disk={_int(disk, 'disk', 1, 2000)}")
        if setup is not None and no_setup:
            raise refuse("Pass setup or no_setup, not both.")
        if setup is not None:
            if not isinstance(setup, str) or not setup or len(setup.encode()) > self.config.limits.command_max_bytes:
                raise refuse("setup must be a non-empty shell command within the size limit.")
            args.append(f"--setup={setup}")
        if no_setup:
            args.append("--no-setup")
        if image is not None:
            if not isinstance(image, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_./:@-]{0,255}", image):
                raise refuse("image must be a container image reference.")
            args.append(f"--image={image}")
        for path in inline:
            args.append(f"--include={path}")

        await self.sweep_work()
        worst = price * seconds / 3600
        try:
            reservation = self.ledger.reserve(caller, worst, self.config.caps.daily_spend_per_caller)
        except LedgerError as exc:
            raise refuse(str(exc)) from None

        launch_id = f"launch-{secrets.token_hex(8)}"
        workdir = self.work / launch_id
        try:
            workdir.mkdir(parents=True, mode=0o700)
            for path, content in inline.items():
                target = workdir / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content)
        except OSError:
            self.ledger.settle(caller, reservation, None)
            raise
        op = Op(launch_id, "start", caller, None)
        result = await self._background(
            op, self._launch(caller, reservation, seconds, [*args, "--", *argv], workdir, op)
        )
        if result is None:
            return {
                "launch_id": launch_id,
                "state": "starting",
                "next": f"Call job_status with {launch_id} until it reports a job_id; booting can take minutes.",
            }
        if not result.get("ok"):
            raise ToolError(result["error"])
        return result

    async def sweep_work(self) -> None:
        """Remove launch folders older than the retention whose jobs have all ended."""
        cutoff = time.time() - self.config.work_retention_days * 86400
        stale: list[tuple[Path, list[str]]] = []
        for folder in self.work.glob("launch-*") if self.work.is_dir() else []:
            if not LAUNCH_ID.fullmatch(folder.name) or not folder.is_dir() or folder.is_symlink():
                continue
            op = self.ops.get(folder.name)
            if op is not None and op.result is None:
                continue
            runs = folder / "gpu-runs"
            jobs = list(runs.iterdir()) if runs.is_dir() else []
            try:
                newest = max(p.stat().st_mtime for p in [folder, *([runs] if jobs else []), *jobs])
            except OSError:
                continue
            if newest <= cutoff:
                stale.append((folder, [p.name for p in jobs]))
        if not stale:
            return
        res = await self._gpu(["job", "ls"])
        if not res.ok:
            return
        records = {j.get("job_id"): j for j in res.result.get("jobs", []) if isinstance(j, dict)}
        for folder, job_ids in stale:
            if not any(_unended(records.get(job_id)) for job_id in job_ids):
                shutil.rmtree(folder, ignore_errors=True)

    async def _launch(self, caller: str, reservation: str, seconds: int, args: list[str],
                      workdir: Path, op: Op) -> dict[str, Any]:
        res = await self._gpu(args, cwd=workdir, timeout=None)
        r = res.result
        if not res.ok:
            # gpu sets created to false while planning and to null once it sends
            # the create request; a failure before planning reports an empty
            # result. Only a launch whose envelope shows it never reached the
            # create request gives back its reservation; with no envelope at
            # all (the process died) an instance may exist, so it stays.
            if res.envelope is not None and r.get("created", False) is False:
                self.ledger.settle(caller, reservation, None)
            return {"ok": False, "launch_id": op.id, "error": res.error}
        job_id = r.get("job_id")
        op.job_id = job_id
        rate = r.get("price_per_hour")
        if isinstance(rate, (int, float)):
            self.ledger.settle(caller, reservation, rate * seconds / 3600, job_id)
        return {"ok": True, "launch_id": op.id, **_job_view(r)}

    # job_status ------------------------------------------------------------

    async def job_status(self, caller: str, job_id: str) -> dict[str, Any]:
        if isinstance(job_id, str) and LAUNCH_ID.fullmatch(job_id):
            op = self.ops.get(job_id)
            if op is None or op.caller != caller or op.kind != "start":
                raise ToolError(f"Launch {job_id} not found (the server may have restarted; see list_jobs).")
            if op.result is None:
                return {"launch_id": job_id, "state": "starting",
                        "elapsed_seconds": int(time.time() - op.started)}
            if not op.result.get("ok"):
                return {"launch_id": job_id, "state": "start_failed", "error": op.result["error"]}
            job_id = op.result["job_id"]
        status = await self._owned(caller, job_id)
        view = _job_view(status)
        for kind in ("outputs", "cancel"):
            if self._running_op(caller, kind, status["job_id"]):
                view["pending"] = kind
        return view

    # job_logs --------------------------------------------------------------

    async def job_logs(self, caller: str, job_id: str, offset: int | None = None,
                       max_bytes: int | None = None) -> dict[str, Any]:
        limit = self.config.limits.log_max_bytes
        if max_bytes is not None:
            limit = min(limit, _int(max_bytes, "max_bytes", 1, limit))
        status = await self._owned(caller, job_id)
        job = status["job_id"]
        tail = offset is None
        start = _int(offset, "offset", 0, 2**62) if not tail else 0
        if tail and isinstance(status.get("log_bytes"), int):
            start = max(0, status["log_bytes"] - limit)
        res = await self._gpu(["job", "logs", job, f"--offset={start}"])
        if not res.ok:
            raise ToolError(res.error)
        r = res.result
        data = str(r.get("log", "")).encode()
        next_offset = r.get("next_offset")
        if tail:
            truncated = len(data) > limit or start > 0
            data = data[-limit:]
        else:
            truncated = len(data) > limit
            if truncated:
                data = data[:limit]
                next_offset = start + limit
        return {
            "job_id": job,
            "state": status.get("state"),
            "log": data.decode("utf-8", "ignore"),
            "offset": start if not tail else None,
            "next_offset": next_offset,
            "log_bytes": r.get("log_bytes"),
            "truncated": truncated,
        }

    # job_outputs -----------------------------------------------------------

    async def job_outputs(self, caller: str, job_id: str, path: str | None = None,
                          offset: int = 0) -> dict[str, Any]:
        status = await self._owned(caller, job_id)
        job = status["job_id"]
        if path is not None:
            return self._read_output(status, path, _int(offset, "offset", 0, 2**62))
        if not status.get("outputs_collected"):
            running = self._running_op(caller, "outputs", job)
            if running is None:
                op = Op(f"outputs-{secrets.token_hex(8)}", "outputs", caller, job)
                result = await self._background(op, self._collect(job))
            else:
                done, _ = await asyncio.wait({running.task}, timeout=self.config.limits.tool_wait_seconds)
                result = running.result if done else None
            if result is None:
                return {"job_id": job, "state": "collecting",
                        "next": "Downloading outputs; call job_outputs again shortly."}
            if not result.get("ok"):
                raise ToolError(result["error"])
            status = result["status"]
        return self._listing(status)

    async def _collect(self, job: str) -> dict[str, Any]:
        res = await self._gpu(["job", "outputs", job], timeout=None)
        if not res.ok:
            return {"ok": False, "error": res.error}
        return {"ok": True, "status": res.result}

    def _results_dir(self, status: dict[str, Any]) -> Path:
        root = status.get("results_dir")
        if not isinstance(root, str):
            raise ToolError("This job has no results folder.")
        root_path = Path(root).resolve()
        if not root_path.is_relative_to(self.work.resolve()):
            raise ToolError("This job was not started by this server; its outputs are not served.")
        return root_path

    def _listing(self, status: dict[str, Any]) -> dict[str, Any]:
        root = self._results_dir(status)
        limits = self.config.limits
        budget = limits.outputs_inline_total_bytes
        files = []
        if root.is_dir():
            for item in sorted(p for p in root.rglob("*") if p.is_file() and not p.is_symlink()):
                rel = item.relative_to(root).as_posix()
                size = item.stat().st_size
                entry: dict[str, Any] = {"path": rel, "bytes": size}
                if size <= limits.output_file_max_bytes and size <= budget:
                    try:
                        entry["content"] = item.read_bytes().decode("utf-8")
                        budget -= size
                    except UnicodeDecodeError:
                        entry["binary"] = True
                files.append(entry)
        return {
            **_job_view(status),
            "files": files,
            "note": "Small text files are inline; read others with job_outputs(job_id, path, offset).",
        }

    def _read_output(self, status: dict[str, Any], path: str, offset: int) -> dict[str, Any]:
        if not status.get("outputs_collected"):
            raise refuse("Collect the outputs first: call job_outputs without path.")
        root = self._results_dir(status)
        if not isinstance(path, str) or PurePosixPath(path).is_absolute() or ".." in PurePosixPath(path).parts:
            raise refuse("path must be relative to the job's results, as listed by job_outputs.")
        target = (root / path)
        resolved = target.resolve()
        if not resolved.is_relative_to(root) or target.is_symlink() or not resolved.is_file():
            raise ToolError(f"No output file {path}.")
        limit = self.config.limits.output_file_max_bytes
        size = resolved.stat().st_size
        with resolved.open("rb") as f:
            f.seek(offset)
            data = f.read(limit)
        try:
            text, binary = data.decode("utf-8"), False
        except UnicodeDecodeError:
            text, binary = data.decode("utf-8", "replace"), True
        end = offset + len(data)
        return {
            "job_id": status["job_id"],
            "path": path,
            "bytes": size,
            "offset": offset,
            "content": text,
            "binary": binary,
            "next_offset": end if end < size else None,
        }

    # cancel_job / list_jobs -----------------------------------------------

    async def cancel_job(self, caller: str, job_id: str) -> dict[str, Any]:
        status = await self._owned(caller, job_id)
        job = status["job_id"]
        running = self._running_op(caller, "cancel", job)
        if running is None:
            op = Op(f"cancel-{secrets.token_hex(8)}", "cancel", caller, job)
            result = await self._background(op, self._cancel(job))
        else:
            done, _ = await asyncio.wait({running.task}, timeout=self.config.limits.tool_wait_seconds)
            result = running.result if done else None
        if result is None:
            return {"job_id": job, "state": "cancelling",
                    "next": "Destroying the instance; check job_status shortly."}
        if not result.get("ok"):
            raise ToolError(result["error"])
        return result

    async def _cancel(self, job: str) -> dict[str, Any]:
        res = await self._gpu(["job", "cancel", job], timeout=None)
        if not res.ok:
            return {"ok": False, "error": res.error}
        return {"ok": True, **_job_view(res.result)}

    async def list_jobs(self, caller: str) -> dict[str, Any]:
        owner = self._owner(caller)
        res = await self._gpu(["job", "ls", f"--owner={owner}"])
        if not res.ok:
            raise ToolError(res.error)
        jobs = [_job_view(j) for j in res.result.get("jobs", []) if isinstance(j, dict) and j.get("owner") == owner]
        starting = [
            {"launch_id": op.id, "state": "starting", "elapsed_seconds": int(time.time() - op.started)}
            for op in self.ops.values()
            if op.caller == caller and op.kind == "start" and op.result is None
        ]
        return {"jobs": jobs, "starting": starting, "daily_spent_estimate": self._spent(caller),
                "server_caps": self._cap_view()}


# Helpers ------------------------------------------------------------------

JOB_FIELDS = (
    "job_id", "state", "exit_code", "timed_out", "elapsed_seconds", "cost_usd", "price_per_hour",
    "gpu", "id", "owner", "expires_at", "outputs_collected", "destroyed", "command",
    "source_repo", "source_ref", "source_commit", "provider_status", "stale", "warning",
    "log_bytes", "retained_until",
)


ENDED = {"succeeded", "failed", "cancelled", "expired"}


def _unended(job: dict[str, Any] | None) -> bool:
    """Whether a job record shows a job that has not ended; no record means it is gone."""
    if job is None:
        return False
    return not (job.get("state") in ENDED or job.get("outputs_collected") is True)


def _job_view(result: dict[str, Any]) -> dict[str, Any]:
    """The job fields a caller needs, without local paths of the server."""
    view = {k: result[k] for k in JOB_FIELDS if k in result}
    if "id" in view:
        view["instance_id"] = view.pop("id")
    return view


def _int(value: Any, name: str, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise refuse(f"{name} must be an integer from {low} to {high}.")
    return value


def _gpu_name(value: Any) -> str:
    if not isinstance(value, str) or not GPU_NAME.fullmatch(value) or value.startswith("-"):
        raise refuse("gpu must be a GPU model such as 'RTX 4090'.")
    return value


def _command(command: Any, max_bytes: int) -> list[str]:
    if (not isinstance(command, list) or not command
            or not all(isinstance(a, str) for a in command) or not command[0]):
        raise refuse("command must be a non-empty list of strings, e.g. ['python', 'train.py'].")
    if any("\0" in a for a in command) or sum(len(a.encode()) + 1 for a in command) > max_bytes:
        raise refuse(f"command must be at most {max_bytes} bytes without NUL characters.")
    return command


def _inline_files(files: Any, limits) -> dict[str, str]:
    """Validated small extra files: relative path -> UTF-8 text."""
    if not isinstance(files, dict):
        raise refuse("files must map relative paths to text contents.")
    if len(files) > limits.inline_files_max_count:
        raise refuse(f"At most {limits.inline_files_max_count} inline files.")
    total = 0
    clean: dict[str, str] = {}
    for path, content in files.items():
        p = PurePosixPath(path) if isinstance(path, str) else None
        if (p is None or not path or p.is_absolute() or "\\" in path or "\0" in path
                or any(part in ("..", ".", "") for part in path.split("/"))
                or p.parts[0] in RESERVED_TOP):
            raise refuse(f"Inline file path {path!r} must be relative, without '..', and not under gpu-runs/.")
        if not isinstance(content, str):
            raise refuse(f"Inline file {path} must be text.")
        size = len(content.encode())
        if size > limits.inline_file_max_bytes:
            raise refuse(f"Inline file {path} is {size} bytes; the limit is {limits.inline_file_max_bytes}.")
        total += size
        clean[p.as_posix()] = content
    if total > limits.inline_files_max_total_bytes:
        raise refuse(f"Inline files total {total} bytes; the limit is {limits.inline_files_max_total_bytes}.")
    return clean


def _audit_args(args: dict[str, Any]) -> dict[str, Any]:
    """Arguments as logged: inline file contents reduced to names and sizes."""
    out = {}
    for key, value in args.items():
        if key == "files" and isinstance(value, dict):
            out[key] = {str(k): len(v.encode()) if isinstance(v, str) else None for k, v in value.items()}
        else:
            out[key] = value
    return out
