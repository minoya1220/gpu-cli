# /// script
# requires-python = ">=3.11"
# dependencies = ["modal==1.6.3"]
# ///
"""Private gpu transport. Control ops emit JSON; exec is a byte stream.

No credentials or forwarded environment values belong in argv or create metadata.
The Rust caller supplies bounded subprocess deadlines and redacts diagnostics.
"""
import asyncio
import contextlib
from datetime import datetime, timedelta, timezone
import errno
import io
import json
import os
from pathlib import Path, PurePosixPath
import shlex
import shutil
import signal
import stat
import sys
import tarfile
import tempfile

VOLUME_PREFIX = "gpu-cli-run-"
VOLUME_MOUNT = "/gpu-volume"
# Job outputs live below this Volume directory; gpu-cli state stays outside it.
OUTPUT_DIR = "outputs"
# The supervisor writes and commits this marker when its job starts.
CLAIM_MARKER = ".gpu-cli-claimed"
# A create makes the Volume, then waits up to --timeout (default 600s) for a
# Sandbox. An unclaimed Volume this young may belong to a run still starting.
PENDING_CREATE = timedelta(minutes=15)


def volume_name(name):
    if not (name.startswith(VOLUME_PREFIX) and len(name) > len(VOLUME_PREFIX)
            and all(c.isascii() and (c.isalnum() or c == "-") for c in name)):
        raise ValueError()
    return name


def delete_volume(modal, name):
    name = volume_name(name)
    modal.Volume.objects.delete(name, allow_missing=True)
    # A successful mutation alone is not proof that storage billing stopped.
    try:
        modal.Volume.from_name(name).hydrate()
    except modal.exception.NotFoundError:
        return
    raise CleanupUnverified()


class CleanupUnverified(Exception):
    pass


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
        # COPY uses Modal's Dockerfile builder directly: custom registry images
        # need not contain Python or pip just to install the runtime bootstrap.
        image = image.dockerfile_commands(
            "COPY --from=ghcr.io/astral-sh/uv:0.10.4 /uv /uvx /usr/local/bin/"
        )
        app = await modal.App.lookup.aio("gpu-cli", create_if_missing=True)
        volumes = {}
        if spec.get("volume"):
            # VolumeFS v2: an in-Sandbox sync commits, so checkpoints survive preemption.
            volume = modal.Volume.from_name(volume_name(spec["volume"]), create_if_missing=True, version=2)
            await volume.hydrate.aio()
            volumes[VOLUME_MOUNT] = volume
        return await modal.Sandbox.create.aio(
            *spec["command"], image=image, gpu=spec["gpu"],
            cpu=(spec["cpu"], spec["cpu"]), memory=(spec["memory"], spec["memory"]),
            timeout=spec["timeout"], tags={**spec.get("tags", {}), "gpu-cli": "true"},
            app=app, volumes=volumes,
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


async def stdin_chunk():
    # A thread blocked on terminal input would prevent asyncio.run() shutdown
    # when the remote shell exits. add_reader is cancellable and reads partial
    # input immediately (buffered read(64K) can wait forever on an interactive fd).
    loop = asyncio.get_running_loop()
    ready = loop.create_future()
    fd = sys.stdin.fileno()
    # kqueue accepts regular files but may never report their EOF. epoll
    # rejects them outright. Reading a regular file cannot block for new input.
    if stat.S_ISREG(os.fstat(fd).st_mode):
        return os.read(fd, 65536)
    def readable():
        if not ready.done():
            ready.set_result(None)
    try:
        loop.add_reader(fd, readable)
    except OSError as error:
        # epoll refuses regular files with EPERM; kqueue refuses /dev/null
        # with EINVAL. These descriptors can be read synchronously, unlike a
        # terminal or pipe. Do not hide other errors (e.g. a closed stdin fd).
        if error.errno not in (errno.EPERM, errno.EINVAL, errno.ENOTSUP):
            raise
        return os.read(fd, 65536)
    try:
        await ready
        return os.read(fd, 65536)
    finally:
        loop.remove_reader(fd)


@contextlib.contextmanager
def terminal(pty):
    if not pty or not sys.stdin.isatty():
        yield
        return
    import termios
    import tty
    fd = sys.stdin.fileno()
    previous = termios.tcgetattr(fd)
    def interrupted(number, _frame):
        raise SystemExit(128 + number)
    handlers = {number: signal.signal(number, interrupted)
                for number in (signal.SIGTERM, signal.SIGHUP)}
    try:
        tty.setraw(fd)
        yield
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, previous)
        for number, handler in handlers.items():
            signal.signal(number, handler)


