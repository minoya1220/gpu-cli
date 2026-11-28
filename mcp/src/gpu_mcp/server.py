"""The MCP server: tools over OAuth-protected streamable HTTP on localhost."""

from __future__ import annotations

from typing import Annotated, Any
from urllib.parse import urlparse

import httpx
from pydantic import Field
from starlette.applications import Starlette
from starlette.requests import Request

from mcp.server.auth.middleware.auth_context import get_access_token
from mcp.server.auth.settings import AuthSettings, ClientRegistrationOptions, RevocationOptions
from mcp.server.mcpserver import MCPServer
from mcp.server.transport_security import TransportSecuritySettings

from .audit import Audit
from .auth import SCOPE, GitHubOAuthProvider
from .config import Config
from .tools import GpuTools, ToolError

INSTRUCTIONS = """Runs GPU jobs on Vast.ai through the owner's gpu CLI.
Code comes from a git repository and ref the job's machine fetches through the
server (push your branch first). run_job returns quickly: poll job_status until
the state is succeeded, failed, cancelled, expired or lost, read job_logs while
it runs, then call job_outputs before the job's expires_at, which downloads the
outputs and destroys the instance. Prices are USD per hour; the server caps the
hourly price, the lifetime and the daily spend per caller."""


def _caller() -> str:
    token = get_access_token()
    if token is None or not token.subject:
        raise ToolError("Not authenticated.")
    return token.subject


