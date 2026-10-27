from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from gpu import __version__
from gpu.editors import (
    EDITORS,
    after_up_screen,
    check_editor,
    launch_editor,
    select_editor,
)
from gpu.expiry import (
    expiry_summary,
    manage_expiry,
    parse_deadline,
    parse_expire_arguments,
    parse_when,
)
from gpu.instances import (
    format_uptime,
    remove_host,
    repo_name,
    select_instance,
    update_host,
)
from gpu.offers import (
    NO_OFFERS,
    OfferSearch,
    category_label,
    gpu_stats,
    group_by_category,
    known_gpu_models,
    model_label,
    offer_label,
    offer_price,
)
from gpu.picker import Job, PickerCancelled, pick
from gpu.repos import CLONE_URL, NO_REPO, repo_label, resolve_repo, save_cached_repos
from gpu.wizard import MODES, LaunchWizard, Picks

DEFAULTS: dict[str, Any] = {
    "owner": "minoya1220",
    "git_name": "minoya1220",
    "git_email": "minoya1220@gmail.com",
    "disk": 20,
    "vm_disk": 130,
    "reliability": 0.95,
    "container_image": "vastai/base-image:@vastai-automatic-tag",
    "vm_image": "docker.io/vastai/kvm",
}
CONTAINER_ENV = '-p 1111:1111 -p 6006:6006 -p 8080:8080 -p 8384:8384 -e OPEN_BUTTON_PORT=1111 -e OPEN_BUTTON_TOKEN=1 -e JUPYTER_DIR=/ -e DATA_DIRECTORY=/workspace/ -e PORTAL_CONFIG="localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|localhost:8080:8080:/terminals/1:Jupyter Terminal|localhost:8384:18384:/:Syncthing|localhost:6006:16006:/:Tensorboard"'


def config_path() -> Path:
    return (
        Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
        / "gpu"
        / "config.json"
    )


def load_config() -> dict[str, Any]:
    path = config_path()
    data = json.loads(path.read_text()) if path.exists() else {}
    if not isinstance(data, dict):
        raise TypeError(f"Config {path} must contain a JSON object")
    return {**DEFAULTS, **data}


# Key names Vast treats as secrets (mirrors vastai's support-bundle SENSITIVE_KEY_RE).
SECRET_KEY_RE = re.compile(
    r"api[_-]?key|token|secret|password|passwd|authorization", re.IGNORECASE
)
SECRET_ASSIGNMENT_RE = re.compile(
    r"""(?i)([\w-]*(?:api[_-]?key|token|secret|password|passwd|authorization)[\w-]*["']?\s*[:=]\s*["']?(?:Bearer\s+)?)[^\s,;'"}\]]+"""
)
REDACTED = "REDACTED"


