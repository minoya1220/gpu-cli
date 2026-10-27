# gpu

`gpu` is a small local CLI for launching Vast.ai GPU workspaces with an optional git repository. It replaces per-repository Vast templates with generated startup scripts and supports inexpensive containers or CUDA/Nsight profiling VMs.

`gpu` never stores or forwards a GitHub token or personal Vast account API key. It uses the authenticated local `gh` and `vastai` CLIs and makes no network request of its own. On the remote machine, private-repository authentication relies on `GH_TOKEN` already configured as a Vast account-level environment variable; VM startup prints a clear warning when it is absent.

## Install

Prerequisites: a Rust toolchain (`rustup`), authenticated [`vastai`](https://github.com/vast-ai/vast-python) and `gh` CLIs. macOS and Linux are supported.

```sh
cargo install --path . --locked
gpu --help
```

Coming from the Python version? Run `uv tool uninstall gpu` first so the old `gpu` does not shadow the new one on `PATH`. The config file, launch history, caches, managed SSH aliases and editor history are read as they are; nothing needs migrating. Timers armed by the Python version use a different remote worker: re-arm them with `gpu expire ID WHEN`.

For development:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo run -- --help
```

## Usage

Interactive launch:

```sh
gpu up
```

The prompts choose a repository, container or VM mode, a GPU model, and one of the ten cheapest matching offers. In a TTY the whole flow runs in one keyboard picker: use ↑/↓ to move and Enter or → to select, type to filter live, and Backspace to edit. Backtick (`) or ← clears the filter, otherwise goes back one screen and restores it as you left it. On the first screen with an empty filter, press backtick or ← twice in a row to quit; the first press shows a reminder, and any other key cancels the pending quit. Ctrl-C and Ctrl-D quit from any screen, and Esc is ignored. Repositories, offers, and prices load in the background, so every key responds immediately, including ` and Ctrl-C while a search is still running; Enter retries a failed load. Keys 1-9 select immediately only when the list has at most nine entries and the filter is empty; on GPU screens and longer lists digits remain part of the filter (for GPU names such as `RTX 4090` and `Tesla T4`). GPU names come from the bundled list merged with a local cache refreshed by offer queries, so the picker appears without a price search. Press F2 to load prices for the GPU categories and models; they stay loaded for that mode. F2 is reserved so `p` remains available for filtering. Non-TTY input uses line-based prompts instead.

Recent picks are stored in `$XDG_STATE_HOME/gpu/history.json` (or `~/.local/state/gpu/history.json`) only after a launch is confirmed and succeeds: the mode and GPU model, which rank the line prompts and put recent GPUs first in the picker. Dry runs and cancellations do not update history. Offers show hourly price, reliability, verification, offer ID, and location. The final confirmation repeats the hourly price.

On the Repository screen, choose **No repository** for an empty workspace, or **Clone from URL...** and type or paste a URL or `owner/name` inline, then press Enter or → once. Backtick or ← clears that input, then returns to the repository screen. URL entry is append-only (Backspace edits); left/right keep the same back/select behavior as the rest of the picker rather than moving a text cursor. These options remain available even while GitHub is loading or unavailable (Ctrl-R retries a failed repository load). The other choices are your repositories under the configured owner, from `gh repo list`. The list is cached in `$XDG_CACHE_HOME/gpu/repos-<owner>.json` (names only) so it shows at once; a cached pick is checked against the fresh GitHub list before anything launches.

Repository flags (mutually exclusive):

- `--no-repo`: no clone or repo-specific dependency sync.
- `--repo gpu-kernels`: a repository under the configured owner (default `minoya1220`).
- `--repo sgl-project/sglang`: any GitHub owner's repository.
- `--repo https://github.com/sgl-project/sglang.git`: an HTTPS git URL, including other git hosts.
- `--repo git@github.com:sgl-project/sglang.git`: an SSH git URL. The remote instance must already have suitable SSH authentication and host trust; the CLI does not transfer keys. Prefer HTTPS for public repos.

URLs must have a plain repository path, without embedded credentials, query strings, or fragments. Explicit repositories are validated locally, not restricted to the configured owner's GitHub listing. Instance labels and `gpu ls` show the repository basename or `no-repo`.

Safe previews (no instance created):

