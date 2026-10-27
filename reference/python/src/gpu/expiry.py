"""Local deadline parsing and SSH transport; no account credentials cross SSH."""
from __future__ import annotations

import json
import math
import re
import shlex
import subprocess
from datetime import datetime, timedelta, timezone, tzinfo
from pathlib import Path

UTC = timezone.utc


def parse_duration(value: str) -> int:
    match = re.fullmatch(r"(?:(\d+)d)?(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?", value)
    if not match or not any(match.groups()):
        raise ValueError("Use a duration such as 30m, 2h, or 1h30m (units: d h m s).")
    seconds = sum(int(n or 0) * unit for n, unit in zip(match.groups(), (86400, 3600, 60, 1)))
    if seconds <= 0:
        raise ValueError("Expiry duration must be positive.")
    return seconds


def _local_candidates(wall: datetime, zone: tzinfo | None) -> list[datetime]:
    """Round trips reject DST gaps; folds yield both possible UTC instants.

    With zone=None, Python uses the OS local timezone rules for that date, not
    today's fixed UTC offset (which is wrong across a DST transition).
    """
    candidates = set()
    for fold in (0, 1):
        candidate = wall.replace(tzinfo=zone, fold=fold).astimezone(UTC)
        back = candidate.astimezone(zone).replace(tzinfo=None)
        if back == wall:
            candidates.add(candidate)
    return sorted(candidates)


def parse_expire_arguments(values: list[str]) -> tuple[str | None, str]:
    values = list(values)
    instance_id = values.pop(0) if values and re.fullmatch(r"[0-9]+", values[0]) else None
    if len(values) != 1 or re.fullmatch(r"[0-9]+", values[0]):
        raise ValueError("Use gpu expire [ID] WHEN|cancel|show; e.g. 2h, 18:30, 6:30PM. Bare numbers are IDs, not times.")
    return instance_id, values[0]


def parse_when(value: str, **kwargs) -> datetime:
    if re.fullmatch(r"(?:\d+[dhms])+", value):
        return parse_deadline(duration=value, **kwargs)
    return parse_deadline(at=value, **kwargs)


def parse_deadline(*, duration: str | None = None, at: str | None = None,
                   now: datetime | None = None, zone: tzinfo | None = None) -> datetime:
    if (duration is None) == (at is None):
        raise ValueError("Specify exactly one duration or clock time.")
    now = (now or datetime.now(UTC)).astimezone(UTC)
    try:
        if duration is not None:
            return now + timedelta(seconds=parse_duration(duration))
        clock = re.fullmatch(r"(\d{1,2})(?::(\d{2}))?([aApP][mM])", at)
        if clock or re.fullmatch(r"\d{1,2}:\d{2}", at):
            if clock:
                hour, minute = int(clock[1]), int(clock[2] or 0)
                if not 1 <= hour <= 12:
                    raise ValueError("12-hour clock hours must be 1–12.")
                hour = hour % 12 + (12 if clock[3].lower() == "pm" else 0)
            else:
                hour, minute = map(int, at.split(":"))
            day = now.astimezone(zone).date()
            for offset in range(3):  # skip a nonexistent spring-forward wall time
                wall = datetime.combine(day + timedelta(days=offset), datetime.min.time())
                wall = wall.replace(hour=hour, minute=minute)
                future = [dt for dt in _local_candidates(wall, zone) if dt > now]
                if future:
                    return future[0]
            raise ValueError("No next occurrence found; use an ISO datetime with UTC offset.")
        if "T" not in at and " " not in at:
            raise ValueError("Use HH:MM or an ISO datetime, preferably with UTC offset.")
        value = datetime.fromisoformat(at.replace("Z", "+00:00"))
        if value.tzinfo is None:
            candidates = _local_candidates(value, zone)
            if len(candidates) != 1:
                raise ValueError("Local time is nonexistent or ambiguous (DST); include a UTC offset.")
            value = candidates[0]
        value = value.astimezone(UTC)
        if value <= now:
            raise ValueError("Expiry time must be in the future.")
        return value
    except OverflowError as exc:
        raise ValueError("Expiry is out of range.") from exc


def remote_command(alias: str, instance_id: str, action: str, deadline: datetime | None) -> list[str]:
    if not re.fullmatch(r"[0-9]+", instance_id) or alias != f"vast-{instance_id}":
        raise ValueError("Invalid managed instance SSH alias or ID.")
    if action not in {"arm", "show", "cancel"}:
        raise ValueError("Invalid expiry action.")
    args = ["python3", "-", action, instance_id]
    if action == "arm":
        if deadline is None:
            raise ValueError("Arming requires a deadline.")
        args.append(str(math.ceil(deadline.timestamp())))
    return ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", alias, shlex.join(args)]


def manage_expiry(alias: str, instance_id: str, action: str,
                  deadline: datetime | None = None) -> dict:
    source = Path(__file__).with_name("expiry_remote.py").read_text()
    payload = f"SOURCE = {source!r}\n" + source
    try:
        result = subprocess.run(remote_command(alias, instance_id, action, deadline),
                                input=payload, text=True, capture_output=True, timeout=45, check=False)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError("Expiry SSH timed out; arming/cancellation is unverified. Run gpu expire ID --show.") from exc
    # Never echo SSH stderr or arbitrary remote exception text: it may contain secrets.
    try:
        reply = json.loads(result.stdout)
    except ValueError:
        reply = {}
    errors = {
        "credentials": "Instance-scoped CONTAINER_ID/CONTAINER_API_KEY missing or mismatched; expiry NOT armed.",
        "past": "Deadline passed before SSH was ready; expiry NOT armed.",
        "verification": "Timer startup verification failed; new expiry NOT armed (previous deadline unchanged).",
        "remote": "Remote expiry operation failed; check python3 and writable ~/.local/state/gpu-expire; run --show.",
    }
    if not isinstance(reply, dict) or result.returncode or reply.get("error"):
        code = reply.get("error") if isinstance(reply, dict) else None
        raise RuntimeError(errors.get(code, errors["remote"]))
    status = reply.get("status")
    if status not in {"armed", "retrying", "inactive", "none", "cancelled", "destroy-request-accepted"}:
        raise RuntimeError("Invalid expiry response; completion unverified. Run gpu expire ID --show.")
    if action == "arm" and status != "armed":
        raise RuntimeError(errors["verification"])
    return reply


def expiry_summary(instance_id: str, reply: dict) -> str:
    status = reply["status"]
    if status in {"none", "cancelled"}:
        return f"Instance {instance_id}: no auto-destroy deadline" + (" (cancelled)." if status == "cancelled" else ".")
    deadline = datetime.fromtimestamp(reply["deadline"], UTC)
    local_dt = deadline.astimezone()
    local = local_dt.isoformat(timespec="seconds") + " " + local_dt.strftime("%Z")
    seconds = max(0, math.ceil((deadline - datetime.now(UTC)).total_seconds()))
    hours, rest = divmod(seconds, 3600)
    minutes, seconds = divmod(rest, 60)
    return (f"Instance {instance_id}: auto-DESTROY at {local} "
            f"(in {hours}h {minutes}m {seconds}s); timer {status}. Disk will be deleted; all billing stops on destruction.")