def redact(value: Any) -> Any:
    """Return value with every secret-looking field or assignment replaced by REDACTED."""
    if isinstance(value, dict):
        return {
            key: REDACTED if SECRET_KEY_RE.search(str(key)) else redact(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        # Vast reports env vars as [name, value] pairs, e.g. extra_env.
        if len(value) == 2 and isinstance(value[0], str) and SECRET_KEY_RE.search(value[0]):
            return [value[0], REDACTED]
        return [redact(item) for item in value]
    if isinstance(value, str):
        return SECRET_ASSIGNMENT_RE.sub(rf"\1{REDACTED}", value)
    return value


def run(argv: list[str], *, timeout: float | None = None) -> str:
    """Run a CLI and return its stdout. It never reads the terminal, which the picker owns."""
    try:
        result = subprocess.run(
            argv, check=True, text=True, capture_output=True, stdin=subprocess.DEVNULL,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"Timed out running {argv[0]}") from exc
    except FileNotFoundError as exc:
        raise RuntimeError(f"Required command not found: {argv[0]}") from exc
    except subprocess.CalledProcessError as exc:
        detail = redact((exc.stderr or exc.stdout or "").strip())
        raise RuntimeError(detail or f"Command failed: {argv[0]}") from exc
    return result.stdout.strip()


class Vast:
    """Small adapter around the authenticated Vast CLI; injectable for tests."""

    def __init__(self, command=None):
        self.command = run if command is None else command

    def json(self, *args: str) -> Any:
        return json.loads(self.command(["vastai", *args, "--raw"]))

    def offers(self, query: str, *, bid: bool = False) -> list[dict[str, Any]]:
        order = "min_bid" if bid else "dph"
        args = [
            "search",
            "offers",
            query,
            "--no-default",
            "--limit",
            "1000",
            "--order",
            order,
        ]
        if bid:
            args.append("--interruptible")
        return self.json(*args)

    def request(
        self, offer: dict[str, Any], args: argparse.Namespace, cfg: dict[str, Any]
    ) -> list[str]:
        mode = args.mode
        repo = args.repo
        script = startup_script(repo, mode, cfg)
        hourly = float(
            offer.get("min_bid")
            if args.interruptible
            else offer.get("dph_total", offer.get("dph", 0))
        )
        default_disk = cfg["vm_disk"] if mode == "vm" else cfg["disk"]
        command = [
            "vastai",
            "create",
            "instance",
            str(offer["id"]),
            "--image",
            cfg[f"{mode}_image"],
            "--disk",
            str(args.disk or default_disk),
            "--onstart-cmd",
            script,
            "--ssh",
            "--direct",
            "--label",
            resolve_repo(repo, str(cfg["owner"]))[1],
        ]
        if mode == "container":
            command.extend(["--env", CONTAINER_ENV])
        if args.interruptible:
            command.extend(["--bid_price", str(hourly)])
        command.append("--raw")
        return command


def startup_script(repo: str, mode: str, cfg: dict[str, Any]) -> str:
    if mode not in {"container", "vm"}:
        raise ValueError(f"Unsupported mode: {mode}")
    clone_url, name = resolve_repo(repo, str(cfg["owner"]))
    target = f"/workspace/{name}"
    lines = [
        "#!/bin/bash",
        "source /etc/environment 2>/dev/null || true",
        "export GH_TOKEN",
    ]
    if mode == "container":
        lines.extend(
            [
                "apt update && apt install -y gh",
                "touch /root/.no_auto_tmux",
                "rm -rf /venv",
            ]
        )
    else:
        lines.extend(
            [
                "if ! command -v ncu >/dev/null 2>&1; then cd /tmp; wget -q https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/cuda-keyring_1.1-1_all.deb; dpkg -i cuda-keyring_1.1-1_all.deb; apt update; apt install -y gh git curl build-essential nsight-compute-2025.4.0 cuda-nvcc-12-8; fi",
                "grep -q nsight-compute /root/.bashrc || cat >> /root/.bashrc <<'EOF'\nexport PATH=$PATH:/opt/nvidia/nsight-compute/2025.4.0:/usr/local/cuda-12.8/bin\nexport LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}:/usr/local/cuda-12.8/lib64\nexport PATH=\"/root/.local/bin:$PATH\"\nEOF",
                "export PATH=$PATH:/opt/nvidia/nsight-compute/2025.4.0:/usr/local/cuda-12.8/bin",
                "command -v uv >/dev/null 2>&1 || (curl -LsSf https://astral.sh/uv/install.sh | sh)",
            ]
        )
    lines.extend(
        [
            "mkdir -p /workspace",
            f"git config --global user.name {shlex.quote(cfg['git_name'])}",
            f"git config --global user.email {shlex.quote(cfg['git_email'])}",
            'if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git; else echo "WARNING: GH_TOKEN is not set in Vast account environment; private GitHub repos may not clone." >&2; fi',
        ]
    )
    if repo != NO_REPO:
        lines.extend([
            f"[ -d {shlex.quote(target)} ] || git clone {shlex.quote(clone_url)} {shlex.quote(target)}",
            f"if [ -f {shlex.quote(target + '/pyproject.toml')} ]; then uv sync --directory {shlex.quote(target)}; fi",
        ])
    if mode == "container":
        lines.extend(
            [
                'sed -i \'s|^cd \\${WORKSPACE}$|[ "$PWD" = "$HOME" ] \\&\\& cd "${WORKSPACE:-/workspace}"|\' /root/.bashrc',
                "touch /root/.works",
                "entrypoint.sh",
            ]
        )
    else:
        lines.append("touch /root/.works")
    return "\n".join(lines) + "\n"


def sync_repos(owner: str) -> list[str]:
    names = github_repos(owner)
    save_cached_repos(owner, names)
    return names


def github_repos(owner: str) -> list[str]:
    """Repository names under `owner`. Blocks on the GitHub CLI."""
    repos = json.loads(run(["gh", "repo", "list", owner, "--json", "name", "--limit", "1000"]))
    return [repo["name"] for repo in repos]


def parse_args() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gpu", description="Launch GPU workspaces on Vast.ai"
    )
    parser.add_argument("-v", "-V", "--version", action="version", version=__version__)
    sub = parser.add_subparsers(dest="command", required=True)
    up = sub.add_parser("up", help="Launch a GPU workspace with an optional repository")
    repository = up.add_mutually_exclusive_group()
    repository.add_argument("--repo", help="Clone HTTPS/git@ URL, GitHub owner/name, or a name under configured owner")
    repository.add_argument("--no-repo", dest="repo", action="store_const", const=NO_REPO,
                            help="Launch without cloning a repository")
    up.add_argument("--mode", choices=MODES)
    up.add_argument("--gpu")
    up.add_argument("--num-gpus", type=int, default=1)
    up.add_argument("--max-price", type=float)
    up.add_argument("--interruptible", action="store_true")
    up.add_argument("--offer", type=int)
    up.add_argument("--disk", type=int)
    up.add_argument("--yes", action="store_true")
    up.add_argument("--dry-run", action="store_true")
    up.add_argument("--expire", metavar="DURATION", help="Auto-destroy after e.g. 2h or 1h30m (includes boot time)")
    up.add_argument("--reliability", type=float)
    up.add_argument(
        "--unverified",
        action="store_true",
        help="include verified, unverified, and deverified hosts",
    )
    ls = sub.add_parser("ls", help="List rented instances")
    ls.add_argument("--json", action="store_true", help="print raw JSON for scripts")
    ssh = sub.add_parser("ssh", help="Print or execute the SSH command")
    ssh.add_argument("id", nargs="?")
    cursor = sub.add_parser("cursor", help="Open an instance's repository in Cursor over SSH")
    cursor.add_argument("id", nargs="?")
    edit = sub.add_parser("edit", help="Open an instance workspace in an editor over SSH")
    edit.add_argument("id", nargs="?")
    edit.add_argument("--editor", choices=EDITORS)
    edit.add_argument("--nvim-config", help="Neovim config git URL or local directory")
    ssh.add_argument("--exec", action="store_true")
    stop = sub.add_parser("stop", help="Stop an instance")
    stop.add_argument("id", nargs="?")
    down = sub.add_parser("down", help="Destroy an instance")
    down.add_argument("id", nargs="?")
    down.add_argument("--yes", action="store_true")
    expire = sub.add_parser(
        "expire", help="Schedule instance-side destruction, show or cancel it",
        usage="gpu expire [ID] WHEN|cancel|show",
        description="Auto-destroy (disk deleted). WHEN: 2h, 1h30m, 18:30, 6:30PM, 6pm, or ISO datetime. "
                    "Clock times use the next local occurrence; bare numbers are instance IDs.",
        epilog="Examples: gpu expire 2h; gpu expire 12345678 6:30pm; gpu expire 12345678 cancel; gpu expire show",
    )
    expire.add_argument("expiry_args", nargs="*", metavar="ID/WHEN", help="Optional digits-only ID, then deadline, cancel, or show")
    return parser


def history_path() -> Path:
    return (
        Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
        / "gpu"
        / "history.json"
    )


def load_history() -> dict[str, list[str]]:
    try:
        data = json.loads(history_path().read_text())
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def recent_picks(step: str) -> list[str]:
    """Earlier picks for `step`, most recent first, with duplicates."""
    entries = load_history().get(step, [])
    return [item for item in entries if isinstance(item, str)] if isinstance(entries, list) else []


def record_picks(picks: dict[str, str]) -> None:
    history = load_history()
    for step, value in picks.items():
        entries = history.get(step, [])
        if not isinstance(entries, list):
            entries = []
        history[step] = [value, *entries][:100]
    path = history_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(history, indent=2) + "\n")


