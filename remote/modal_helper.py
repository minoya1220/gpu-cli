# /// script
# requires-python = ">=3.11"
# dependencies = ["modal==1.6.3"]
# ///
"""Private gpu transport. Control ops emit JSON; exec is a byte stream.

No credentials or forwarded environment values belong in argv or create metadata.
The Rust caller supplies bounded subprocess deadlines and redacts diagnostics.
"""
import asyncio
import json
import sys


class MissingToken(Exception):
    pass


class VersionError(Exception):
    pass


class SchedulingTimeout(Exception):
    pass


async def create(modal, spec, timeout):
    async def allocate():
        image = (modal.Image.from_registry(spec["image"]) if spec.get("image")
                 else modal.Image.debian_slim(python_version="3.13"))
        image = image.pip_install("uv==0.10.4")
        app = await modal.App.lookup.aio("gpu-cli", create_if_missing=True)
        return await modal.Sandbox.create.aio(
            *spec["command"], image=image, gpu=spec["gpu"],
            cpu=(spec["cpu"], spec["cpu"]), memory=(spec["memory"], spec["memory"]),
            timeout=spec["timeout"], tags={**spec.get("tags", {}), "gpu-cli": "true"},
            app=app,
        )

    try:
        # Cancellation reaches the SDK's pending-sandbox terminate handler.
        # Rust also reconciles by run_id if this process/reply is lost.
        return await asyncio.wait_for(allocate(), timeout=timeout)
    except TimeoutError:
        raise SchedulingTimeout() from None


def emit(value):
    print(json.dumps(value), flush=True)


def authenticate(modal):
    # Use Modal's own config resolution (profile, config path and environment),
    # including OAuth, without reading/printing a credential file ourselves.
    from modal.config import config

    keys = ("token_id", "token_secret", "oauth_refresh_token", "oauth_client_id",
            "oauth_client_secret", "oauth_jwt_key")
    if not any(config.get(key) for key in keys):
        raise MissingToken()
    if modal.__version__ != "1.6.3":
        raise VersionError()


async def execute(sandbox, command):
    process = await sandbox.exec.aio(*command, text=False)

    async def upload():
        while data := await asyncio.to_thread(sys.stdin.buffer.read, 65536):
            process.stdin.write(data)
            await process.stdin.drain.aio()
        process.stdin.write_eof()
        await process.stdin.drain.aio()

    async def copy(reader, writer):
        async for data in reader:
            writer.write(data)
            writer.flush()

    await asyncio.gather(upload(), copy(process.stdout, sys.stdout.buffer),
                         copy(process.stderr, sys.stderr.buffer))
    return await process.wait.aio()


def dispatch(modal, args):
    op = args[0]
    if op in ("check", "rates"):
        if len(args) != 1:
            raise ValueError()
        workspace = modal.Workspace.from_context()
        rates = workspace.billing.rates()  # hydrates and authenticates, read-only
        if op == "rates":
            emit({key: str(value) for key, value in rates.items()})
        else:
            from modal.config import config
            emit({"client_version": modal.__version__, "workspace": workspace.name,
                  "environment": config.get("environment") or "main"})
    elif op == "create":
        if len(args) not in (1, 2):
            raise ValueError()
        spec = json.load(sys.stdin)
        # Forwarded secrets are deliberately NOT accepted here. They travel in
        # the shared supervisor's start payload over exec stdin instead.
        allowed = {"image", "gpu", "cpu", "memory", "timeout", "tags", "command", "env_names"}
        if set(spec) - allowed or not spec.get("command"):
            raise ValueError()
        cpu, memory, timeout = spec["cpu"], spec["memory"], spec["timeout"]
        if not (cpu > 0 and memory > 0 and 0 < timeout <= 86400):
            raise ValueError()
        scheduling_timeout = float(args[1]) if len(args) == 2 else 120.0
        if not (0 < scheduling_timeout <= 86400):
            raise ValueError()
        sandbox = asyncio.run(create(modal, spec, scheduling_timeout))
        emit({"id": sandbox.object_id})
    elif op == "exec":
        if len(args) < 4 or args[2] != "--":
            raise ValueError()
        sandbox = modal.Sandbox.from_id(args[1])
        return asyncio.run(execute(sandbox, args[3:]))
    elif op == "status":
        if len(args) != 2:
            raise ValueError()
        code = modal.Sandbox.from_id(args[1]).poll()
        emit({"running": code is None, "returncode": code})
    elif op == "terminate":
        if len(args) == 3 and args[1] == "--tag" and args[2].startswith("run_id="):
            sandboxes = list(modal.Sandbox.list(tags={"gpu-cli": "true", "run_id": args[2][7:]}))
        elif len(args) == 2:
            sandboxes = [modal.Sandbox.from_id(args[1])]
        else:
            raise ValueError()
        confirmed = True
        for sandbox in sandboxes:
            sandbox.terminate(wait=True)
            confirmed = (sandbox.poll() is not None) and confirmed
        emit({"finished": confirmed})
    elif op == "list":
        if len(args) != 1:
            raise ValueError()
        rows = []
        for sandbox in modal.Sandbox.list(tags={"gpu-cli": "true"}):
            if sandbox.poll() is None:
                rows.append({"id": sandbox.object_id, "tags": sandbox.get_tags()})
        emit({"sandboxes": rows})
    else:
        raise ValueError()
    return 0


def failure(exc):
    # Class identity, never human-message parsing. Never emit raw SDK exception
    # text/tracebacks: those can contain credentials or an echoed exec payload.
    messages = {
        "MissingToken": "Modal has no token. Next: uvx modal@1.6.3 token new",
        "AuthError": "Modal authentication failed or expired. Next: uvx modal@1.6.3 token new",
        "ResourceExhaustedError": "Modal has insufficient capacity. Try another GPU or retry later.",
        "SchedulingTimeout": "Modal scheduling exceeded --timeout. Try another GPU or a longer --timeout.",
        "NotFoundError": "Modal sandbox was not found.",
        "ImageBuildError": "Modal image build failed. Check the image and dependency setup.",
        "VersionError": "Modal client version is incompatible. Update gpu.",
        "ValueError": "Invalid Modal helper request.",
    }
    kind = type(exc).__name__
    # SDK exceptions may have internal subclasses; retain their public base kind.
    for cls in type(exc).__mro__:
        if cls.__name__ in messages:
            kind = cls.__name__
            break
    return {"error": {"kind": kind, "message": messages.get(kind, "Modal helper failed. Retry or update gpu.")}}


def main():
    args = sys.argv[1:]
    try:
        import modal
        authenticate(modal)
        return dispatch(modal, args)
    except Exception as exc:
        error = failure(exc)
        if args and args[0] == "exec":
            # exec stdout is exclusively remote bytes, including on failure.
            print(json.dumps(error), file=sys.stderr)
            return 255
        emit(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
