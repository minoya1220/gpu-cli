"""OAuth for the MCP endpoint, with GitHub as the only way to log in.

This server is its own OAuth authorization server (claude.ai registers itself
through dynamic client registration). Its /authorize step sends the browser to
GitHub; the GitHub callback admits only the usernames listed in the config and
then issues this server's own short-lived tokens. GitHub tokens are used once
to read the login and are never stored.

Issued tokens are random strings stored only as SHA-256 hashes. Every request
re-checks the expiry and that the token's GitHub user is still in the config.
"""

from __future__ import annotations

import hashlib
import html
import json
import os
import secrets
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import httpx
from starlette.requests import Request
from starlette.responses import HTMLResponse, RedirectResponse, Response

from mcp.server.auth.provider import (
    AccessToken,
    AuthorizationCode,
    AuthorizationParams,
    AuthorizeError,
    RefreshToken,
    RegistrationError,
    TokenError,
    construct_redirect_uri,
)
from mcp.shared.auth import OAuthClientInformationFull, OAuthToken

from .audit import Audit
from .config import Config

GITHUB_AUTHORIZE = "https://github.com/login/oauth/authorize"
GITHUB_TOKEN = "https://github.com/login/oauth/access_token"
GITHUB_USER = "https://api.github.com/user"

CODE_SECONDS = 300
PENDING_SECONDS = 600
SCOPE = "gpu"