def by_frequent_recency(options: list[Any], history: list[str]) -> list[Any]:
    """Options picked before, ranked by recency-weighted frequency."""
    scored = [
        (sum(0.85**age for age, item in enumerate(history) if item == value), value)
        for value in options
    ]
    ranked = sorted((pair for pair in scored if pair[0]), key=lambda pair: pair[0], reverse=True)
    return [value for _, value in ranked]


def choose(
    options: list[Any],
    prompt: str,
    *,
    step: str | None = None,
    labels: list[str] | None = None,
    filter_across: tuple[list[Any], list[str]] | None = None,
) -> Any:
    """Line-based picker used when stdin or stdout is not a terminal."""
    if not options:
        raise RuntimeError(f"No choices available for {prompt.lower()}")
    labels = labels or [str(value) for value in options]
    history = recent_picks(step) if step else []
    recent = by_frequent_recency(options, history)[:5]
    page_size, page, filter_text = 12, 0, ""
    while True:
        filtered = [
            i
            for i, label in enumerate(labels)
            if filter_text.casefold() in label.casefold()
        ]
        if filter_text and len(filtered) == 1:
            print(f"Unique match: {labels[filtered[0]]}")
            return options[filtered[0]]
        pages = max(1, (len(filtered) + page_size - 1) // page_size)
        page = min(page, pages - 1)
        if recent and not filter_text:
            print("Recent/frequent:")
            for i, option in enumerate(recent, 1):
                print(f"  {i}. {labels[options.index(option)]}")
        print(
            f"{prompt} (page {page + 1}/{pages})"
            + (f" filter={filter_text!r}" if filter_text else "")
        )
        for number, index in enumerate(
            filtered[page * page_size : (page + 1) * page_size], 1
        ):
            print(f"{number}. {labels[index]}")
        latest = next((item for item in history if item in options), None)
        default = (
            options.index(latest)
            if latest is not None
            else (filtered[0] if filtered else None)
        )
        hint = (
            "Enter default"
            if default is not None and (not filter_text or default in filtered)
            else ""
        )
        prompt_text = (
            "Select number, text to filter, n/p page"
            + (f", {hint}" if hint else "")
            + ": "
        )
        try:
            answer = input(prompt_text).strip()
        except EOFError as exc:
            raise RuntimeError(
                f"Input unavailable while choosing {prompt.lower()}; pass the corresponding flag"
            ) from exc
        if not answer:
            if default is not None and (not filter_text or default in filtered):
                return options[default]
            print("No default in this filter; choose a listed number.")
        elif answer.casefold() == "n" and page + 1 < pages:
            page += 1
        elif answer.casefold() == "p" and page > 0:
            page -= 1
        elif answer.isdigit() and 1 <= int(answer) <= min(
            page_size, len(filtered) - page * page_size
        ):
            return options[filtered[page * page_size + int(answer) - 1]]
        elif not answer.isdigit():
            if filter_across is not None:
                all_options, all_labels = filter_across
                matches = [
                    (option, label)
                    for option, label in zip(all_options, all_labels)
                    if answer.casefold() in label.casefold()
                ]
                if len(matches) == 1:
                    return matches[0][0]
                if matches:
                    return choose(
                        [option for option, _ in matches],
                        "GPU model filter",
                        step="gpu",
                        labels=[label for _, label in matches],
                    )
                print("No GPU models match that filter.")
            else:
                filter_text = answer
                page = 0
        else:
            print("Choose a listed number, filter text, n, or p.")


def choose_gpu(models: list[str], stats: dict[str, tuple[float, int]]) -> str:
    """Line-based GPU prompt: recent models, categories, or a typed model filter."""
    history = recent_picks("gpu")
    recent = by_frequent_recency(models, history)[:5]
    categories = group_by_category(models)
    names = list(categories)
    all_labels = [model_label(model, stats) for model in models]
    while True:
        if recent:
            print("Recent/frequent GPU models:")
            for number, model in enumerate(recent, 1):
                print(f"{number}. {model_label(model, stats)}")
        for number, name in enumerate(names, len(recent) + 1):
            print(f"{number}. {category_label(name, categories[name], stats)}")
        print("Type a model substring to filter all GPUs, or choose a category.")
        has_default = bool(history and history[0] in models)
        try:
            answer = input("GPU [Enter = most recent]: " if has_default else "GPU: ").strip()
        except EOFError as exc:
            raise RuntimeError(
                "Input unavailable while choosing GPU model; pass --gpu"
            ) from exc
        if not answer and has_default:
            return history[0]
        if answer.isdigit() and 1 <= int(answer) <= len(recent):
            return recent[int(answer) - 1]
        if answer.isdigit() and len(recent) < int(answer) <= len(recent) + len(names):
            name = names[int(answer) - len(recent) - 1]
            members = categories[name]
            return choose(
                members,
                name,
                step="gpu",
                labels=[model_label(model, stats) for model in members],
                filter_across=(models, all_labels),
            )
        matches = (
            [m for m in models if answer.casefold() in m.casefold()] if answer else []
        )
        if len(matches) == 1:
            return matches[0]
        if matches:
            return choose(
                matches,
                "GPU model filter",
                step="gpu",
                labels=[model_label(model, stats) for model in matches],
            )
        print("Choose a listed category or type a matching model substring.")


def wait_for_lifecycle(instance_id: str, action: str, *, timeout: float = 60) -> None:
    """Verify provider state, not just acceptance of a lifecycle request."""
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            detail = "still listed" if action == "destroy" else "stopped status not observed"
            raise RuntimeError(f"Instance {instance_id}: {action} not verified within {timeout:g}s ({detail})")
        try:
            live = list_instances(timeout=remaining)
        except (RuntimeError, OSError) as exc:
            raise RuntimeError(f"Instance {instance_id}: {action} could not be verified: {exc}") from exc
        instance = next((item for item in live if str(item.get("id")) == instance_id), None)
        if action == "destroy" and instance is None:
            return
        if action == "stop" and instance is not None and instance.get("actual_status") == "stopped":
            return
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(2, remaining))