```sh
gpu up --no-repo --mode container --gpu 'RTX 4090' --yes --dry-run
gpu up --repo sgl-project/sglang --mode container --gpu 'RTX 4090' --yes --dry-run
```

Fully non-interactive launch (this spends money):

```sh
gpu up --repo gpu-kernels --mode container --gpu 'RTX 4090' \
  --num-gpus 1 --max-price 0.50 --offer 12345678 --disk 30 --yes
```

Preview the exact argument-array request without creating anything:

```sh
gpu up --repo gpu-kernels --mode vm --gpu 'RTX 4090' \
  --num-gpus 1 --offer 12345678 --disk 130 --dry-run
```

Use `--interruptible` for bid pricing. `--reliability` overrides the configured floor. Interactive offer selection includes verified and unverified hosts that meet the reliability floor, labels each verification status, and excludes deverified hosts. Flag-driven launches retain the verified-only default; add `--unverified` to include verified, unverified, and deverified hosts while preserving the reliability floor. An explicitly selected offer must satisfy all current filters. Long options can be shortened to any unambiguous prefix (`--dry` for `--dry-run`).

Instance operations:

```sh
gpu ls
gpu ls --json              # raw JSON for scripts
gpu ssh 12345678           # print the SSH URL
gpu ssh 12345678 --exec    # replace gpu with ssh
gpu stop 12345678
gpu down 12345678          # requires typed confirmation
gpu down 12345678 --yes
gpu edit 12345678          # reuse the last editor (picker on first use)
gpu edit 12345678 --editor code   # cursor, code (VS Code), zed, or nvim
gpu edit 12345678 --editor nvim --nvim-config ~/.config/nvim
gpu cursor 12345678        # alias for gpu edit 12345678 --editor cursor
```

Omit the ID for `ssh`, `stop`, `down`, `edit`, or `cursor` to choose an instance with the arrow-key picker. Even with one instance, selection is required. `down` still asks for typed confirmation unless `--yes` is passed. `stop` preserves the instance; `down` destroys it. Both poll Vast for up to 60 seconds after the request: `down` confirms the instance is gone, and `stop` confirms its status is stopped (storage charges may continue). A failed request or unverified completion exits non-zero without claiming success.

`gpu edit` waits up to two minutes for the instance and SSH, then opens `/workspace/<repo>` from the instance's label (or `/workspace` when no repo is recorded). It manages `vast-<id>` SSH aliases in `~/.ssh/vst_config`, included ahead of existing Host rules in `~/.ssh/config`; destroying an instance removes its managed alias.

After a successful `gpu up` in an interactive terminal, choose **Open in <last editor>** (default), **Select editor...**, or **Don't open**. With no editor history, only **Select editor...** (default) and **Don't open** appear. Only successful editor launches update `editor` history in `$XDG_STATE_HOME/gpu/history.json` (default `~/.local/state/gpu/history.json`). Non-interactive first use requires `--editor`; dry runs never open an editor.

Exit status: 0 on success or when a confirmation is answered with anything but `yes`; 1 for an error (printed as `gpu: <message>` on stderr, with secrets redacted), for quitting the picker, or for end of input at a confirmation; 130 for Ctrl-C; 2 for a usage error.

### Auto-destroy an instance

```sh
gpu expire 2h                     # pick an instance; destroy after two hours
gpu expire 12345678 1h30m         # explicit ID, no picker
gpu expire 18:30                  # next 18:30 in your local timezone
gpu expire 6:30PM                 # also 6:30pm, 6pm
gpu expire 12345678 2027-01-15T18:30:00-08:00
gpu expire show                   # pick an instance; show deadline and timer health
gpu expire 12345678 cancel        # remove its deadline; safe to repeat
gpu up --expire 2h                # normal launch flow, then arm before opening an editor
```

**Expiry destroys, not stops:** the instance's disk is deleted and all instance billing stops when Vast completes destruction. Scheduling is itself authorization to destroy; there is no additional confirmation prompt. Re-running `expire` replaces the deadline. A bare number is always an instance ID, never a time: `gpu expire 18` is incomplete; use `18:00` or `18h`. The optional ID goes **first**, including `gpu expire ID show` and `gpu expire ID cancel`. There are no `--in`, `--at`, `--show`, or `--cancel` flags.

