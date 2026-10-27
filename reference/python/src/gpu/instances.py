"""Instance selection and managed editor SSH configuration (no lifecycle calls here)."""

from __future__ import annotations

import math
import os
import re
from pathlib import Path
from typing import Any

from gpu.picker import Option, Screen, pick

_SAFE_NAME = re.compile(r"[A-Za-z0-9._-]+\Z")
_HOST_BLOCK = re.compile(r"(?m)^Host vast-(\d+)\n(?:[ \t]+[^\n]*\n)*")
_INCLUDE = b"Include ~/.ssh/vst_config\n"


def repo_name(instance: dict[str, Any]) -> str | None:
    """Use metadata stored on the instance by `gpu up`, never local history."""
    label = instance.get("label")
    if isinstance(label, str) and label != "no-repo" and _SAFE_NAME.fullmatch(label) and label not in {".", ".."}:
        return label
    # Older instances may have a clone command but no label.
    script = instance.get("onstart") or instance.get("onstart_cmd") or ""
    if isinstance(script, str):
        match = re.search(r"git clone [^\n]+ /workspace/([A-Za-z0-9._-]+)(?:\s|$)", script)
        if match and match[1] not in {".", ".."}:
            return match[1]
    return None


def format_uptime(value: Any) -> str:
    """Format Vast duration seconds, truncating partial units; invalid values are unknown."""
    if isinstance(value, bool):
        return "-"
    try:
        seconds = float(value)
    except (TypeError, ValueError, OverflowError):
        return "-"
    if not math.isfinite(seconds) or seconds < 0:
        return "-"
    seconds = int(seconds)
    if seconds < 60:
        return f"{seconds}s"
    minutes = seconds // 60
    if minutes < 60:
        return f"{minutes}m"
    hours, minutes = divmod(minutes, 60)
    if hours < 24:
        return f"{hours}h {minutes:02d}m"
    days, hours = divmod(hours, 24)
    return f"{days}d {hours:02d}h"


def instance_label(item: dict[str, Any]) -> str:
    price = item.get("dph_total", item.get("dph", "-"))
    try:
        price = f"${float(price):.3f}/hr"
    except (ValueError, TypeError):
        price = "-"
    repo = repo_name(item)
    return (f"{item['id']}  {item.get('gpu_name') or '-'}  "
            f"{item.get('actual_status') or '-'}  {price}  "
            f"{format_uptime(item.get('duration'))}  {repo or '-'}")


def select_instance(instances: list[dict[str, Any]], action: str) -> str | None:
    if not instances:
        print("No instances to select.")
        return None
    options = [Option(str(item["id"]), instance_label(item)) for item in instances]
    # Even a single instance must be explicitly selected.
    return pick(Screen(f"Instance to {action}", options=lambda: options,
                       select=lambda value: value, empty="No instances"))


def _ssh_dir() -> Path:
    return Path.home() / ".ssh"


def _managed_path() -> Path:
    return _ssh_dir() / "vst_config"


def _ensure_include() -> None:
    directory = _ssh_dir()
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    config = directory / "config"
    original = config.read_bytes() if config.exists() else b""
    if not any(re.fullmatch(rb"[ \t]*Include[ \t]+~/.ssh/vst_config[ \t\r]*", line, re.IGNORECASE)
               for line in original.splitlines()):
        # The Include must precede all Host/Match blocks for SSH precedence.
        config.write_bytes(_INCLUDE + original)
        os.chmod(config, 0o600)


def _without_entry(content: str, instance_id: str) -> str:
    return _HOST_BLOCK.sub(lambda match: "" if match[1] == instance_id else match[0], content)


def update_host(instance: dict[str, Any]) -> str:
    instance_id = str(instance["id"])
    if not re.fullmatch(r"\d+", instance_id):
        raise ValueError("Invalid instance ID for SSH host")
    host = instance.get("ssh_host")
    port = instance.get("ssh_port")
    if not isinstance(host, str) or not re.fullmatch(r"[A-Za-z0-9.-]+", host):
        raise RuntimeError(f"Instance {instance_id} has no usable SSH host yet")
    if not str(port).isdigit() or not 1 <= int(port) <= 65535:
        raise RuntimeError(f"Instance {instance_id} has no usable SSH port yet")
    _ensure_include()
    path = _managed_path()
    current = path.read_text() if path.exists() else ""
    entry = (f"Host vast-{instance_id}\n  HostName {host}\n  Port {port}\n"
             "  User root\n  IdentityFile ~/.ssh/id_ed25519\n"
             "  StrictHostKeyChecking accept-new\n")
    path.write_text(_without_entry(current, instance_id).rstrip("\n") + "\n" + entry if current else entry)
    os.chmod(path, 0o600)
    return f"vast-{instance_id}"


def remove_host(instance_id: str) -> None:
    """Remove only this instance's managed stanza; never edit user SSH config."""
    path = _managed_path()
    if path.exists():
        path.write_text(_without_entry(path.read_text(), instance_id))