def build(config: Config, http: httpx.AsyncClient | None = None) -> Starlette:
    """The ASGI app; `app.state` carries `provider` and `tools` for tests."""
    audit = Audit(config.data_dir / "audit.jsonl")
    provider = GitHubOAuthProvider(config, audit, http)
    tools = GpuTools(config, audit)
    auth = AuthSettings(
        issuer_url=config.issuer_url,
        resource_server_url=config.resource_url,
        # load_access_token checks the token's resource itself, so a client
        # that sends no RFC 8707 resource indicator still works.
        validate_token_resource=False,
        required_scopes=[SCOPE],
        client_registration_options=ClientRegistrationOptions(
            enabled=True, valid_scopes=[SCOPE], default_scopes=[SCOPE]
        ),
        revocation_options=RevocationOptions(enabled=True),
    )
    server = MCPServer(
        "gpu",
        instructions=INSTRUCTIONS,
        auth_server_provider=provider,
        auth=auth,
    )

    @server.custom_route("/github/callback", methods=["GET"])
    async def github_callback(request: Request):
        return await provider.github_callback(request)

    @server.tool()
    async def search_offers(
        gpu: Annotated[str, Field(description="GPU model, e.g. 'RTX 4090' or 'H100 SXM'.")],
        num_gpus: Annotated[int, Field(description="GPUs per instance.")] = 1,
        max_price: Annotated[float | None, Field(description="Max total USD/hour; default the server cap.")] = None,
        expire: Annotated[str, Field(description="Planned lifetime such as 30m or 2h, for the estimate.")] = "1h",
        disk: Annotated[int | None, Field(description="Disk GB; default the gpu config.")] = None,
    ) -> dict[str, Any]:
        """Price check: the cheapest matching Vast offer run_job would rent now, its hourly price,
        the estimated cost for `expire`, the server caps and the caller's remaining daily budget.
        Rents nothing."""
        caller = _caller()
        args = {"gpu": gpu, "num_gpus": num_gpus, "max_price": max_price, "expire": expire, "disk": disk}
        return await tools.call("search_offers", caller, args, lambda: tools.search_offers(
            caller, gpu, num_gpus, max_price, expire, disk))

    @server.tool()
    async def run_job(
        source_repo: Annotated[str, Field(description="Git repo: GitHub owner/name, https:// URL or git@host:path.")],
        ref: Annotated[str, Field(description="Commit SHA, branch or tag to run (push it first).")],
        command: Annotated[list[str], Field(description="Program and arguments, e.g. ['python', 'train.py']. "
                                            "Use ['sh', '-c', '...'] for shell syntax.")],
        gpu: Annotated[str, Field(description="GPU model, e.g. 'RTX 4090'.")],
        expire: Annotated[str, Field(description="Lifetime ceiling such as 45m or 2h, boot and output "
                                     "collection included; must exceed 10m. The instance is destroyed then.")],
        num_gpus: int = 1,
        max_price: Annotated[float | None, Field(description="Max total USD/hour; default the server cap.")] = None,
        outputs: Annotated[str, Field(description="Results folder relative to the repo root.")] = "outputs",
        files: Annotated[dict[str, str] | None, Field(
            description="Small extra text files to add to the checkout, relative path -> content.")] = None,
        setup: Annotated[str | None, Field(description="Shell command that installs dependencies "
                                           "(default: uv for uv.lock/pyproject.toml/requirements.txt).")] = None,
        no_setup: Annotated[bool, Field(description="Skip dependency setup.")] = False,
        image: Annotated[str | None, Field(description="Container image override.")] = None,
        disk: Annotated[int | None, Field(description="Disk GB; default the gpu config.")] = None,
    ) -> dict[str, Any]:
        """Start a GPU job on Vast from a git commit. Returns a job_id once the job runs, or a
        launch_id to poll with job_status while the machine boots. The job writes results to
        `outputs`; collect them with job_outputs before expires_at."""
        caller = _caller()
        args = {"source_repo": source_repo, "ref": ref, "command": command, "gpu": gpu,
                "expire": expire, "num_gpus": num_gpus, "max_price": max_price, "outputs": outputs,
                "files": files, "setup": setup, "no_setup": no_setup, "image": image, "disk": disk}
        return await tools.call("run_job", caller, args, lambda: tools.run_job(
            caller, source_repo, ref, command, gpu, expire, num_gpus, max_price, outputs, files,
            setup, no_setup, image, disk))

    @server.tool()
    async def job_status(
        job_id: Annotated[str, Field(description="job_id or launch_id from run_job.")],
    ) -> dict[str, Any]:
        """A job's state (starting, booting, running, succeeded, failed, cancelled, expired or lost),
        exit code, elapsed time, estimated cost and deadline."""
        caller = _caller()
        return await tools.call("job_status", caller, {"job_id": job_id},
                                lambda: tools.job_status(caller, job_id))

    @server.tool()
    async def job_logs(
        job_id: str,
        offset: Annotated[int | None, Field(description="Byte offset to read from (next_offset of an "
                                            "earlier call); omit for the tail.")] = None,
        max_bytes: Annotated[int | None, Field(description="At most this many bytes (server limit applies).")] = None,
    ) -> dict[str, Any]:
        """The job's combined output: the tail by default, or from `offset`, size-limited."""
        caller = _caller()
        return await tools.call("job_logs", caller, {"job_id": job_id, "offset": offset, "max_bytes": max_bytes},
                                lambda: tools.job_logs(caller, job_id, offset, max_bytes))

    @server.tool()
    async def job_outputs(
        job_id: str,
        path: Annotated[str | None, Field(description="One result file to read, as listed.")] = None,
        offset: Annotated[int, Field(description="Byte offset within `path`.")] = 0,
    ) -> dict[str, Any]:
        """Collect a finished job's results: downloads them to the server and destroys the instance,
        then lists the files with sizes, small text files inline. With `path`, reads one file in
        size-limited chunks. Returns state 'collecting' if the download takes longer; call again."""
        caller = _caller()
        return await tools.call("job_outputs", caller, {"job_id": job_id, "path": path, "offset": offset},
                                lambda: tools.job_outputs(caller, job_id, path, offset))

    @server.tool()
    async def cancel_job(job_id: str) -> dict[str, Any]:
        """Stop a job and destroy its instance. Its outputs are lost."""
        caller = _caller()
        return await tools.call("cancel_job", caller, {"job_id": job_id},
                                lambda: tools.cancel_job(caller, job_id))

    @server.tool()
    async def list_jobs() -> dict[str, Any]:
        """The caller's jobs with their states, launches still starting, the caller's estimated
        spend in the last 24 hours and the server caps."""
        caller = _caller()
        return await tools.call("list_jobs", caller, {}, lambda: tools.list_jobs(caller))

    public = urlparse(config.public_url)
    public_host = public.netloc
    security = TransportSecuritySettings(
        enable_dns_rebinding_protection=True,
        allowed_hosts=[public_host, f"127.0.0.1:{config.port}", f"localhost:{config.port}"],
        allowed_origins=[config.public_url, "https://claude.ai", "https://claude.com"],
    )
    app = server.streamable_http_app(
        json_response=True,
        stateless_http=True,
        transport_security=security,
        host=config.host,
    )
    app.state.provider = provider
    app.state.tools = tools
    return app