def confirm(prompt: str) -> bool:
    try:
        return input(prompt).strip().lower() == "yes"
    except EOFError as exc:
        raise PickerCancelled(1) from exc


def interactive_terminal() -> bool:
    return sys.stdin.isatty() and sys.stdout.isatty()


def validate_launch_args(args: argparse.Namespace) -> None:
    if args.num_gpus < 1:
        raise ValueError("--num-gpus must be at least 1")
    if args.disk is not None and args.disk < 1:
        raise ValueError("--disk must be at least 1")
    if args.max_price is not None and args.max_price <= 0:
        raise ValueError("--max-price must be greater than 0")
    if args.reliability is not None and not 0 <= args.reliability <= 1:
        raise ValueError("--reliability must be between 0 and 1")
    if args.offer is not None and args.offer < 1:
        raise ValueError("--offer must be a positive ID")


def prompt_for_picks(
    picks: Picks, search: OfferSearch, owner: str
) -> tuple[Picks, list[dict[str, Any]]]:
    """Line-based prompts for the missing picks; also returns the chosen GPU's offers."""
    repo = picks.repo
    if repo is None:
        try:
            repos = github_repos(owner)
        except RuntimeError as exc:
            print(f"Could not list repositories: {exc}", file=sys.stderr)
            repos = []
        choices = [NO_REPO, CLONE_URL, *repos]
        repo = choose(
            choices, "Repository",
            labels=["Clone from URL..." if r == CLONE_URL else repo_label(r) for r in choices],
        )
        if repo == CLONE_URL:
            try:
                repo = input("Repository URL or owner/name: ").strip()
            except EOFError as exc:
                raise RuntimeError("Pass --repo URL or --no-repo") from exc
            if not repo:
                raise ValueError("Repository URL is required; use --no-repo instead")
        resolve_repo(repo, owner)
    mode = picks.mode or choose(MODES, "Mode", step="mode")
    # Without a GPU, search every model so the prompt can show real prices.
    offers = search.offers(mode, picks.gpu)
    if not offers:
        raise RuntimeError(NO_OFFERS)
    gpu = picks.gpu
    if gpu is None:
        gpu = choose_gpu(sorted({offer["gpu_name"] for offer in offers}), gpu_stats(offers))
        offers = [offer for offer in offers if offer["gpu_name"] == gpu]
    return Picks(repo, mode, gpu), offers