Durations use ordered whole-number units `d`, `h`, `m`, `s` (e.g. `45m`, `1h30m`), must be positive, and measure elapsed time even across DST. Clock times select the next local occurrence (including the second occurrence during fall-back; nonexistent spring-forward times skip to the next day). ISO datetimes must be in the future; a timezone-less ISO datetime uses local rules and rejects ambiguous or nonexistent DST times, so include a UTC offset to disambiguate. The local timezone is `TZ` when set, otherwise the system default. The confirmation prints the absolute **local date/time, UTC offset, timezone abbreviation, and time remaining**. Conversion to a UTC deadline happens locally before SSH. `up --expire` starts counting at CLI invocation, including picker and boot time; if SSH is not ready before the deadline, arming fails rather than silently extending it.

The controller uses the managed `vast-<id>` SSH alias and requires **Linux with bash, `flock` and `setsid` (util-linux), and `curl`**, a writable `~/.local/state/gpu-expire`, and outbound HTTPS. No `python3`, systemd, cron, Vast CLI, or package installation is needed on the instance, so it works with container and VM images without an init manager. The CLI sends a bash worker over SSH stdin; it runs in its own session with stdin, stdout and stderr disconnected. Arming checks the worker's readiness handshake and its Linux process identity (`/proc/<pid>/cmdline`) before reporting success. The private `~/.local/state/gpu-expire/state` file stores the UTC deadline and timer identity, **not a key**. `show` checks whether that worker is still running; an inactive worker returns nonzero. Replacing or cancelling is serialized with the destruction request through a shared lock, so an obsolete worker cannot later destroy a re-armed instance. Cancellation cannot undo a request already sent to Vast.

**Your computer may sleep or disconnect after successful arming.** The instance must remain running and able to reach Vast: stopping or rebooting the instance, host failure, or killing the worker prevents expiry. This is not an external billing guarantee. After a reboot, `show` reports the stale timer as inactive; re-arm it. Failed API requests retry every 30 seconds while running, shown as `retrying`; `destroy-request-accepted` means Vast accepted the request, not an independent billing verification. Check `gpu ls` or the Vast console for disappearance. A timeout means arming or cancellation is **unverified**: reconnect and use `gpu expire ID show`. A failed `up --expire` leaves the new instance running and prints its ID; re-arm or use `gpu down ID` yourself. Auto-destruction does not clean the local SSH alias, because your computer may be offline.

#### Credential mechanism and research sources

