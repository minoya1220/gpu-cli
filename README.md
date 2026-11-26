# gpu

`gpu` rents GPU workspaces on Vast.ai, Runpod and DigitalOcean, TPU VMs on Google Cloud, and runs one-shot jobs on Vast or Modal. It runs on macOS and Linux.

Every launch rents a paid machine. Use `--expire` where supported; experimental AMD workspaces require manual deletion. Check `gpu ls` for anything still running. `gpu <command> --help` lists every flag.

## Install

1. Install [Rust](https://rustup.rs/), the [`vastai` CLI](https://github.com/vast-ai/vast-python) and [GitHub CLI](https://cli.github.com/).
2. Sign in to `vastai`, and run `gh auth login` so `gpu` can list your repos.
3. From this checkout:

   ```sh
   cargo install --path . --locked
   gpu up --no-repo --mode container --gpu 'RTX 4090' --yes --dry-run   # preview, rents nothing
   ```

## Launch a workspace

```sh
gpu up                            # pick repo, mode, GPU and offer
gpu up --repo sgl-project/sglang  # public repo
gpu up --no-repo                  # empty /workspace
gpu up --repo gpu-kernels --mode container --gpu 'RTX 4090' \
  --max-price 0.50 --disk 30 --expire 2h --yes
```

A container suits general work; a VM adds CUDA 12.8 and Nsight Compute for profiling. The repo is cloned to `/workspace/<name>` without `.git`, and `uv sync` runs if it has `pyproject.toml`.

In the picker, type to filter, use arrows to move, and type `yes` then Enter to approve a paid launch. If the offer is gone by then, nothing is rented and the picker returns to the offer list with the reason. Ctrl-C quits.

- `--yes` without `--offer` takes the cheapest matching offer. Flag launches use verified hosts only; `--unverified` adds the rest.
- `--repo NAME` uses your configured owner or `gh` login. Other HTTPS hosts work; `git@` URLs need keys already on the instance.
- Private GitHub repos need `GH_TOKEN` set in your Vast account environment. `gpu` never uploads your local GitHub token or Vast API key.
- `--expire 2h` counts from now, boot included, and is armed on the instance before SSH works, so it holds if you press Ctrl-C or your computer dies.

## Run a one-shot job

```sh
gpu run --gpu 'RTX 4090' --expire 30m --dry-run -- python bench.py   # list files, rent nothing
gpu run --gpu 'RTX 4090' --expire 30m --yes --env HF_TOKEN -- python train.py
```

`gpu run` rents a Vast container, uploads your files, runs setup and the command, downloads results, then destroys the instance.

- Upload: the current folder and below, nothing above it; the job starts in that folder. In a git repo, every non-ignored file there including uncommitted edits; elsewhere, all files minus `.venv`, `node_modules` and caches. `--dry-run` lists them. Secret-looking files (`.env*`, SSH keys, `*.pem`, `.netrc`, `.aws/`, `credentials*.json`) are refused unless you pass `--allow-secrets`. The cap is `--max-upload` (default 1G).
- `--include PATH` (repeatable) also uploads a file that git ignores or does not track, such as a job script you just wrote: `gpu run --gpu 'RTX 4090' --expire 30m --include gpu-runs/job.sh --yes -- sh gpu-runs/job.sh`. The path must stay inside the current folder (no absolute paths, `..`, directories or escaping symlinks) and goes through the same secret check, `--max-upload` and `--dry-run` listing.
- Setup: `uv.lock`, `pyproject.toml` or `requirements.txt` is installed with uv. On a CUDA/torch `--image`, pass `--no-setup` or your own `--setup CMD` so uv does not reinstall torch.
- Secrets: `--env NAME` sends a local variable over SSH stdin, never in arguments or Vast metadata. Job output is not filtered.
- Results: write to `outputs/`. They land in `./gpu-runs/<time>-<id>/` with `log.txt` and `result.json`; add `gpu-runs/` to `.gitignore`.
- `--expire` must exceed 10 minutes, which are reserved for downloading results. `--timeout` limits the readiness wait (SSH; on Modal, image build/pull and GPU scheduling), not the job.
- Ctrl-C once stops the job, downloads partial outputs and destroys the instance; twice skips the download; three times exits at once and prints the cleanup command. If the CLI dies, only the deadline remains.

The job's exit code passes through. `run` adds 17 job timeout, 18 instance lost, 19 cleanup unverified (it may still bill: run `gpu down ID --yes`) and 130 cancelled.

### Modal

Modal needs [uv](https://docs.astral.sh/uv/getting-started/installation/), not the `modal` command.

```sh
uvx modal@1.6.3 token new
gpu setup --provider modal
gpu run --provider modal --gpu T4 --expire 30m --yes -- python train.py
gpu ls --provider modal
gpu down modal:sb-EXAMPLE --yes
```

- Use Modal GPU names. `--cpu` (default 4) and `--memory` (GiB, default 16) set the size. `--expire` must exceed 10 minutes and be at most 24 hours; build, scheduling, upload and setup count against it.
- `--timeout` defaults to 20 minutes on Modal, as the first pull of a large CUDA image can take over 10. The wait is also capped at `--expire` minus 10 minutes. A timeout says whether the image or GPU scheduling was still pending.
- Behind a proxy, Modal uses `HTTPS_PROXY`/`ALL_PROXY` (HTTP or SOCKS) and `NO_PROXY`.
- `--max-price`, `max_session_spend` and `max_daily_spend` check an estimate, not your bill; Modal also bills setup, idle and download time.
- GPU Sandboxes can be preempted and the job is never rerun. Outputs sync to a Volume every 30 seconds; checkpoint your work.
- If the CLI dies, the Sandbox still ends at its deadline, but its output Volume keeps billing storage. `gpu ls --provider modal` shows it as `outputs-only`. Recover and delete it:

  ```sh
  gpu cp modal:volume:gpu-cli-run-<run-id> :/ ./recovered --recursive
  gpu down modal:volume:gpu-cli-run-<run-id> --yes
  ```

While a Sandbox runs, `gpu exec`, `gpu cp` and `gpu ssh --exec` work on its `modal:sb-...` ID. Modal has no `up`, `edit`, `stop` or `expire`.

## AMD Instinct (experimental)

Runpod Secure Cloud containers and DigitalOcean VMs support **one MI300X only**. Both backends are **untested against real rentals**; stock, image/SSH/copy and profiler hardware counters remain unverified. No automatic fallback.

Install `runpodctl >= 3.1.1` or `doctl >= 2.4.0`; authenticate with that CLI, never gpu config. Register `~/.ssh/id_ed25519.pub` with the provider. Add provider settings to `~/.config/gpu/config.json` (no secrets):

```json
{
  "runpod": {"image": "YOUR_ROCM_IMAGE@sha256:DIGEST", "datacenter": "DATACENTER"},
  "do": {"region": "atl1", "ssh_keys": "REGISTERED_KEY_ID"}
}
```

AMD repositories clone to `/workspace/repo`; no automatic dependency install. The Runpod image must start sshd/SFTP from its own start command and provide bash, git and tar; gpu keeps that command and starts workspace setup on the first SSH connection (`gpu wait ID --wait-setup`, `ssh`, `exec`, `cp` or an editor). Port 22 must map to a public IP. Proxy SSH cannot support copy/editors. DO resolves `gpu-amd-base` to an image ID; override with `do.image` after validation. These paths never install CUDA or Nsight.

```sh
gpu setup --provider runpod       # or --provider do; read-only CLI/auth/key checks
gpu offers --provider runpod      # choose runpod.datacenter from stock hints
gpu up --provider runpod --gpu MI300X --no-repo --dry-run
gpu up --provider do --gpu MI300X --no-repo --yes --wait
gpu ls --provider do
gpu cp do:atl1:ID ./input :/workspace/input
gpu down do:atl1:ID --yes          # polls for verified absence
```

Manual AMD workspaces have **no auto-destroy**. `--yes` approves renting until manual deletion; remove `default_expire` to use them. `--expire`, `gpu expire`, live `gpu run` and a configured `max_session_spend` or `max_daily_spend` on manual launches fail closed. `gpu run --provider runpod|do --gpu MI300X --expire 30m --dry-run -- COMMAND` uses the shared upload/planning checks only; its estimate is hypothetical, not an enforced deadline.

`--max-price` checks the hourly estimate: Runpod includes container disk ($0.10/GB-month, estimated at 730h/month); DO includes its fixed boot disk. Network/tax and independent retained resources are extra. Runpod stop loses the container filesystem and still bills retained storage; DO power-off continues **full GPU billing**. `down` deletes the Pod/Droplet, not independent volumes, snapshots or backups. IDs are `runpod:ID` and `do:REGION:ID`; bare numbers remain Vast.

## Owners

Several agents can rent through one account. Label your launches so `gpu ls` shows who started what: pass `--owner NAME` to `gpu up` or `gpu run`, or set `GPU_OWNER=NAME` (the flag wins). NAME is 1-32 letters, digits, `.`, `_` or `-`. The owner is stored in the instance label as `REPO@NAME`, so it survives across processes.

```sh
GPU_OWNER=firstmate gpu up --no-repo --gpu 'RTX 4090' --expire 2h --yes
gpu ls --owner firstmate        # only that owner's instances; `--json` rows carry "owner"
```

If the same owner already has a running instance, `gpu up`/`gpu run` print a warning naming its ID and still launch (`owner_warning` with `--json`). Other owners' instances never trigger it. Instances without an owner show a blank Owner column.

## Connect and edit

```sh
gpu ls                          # IDs, status, price, uptime, owner
gpu ssh 12345678 --exec         # connect; without --exec, print the command
gpu edit 12345678 --editor code # cursor, code, zed or nvim
gpu edit 12345678 --editor nvim --nvim-config ~/.config/nvim
```

Omit the ID to pick an instance.

- Cursor and VS Code need the Remote SSH extension and the `cursor` or `code` shell command; Zed needs `cli: install`.
- Neovim runs over SSH. `gpu` installs it on Linux hosts that lack it. `--nvim-config` takes a folder or git URL; it can run code, so use only configs you trust.

## Attach a GPU to a local directory

For code that lives only on your computer: copy it up once, edit on the GPU (Zed or VS Code remote, or Neovim over SSH), and let `gpu sync` mirror the GPU's changes back. The GPU copy leads; the local directory is a mirror. Vast, Runpod, DigitalOcean and Google Cloud only; Modal has no SSH. In the interactive `gpu up` Repository picker, "From local directory..." (key 7) does the same: it asks for a path (Enter for the current directory), then asks about autosync.

```sh
gpu up --dir .                          # normal picker (it asks about autosync), then copy this folder to /workspace/NAME
gpu up --dir . --autosync               # same, answering yes without asking
gpu link 12345678 [--autosync]          # or attach an instance that is already running
gpu edit 12345678                       # opens /workspace/NAME, not /workspace
gpu sync                                # list linked instances and pull the GPU's changes here
gpu sync --watch                        # keep pulling until Ctrl-C
gpu sync --autosync                     # keep pulling in the background; gpu sync --no-autosync stops it
gpu sync 12345678 --include outputs     # also pull a gitignored folder (remembered)
gpu unlink 12345678
```

- Copy up: the same files as `gpu run` (everything non-ignored below the folder, uncommitted edits included), once, at launch or link. Secret-looking files are never sent. The cap is `--max-upload` (default 1G); `--dry-run` lists the upload. It needs rsync on both sides; `gpu` installs it on the instance if it can.
- Pull: the instance lists its files over SSH (inotify when available, polling otherwise); bursts of saves become one rsync of the changed files. `gpu sync` shows each linked instance as `in sync` or `behind by N files`.
- Conflicts: your local file is never overwritten silently. If you edited a file here since the last sync and the GPU changed it too, your file stays and the GPU version is saved beside it as `NAME.gpu-ID-TIME.EXT`, with a `CONFLICT` line. Move the saved copy over yours to take it. Local files are never deleted, including ones deleted on the GPU.
- What is pulled: new and changed GPU files that git would not ignore here, so `.venv`, caches and outputs stay on the GPU unless named with `--include`. Secret-looking files stay put. Files above `--max-file` (default 256M) are skipped with a warning; a terminal asks first.
- `gpu down` pulls once more before it destroys a linked instance. If that pull fails, skips files, or leaves changed gitignored or secret-looking files on the GPU, it stops: fix it (`gpu sync ID --include PATH` brings ignored files home), or pass `--discard-unsynced` to destroy anyway. Auto-destroy (`--expire`, `gpu expire`) cannot pull first, so changes not yet synced are lost at the deadline (`gpu expire` and `gpu link` say so for a linked instance); run autosync (below), keep `gpu sync --watch` running, or run `gpu sync` beforehand. `gpu sync` forgets links whose instance is gone.
- Autosync is `--watch` in a detached background process, one per link, so it outlives the terminal. It reconnects with a growing pause (10s up to 5 minutes) after a dropped connection or a sleeping laptop, and ends by itself when the instance is destroyed or the link removed. `gpu sync` lists each link's autosync as `running`, `running, reconnecting` or `off`, with its recent errors; `gpu down` stops it after the final pull, and `gpu unlink` stops it. While it runs it also pulls once more about three minutes before an armed auto-destroy deadline, if the instance can be reached; changes made after that, or while it cannot connect, are still lost. It does not start again after a reboot. Starting and stopping it is the only background work `gpu` does for you; there is nothing to install on the instance.
- Links and their last-synced state live in `~/.local/state/gpu/sync/`, and so do each autosync's log (`ID-HASH.log`, never secrets) and status. Only one sync of a link runs at a time; a manual `gpu sync` waits for autosync's pull in progress.

## Stop or destroy

```sh
gpu stop 12345678       # stop compute, keep the disk
gpu down 12345678       # delete the instance and disk; --yes skips the prompt
```

A stopped instance still bills for storage. When `gpu down` destroys a running instance, it prints the runtime and an estimated cost (hourly price times runtime; `billed_usd` and `runtime_seconds` with `--json`); Vast's own billing is authoritative.

## Auto-destroy a Vast instance

```sh
gpu expire 12345678 1h30m
gpu expire 12345678 18:30       # next 18:30 local time
gpu expire 12345678 show
gpu expire 12345678 cancel
```

Setting a deadline approves destruction, disk included, with no further prompt. A new deadline replaces the old one. Formats: `45m`, `1h30m`, `6pm`, `18:30`, or ISO `2027-01-15T18:30:00-08:00`. A bare number is an instance ID, so write `18:00`, not `18`.

The timer runs on the instance using its own Vast key, so your computer can go offline. It is not a billing guarantee: a stop, host failure or lost HTTPS access prevents expiry. After a reboot or a failed `expire`, run `gpu expire ID show`, then re-arm or run `gpu down ID`. The host needs bash, curl, flock and setsid. Sources: [instance keys](https://docs.vast.ai/api-reference/hello-world), [destroy endpoint](https://docs.vast.ai/api-reference/instances/destroy-instance).

## Google Cloud TPUs

Setup, once:

1. Install [gcloud](https://cloud.google.com/sdk/docs/install) and run `gcloud auth login`. Use a throwaway project with billing, TPU quota, a default VPC and SSH firewall access, and `roles/tpu.admin`, `roles/editor` or `roles/owner`.
2. Run `gpu setup`, pick Google Cloud, and type the project ID to approve.

Setup enables the Compute, TPU, Cloud Tasks and Cloud Billing APIs and creates a `gpu-expiry` queue. It changes no IAM, networks or billing. TPU VMs run as the project's default Compute service account, so anyone on the VM gets its roles, possibly including deleting any TPU in the project. Let only trusted users and code on.

```sh
gpu setup --provider gcp --project PROJECT --location us-central1 --confirm-project PROJECT
gpu setup --provider gcp --remove --project PROJECT --confirm-project PROJECT
```

Launch and manage:

```sh
gpu up --tpu v6e-1 --no-repo --expire 2h --spot --yes
gpu ls --provider gcp
gpu ssh gcp:PROJECT:ZONE:NAME --exec
gpu stop gcp:PROJECT:ZONE:NAME            # on-demand only; expiry stays armed
gpu down gcp:PROJECT:ZONE:NAME --yes
gpu expire gcp:PROJECT:ZONE:NAME show
```

Short names from `gpu ls` work in place of full IDs. Without a terminal, also pass `--zone`. Prices are list rates without storage, network or tax.

Every TPU launch needs a deadline, 1 minute to 29 days ahead. Cloud Tasks deletes the node then, even with your computer and the VM offline. It is not a billing guarantee: a failed delete is not retried, so check the node is gone. Keep the setup config until every node is gone.

## Scripts and JSON

Without a terminal, `gpu` never prompts. Vast/TPU `up` needs hardware, `--repo` or `--no-repo`, `--mode` (Vast) or `--zone` (TPU), `--expire` and `--yes`. Manual AMD workspaces instead require explicit hardware/repo and `--yes`, with no supported deadline. `down` needs an ID and `--yes`.

```sh
gpu up --no-repo --mode container --gpu 'RTX 4090' --expire 2h --yes --wait --json
gpu exec 12345678 -- python -c 'print("hello")'
gpu cp 12345678 ./data :/workspace/data -r
gpu cp 12345678 :/workspace/results ./results -r
```

`--wait-setup` also waits for the clone and setup. `exec` returns the remote exit code; use `sh -c` for shell syntax.

With `--json`, each command prints one object, `{"command":...,"ok":...,"result":...}`, plus `error` on failure. `gpu ls --json` prints a bare array unless you add `--envelope`.

| Exit | Meaning |
| --- | --- |
| 1 | error |
| 2 | usage or missing flags |
| 10 | no capacity |
| 11 | over `--max-price` |
| 12 | not set up (`gpu setup`) |
| 13 | sign-in expired |
| 14 | readiness timeout |
| 15 | over `max_session_spend` or `max_daily_spend` |
| 16 | ID not found |
| 20 | `--command-timeout` or Modal helper timeout |

A create timeout does not mean nothing was created: check `gpu ls` before retrying.

## Configuration

`gpu setup` creates `~/.config/gpu/config.json` (or `$XDG_CONFIG_HOME/gpu/`). Keys override the defaults in `src/config.rs`:

```json
{
  "owner": "YOUR_GITHUB_USER",
  "disk": 20,
  "reliability": 0.95,
  "default_expire": "2h",
  "max_session_spend": 5,
  "max_daily_spend": 20
}
```

`max_session_spend` refuses a launch whose estimated cost up to its deadline exceeds that many USD. `max_daily_spend` bounds the total: a launch is refused (exit 15) when the estimated cost of every launch started in the last 24 hours plus this launch's estimate to its deadline exceeds that many USD, and the message states the three amounts. Each launch reserves its estimate in `~/.local/state/gpu/spend.json` (`$XDG_STATE_HOME/gpu/`) before its create request, under the same lock as the check, only while the cap is set; a launch the provider definitely rejected gives its reservation back, and one whose create is unverified (a timeout) keeps it; `gpu down` of a Vast instance replaces its estimate with the billed estimate, and entries older than 24 hours are pruned. A launch whose cost cannot be estimated (no deadline, unknown price, manual AMD workspaces) is refused when either cap is set. A ledger that cannot be read, parsed or written also refuses launches (exit 15) until it is fixed or removed; it is never treated as empty. `--dry-run` prints the window total and the remaining budget and records nothing. The cap counts estimates, not your bill. Keep secrets out of this file. SSH aliases go in `~/.ssh/vst_config`, `~/.ssh/gpu_gcp_config` and `~/.ssh/gpu_amd/`.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI also runs `cargo deny`, gitleaks, shellcheck, ruff and actionlint at versions pinned in `.github/workflows/ci.yml` and `scripts/ci-tool.sh`. Tests never touch real cloud accounts; test paid behavior by hand on throwaway instances with a 5-minute deadline.
