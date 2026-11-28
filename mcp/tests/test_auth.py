import base64
import hashlib
import json
import secrets
from urllib.parse import parse_qs, urlparse

import httpx
import pytest
from conftest import audit_lines, make_config
from gpu_mcp.server import build
from starlette.testclient import TestClient

PUBLIC = "https://gpu.example.ts.net"
CALLBACK = "https://claude.ai/api/mcp/auth_callback"
MCP_HEADERS = {"accept": "application/json, text/event-stream", "content-type": "application/json",
               "mcp-protocol-version": "2025-06-18"}


class GitHub:
    """A scripted GitHub: which login the next OAuth code belongs to."""

    def __init__(self):
        self.login = "Owner"
        self.requests: list[httpx.Request] = []

    def handler(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        if request.url == "https://github.com/login/oauth/access_token":
            return httpx.Response(200, json={"access_token": "gh-token", "token_type": "bearer"})
        if request.url == "https://api.github.com/user":
            assert request.headers["authorization"] == "Bearer gh-token"
            return httpx.Response(200, json={"login": self.login})
        return httpx.Response(404)


@pytest.fixture
def github():
    return GitHub()


@pytest.fixture
def client(config, github):
    app = build(config, httpx.AsyncClient(transport=httpx.MockTransport(github.handler)))
    with TestClient(app, base_url=PUBLIC) as c:
        yield c


def register(client, redirect=CALLBACK):
    return client.post("/register", json={
        "redirect_uris": [redirect], "client_name": "Claude", "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"],
    })


def login(client, github, user="Owner"):
    """Run the browser part of the flow; return the final redirect response."""
    reg = register(client)
    assert reg.status_code == 201, reg.text
    client_id = reg.json()["client_id"]
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    auth = client.get("/authorize", params={
        "response_type": "code", "client_id": client_id, "redirect_uri": CALLBACK, "state": "claude-state",
        "code_challenge": challenge, "code_challenge_method": "S256", "scope": "gpu",
        "resource": f"{PUBLIC}/mcp",
    }, follow_redirects=False)
    assert auth.status_code == 302, auth.text
    to_github = urlparse(auth.headers["location"])
    assert to_github.netloc == "github.com"
    q = parse_qs(to_github.query)
    assert q["client_id"] == ["test-client-id"]
    assert q["redirect_uri"] == [f"{PUBLIC}/github/callback"]
    github.login = user
    back = client.get("/github/callback", params={"code": "gh-code", "state": q["state"][0]},
                      follow_redirects=False)
    return back, client_id, verifier


def tokens(client, github, user="Owner"):
    back, client_id, verifier = login(client, github, user)
    assert back.status_code == 302, back.text
    target = urlparse(back.headers["location"])
    assert f"{target.scheme}://{target.netloc}{target.path}" == CALLBACK
    q = parse_qs(target.query)
    assert q["state"] == ["claude-state"]
    tok = client.post("/token", data={
        "grant_type": "authorization_code", "code": q["code"][0], "redirect_uri": CALLBACK,
        "client_id": client_id, "code_verifier": verifier, "resource": f"{PUBLIC}/mcp",
    })
    assert tok.status_code == 200, tok.text
    return tok.json(), client_id


def list_jobs(client, token=None):
    headers = dict(MCP_HEADERS)
    if token is not None:
        headers["authorization"] = f"Bearer {token}"
    return client.post("/mcp", headers=headers, json={
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_jobs", "arguments": {}},
    })


def test_full_login_then_tool_call(client, github, fake_gpu, config):
    fake_gpu.reply("job ls", {"jobs": []})
    tok, _ = tokens(client, github)
    assert tok["expires_in"] == 3600 and tok["refresh_token"]
    reply = list_jobs(client, tok["access_token"])
    assert reply.status_code == 200, reply.text
    body = reply.json()["result"]
    assert body["isError"] is False
    assert json.loads(body["content"][0]["text"])["jobs"] == []
    assert fake_gpu.calls()[0]["argv"] == ["--json", "job", "ls", "--owner=owner"]
    lines = audit_lines(config)
    assert [(e["tool"], e["caller"], e["status"]) for e in lines] == [
        ("login", "owner", "ok"), ("list_jobs", "owner", "ok")]
    # The GitHub secret went only to GitHub's token endpoint, never into the audit log.
    assert "test-client-secret" not in (config.data_dir / "audit.jsonl").read_text()
    assert "test-client-secret" in github.requests[0].content.decode()
    # Tokens are stored hashed.
    assert tok["access_token"] not in (config.data_dir / "oauth.json").read_text()


