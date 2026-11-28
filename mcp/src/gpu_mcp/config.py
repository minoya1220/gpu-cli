"""Server configuration: a TOML file for policy, the environment for secrets."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

import tomllib

CLIENT_ID_ENV = "GPU_MCP_GITHUB_CLIENT_ID"
CLIENT_SECRET_ENV = "GPU_MCP_GITHUB_CLIENT_SECRET"
CONFIG_ENV = "GPU_MCP_CONFIG"

# Environment variables never passed to the gpu child process.
SECRET_ENV = (CLIENT_ID_ENV, CLIENT_SECRET_ENV)

# The claude.ai connector callback (and the one Anthropic says it may move to).
DEFAULT_REDIRECT_URIS = (
    "https://claude.ai/api/mcp/auth_callback",
    "https://claude.com/api/mcp/auth_callback",
)

_DURATION = re.compile(r"^(?:([0-9]+)d)?(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?$")


class ConfigError(Exception):
    pass


def parse_duration(text: str) -> int:
    """Seconds in a gpu-style duration such as 30m, 2h or 1h30m."""
    match = _DURATION.match(text) if isinstance(text, str) and text else None
    if not match or not any(match.groups()):
        raise ValueError(f"Use a duration such as 30m, 2h or 1h30m, not {text!r}.")
    seconds = sum(int(n or 0) * unit for n, unit in zip(match.groups(), (86400, 3600, 60, 1)))
    if seconds <= 0:
        raise ValueError("Duration must be positive.")
    return seconds


@dataclass(frozen=True)
class Caps:
    max_price_per_hour: float
    max_expire_seconds: int
    daily_spend_per_caller: float


@dataclass(frozen=True)
class Limits:
    inline_files_max_count: int = 20
    inline_file_max_bytes: int = 64 * 1024
    inline_files_max_total_bytes: int = 256 * 1024
    log_max_bytes: int = 32 * 1024
    output_file_max_bytes: int = 32 * 1024
    outputs_inline_total_bytes: int = 64 * 1024
    command_max_bytes: int = 16 * 1024
    tool_wait_seconds: float = 20.0
    read_timeout_seconds: float = 90.0


@dataclass(frozen=True)
class Config:
    public_url: str
    allowed_github_users: frozenset[str]
    caps: Caps
    limits: Limits = field(default_factory=Limits)
    host: str = "127.0.0.1"
    port: int = 8765
    gpu_binary: str = "gpu"
    data_dir: Path = Path("~/.local/state/gpu-mcp").expanduser()
    redirect_uris: tuple[str, ...] = DEFAULT_REDIRECT_URIS
    allow_loopback_redirects: bool = False
    owner_labels: dict[str, str] = field(default_factory=dict)
    access_token_seconds: int = 3600
    refresh_token_seconds: int = 30 * 86400
    github_client_id: str = field(default="", repr=False)
    github_client_secret: str = field(default="", repr=False)

    @property
    def issuer_url(self) -> str:
        return self.public_url

    @property
    def resource_url(self) -> str:
        return self.public_url + "/mcp"

    @property
    def github_callback_url(self) -> str:
        return self.public_url + "/github/callback"

    def redirect_allowed(self, uri: str) -> bool:
        """A listed redirect URI, or (when enabled) any http loopback one, as native apps
        such as Claude Code use (RFC 8252 section 7.3)."""
        if uri in self.redirect_uris:
            return True
        parsed = urlparse(uri)
        return (self.allow_loopback_redirects and parsed.scheme == "http"
                and parsed.hostname in ("127.0.0.1", "localhost", "::1"))

    def allowed(self, login: str | None) -> bool:
        return bool(login) and login.lower() in self.allowed_github_users

    def owner_label(self, login: str) -> str:
        """The gpu --owner label for a GitHub login (gpu allows 1-32 of [a-z0-9_-])."""
        label = self.owner_labels.get(login.lower(), login.lower())
        if not re.fullmatch(r"[a-z0-9_-]{1,32}", label):
            raise ValueError(
                f"GitHub login {login!r} is not a valid gpu owner label; "
                "map it under [owner_labels] in the server config."
            )
        return label


def _number(table: dict, key: str, default=None, kind=float):
    value = table.get(key, default)
    if value is None:
        raise ConfigError(f"Missing config key {key}.")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ConfigError(f"Config key {key} must be a number.")
    value = kind(value)
    if value <= 0:
        raise ConfigError(f"Config key {key} must be positive.")
    return value


def load(path: str | os.PathLike | None = None, env: dict[str, str] | None = None) -> Config:
    env = os.environ if env is None else env
    path = Path(path or env.get(CONFIG_ENV) or "~/.config/gpu-mcp/config.toml").expanduser()
    try:
        raw = tomllib.loads(path.read_text())
    except FileNotFoundError:
        raise ConfigError(f"No config file at {path}.") from None
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"Invalid TOML in {path}: {exc}") from None
    return from_dict(raw, env)


def from_dict(raw: dict, env: dict[str, str]) -> Config:
    public_url = str(raw.get("public_url", "")).rstrip("/")
    parsed = urlparse(public_url)
    loopback = parsed.hostname in ("127.0.0.1", "localhost")
    if not parsed.hostname or (parsed.scheme != "https" and not loopback) or parsed.path:
        raise ConfigError("public_url must be an https origin such as https://mac.tailnet.ts.net.")
    users = raw.get("allowed_github_users")
    if not isinstance(users, list) or not users or not all(isinstance(u, str) and u for u in users):
        raise ConfigError("allowed_github_users must list at least one GitHub username.")
    caps_raw = raw.get("caps", {})
    try:
        max_expire = parse_duration(caps_raw.get("max_expire", ""))
    except ValueError as exc:
        raise ConfigError(f"caps.max_expire: {exc}") from None
    caps = Caps(
        max_price_per_hour=_number(caps_raw, "max_price_per_hour"),
        max_expire_seconds=max_expire,
        daily_spend_per_caller=_number(caps_raw, "daily_spend_per_caller"),
    )
    limits_raw = raw.get("limits", {})
    defaults = Limits()
    limits = Limits(
        **{
            name: _number(limits_raw, name, getattr(defaults, name), type(getattr(defaults, name)))
            for name in Limits.__dataclass_fields__
        }
    )
    client_id = env.get(CLIENT_ID_ENV, "")
    client_secret = env.get(CLIENT_SECRET_ENV, "")
    if not client_id or not client_secret:
        raise ConfigError(f"Set {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV} in the environment.")
    redirect_uris = raw.get("redirect_uris", list(DEFAULT_REDIRECT_URIS))
    if not isinstance(redirect_uris, list) or not all(isinstance(u, str) for u in redirect_uris):
        raise ConfigError("redirect_uris must be a list of URLs.")
    host = str(raw.get("host", "127.0.0.1"))
    if host not in ("127.0.0.1", "localhost", "::1"):
        raise ConfigError("host must be a loopback address; put Tailscale Funnel in front for remote access.")
    owner_labels = {str(k).lower(): str(v) for k, v in raw.get("owner_labels", {}).items()}
    return Config(
        public_url=public_url,
        allowed_github_users=frozenset(u.lower() for u in users),
        caps=caps,
        limits=limits,
        host=host,
        port=int(_number(raw, "port", 8765, int)),
        gpu_binary=str(raw.get("gpu_binary", "gpu")),
        data_dir=Path(str(raw.get("data_dir", "~/.local/state/gpu-mcp"))).expanduser(),
        redirect_uris=tuple(redirect_uris),
        allow_loopback_redirects=raw.get("allow_loopback_redirects", False) is True,
        owner_labels=owner_labels,
        access_token_seconds=int(_number(raw, "access_token_seconds", 3600, int)),
        refresh_token_seconds=int(_number(raw, "refresh_token_seconds", 30 * 86400, int)),
        github_client_id=client_id,
        github_client_secret=client_secret,
    )