def select_offer(offers: list[dict[str, Any]], args: argparse.Namespace) -> dict[str, Any]:
    """Offer chosen by --offer, --yes (cheapest), or a line-based prompt."""
    if not offers:
        raise RuntimeError(NO_OFFERS)
    displayed = offers[:10]
    labels = [offer_label(offer) for offer in displayed]
    if args.offer is not None or args.yes:
        print("Matching offers (hourly, reliability, location, verification):")
        for number, label in enumerate(labels, 1):
            print(f"{number}. {label}")
    if args.offer is not None:
        offer = next((o for o in offers if o["id"] == args.offer), None)
        if offer is None:
            raise RuntimeError(f"Offer {args.offer} does not match the current search")
        return offer
    if args.yes:
        return displayed[0]
    return choose(displayed, "Offer", labels=labels)


def _launch(args: argparse.Namespace, vast: Vast, cfg: dict[str, Any]) -> int:
    validate_launch_args(args)
    owner = cfg["owner"]
    if args.repo is not None:
        resolve_repo(args.repo, owner)
    search = OfferSearch(vast, args, cfg)
    picks = Picks(repo=args.repo, mode=args.mode, gpu=args.gpu)
    if interactive_terminal():
        wizard = LaunchWizard(
            repos=Job(lambda: sync_repos(owner)),
            search=search,
            models=known_gpu_models(),
            recent_gpus=list(dict.fromkeys(recent_picks("gpu"))),
            choose_offer=not args.yes and args.offer is None,
            owner=owner,
        )
        picks = wizard.run(picks)
        offer = picks.offer or select_offer(search.offers(picks.mode, picks.gpu), args)
    else:
        picks, offers = prompt_for_picks(picks, search, owner)
        offer = select_offer(offers, args)
    chosen = argparse.Namespace(**{**vars(args), "repo": picks.repo, "mode": picks.mode, "gpu": picks.gpu})
    request = vast.request(offer, chosen, cfg)
    if args.dry_run:
        print("DRY RUN: no instance created")
        print(json.dumps(request, indent=2))
        if getattr(args, "expire", None):
            print(f"Auto-destroy requested after {args.expire}; no timer installed (dry run).")
        return 0
    print(
        f"Launch {picks.mode} for {repo_label(picks.repo)} on offer {offer['id']} at ${offer_price(offer):.3f}/hr?"
    )
    if not args.yes and not confirm("Type 'yes' to create this paid instance: "):
        print("Cancelled.")
        return 0
    response = run(request)
    print(created_summary(response))
    record_picks({"mode": picks.mode, "gpu": picks.gpu})
    try:
        instance_id = json.loads(response).get("new_contract")
    except (ValueError, AttributeError):
        instance_id = None
    if getattr(args, "expire", None):
        if instance_id is None:
            raise RuntimeError("Instance created but no ID returned; expiry NOT armed. Run gpu ls, then gpu expire ID WHEN.")
        try:
            expire_instance(str(instance_id), "arm", args.expiry_deadline)
        except (RuntimeError, OSError, ValueError) as exc:
            raise RuntimeError(f"Instance {instance_id} created but expiry NOT verified; billing continues. "
                               f"Run gpu expire {instance_id} show or gpu down {instance_id}. {exc}") from exc
    if instance_id is not None and interactive_terminal():
        editor = pick(after_up_screen(last_editor()))
        if editor != "none":
            edit_instance(str(instance_id), editor, nvim_config=cfg.get("editor", {}).get("nvim_config"))
    return 0