async def execute(sandbox, command, pty=False, source=None, destination=None):
    process = await sandbox.exec.aio(*command, text=False, pty=pty)

    async def upload():
        while True:
            data = source.read(65536) if source is not None else await stdin_chunk()
            if not data:
                break
            process.stdin.write(data)
            await process.stdin.drain.aio()
        process.stdin.write_eof()
        await process.stdin.drain.aio()

    async def copy(reader, writer):
        async for data in reader:
            writer.write(data)
            writer.flush()

    with terminal(pty):
        sender = asyncio.create_task(upload())
        try:
            # Don't await terminal stdin EOF after remote exit. Conversely, an
            # upload failure must not be silently lost if the remote exits first.
            drains = asyncio.gather(copy(process.stdout, destination or sys.stdout.buffer),
                                    copy(process.stderr, sys.stderr.buffer), process.wait.aio())
            done, _ = await asyncio.wait([sender, drains], return_when=asyncio.FIRST_COMPLETED)
            if sender in done:
                await sender
            result = await drains
            if sender.done() and not sender.cancelled():
                sender.result()
            return result[2]
        finally:
            sender.cancel()
            drains.cancel()
            # Consume errors/cancellation from every stream task: otherwise
            # asyncio can print an unredacted SDK traceback during shutdown.
            await asyncio.gather(sender, drains, return_exceptions=True)


def safe_relative(path):
    p = PurePosixPath(path)
    if p.is_absolute() or ".." in p.parts or not p.parts:
        raise ValueError()
    return p


def volume_archive(modal, name, prefix, destination):
    volume = modal.Volume.from_name(volume_name(name))
    prefix = safe_relative(prefix)
    with tarfile.open(fileobj=destination, mode="w|") as archive:
        root = tarfile.TarInfo(str(prefix))
        root.type, root.mode = tarfile.DIRTYPE, 0o700
        archive.addfile(root)
        volume.hydrate()
        try:
            entries = volume.iterdir(f"/{OUTPUT_DIR}", recursive=True)
        except modal.exception.NotFoundError:
            entries = []
        for entry in entries:
            path = safe_relative(PurePosixPath(entry.path.lstrip("/")).relative_to(OUTPUT_DIR))
            info = tarfile.TarInfo(str(prefix / path))
            info.mode = 0o700 if int(entry.type) == 2 else 0o600
            if int(entry.type) == 2:
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
            elif int(entry.type) == 1:
                # Bound RAM independently of checkpoint size; never load an
                # entire model or the entire archive into memory.
                with tempfile.TemporaryFile() as file:
                    for chunk in volume.read_file(entry.path):
                        file.write(chunk)
                    info.size = file.tell()
                    file.seek(0)
                    archive.addfile(info, file)
            else:
                print("Skipping non-regular Volume output entry.", file=sys.stderr)


def extract_copy(archive, destination, recursive):
    # This archive is untrusted remote data. Reject links/special files and
    # multiple top-level names before materializing anything at the target.
    with tempfile.TemporaryDirectory() as temp:
        with tarfile.open(fileobj=archive) as tar:
            members = tar.getmembers()
            roots = set()
            for member in members:
                path = safe_relative(member.name)
                roots.add(path.parts[0])
                if not (member.isfile() or member.isdir()):
                    raise ValueError()
            if len(roots) != 1:
                raise ValueError()
            for member in members:
                target = Path(temp) / member.name
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with tar.extractfile(member) as source, target.open("wb") as output:
                        shutil.copyfileobj(source, output)
                    target.chmod(member.mode & 0o777)
        source = Path(temp) / roots.pop()
        if source.is_dir() and not recursive:
            raise ValueError()
        target = Path(destination)
        if target.is_symlink():
            raise ValueError()
        if target.is_dir():
            target /= source.name
        copy_download(source, target)


def copy_download(source, target):
    # Existing links in a destination tree must not redirect remote members
    # outside that tree. The source lives in our private, validated staging dir.
    if target.is_symlink():
        raise ValueError()
    if source.is_dir():
        target.mkdir(exist_ok=True)
        for child in source.iterdir():
            copy_download(child, target / child.name)
    else:
        shutil.copy2(source, target)


