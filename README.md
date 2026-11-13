# gpu

`gpu` rents GPU workspaces on Vast.ai and TPU VMs on Google Cloud, and runs one-shot jobs on Vast or Modal. It runs on macOS and Linux.

Every launch rents a paid machine. Set a deadline with `--expire` and check `gpu ls` for anything still running.

## Install

1. Install [Rust](https://rustup.rs/), the [`vastai` CLI](https://github.com/vast-ai/vast-python) and [GitHub CLI](https://cli.github.com/).
2. Sign in to `vastai`, and run `gh auth login` so `gpu` can list your repos.
3. From this checkout:

   ```sh
   cargo install --path . --locked
   gpu up
   ```

To see a launch without renting:

```sh
gpu up --no-repo --mode container --gpu 'RTX 4090' --yes --dry-run
```

For TPUs and Modal, see [Google Cloud TPUs](#google-cloud-tpus) and [Modal jobs](#modal-jobs).

## Launch a workspace

```sh
gpu up                            # choose repo, mode, GPU and offer
gpu up --repo sgl-project/sglang  # public repo
gpu up --no-repo                  # empty /workspace
gpu up --expire 2h                # destroy in 2 hours, boot time included
```

Pick a container for general work or a VM for CUDA/Nsight profiling. `gpu` clones the repo to `/workspace/<name>`, removes `.git`, and runs `uv sync` if the repo has `pyproject.toml`. VMs also install CUDA 12.8 and Nsight Compute 2025.4.0. The confirmation shows the hourly price.

In the picker:

- Type to filter. ↑/↓ move; → or Enter select; ← clears the filter, then goes back. ← twice on the first screen, or Ctrl-C or Ctrl-D anywhere, quits.
- To approve a paid launch, type **yes** and press Enter. → never approves.
- On the repo screen, 1–7 pick the first seven repos, 8 opens **All repositories...**, 9 opens **Clone from URL...** (paste a URL or `owner/name`), and 0 picks **No repository**. On other short lists, 1–9 pick a row.
- → retries a failed offer load; Ctrl-R retries repos and prices.
- Offers show cheapest first, ten at most. `-` in a price column means no quote. Your recent picks come first.

Without a terminal, `gpu up` fails at once and names the flags it needs.

### Launch with flags

```sh
gpu up --repo gpu-kernels --mode container --gpu 'RTX 4090' \
  --num-gpus 1 --max-price 0.50 --offer 12345678 --disk 30 --expire 2h --yes
```

Use a current offer ID, or drop `--offer` and `--yes` takes the cheapest match. `--dry-run` prints the create request instead of renting. If the host cannot start the instance now, Vast creates nothing and `gpu up` exits 10.

- `--repo NAME` uses the configured owner or your `gh` login; `--repo owner/name` names another GitHub owner. HTTPS URLs from other hosts work but must not hold credentials, queries or fragments. `git@host:...` URLs need SSH keys and host trust already on the instance; `gpu` copies no keys.
- Private GitHub repos need `GH_TOKEN` set in your Vast account environment. `gpu` never uploads your local GitHub token or Vast API key.
- `--interruptible` uses bid pricing. `--reliability` sets the host floor. `--disk` sets the disk size in GB.
- The picker offers verified and unverified hosts; flag launches take verified hosts only. `--unverified` adds unverified and deverified hosts.
- Long flags accept unique prefixes, such as `--dry` for `--dry-run`.

## Run a one-shot job

```sh
gpu run --gpu 'RTX 4090' --expire 30m -- python train.py
gpu run --gpu 'RTX 4090' --expire 30m --dry-run -- python bench.py
gpu run --gpu 'RTX 4090' --expire 2h --image pytorch/pytorch:2.8.0-cuda12.9-cudnn9-runtime \
  --no-setup --env HF_TOKEN --yes --json -- python train.py
```

`gpu run` rents a new non-interruptible Vast container, uploads your files, runs the command, downloads results, then destroys the instance and checks it is gone. In a terminal it asks for what is missing. Without one it needs `--gpu`, `--expire` (or config `default_expire`) and `--yes` or `--dry-run`. The offer flags from `up` work too. `--timeout` (default 600 seconds) limits the wait for SSH, not the job.

- **Upload:** in a git repo, tracked and untracked files that are not ignored, from the repo root, with uncommitted edits; the command runs in the matching subfolder. It refuses submodules. Outside git, the current folder without `.venv`, `venv`, `__pycache__`, `node_modules` and `.DS_Store`. It never uploads `.git` or `gpu-runs/`. It refuses secret-looking files unless you pass `--allow-secrets`: `.env*` (except `.env.example`, `.env.sample`, `.env.template`, `.env.dist`), `id_rsa*`, `id_ed25519*`, `*.pem`, `.netrc`, `.aws/`, `.ssh/`, `credentials*.json`. `--max-upload` caps the size (default 1G). `--dry-run` lists the files.
- **Setup:** the first match runs: `uv.lock` → `uv sync --frozen`; `pyproject.toml` → `uv sync`; `requirements.txt` → `uv venv --system-site-packages .venv && uv pip install -r requirements.txt`. `--setup CMD` replaces it; `--no-setup` skips it. The job puts `.venv/bin` first on `PATH`. uv may reinstall packages the image already has, so with a CUDA/torch image, use `--no-setup` or your own `--setup`.
- **Secrets:** `--env NAME` (repeatable) sends a local variable over SSH stdin, never in arguments or Vast metadata. `gpu` does not filter the job's own output. Arguments pass as-is; use `-- sh -c '...'` for shell syntax.
- **Results:** write to `outputs/` (or `--outputs DIR`; also in `$GPU_RUN_OUTPUTS`). They land in `./gpu-runs/<UTC time>-<id>/` with `log.txt` and `result.json`. Add `gpu-runs/` to `.gitignore`. `gpu` keeps relative links that stay inside the outputs and skips other links and special files. If the download still fails after retries, `gpu` destroys the instance anyway, so save important work as you go.
- **Disconnects:** the job keeps running if SSH drops, and the CLI resumes the log where it stopped. With `--json`, logs go to stderr and stdout gets one JSON object.
- **Lifetime:** `--expire` covers boot, upload and download and must exceed 10 minutes, the time kept for fetching results. The instance arms its deadline at boot, before SSH works. If the job ends while no CLI is attached, the instance destroys itself after `--linger` (default 0). A host failure or stop can kill these guards and leave storage billing; check `gpu ls` and run `gpu down ID --yes`.
- **Ctrl-C:** before the create request, it cancels. After it, one press (or HUP/TERM) stops the job, downloads partial outputs and destroys the instance; two skip the download; three exit at once and print the ID, deadline and cleanup command. If something kills the CLI or you lose your computer, only the deadline remains. There is no `--detach`, `logs` or `attach`.

The job's exit code passes through. `run` adds **17** job timeout, **18** instance lost, **19** cleanup unverified (it may still bill; run `gpu down ID --yes`), and **130** cancelled after cleanup. Exit 19 overrides a successful job.

Custom images need SSH and startup behavior that matches [Vast's base image](https://github.com/vast-ai/base-image/blob/main/Dockerfile). If the image lacks uv, `gpu` installs uv 0.10.4 and checks its SHA-256.

### Modal jobs

Modal jobs need [uv](https://docs.astral.sh/uv/getting-started/installation/), not the `modal` command: `gpu` runs a bundled helper pinned to `modal==1.6.3`.

```sh
uvx modal@1.6.3 token new
gpu setup --provider modal
gpu run --provider modal --gpu T4 --expire 30m --dry-run --json -- python train.py
gpu run --provider modal --gpu T4 --expire 30m --yes -- python train.py
gpu ls --provider modal
gpu down modal:sb-EXAMPLE --yes
```

Setup checks the token and shows its workspace and environment; it creates nothing and stores no credentials. Modal's own profiles, `MODAL_CONFIG_PATH`, `MODAL_PROFILE` and token variables work. The first run needs network access to fill uv's cache. Dry runs fetch live prices but create nothing.

In a terminal, the `run` GPU screen lists Modal rows with live prices; type `modal` to find them. `--provider vast` hides them; `--provider modal` shows only them. If Modal prices fail to load, Vast rows still work.

- Use Modal's GPU names (any case; spaces become hyphens), not Vast's. `--num-gpus`, `--cpu` (cores, default 4) and `--memory` (GiB, default 16) set the size. `gpu` refuses `--offer`, `--disk`, `--reliability` and `--unverified`.
- `--expire` must be over 10 minutes and at most 24 hours. `--timeout` limits image build and scheduling.
- The default image is Debian slim with Python 3.13. An `--image` from a registry does not need Python; `gpu` adds uv.
- `--max-price` and `max_session_spend` check the estimate: GPU × count + CPU + memory at Sandbox rates (CPU and memory cost 3× Function rates). It leaves out image builds, egress and output Volume storage, and is not a billing cap. Modal bills setup, idle and download time too. If a rate is missing, the run stops before launch.
- The job must finish by the expiry minus a 10-minute results reserve, counting build, scheduling, upload and setup: `--expire 12m` leaves at most two minutes for all of it. The job is stopped then and its outputs flushed.
- The Sandbox exits if no job starts within five minutes of startup or the last upload progress, at the deadline, or `--linger` after the job ends. A run downloads outputs, then deletes the Sandbox and its output Volume, and exits 19 if it cannot confirm both. The Volume is kept while the Sandbox deletion is unconfirmed. The `gpu-cli` App and its cached image remain; no secrets or warm functions do.
- `gpu ls` lists running Modal Sandboxes and leftover output Volumes. `gpu down modal:sb-... --yes` deletes the Sandbox and its Volume.
- Modal rejections of the request or account (bad GPU or image, billing, no payment method) exit 2. Image build failures exit 1; see the build log in the Modal dashboard.
- Modal has no `up`, `edit`, `stop` or `expire`.

#### Debug a live Modal job

```sh
gpu exec modal:sb-... -- nvidia-smi
gpu cp modal:sb-... ./input.bin :/workspace/input.bin
gpu cp modal:sb-... :/gpu-volume/outputs ./checkpoint --recursive
gpu ssh modal:sb-...          # print the reconnect command
gpu ssh modal:sb-... --exec   # a terminal through the helper; no sshd or keys
```

These work only while the Sandbox runs, including an unattended `--linger` window; an attached `gpu run` still downloads and deletes it when the job ends. `exec` and `cp` return the command's exit code and support `--json` and `--command-timeout`; `ssh --exec` cannot use `--json`. JSON results keep `ssh_alias` and `setup_done` null. `cp` copies files and, with `--recursive`, folders; it refuses remote archives with links or special files.

#### Preemption and output Volumes

**GPU Sandboxes can be preempted.** `gpu` never reruns the job. Use Vast if you need a host that stays up.

- Each run mounts a Volume at `/gpu-volume`, and `outputs/` (or `--outputs`, also in `GPU_RUN_OUTPUTS`) links to `/gpu-volume/outputs`. Write inside it, and do not replace the link. Only outputs persist, not source, environments or logs. An output path that is itself a link is refused.
- `gpu` syncs the Volume every 30 seconds and when the job ends. Preemption keeps only the last sync, so checkpoint and close your files. Images without `sync` commit only when the Sandbox ends, which preemption may skip.
- If the Sandbox ends during a job, the run exits 18, downloads the last synced outputs from the Volume, and deletes it. Download failure does not keep the Volume.
- If the CLI dies, the laptop disconnects, or you press Ctrl-C three times, the Sandbox still ends by its deadline but **the Volume keeps billing storage** until you delete it. `gpu ls --provider modal` shows it as `outputs-only`. A Volume whose job never started stays hidden for 15 minutes, while its create may still wait for capacity.

```sh
gpu cp modal:volume:gpu-cli-run-<run-id> :/ ./recovered --recursive
gpu down modal:volume:gpu-cli-run-<run-id> --yes
```

## Connect and edit

```sh
gpu ls                          # IDs, status, price, uptime
gpu ssh 12345678                # print the SSH command
gpu ssh 12345678 --exec         # connect in this terminal
gpu edit 12345678               # last editor, or choose one
gpu edit 12345678 --editor code # cursor, code, zed or nvim
gpu edit 12345678 --editor nvim --nvim-config ~/.config/nvim
```

Omit the ID to pick an instance. `gpu ls --long` shows full IDs; `gpu ls --json` prints full IDs with credentials removed.

After an interactive launch, pick an editor or **Don't open**. `gpu edit` waits up to 120 seconds for SSH, then opens `/workspace/<repo>`, or `/workspace` without a repo. Without a terminal, the first `gpu edit` needs `--editor`. A missing editor command is an error.

- **Cursor / VS Code:** install Remote SSH, then use the command palette to install the `cursor` or `code` shell command.
- **Zed:** run **cli: install** from the command palette.
- **Neovim:** runs over SSH in your terminal. If the host lacks it, `gpu` installs Neovim 0.11.5, which needs Linux x86_64 or arm64, a recent glibc, curl, tar and sha256sum. If tmux is there, session `gpu-nvim` survives disconnects; detach with Ctrl-B, D.

`--nvim-config` takes a local folder or a git URL, and the config can run code, so use only ones you trust. `gpu` copies a folder with rsync (needed on both machines) and deletes no remote files. It clones a git URL to `~/.config/nvim`, or pulls if already cloned there; remote git must already have access. A clone never overwrites a folder that is not a git checkout.

## Stop or destroy

```sh
gpu stop 12345678       # stop compute; keep the disk
gpu down 12345678       # delete the instance and disk; type yes to confirm
```

**A stopped instance can still bill for storage. `down` deletes the disk.** `--yes` skips the prompt. Omit the ID to pick an instance. Both commands confirm the new state (Vast waits up to 60 seconds) and exit nonzero if they cannot.

## Auto-destroy a Vast instance

```sh
gpu expire 2h                   # pick an instance; destroy in 2 hours
gpu expire 12345678 1h30m
gpu expire 12345678 18:30       # next 18:30 local time
gpu expire 12345678 show        # deadline and timer health
gpu expire 12345678 cancel      # safe to repeat
```

**Setting a deadline approves destruction, disk included, with no further prompt.** A new deadline replaces the old one. Once armed, your computer can sleep or go offline.

Deadlines:

- Durations such as `45m` or `1h30m` (whole `d`, `h`, `m`, `s`, in that order) measure real time across DST.
- Clock times `18:30`, `6:30PM` or `6pm` mean the next one in local time (`TZ`, or the system zone). A time skipped by DST moves to the next day; a repeated one may take its second instance.
- ISO times such as `2027-01-15T18:30:00-08:00`. Without an offset, `gpu` uses local time and refuses times in a DST gap or overlap.
- A bare number is an instance ID: write `18:00` or `18h`, not `18`. The ID goes first.

The timer runs on the instance. It needs the instance running with HTTPS access to Vast; a stop, host failure or killed timer prevents expiry. This is not a billing guarantee. After a reboot, run `gpu expire ID show` and re-arm if the timer is inactive.

`gpu up --expire 2h` counts from when you run the command. Its startup script arms the deadline before SSH works, so it holds if you press Ctrl-C or your computer dies, and it resumes after a restart unless cancelled. If SSH comes up after the deadline, `gpu up` fails and the instance is still destroyed on time. Instances launched without `--expire` have no boot-time timer.

If expiry fails:

1. Run `gpu expire ID show`. A timeout means arming or cancelling is unconfirmed.
2. Re-arm with `gpu expire ID 2h`, or run `gpu down ID`.

A failed `up --expire` leaves the instance running and prints its ID. `retrying` means the timer retries failed requests every 30 seconds. `destroy-request-accepted` means Vast took the request; check `gpu ls`. Cancelling cannot undo a request already sent.

The timer host needs bash, curl, flock, setsid, outbound HTTPS and a writable `~/.local/state/gpu-expire`. It uses only the instance's own Vast key (`CONTAINER_ID`, `CONTAINER_API_KEY`) and fails if they are missing or wrong; `gpu` never uploads your account key. Sources: [instance keys](https://docs.vast.ai/api-reference/hello-world), [container environment](https://docs.vast.ai/guides/instances/docker-environment), [VM environment](https://docs.vast.ai/linux-virtual-machines), [destroy endpoint](https://docs.vast.ai/api-reference/instances/destroy-instance), [onstart reruns](https://docs.vast.ai/guides/reference/faq/instances).

## Google Cloud TPUs

```sh
gpu setup                                # pick Google Cloud and a project
gpu up --tpu v6e-1 --no-repo --expire 2h --spot --dry-run
gpu up --tpu v6e-1 --no-repo --expire 2h --spot --yes
gpu ls --provider gcp
```

### Set up once

1. Install [gcloud](https://cloud.google.com/sdk/docs/install) and run `gcloud auth login`. Use an existing throwaway project with billing, TPU quota (GPU quota does not count), a default VPC and subnet, and SSH firewall access.
2. Run `gpu setup`, pick Google Cloud, read the plan, and type the project ID to approve.

Until setup succeeds, `gpu` never calls gcloud and the picker shows only a **TPU (Google Cloud)** row that points to `gpu setup`. The project always comes from config, never gcloud's default.

Setup enables the Compute, TPU, Cloud Tasks and Cloud Billing APIs and creates one `gpu-expiry` queue (or reuses a compatible one). It creates no TPUs, identities, IAM grants, networks or firewalls, and does not touch billing or quotas; enabling APIs may make Google create its own default accounts and networks.

You need an unconditional `roles/tpu.admin`, `roles/editor` or `roles/owner` grant on the project; setup never fixes IAM. **TPU VMs and the expiry queue use the default Compute service account (`PROJECT_NUMBER-compute@developer.gserviceaccount.com`), so anyone on the VM gets its roles,** which may include deleting any TPU in the project. Let only trusted users and code on. `gpu` uploads no local credentials or keys.

```sh
gpu setup --provider gcp --project PROJECT --location us-central1 --dry-run
gpu setup --provider gcp --project PROJECT --location us-central1 --confirm-project PROJECT
gpu setup --provider gcp --remove --project PROJECT --confirm-project PROJECT
```

`--location` is the queue region only; you pick zones at launch. Without a terminal, writes need `--confirm-project`. Setup records each step in `~/.config/gpu/config.json` first; if it fails, rerun it. Remove setup before switching project or queue region.

`--remove` (or **Remove gpu setup** in the picker) refuses while a `gpu` TPU or any pending expiry task exists; destroy the TPU or run `gpu expire ID cancel`. It deletes the queue only if setup created it and it is empty, disables only the APIs setup enabled, and removes the local `gcp` config. Deleting the config by hand does not undo setup. Google may hold a deleted queue name for seven days.

### Launch

`gpu up` lists TPUs beside GPUs; `--provider gcp` or any TPU flag goes straight to them. Pick **generation → size → capacity → area → zone**, then the deadline. v5e and v6e sizes count chips; v2, v3, v4 and v5p sizes count TensorCores (two per chip). Areas list only zones that offer the type, nearest first by your time zone; `gpu` remembers your last zone per project. Without a terminal, give `--tpu`, `--repo` or `--no-repo`, `--expire` and `--zone` (or rely on a remembered zone). On-demand is the default; `--spot` or `--on-demand` skips that screen.

`gpu` checks the exact type and zone live before you confirm. Stock and quota can still fail; after a confirmed capacity refusal, an interactive launch offers the next-nearest zone with its price, keeping the same deadline. `--runtime IMAGE` overrides the image picked by generation (v2–v4 `tpu-ubuntu2204-base`, v5e `v2-alpha-tpuv5-lite`, v5p `v2-alpha-tpuv5`, v6e `v2-alpha-tpuv6e`).

Prices are USD list rates for the whole slice from Google's [pricing API](https://cloud.google.com/billing/docs/reference/pricing-api/rest/v1beta/skus.price/get), cached for 10 minutes. They leave out storage, network, tax and discounts. A missing rate shows `price unavailable`, and you may still approve the launch. TPU commands need curl 7.66 or later.

### Connect and destroy

```sh
gpu ssh gcp:PROJECT:ZONE:NAME --exec
gpu edit gcp:PROJECT:ZONE:NAME --editor code
gpu stop gcp:PROJECT:ZONE:NAME             # on-demand only; expiry stays armed
gpu down gcp:PROJECT:ZONE:NAME --yes       # deletes the node and boot disk
```

Short names from `gpu ls`, such as `gpu-uwhcp7`, work in place of full IDs within the configured project; full IDs keep working after config changes. SSH and editors use worker 0. Public repos clone into `/workspace/NAME`; `gpu` installs no TPU framework and no credentials. `gpu` does not support private or IAP-only SSH, multi-host setups, Google GPUs, reservations or queued resources. Uptime in `gpu ls` includes stopped time, and separate disks or addresses may still bill.

### Expiry

```sh
gpu expire gcp:PROJECT:ZONE:NAME 1h30m
gpu expire gcp:PROJECT:ZONE:NAME show
gpu expire gcp:PROJECT:ZONE:NAME cancel
```

Every TPU launch needs a deadline from 1 minute to 29 days ahead, in the same formats as Vast. Before creating the node, `gpu` queues a Cloud Tasks job that deletes it at the deadline. Google runs that job, so it works with your computer and the VM offline, and stops, Spot preemption and VM crashes do not disarm it. Cancelling cannot stop a deletion already under way.

<details>
<summary>Expiry limits: check deletion yourself</summary>

- It is not exact or a billing guarantee. IAM, API, billing or regional failures can delay or prevent deletion, and a failed delete is not retried. Check that the node is gone; a project budget alerts only late.
- Never reuse a node name while its tasks remain: a task deletes whatever has that name. `gpu` picks random names.
- `show` lists pending tasks, not VM or IAM health. If edits fail, several tasks may remain; the earliest wins.
- Keep the setup config until every node is gone. `gpu down` cancels tasks only after it confirms the node is gone. Cloud Tasks may add small charges.

Sources: [scheduling](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes#SchedulingConfig), [OAuth tasks](https://cloud.google.com/sdk/gcloud/reference/tasks/create-http-task), [retries](https://cloud.google.com/tasks/docs/configuring-queues), [limits](https://cloud.google.com/tasks/docs/quotas), [delete](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes/delete).

</details>

## Scripts and JSON

Without a terminal, `gpu` never prompts. `up` needs hardware, `--repo` or `--no-repo`, `--mode` (Vast) or `--zone` (TPU), `--yes`, and `--expire` or config `default_expire`. A Vast `--dry-run` needs `--yes` or `--offer`. `down` needs an ID and `--yes`.

```sh
gpu up --no-repo --mode container --gpu 'RTX 4090' --yes --expire 2h --wait --json
gpu wait 12345678 --timeout 180 --json
gpu exec 12345678 -- python -c 'print("hello")'
gpu cp 12345678 ./data :/workspace/data
gpu cp 12345678 :/workspace/results ./results
```

- `--wait` waits for SSH; `--wait-setup` also waits for the repo clone and setup to succeed. `--timeout` (default 120, max 86400 seconds) limits each wait.
- `exec` and `cp` wait for SSH first. `--command-timeout` limits the command or copy and exits 20. Stopping SSH may not stop the remote command.
- `exec` passes arguments as-is (use `sh -c` for shell syntax), gives no terminal or stdin, and returns the remote exit code.
- `cp` needs exactly one `:/absolute/path`, `-r` for folders, and `scp -s` support.
- Neither bypasses SSH host-key checks.
- `exec` and `cp` also accept Modal IDs; see [Debug a live Modal job](#debug-a-live-modal-job).

With `--json`, each command except `ls` prints one object:

```json
{"command":"wait","ok":true,"result":{"id":"12345678","ready":true,"ssh_alias":"vast-12345678"}}
```

On failure, `ok` is false, `error` holds `code`, `message` and `exit_code`, and `result` keeps what is known, such as a created ID. `gpu ls --json` prints a bare array; add `--envelope` for the standard object. In `up` results, check `created` and `expiry_armed`. Signals print an `interrupted` object and exit 128 + signal; an `up` instance keeps running.

| Exit | Meaning |
| --- | --- |
| 0 | success, or you declined |
| 1 | other error |
| 2 | usage or missing flags |
| 10 | no capacity |
| 11 | over `--max-price` |
| 12 | not set up (`gpu setup`) |
| 13 | sign-in expired |
| 14 | readiness timeout |
| 15 | over `max_session_spend` |
| 16 | ID not found (`gpu ls`) |
| 17–19 | `run`: job timeout, instance lost, cleanup unverified |
| 20 | command or helper timeout |
| 130 | Ctrl-C |

`exec`, `cp` and `run` pass the remote exit code through, which may overlap these; JSON shows it as `remote_exit`.

Create calls time out after 120 seconds (Vast) or up to 600 (Google Cloud). A timeout does not mean nothing was created: check `gpu ls` before retrying.

## Configuration

`gpu` reads `~/.config/gpu/config.json`, or `$XDG_CONFIG_HOME/gpu/config.json`. Only `gpu setup` creates it. Keys you set override the defaults in `src/config.rs`:

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
  "default_expire": "2h",
  "max_session_spend": 5,
  "editor": {
    "nvim_config": "https://github.com/YOU/nvim-config.git"
  }
}
```

If `owner`, `git_name` or `git_email` is unset, `gpu` reads it from `gh api user` and `git config` at each launch and does not save it. `max_session_spend` (off by default) refuses a launch whose estimated compute cost up to its deadline exceeds that many USD, or whose price or deadline is unknown. It is not a budget. Keep secrets out of this file.

`gpu` also writes:

- `~/.ssh/vst_config`: `vast-<id>` SSH aliases, included from `~/.ssh/config`. `gpu down` removes them. TPU SSH uses `~/.ssh/gpu_gcp_config`.
- `$XDG_STATE_HOME/gpu/history.json` (default `~/.local/state/gpu/`): recent picks.
- `$XDG_CACHE_HOME/gpu/` (default `~/.cache/gpu/`): repo lists, GPU models, and TPU types, zones and prices.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Tests never touch real Vast, Google Cloud or Modal accounts. Paid tests are manual, on throwaway instances: arm a 5-minute deadline, replace and cancel it, disconnect and confirm deletion, and repeat after a reboot and a Ctrl-C during `gpu up --expire 10m`. For TPUs, also cover Spot, stopped on-demand nodes and each runtime. Never expose credentials while checking.