def test_tools_refuse_without_or_with_a_bad_token(client, fake_gpu):
    fake_gpu.reply("job ls", {"jobs": []})
    for token in (None, "not-a-token"):
        reply = list_jobs(client, token)
        assert reply.status_code == 401
        assert "resource_metadata" in reply.headers["www-authenticate"]
    assert fake_gpu.calls() == []


def test_wrong_github_user_gets_no_code(client, github, fake_gpu, config):
    back, _, _ = login(client, github, user="mallory")
    assert back.status_code == 403
    assert "not allowed" in back.text
    assert [(e["tool"], e["caller"], e["status"]) for e in audit_lines(config)] == [
        ("login", "mallory", "refused")]


def test_user_removed_from_config_loses_access_at_once(client, github, fake_gpu, tmp_path):
    fake_gpu.reply("job ls", {"jobs": []})
    tok, client_id = tokens(client, github)
    assert list_jobs(client, tok["access_token"]).status_code == 200
    client.app.state.provider.config = make_config(tmp_path, fake_gpu, allowed_github_users=["someone"])
    assert list_jobs(client, tok["access_token"]).status_code == 401
    refresh = client.post("/token", data={"grant_type": "refresh_token", "refresh_token": tok["refresh_token"],
                                          "client_id": client_id})
    assert refresh.status_code == 400


def test_expired_token_is_refused(client, github, fake_gpu):
    tok, _ = tokens(client, github)
    store = client.app.state.provider.store
    for entry in store.data["access"].values():
        entry["expires_at"] = 1
    assert list_jobs(client, tok["access_token"]).status_code == 401


def test_refresh_rotates_and_old_tokens_die(client, github, fake_gpu):
    fake_gpu.reply("job ls", {"jobs": []})
    tok, client_id = tokens(client, github)
    new = client.post("/token", data={"grant_type": "refresh_token", "refresh_token": tok["refresh_token"],
                                      "client_id": client_id})
    assert new.status_code == 200, new.text
    assert list_jobs(client, new.json()["access_token"]).status_code == 200
    assert list_jobs(client, tok["access_token"]).status_code == 401
    again = client.post("/token", data={"grant_type": "refresh_token", "refresh_token": tok["refresh_token"],
                                        "client_id": client_id})
    assert again.status_code == 400


def test_codes_are_single_use(client, github):
    back, client_id, verifier = login(client, github)
    code = parse_qs(urlparse(back.headers["location"]).query)["code"][0]
    data = {"grant_type": "authorization_code", "code": code, "redirect_uri": CALLBACK,
            "client_id": client_id, "code_verifier": verifier}
    assert client.post("/token", data=data).status_code == 200
    assert client.post("/token", data=data).status_code == 400


def test_registration_only_accepts_configured_redirects(client):
    reply = register(client, redirect="https://evil.example/callback")
    assert reply.status_code == 400
    assert reply.json()["error"] == "invalid_redirect_uri"


def test_loopback_redirects_only_when_enabled(tmp_path, fake_gpu, github):
    for enabled, status in ((False, 400), (True, 201)):
        config = make_config(tmp_path, fake_gpu, allow_loopback_redirects=enabled)
        app = build(config, httpx.AsyncClient(transport=httpx.MockTransport(github.handler)))
        with TestClient(app, base_url=PUBLIC) as c:
            assert register(c, redirect="http://localhost:53682/callback").status_code == status
            assert register(c, redirect="http://evil.example/callback").status_code == 400


def test_github_callback_rejects_unknown_state(client, github):
    reply = client.get("/github/callback", params={"code": "x", "state": "forged"})
    assert reply.status_code == 400
    assert github.requests == []


def test_metadata_points_at_this_server(client):
    meta = client.get("/.well-known/oauth-authorization-server").json()
    assert meta["issuer"].rstrip("/") == PUBLIC
    assert meta["registration_endpoint"] == f"{PUBLIC}/register"
    resource = client.get("/.well-known/oauth-protected-resource/mcp").json()
    assert resource["resource"] == f"{PUBLIC}/mcp"


def test_unknown_host_header_is_refused(client, github, fake_gpu):
    fake_gpu.reply("job ls", {"jobs": []})
    tok, _ = tokens(client, github)
    reply = client.post("/mcp", headers={**MCP_HEADERS, "authorization": f"Bearer {tok['access_token']}",
                                         "host": "evil.example"},
                        json={"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
    assert reply.status_code == 421
    assert fake_gpu.calls() == []


def test_config_never_binds_beyond_loopback(tmp_path, fake_gpu):
    from gpu_mcp.config import ConfigError

    with pytest.raises(ConfigError, match="loopback"):
        make_config(tmp_path, fake_gpu, host="0.0.0.0")
    with pytest.raises(ConfigError, match="https"):
        make_config(tmp_path, fake_gpu, public_url="http://gpu.example.ts.net")