- [Vast API Hello World](https://docs.vast.ai/api-reference/hello-world), "Create an Instance": `instance_api_key` is injected as `CONTAINER_API_KEY` and **can only start, stop, or destroy that specific instance**.
- [Docker Execution Environment](https://docs.vast.ai/guides/instances/docker-environment), "Predefined Variables" and "Using the CLI from Inside an Instance": Vast supplies `CONTAINER_ID` and `CONTAINER_API_KEY`; self-destruction from the instance is supported, not just stopping.
- [Destroy instance API](https://docs.vast.ai/api-reference/instances/destroy-instance): `DELETE https://console.vast.ai/api/v0/instances/{id}/` with `Authorization: Bearer <instance key>` permanently deletes the instance.
- [Linux Virtual Machines](https://docs.vast.ai/linux-virtual-machines), "Environment Variables": VM variables are written to `/etc/environment`. The worker reads only the two scoped variables from its environment, `/etc/environment`, or PID 1's environment. The ID must match the selected instance and the key must exist, or arming fails closed. Image-specific credential availability is checked at runtime.

No personal account key, local Vast config, or create-response key is uploaded. The worker uses only the key Vast already injected; it does not regenerate keys or fall back to an account credential. The key reaches `curl` on stdin, never on a command line; the request uses no proxy and follows no redirects. Keys and API response or error bodies are never logged. Transport failures suppress arbitrary remote stderr rather than risk exposing a credential.

#### Manual acceptance checks (paid and destructive: use disposable instances)

These are for the operator, **not automated tests**. Repeat on a cheap container and a VM:

1. On an existing disposable instance, run `gpu expire ID 5m`, then `gpu expire ID show`; verify the local timezone and `armed` status.
2. Re-arm to `10m`, show the new deadline, cancel twice, and confirm the instance survives the original deadline.
3. Arm for `5m` again, disconnect or sleep your computer, and later confirm the instance disappears from `gpu ls` and the Vast console (disk gone; billing ended).
4. Check `gpu expire ID 6:30pm` resolves to the expected next local occurrence, then cancel. Test `gpu up --expire 5m` only when ready to rent and lose a fresh instance; confirm boot-time failures do not claim the timer was armed.
5. Optionally reboot a disposable instance with a long deadline: `show` must report `inactive`; re-arm it. Never print the instance credential when investigating a failure.

### Editor requirements

- **Cursor / VS Code:** install the application and its Remote SSH support. Run **Shell Command: Install 'cursor' / 'code' command in PATH** from its command palette.
- **Zed:** install Zed and run **cli: install** from its command palette. Zed uses the managed SSH alias and installs its own remote server.
- **Neovim:** runs in your current terminal over `ssh -t`. If absent remotely, `gpu` installs official Neovim **v0.11.5** into `~/.local`, selecting Linux x86_64 or arm64 and verifying a pinned SHA-256 checksum before extraction. The remote host needs curl, tar, sha256sum, and a compatible glibc. If tmux is available, it starts or reattaches session `gpu-nvim` so disconnects preserve your editor; otherwise it runs nvim directly. Detach with tmux's usual Ctrl-B, D.

A missing GUI shell command is an error, never a silent fallback. GUI success means the CLI accepted the handoff; connection errors inside the editor still need attention.

Neovim defaults work without configuration. To use **your own** config, set `editor.nvim_config` below, or override it with `gpu edit --editor nvim --nvim-config <git-URL-or-local-directory>`. Git URLs are cloned into remote `~/.config/nvim` (existing git checkouts are updated with a fast-forward pull); local directories are uploaded with rsync, without deleting extra remote files. Local sync requires rsync on both machines; git sources need remote git and any necessary authentication already configured. Existing non-git config directories are not overwritten by a clone. Only use trusted configs: Neovim configs can execute code. No config is copied from this tool's author.

## Configuration

Built-in defaults live in `defaults()` in `src/config.rs`. Override only what you need in `~/.config/gpu/config.json` (or `$XDG_CONFIG_HOME/gpu/config.json`):

```json
{
  "owner": "minoya1220",
  "git_name": "minoya1220",
  "git_email": "minoya1220@gmail.com",
  "disk": 20,
  "vm_disk": 130,
  "reliability": 0.95,
  "container_image": "vastai/base-image:@vastai-automatic-tag",
  "vm_image": "docker.io/vastai/kvm",
  "editor": {
    "nvim_config": "https://github.com/YOU/nvim-config.git"
  }
}
```

Do **not** put secrets in this file. `gpu` leaves API authentication to the two upstream CLIs.

Containers default to 20 GB and profiling VMs to 130 GB. `--disk` overrides either mode. An empty `XDG_CONFIG_HOME`, `XDG_STATE_HOME` or `XDG_CACHE_HOME` counts as unset.

## Generated workspaces

For a bare repository `NAME`, startup clones `https://github.com/OWNER/NAME` into `/workspace/NAME`. URLs and `owner/name` clone into `/workspace/<basename>` (trailing `.git` removed). With `--no-repo`, startup still creates `/workspace`, configures git identity and conditional `GH_TOKEN` authentication, and performs the normal container/VM setup, but skips cloning and repo-specific sync. It runs `uv sync --directory /workspace/NAME` only when that exact directory contains `pyproject.toml`. Container setup also carries over `.no_auto_tmux`, the conditional bashrc workspace `cd` fix, and `entrypoint.sh`. VM setup installs CUDA 12.8 and Nsight Compute 2025.4.0 using the existing profiling-template logic. All exposed container ports are valid TCP ports.

## Design notes

The command delegates local authenticated network calls to the official `vastai` and `gh` CLIs instead of duplicating account credential handling. The expiry worker is the exception: it calls only the self-destroy endpoint using the instance's existing restricted key. This keeps keys out of generated requests. Offer selection is a live search rather than a static GPU menu, so unavailable models cannot be chosen. Dry-run output is JSON argument-array form rather than a shell command: each argument and the complete multiline startup script are unambiguous, copyable into tests, and free from misleading shell quoting.

This is a Rust rewrite of an earlier Python tool. Its output, files and flags match the Python version; `tests/golden/` holds parity tables generated from it, and `docs/PLAN.md` records the rewrite plan and the few deliberate differences.
