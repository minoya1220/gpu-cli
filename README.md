# gpu

Run `gpu up` to rent a Vast.ai GPU workspace, with or without a git repository. Choose a container for general work or a VM for CUDA/Nsight profiling.

## Install

1. Install a [Rust toolchain](https://rustup.rs/), the [`vastai` CLI](https://github.com/vast-ai/vast-python), and [GitHub CLI](https://cli.github.com/). macOS and Linux are supported.
2. Authenticate `vastai` and run `gh auth login` for repository listing.
3. From this checkout, install and launch:

   ```sh
   cargo install --path . --locked
   gpu up
   ```

Launching rents a paid instance. The final confirmation shows the hourly price before creating it. To preview without renting:

```sh
gpu up --no-repo --mode container --gpu 'RTX 4090' --yes --dry-run
```

## Launch a workspace

```sh
gpu up                          # choose repository, mode, GPU, then offer
gpu up --repo sgl-project/sglang  # public repository
gpu up --no-repo                 # empty /workspace
gpu up --expire 2h               # destroy after 2 hours, including boot time
```

In the terminal picker:

- Type to filter, use ↑/↓ to move, and Enter or → to select.
- Press backtick (`) or ← to clear the filter, then go back. On the first screen, press twice to quit. Ctrl-C or Ctrl-D quits from any screen; Esc does nothing.
- Press F2 on a GPU screen to load prices. Digits filter GPU names; on short, unfiltered lists, 1-9 select directly.
- Choose **No repository** or **Clone from URL...** without waiting for GitHub. Paste a URL or `owner/name`, then Enter. Backspace edits; arrows select or go back, not move the text cursor.
- Press Enter to retry a failed offer load; Ctrl-R retries repositories. With piped input, the CLI uses numbered line prompts instead.

Offers are sorted cheapest first and show hourly price, reliability, verification, ID, and location. Up to ten are shown. Recent successful choices appear first; cancellations and dry runs do not change history.

### Launch with flags

This command rents a paid instance without prompts. Replace the offer ID with a current matching offer:

```sh
gpu up --repo gpu-kernels --mode container --gpu 'RTX 4090' \
  --num-gpus 1 --max-price 0.50 --offer 12345678 --disk 30 --yes
```

Use `--dry-run` instead of `--yes` to print the exact JSON argument-array request without creating anything. With `--yes` and no `--offer`, the cheapest matching offer is selected.

- `--repo NAME` uses the configured owner. `--repo owner/name` accepts another GitHub owner.
- `--repo https://host/owner/repo.git` accepts other git hosts. URLs cannot contain credentials, query strings, or fragments.
- `--repo git@host:owner/repo.git` needs SSH authentication and host trust already on the instance. No keys are transferred. Prefer HTTPS for public repos.
- `--interruptible` uses bid pricing. `--reliability` overrides the configured floor. `--disk` overrides the mode's disk size in GB.
- Interactive offer selection includes verified and unverified hosts, but excludes deverified hosts. Flag-driven selection defaults to verified-only. `--unverified` includes all three statuses while keeping the reliability floor.

`--repo` and `--no-repo` are mutually exclusive. Explicit repositories are validated locally, not limited to the configured owner's list. A selected offer must match every active filter. Long options accept unambiguous prefixes, such as `--dry` for `--dry-run`.

Private GitHub repos need `GH_TOKEN` configured in the Vast account environment before launch. `gpu` does not upload your local GitHub token or personal Vast API key. Do not put secrets in the config file.

## Connect and edit

```sh
gpu ls                          # IDs, status, price, uptime
gpu ssh 12345678                # print the SSH URL
gpu ssh 12345678 --exec         # connect in this terminal
gpu edit 12345678               # reuse the last editor, or choose one
gpu edit 12345678 --editor code # cursor, code, zed, or nvim
```

Omit the ID to pick an instance, even if only one exists. `gpu ls --json` prints credential-redacted JSON for scripts. `gpu cursor ID` is an alias for `gpu edit ID --editor cursor`.

After an interactive launch, choose an editor or **Don't open**. `gpu edit` waits up to 120 seconds for the instance and SSH. It opens `/workspace/<repo>` from the instance label, or `/workspace` without a repository. Non-interactive first use needs `--editor`.

### Set up an editor

- **Cursor / VS Code:** install Remote SSH support. In the command palette, install the `cursor` or `code` shell command in PATH.
- **Zed:** run **cli: install** in the command palette. Zed installs its own remote server.
- **Neovim:** runs over SSH in your terminal. If missing, `gpu` installs Neovim v0.11.5 remotely. The host needs Linux x86_64 or arm64, compatible glibc, curl, tar, and sha256sum. With tmux installed, session `gpu-nvim` survives disconnects; detach with Ctrl-B, D.

GUI success confirms the editor CLI handoff, not the connection inside its window. A missing editor command is an error, not a fallback.

To use your Neovim config:

```sh
gpu edit 12345678 --editor nvim --nvim-config ~/.config/nvim
```

A local directory is uploaded with rsync, without deleting extra remote files; both machines need rsync. A git URL is cloned to remote `~/.config/nvim`, or an existing checkout is updated with a fast-forward pull. Remote git and authentication must already work. Existing non-git config directories are not overwritten by a clone. Only use trusted configs: they can execute code. Without a config, Neovim uses its defaults.

## Stop or destroy

```sh
gpu stop 12345678       # stop compute; keep the disk
gpu down 12345678       # delete the instance and disk; requires typed yes
```

**Stopping can still incur storage charges. Destroying deletes the disk.** Omit the ID to pick an instance. Use `gpu down ID --yes` only to skip the destruction confirmation.

Both commands verify Vast's state for up to 60 seconds. Failed or unverified completion exits nonzero and does not claim success.

## Auto-destroy an instance

```sh
gpu expire 2h                   # choose an instance; destroy in 2 hours
gpu expire 12345678 1h30m       # explicit instance
gpu expire 12345678 18:30       # next 18:30 in your local timezone
gpu expire 12345678 show        # deadline and worker health
gpu expire 12345678 cancel      # remove deadline; safe to repeat
```

**Scheduling authorizes destruction, including disk deletion. There is no extra confirmation.** Re-running `expire` replaces the deadline. Your computer can sleep or disconnect after arming succeeds.

The instance must stay running and reach Vast over HTTPS. Stop, reboot, host failure, or a killed worker prevents expiry. This is not an external billing guarantee. After a reboot, run `gpu expire ID show`; an inactive timer needs re-arming.

### Deadline formats

- Durations: positive whole-number units in `d`, `h`, `m`, `s` order, such as `45m` or `1h30m`. These measure elapsed time across DST.
- Clock times: `18:30`, `6:30PM`, or `6pm` select the next local occurrence. A missing spring-forward time skips to the next day; a repeated fall-back time can select its second occurrence.
- ISO datetimes: `2027-01-15T18:30:00-08:00`. A timezone-less value uses local time and rejects ambiguous or nonexistent DST times. Include an offset to disambiguate.

The local timezone is `TZ` when set, otherwise the system default. Confirmation shows the local date/time, UTC offset, timezone abbreviation, and remaining time.

A bare number is an instance ID, not a time: use `18:00` or `18h`, not `18`. The ID always goes first, including `gpu expire ID show` and `gpu expire ID cancel`. There are no `--in`, `--at`, `--show`, or `--cancel` flags.

`gpu up --expire 2h` counts from CLI invocation, including selection and boot. If SSH misses the deadline, arming fails rather than extending it.

### If expiry fails

1. Run `gpu expire ID show` to verify the timer. A timeout means arming or cancellation is unverified.
2. Re-arm an inactive timer with `gpu expire ID 2h`, or run `gpu down ID` to destroy the instance yourself.

A failed `up --expire` leaves the new instance running and prints its ID. `retrying` means failed API requests retry every 30 seconds. `destroy-request-accepted` means Vast accepted the request, not that billing has independently been verified. Run `gpu ls` to check disappearance. Cancellation cannot undo a destruction request already sent.

## Configuration

Edit `~/.config/gpu/config.json`, or `$XDG_CONFIG_HOME/gpu/config.json`. Only supplied keys override defaults:

```json
{
  "owner": "YOUR_GITHUB_USER",
  "git_name": "Your Name",
  "git_email": "you@example.com",
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

Set your GitHub owner and git identity before cloning your own repositories. Built-in values are in `src/config.rs`. Containers default to 20 GB; VMs to 130 GB. Empty XDG variables count as unset. Keep secrets out of this file; authentication belongs to `gh` and `vastai`.

## Agent and contributor context

### Validate without renting

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Tests use `Runner` mocks and PATH stubs, never a real Vast account. Use a temporary HOME for SSH config tests. `tests/golden/` pins serialization, numeric formatting, and other byte contracts; update prose expectations deliberately, not unrelated formats. `docs/PLAN.md` is historical implementation context.

### Workspace and local files

`src/startup.rs` generates startup scripts. Repositories clone into `/workspace/<basename>` with `.git` removed; bare names use the configured owner. `uv sync` runs only if that directory has `pyproject.toml`. No-repo launches still create `/workspace` and configure git identity and conditional `GH_TOKEN` authentication. Containers set up the entrypoint and workspace shell directory; VMs install CUDA 12.8 and Nsight Compute 2025.4.0.

`src/ssh_config.rs` manages `vast-<id>` aliases in `~/.ssh/vst_config`, included before existing Host rules in `~/.ssh/config`. `gpu down` removes the alias; remote expiry cannot clean it while the computer is offline. Successful mode, GPU, and editor picks live in `$XDG_STATE_HOME/gpu/history.json` (default `~/.local/state/gpu/history.json`). Repository names are cached under `$XDG_CACHE_HOME/gpu/repos-<owner>.json`; cached picks are checked against a fresh GitHub listing. GPU models combine the bundled list and the offer-query cache. Slow work runs in picker `Job`s, never on its input thread.

### Expiry worker and credentials

`src/expiry.rs` sends `remote/expiry_remote.sh` over SSH stdin. The Linux host needs bash, curl, flock, setsid, writable `~/.local/state/gpu-expire`, and outbound HTTPS. No init manager or package installation is needed. The detached worker's readiness handshake and `/proc` identity are verified before success. State stores the deadline and worker identity, never the key. A shared lock serializes replacement, cancellation, and destruction.

Local network calls use authenticated `gh` and `vastai`. The remote expiry worker is the exception: it uses only Vast-injected `CONTAINER_ID` and `CONTAINER_API_KEY`, from its environment, `/etc/environment`, or PID 1. ID mismatch or a missing scoped key fails closed. No account key, local Vast config, or create-response key is uploaded. The key reaches curl on stdin, not argv; requests use no proxy or redirects. Never print raw Vast output or remote diagnostics that could contain credentials. Neovim release archives use pinned SHA-256 digests in `src/editors.rs`.

Vast references: [instance-scoped keys](https://docs.vast.ai/api-reference/hello-world), [container environment](https://docs.vast.ai/guides/instances/docker-environment), [destroy endpoint](https://docs.vast.ai/api-reference/instances/destroy-instance), [VM environment](https://docs.vast.ai/linux-virtual-machines).

Paid, destructive acceptance checks are manual only. Use disposable container and VM instances:

1. Arm a 5-minute deadline and show its status.
2. Replace it with 10 minutes, cancel twice, and verify the instance survives the original deadline.
3. Arm again, disconnect, and verify disappearance in `gpu ls` and Vast.
4. Test a local clock deadline, then cancel it.
5. Reboot with an armed deadline and verify the timer is inactive. Never expose credentials while investigating.

Exit codes: 0 for success or declining a confirmation; 1 for errors, picker quit, or confirmation EOF; 130 for Ctrl-C; 2 for usage errors. Errors use credential-redacted `gpu: <message>` on stderr. Dry runs print JSON argument arrays, not shell commands. Preserve table and JSON formats used by scripts.