def created_summary(output: str) -> str:
    """Summarize `vastai create instance --raw` without echoing its secrets (instance_api_key)."""
    try:
        response = json.loads(output)
    except ValueError:
        response = None
    if not isinstance(response, dict):
        return "Vast accepted the create request but its response was unreadable; run `gpu ls` to find the instance."
    if not response.get("success"):
        reason = response.get("msg") or response.get("error") or "unknown error"
        raise RuntimeError(f"Vast did not create the instance: {redact(str(reason))}")
    instance = response.get("new_contract")
    if instance is None:
        return "Created instance. Next: `gpu ls` to find it."
    return f"Created instance {instance}. Next: `gpu ls` or `gpu ssh {instance}`."


def render_instances(instances: Any) -> None:
    if not instances:
        print("No instances.")
        return
    columns = [
        ("ID", "id"),
        ("Label", "label"),
        ("GPU", "gpu_name"),
        ("Status", "actual_status"),
        ("$/hr", "dph_total"),
        ("Uptime", "duration"),
    ]
    rows = [
        [format_uptime(item.get(key)) if key == "duration" else str(item.get(key, "-"))
         for _, key in columns]
        for item in instances
    ]
    widths = [
        max(len(title), *(len(row[i]) for row in rows))
        for i, (title, _) in enumerate(columns)
    ]
    print("  ".join(title.ljust(widths[i]) for i, (title, _) in enumerate(columns)))
    print("  ".join("-" * width for width in widths))
    for row in rows:
        print("  ".join(value.ljust(widths[i]) for i, value in enumerate(row)))