def _hash(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()


class Store:
    """Registered clients and issued tokens, persisted to a 0600 JSON file."""

    def __init__(self, path: Path):
        self.path = path
        self.data: dict[str, dict[str, Any]] = {"clients": {}, "access": {}, "refresh": {}}
        if path.exists():
            loaded = json.loads(path.read_text())
            for key in self.data:
                self.data[key] = dict(loaded.get(key, {}))

    def save(self) -> None:
        now = time.time()
        for kind in ("access", "refresh"):
            self.data[kind] = {
                k: v for k, v in self.data[kind].items() if v.get("expires_at", 0) > now
            }
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        tmp = self.path.with_suffix(".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(self.data, f)
        os.replace(tmp, self.path)


class GitHubOAuthProvider:
    """`OAuthAuthorizationServerProvider` that delegates login to GitHub."""

    def __init__(self, config: Config, audit: Audit, http: httpx.AsyncClient | None = None):
        self.config = config
        self.audit = audit
        self.http = http or httpx.AsyncClient(timeout=20)
        self.store = Store(config.data_dir / "oauth.json")
        self.pending: dict[str, tuple[float, str, AuthorizationParams]] = {}
        self.codes: dict[str, AuthorizationCode] = {}

    # Clients -------------------------------------------------------------

    async def get_client(self, client_id: str) -> OAuthClientInformationFull | None:
        raw = self.store.data["clients"].get(client_id)
        return OAuthClientInformationFull.model_validate(raw) if raw else None

    async def register_client(self, client_info: OAuthClientInformationFull) -> None:
        uris = [str(u) for u in client_info.redirect_uris or []]
        if not uris or not all(self.config.redirect_allowed(u) for u in uris):
            raise RegistrationError(
                "invalid_redirect_uri",
                "This server only accepts redirect URIs listed in its config.",
            )
        if len(self.store.data["clients"]) >= 100:
            raise RegistrationError("invalid_client_metadata", "Too many registered clients.")
        self.store.data["clients"][client_info.client_id] = client_info.model_dump(mode="json")
        self.store.save()

    # Authorization -------------------------------------------------------

    async def authorize(self, client: OAuthClientInformationFull, params: AuthorizationParams) -> str:
        if not self.config.redirect_allowed(str(params.redirect_uri)):
            raise AuthorizeError("invalid_request", "Redirect URI not allowed.")
        self._expire()
        state = secrets.token_urlsafe(32)
        self.pending[state] = (time.time() + PENDING_SECONDS, client.client_id, params)
        query = urlencode(
            {
                "client_id": self.config.github_client_id,
                "redirect_uri": self.config.github_callback_url,
                "state": state,
                "allow_signup": "false",
            }
        )
        return f"{GITHUB_AUTHORIZE}?{query}"

    async def github_callback(self, request: Request) -> Response:
        """Finish the GitHub login, then send the browser back to the MCP client."""
        self._expire()
        state = request.query_params.get("state", "")
        pending = self.pending.pop(state, None)
        if pending is None:
            return _page(400, "This login link expired or was already used. Start again from Claude.")
        _, client_id, params = pending
        code = request.query_params.get("code")
        if not code:
            return _deny(params, "access_denied", "GitHub login was cancelled.")
        try:
            login = await self._github_login(code)
        except Exception:  # noqa: BLE001 - any GitHub failure ends this login
            self.audit.write(caller=None, tool="login", status="error", error="GitHub login failed")
            return _page(502, "GitHub login failed. Try again.")
        if not self.config.allowed(login):
            self.audit.write(caller=login, tool="login", status="refused", error="user not allowed")
            return _page(403, "This GitHub account is not allowed to use this server.")
        auth_code = secrets.token_urlsafe(32)
        self.codes[auth_code] = AuthorizationCode(
            code=auth_code,
            scopes=[SCOPE],
            expires_at=time.time() + CODE_SECONDS,
            client_id=client_id,
            code_challenge=params.code_challenge,
            redirect_uri=params.redirect_uri,
            redirect_uri_provided_explicitly=params.redirect_uri_provided_explicitly,
            resource=params.resource,
            subject=login.lower(),
        )
        self.audit.write(caller=login.lower(), tool="login", status="ok")
        target = construct_redirect_uri(str(params.redirect_uri), code=auth_code, state=params.state)
        return RedirectResponse(target, status_code=302)

    async def _github_login(self, code: str) -> str:
        reply = await self.http.post(
            GITHUB_TOKEN,
            data={
                "client_id": self.config.github_client_id,
                "client_secret": self.config.github_client_secret,
                "code": code,
                "redirect_uri": self.config.github_callback_url,
            },
            headers={"Accept": "application/json"},
        )
        reply.raise_for_status()
        token = reply.json().get("access_token")
        if not token:
            raise ValueError("no GitHub access token")
        user = await self.http.get(
            GITHUB_USER,
            headers={"Authorization": f"Bearer {token}", "Accept": "application/vnd.github+json"},
        )
        user.raise_for_status()
        login = user.json().get("login")
        if not isinstance(login, str) or not login:
            raise ValueError("no GitHub login")
        return login

    async def load_authorization_code(
        self, client: OAuthClientInformationFull, authorization_code: str
    ) -> AuthorizationCode | None:
        self._expire()
        code = self.codes.get(authorization_code)
        return code if code and code.client_id == client.client_id else None

    async def exchange_authorization_code(
        self, client: OAuthClientInformationFull, authorization_code: AuthorizationCode
    ) -> OAuthToken:
        if self.codes.pop(authorization_code.code, None) is None:
            raise TokenError("invalid_grant", "Authorization code already used.")
        return self._issue(client.client_id, authorization_code.subject, authorization_code.resource)

    # Tokens --------------------------------------------------------------

    async def load_refresh_token(self, client: OAuthClientInformationFull, refresh_token: str) -> RefreshToken | None:
        raw = self.store.data["refresh"].get(_hash(refresh_token))
        if not raw or raw["client_id"] != client.client_id or raw["expires_at"] < time.time():
            return None
        if not self.config.allowed(raw.get("subject")):
            return None
        return RefreshToken(token=refresh_token, **raw)

    async def exchange_refresh_token(
        self, client: OAuthClientInformationFull, refresh_token: RefreshToken, scopes: list[str]
    ) -> OAuthToken:
        old = self.store.data["refresh"].pop(_hash(refresh_token.token), None)
        if old is None:
            raise TokenError("invalid_grant", "Refresh token already used.")
        self._revoke_family(old.get("family"))
        return self._issue(client.client_id, refresh_token.subject, refresh_token.resource)

    async def load_access_token(self, token: str) -> AccessToken | None:
        raw = self.store.data["access"].get(_hash(token))
        if not raw or raw["expires_at"] < time.time():
            return None
        if not self.config.allowed(raw.get("subject")):
            return None
        resource = raw.get("resource")
        if resource is not None and resource.rstrip("/") != self.config.resource_url:
            return None
        fields = {k: raw[k] for k in ("client_id", "scopes", "expires_at", "resource", "subject")}
        return AccessToken(token=token, **fields)

    async def revoke_token(self, token: AccessToken | RefreshToken) -> None:
        for kind in ("access", "refresh"):
            raw = self.store.data[kind].get(_hash(token.token))
            if raw:
                self._revoke_family(raw.get("family"))
        self.store.save()

    async def exchange_identity_assertion(self, client, params) -> OAuthToken:  # noqa: ANN001
        raise TokenError("unsupported_grant_type", "Not supported.")

    def _issue(self, client_id: str, subject: str | None, resource: str | None) -> OAuthToken:
        now = time.time()
        access, refresh = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        family = secrets.token_hex(8)
        common = {"client_id": client_id, "scopes": [SCOPE], "resource": resource, "subject": subject}
        self.store.data["access"][_hash(access)] = {
            **common,
            "expires_at": int(now + self.config.access_token_seconds),
            "family": family,
        }
        self.store.data["refresh"][_hash(refresh)] = {
            **common,
            "expires_at": int(now + self.config.refresh_token_seconds),
            "family": family,
        }
        self.store.save()
        return OAuthToken(
            access_token=access,
            expires_in=self.config.access_token_seconds,
            scope=SCOPE,
            refresh_token=refresh,
        )

    def _revoke_family(self, family: str | None) -> None:
        if family is None:
            return
        for kind in ("access", "refresh"):
            self.store.data[kind] = {
                k: v for k, v in self.store.data[kind].items() if v.get("family") != family
            }
        self.store.save()

    def _expire(self) -> None:
        now = time.time()
        self.pending = {k: v for k, v in self.pending.items() if v[0] > now}
        self.codes = {k: v for k, v in self.codes.items() if v.expires_at > now}


def _page(status: int, message: str) -> HTMLResponse:
    body = f"<!doctype html><title>gpu MCP</title><p>{html.escape(message)}</p>"
    return HTMLResponse(body, status_code=status)


def _deny(params: AuthorizationParams, error: str, description: str) -> Response:
    target = construct_redirect_uri(
        str(params.redirect_uri), error=error, error_description=description, state=params.state
    )
    return RedirectResponse(target, status_code=302)
