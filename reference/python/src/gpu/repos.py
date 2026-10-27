"""Repository choices shared by flags, picker, and startup generation.

None is an unanswered pick; the empty string explicitly means no repository.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

NO_REPO = ""
CLONE_URL = ":clone-url:"


def cache_path(owner: str) -> Path:
    """Account-scoped repository names, never credentials."""
    safe_owner = re.sub(r"[^A-Za-z0-9_-]", "_", owner)
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "gpu" / f"repos-{safe_owner}.json"


def _names(value: object) -> list[str]:
    if not isinstance(value, list):
        return []
    return [name for name in value if isinstance(name, str) and re.fullmatch(r"[A-Za-z0-9._-]+", name) and name not in {".", ".."}]


def load_cached_repos(owner: str) -> list[str] | None:
    try:
        return _names(json.loads(cache_path(owner).read_text()))
    except (OSError, ValueError):
        return None


def save_cached_repos(owner: str, names: list[str]) -> None:
    path = cache_path(owner)
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
            temp = Path(stream.name)
            json.dump(_names(names), stream)
        temp.replace(path)
    finally:
        if temp is not None:
            temp.unlink(missing_ok=True)
ERROR = "Use a bare repo name, owner/name, https://host/path/repo.git, or git@host:path/repo.git (no credentials or query)."


def resolve_repo(repo: str, owner: str) -> tuple[str, str]:
    """Return a validated clone URL and safe workspace basename; never access the network."""
    if repo == NO_REPO:
        return "", "no-repo"
    if any(char.isspace() or not char.isprintable() for char in repo):
        raise ValueError(ERROR)
    part = r"[A-Za-z0-9._-]+"
    path_pattern = rf"{part}(?:/{part})*"
    if repo.startswith("https://"):
        url = urlsplit(repo)
        if (
            not url.hostname or url.username or url.password or url.query or url.fragment
            or not re.fullmatch(r"[A-Za-z0-9.-]+(?::[0-9]+)?", url.netloc)
            or not re.fullmatch(path_pattern, url.path.strip("/"))
        ):
            raise ValueError(ERROR)
        path = url.path.rstrip("/")
        clone_url = repo
    elif repo.startswith("git@"):
        match = re.fullmatch(rf"git@[A-Za-z0-9.-]+:({path_pattern})/?", repo)
        if not match:
            raise ValueError(ERROR)
        path, clone_url = match[1], repo
    else:
        if not re.fullmatch(rf"{part}(?:/{part})?", repo):
            raise ValueError(ERROR)
        path = repo if "/" in repo else f"{owner}/{repo}"
        if not re.fullmatch(rf"{part}/{part}", path):
            raise ValueError(ERROR)
        clone_url = f"https://github.com/{path}"
    if any(segment in {".", ".."} for segment in path.split("/")):
        raise ValueError(ERROR)
    name = path.rsplit("/", 1)[-1]
    name = name.removesuffix(".git")
    if name in {".", ".."} or not re.fullmatch(part, name):
        raise ValueError(ERROR)
    return clone_url, name


def repo_label(repo: str) -> str:
    return "No repository" if repo == NO_REPO else repo