def list_instances(*, timeout: float | None = None) -> list[dict[str, Any]]:
    return json.loads(run(["vastai", "show", "instances", "--raw"], timeout=timeout))


def selected_id(args: argparse.Namespace) -> str | None:
    if args.id is not None:
        return args.id
    return select_instance(list_instances(), args.command)


def last_editor() -> str | None:
    return next((item for item in recent_picks("editor") if item in EDITORS), None)


def edit_instance(instance_id: str, editor: str | None = None, *,
                  nvim_config: str | None = None, timeout: float = 120) -> None:
    editor = editor or last_editor()
    if editor is None:
        if not interactive_terminal():
            raise RuntimeError("No previous editor; specify --editor cursor|code|zed|nvim")
        editor = select_editor()
    check_editor(editor)
    alias, directory = wait_for_ssh(instance_id, timeout=timeout)
    launch_editor(editor, alias, directory, nvim_config)
    record_picks({"editor": editor})
    print(f"Opened {EDITORS[editor]} at {directory} on {alias}.")


def expire_instance(instance_id: str, action: str, deadline=None) -> None:
    alias, _ = wait_for_ssh(instance_id)
    result = manage_expiry(alias, instance_id, action, deadline)
    print(expiry_summary(instance_id, result))
    if result["status"] == "inactive":
        raise RuntimeError(f"Timer is NOT running; re-arm with gpu expire {instance_id} WHEN.")


def cursor_instance(instance_id: str, *, timeout: float = 120) -> None:
    """Compatibility alias for gpu edit --editor cursor."""
    edit_instance(instance_id, "cursor", timeout=timeout)


def wait_for_ssh(instance_id: str, *, timeout: float = 120) -> tuple[str, str]:
    """Poll Vast and SSH until ready; shared by all editor launchers."""
    deadline = time.monotonic() + timeout
    print(f"Waiting for instance {instance_id} and SSH (up to {timeout:g}s)...", file=sys.stderr)
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f"Timed out waiting for instance {instance_id} to run and answer SSH")
        try:
            live = list_instances(timeout=remaining)
        except RuntimeError as exc:
            if "Timed out" in str(exc):
                raise RuntimeError(f"Timed out waiting for instance {instance_id} to run and answer SSH") from exc
            raise
        instance = next((item for item in live if str(item.get("id")) == instance_id), None)
        if instance is None:
            raise RuntimeError(f"Instance {instance_id} not found")
        if instance.get("actual_status") == "running":
            try:
                alias = update_host(instance)
            except RuntimeError:  # SSH endpoint not provisioned yet
                pass
            else:
                remaining = deadline - time.monotonic()
                if remaining > 0:
                    try:
                        probe = subprocess.run(
                            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5",
                             alias, "true"], stdin=subprocess.DEVNULL,
                            capture_output=True, timeout=min(6, remaining), check=False,
                        )
                        if probe.returncode == 0:
                            repo = repo_name(instance)
                            directory = f"/workspace/{repo}" if repo else "/workspace"
                            if not repo:
                                print("No repository recorded on instance; opening /workspace.")
                            return alias, directory
                    except subprocess.TimeoutExpired:
                        pass
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f"Timed out waiting for instance {instance_id} to run and answer SSH")
        time.sleep(min(3, remaining))