async def copy_path(sandbox, source, destination, recursive):
    if source.startswith(":/") == destination.startswith(":/"):
        raise ValueError()
    with tempfile.TemporaryFile() as file:
        if source.startswith(":/"):
            path = PurePosixPath(source[1:])
            if not path.name:
                raise ValueError()
            command = (f"test {'-e' if recursive else '-f'} {shlex.quote(str(path))} && "
                       f"tar -C {shlex.quote(str(path.parent))} -cf - -- {shlex.quote(path.name)}")
            code = await execute(sandbox, ["bash", "-c", command], source=io.BytesIO(), destination=file)
            if code:
                return code
            file.seek(0)
            extract_copy(file, destination, recursive)
        else:
            path = Path(source)
            if path.is_dir() and not recursive:
                raise ValueError()
            def regular_only(member):
                if not (member.isfile() or member.isdir()):
                    raise ValueError()
                return member
            with tarfile.open(fileobj=file, mode="w", dereference=True) as tar:
                tar.add(path, arcname="item", filter=regular_only)
            file.seek(0)
            # Extract away from the requested destination, then use cp's native
            # file-vs-directory semantics. A shell trap cleans temporary bytes.
            dst = shlex.quote(destination[1:])
            name = shlex.quote(path.name or path.resolve().name)
            command = ("set -eu; t=$(mktemp -d); trap 'rm -rf -- \"$t\"' EXIT; "
                       "tar -xf - --no-same-owner -C \"$t\"; "
                       f"d={dst}; if [ -d \"$d\" ]; then d=\"$d\"/{name}; fi; "
                       "cp -a -- \"$t/item\" \"$d\"")
            return await execute(sandbox, ["bash", "-c", command], source=file)
    return 0


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
        allowed = {"image", "gpu", "cpu", "memory", "timeout", "tags", "command", "env_names", "volume"}
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
        if len(args) < 4:
            raise ValueError()
        sandbox = modal.Sandbox.from_id(args[1])
        if args[2] == "--copy" and len(args) == 6:
            return asyncio.run(copy_path(sandbox, args[4], args[5], args[3] == "recursive"))
        pty = args[2] == "--pty"
        separator = 3 if pty else 2
        if args[separator] != "--" or len(args) <= separator + 1:
            raise ValueError()
        return asyncio.run(execute(sandbox, args[separator + 1:], pty=pty))
    elif op == "volume-get":
        if len(args) != 3:
            raise ValueError()
        volume_archive(modal, args[1], args[2], sys.stdout.buffer)
    elif op == "volume-copy":
        if len(args) != 3:
            raise ValueError()
        with tempfile.TemporaryFile() as file:
            volume_archive(modal, args[1], "outputs", file)
            file.seek(0)
            extract_copy(file, args[2], True)
    elif op == "volume-delete":
        if len(args) != 2:
            raise ValueError()
        delete_volume(modal, args[1])
        emit({"finished": True})
    elif op == "status":
        if len(args) != 2:
            raise ValueError()
        sandbox = modal.Sandbox.from_id(args[1])
        code = sandbox.poll()
        emit({"running": code is None, "returncode": code,
              "volume": sandbox.get_tags().get("volume")})
    elif op == "terminate":
        if len(args) == 3 and args[1] == "--tag" and args[2].startswith("run_id="):
            sandboxes = list(modal.Sandbox.list(tags={"gpu-cli": "true", "run_id": args[2][7:]}))
        elif len(args) == 2:
            sandboxes = [modal.Sandbox.from_id(args[1])]
        else:
            raise ValueError()
        confirmed = True
        for sandbox in sandboxes:
            name = sandbox.get_tags().get("volume")
            sandbox.terminate(wait=True)
            finished = sandbox.poll() is not None
            if finished and name:
                delete_volume(modal, name)
            confirmed = finished and confirmed
        emit({"finished": confirmed})
    elif op == "list":
        if len(args) != 1:
            raise ValueError()
        rows = []
        mounted = set()
        for sandbox in modal.Sandbox.list(tags={"gpu-cli": "true"}):
            if sandbox.poll() is None:
                tags = sandbox.get_tags()
                mounted.add(tags.get("volume"))
                rows.append({"id": sandbox.object_id, "tags": tags})
        cutoff = datetime.now(timezone.utc) - PENDING_CREATE
        def orphan(volume):
            if not (volume.name and volume.name.startswith(VOLUME_PREFIX)) or volume.name in mounted:
                return False
            if volume.info().created_at < cutoff:
                return True
            try:
                return any(entry.path.lstrip("/") == CLAIM_MARKER for entry in volume.listdir("/"))
            except modal.exception.NotFoundError:
                return False
        volumes = [{"name": volume.name} for volume in modal.Volume.objects.list() if orphan(volume)]
        emit({"sandboxes": rows, "volumes": volumes})
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
        "NotFoundError": "Modal sandbox or output Volume was not found.",
        "ImageBuildError": "Modal image build failed. Inspect the build logs in the Modal dashboard; check the registry image and bootstrap steps.",
        "InvalidError": "Modal rejected the request. Check Modal account/billing settings (including a payment method), the GPU type and image.",
        "VersionError": "Modal client version is incompatible. Update gpu.",
        "CleanupUnverified": "Modal Volume deletion is unverified; storage billing may continue. Next: gpu ls --provider modal",
        "FileNotFoundError": "Copy source or destination parent does not exist.",
        "PermissionError": "Copy permission denied. Check source and destination permissions.",
        "ValueError": "Invalid Modal helper or copy request. Check paths and --recursive; only regular files and directories are supported.",
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
        if args and args[0] in ("exec", "volume-get", "volume-copy"):
            # exec stdout is exclusively remote bytes, including on failure.
            print(json.dumps(error), file=sys.stderr)
            return 255
        emit(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