def main(argv: list[str] | None = None) -> int:
    parser = parse_args()
    args = parser.parse_args(argv)
    try:
        # Validate expiry syntax before making dependency calls or creating an instance.
        if args.command == "expire":
            try:
                args.id, when = parse_expire_arguments(args.expiry_args)
                action = when if when in {"show", "cancel"} else "arm"
                deadline = parse_when(when) if action == "arm" else None
            except ValueError as exc:
                parser.error(str(exc))
        if args.command == "up":
            try:
                args.expiry_deadline = parse_deadline(duration=args.expire) if args.expire else None
            except ValueError as exc:
                parser.error(str(exc))
        cfg = load_config()
        vast = Vast()
        if args.command == "up":
            return _launch(args, vast, cfg)
        if args.command == "ls":
            raw = run(["vastai", "show", "instances", "--raw"])
            if args.json:
                print(json.dumps(redact(json.loads(raw)), indent=1))
            else:
                render_instances(json.loads(raw))
            return 0
        if args.command in {"ssh", "stop", "down", "cursor", "edit", "expire"}:
            instance_id = selected_id(args)
            if instance_id is None:
                return 0
        if args.command == "expire":
            expire_instance(instance_id, action, deadline)
            return 0
        if args.command == "edit":
            edit_instance(instance_id, args.editor,
                          nvim_config=args.nvim_config if args.nvim_config is not None
                          else cfg.get("editor", {}).get("nvim_config"))
            return 0
        if args.command == "cursor":
            cursor_instance(instance_id)
            return 0
        if args.command == "ssh":
            cmd = ["vastai", "ssh-url", instance_id]
            url = run(cmd)
            if args.exec:
                os.execvp("ssh", ["ssh", *shlex.split(url)])
            print(redact(url))
            return 0
        if args.command in {"stop", "down"}:
            if (
                args.command == "down"
                and not args.yes
                and not confirm(f"Destroy instance {instance_id}? Type 'yes' to confirm: ")
            ):
                print("Cancelled.")
                return 0
            live = list_instances(timeout=60)
            instance = next((item for item in live if str(item.get("id")) == instance_id), None)
            gpu = (instance.get("gpu_name") or "unknown GPU") if instance else "unknown GPU"
            action = "destroy" if args.command == "down" else "stop"
            command = [
                "vastai",
                action,
                "instance",
                instance_id,
            ]
            if args.command == "down":
                command.append("--yes")
            try:
                run(command, timeout=60)
            except (RuntimeError, OSError) as exc:
                raise RuntimeError(f"Instance {instance_id}: {action} failed; completion not verified: {exc}") from exc
            print(f"Waiting for instance {instance_id} to {action}...", file=sys.stderr)
            wait_for_lifecycle(instance_id, action)
            if args.command == "down":
                remove_host(instance_id)
                print(f"Instance {instance_id} ({gpu}) destroyed; billing stopped.")
            else:
                print(f"Instance {instance_id} ({gpu}) stopped; storage charges may continue.")
            return 0
    except PickerCancelled as exc:
        print("Cancelled.")
        return exc.status
    except KeyboardInterrupt:
        print("\nCancelled.")
        return 130
    except (RuntimeError, OSError, TypeError, ValueError, KeyError) as exc:
        print(f"gpu: {redact(str(exc))}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
