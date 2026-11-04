# gpu

Run `gpu up` to rent a Vast.ai GPU workspace, with or without a git repository. Choose a container for general work or a VM for CUDA/Nsight profiling. Google Cloud TPU VMs are also supported with `--provider gcp`; see [Google Cloud TPUs](#google-cloud-tpus) for the required one-time setup and offline expiry.

## Install

1. Install a [Rust toolchain](https://rustup.rs/), the [`vastai` CLI](https://github.com/vast-ai/vast-python), and [GitHub CLI](https://cli.github.com/). macOS and Linux are supported.
2. Authenticate `vastai` and run `gh auth login` for repository listing.
3. From this checkout, install and launch:

   ```sh
   cargo install --path . --locked
   gpu up
   ```

Launching rents a paid instance. The Vast confirmation shows the hourly price before creating it. TPU prices are not quoted; check Google's pricing before confirming. To preview without renting:

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
- On the Repository screen, **1–7** select the first seven repos, **8** opens **All repositories...** (when there are more than seven), **9** opens **Clone from URL...**, and **0** selects **No repository** at the bottom. Type to filter across all repos, including those not initially shown.
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

Both commands verify Vast's state for up to 60 seconds. TPU commands wait for the Google Cloud operation, then verify the node's state or disappearance. Failed or unverified completion exits nonzero and does not claim success.

## Auto-destroy a Vast instance

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

## Google Cloud TPUs

This adapter uses `gcloud compute tpus tpu-vm` (the Cloud TPU v2 API), not Compute Engine GPU VMs. It launches one TPU node, on-demand or Spot, with worker **0** used for SSH/editing. `v5e-1` is accepted as an alias for the API's `v5litepod-1`; `v6e-1` is passed through. Larger types may have multiple workers; distributed training orchestration is not included. Queued-resource provisioning, reservations, Google Cloud GPUs, AWS/Trainium and serverless `gpu run` are not implemented. Existing nodes owned by queued resources are listed, but `stop`, `down` and `expire` refuse to mutate them rather than leave an orphaned queued request.

### Explicit one-time setup (owner/admin only)

**No `gpu` command enables APIs, creates queues or service accounts, or changes IAM, billing or quotas.** Launch creates only the requested paid TPU and its deletion task. `expire` creates/replaces/deletes deletion tasks. Cloud infrastructure setup is a separate, explicit administrator action. Do not put service-account keys or local credentials on the VM.

1. Install and authenticate a recent [Google Cloud CLI](https://cloud.google.com/sdk/docs/install): `gcloud auth login`. Use a dedicated project with billing, TPU quota in the intended zone, a VPC/subnet, and firewall access for SSH. The adapter uses the default network and external IPs; IAP-only/private-network SSH is not currently supported. TPU quota is independent of GPU quota. A budget that disables billing is only a backstop: billing reports can lag by hours.
2. Have the project administrator review and run the following **mutating setup example**, substituting their project and email. It creates a dedicated queue and two distinct identities. The reaper can delete **any TPU node in this project**; it must not be attached to a VM. The VM identity has no workload API roles by default. Existing equivalent setup can be reused instead.

   ```sh
   PROJECT=gcp-sandbox-355rm   # example only; also set gcp.project below
   LOCATION=us-central1
   OWNER_EMAIL=you@example.com
   gcloud services enable tpu.googleapis.com compute.googleapis.com cloudtasks.googleapis.com iam.googleapis.com --project="$PROJECT"
   gcloud iam service-accounts create gpu-reaper --project="$PROJECT"
   gcloud iam service-accounts create gpu-workspace --project="$PROJECT"
   gcloud iam roles create gpuTpuReaper --project="$PROJECT" \
     --title='Delete TPU nodes for gpu expiry' --permissions=tpu.nodes.delete --stage=GA
   gcloud projects add-iam-policy-binding "$PROJECT" \
     --member="serviceAccount:gpu-reaper@$PROJECT.iam.gserviceaccount.com" \
     --role="projects/$PROJECT/roles/gpuTpuReaper"
   gcloud tasks queues create gpu-expiry --project="$PROJECT" --location="$LOCATION" \
     --max-attempts=-1 --max-retry-duration=0s --min-backoff=5s --max-backoff=60s
   gcloud iam service-accounts add-iam-policy-binding "gpu-reaper@$PROJECT.iam.gserviceaccount.com" \
     --project="$PROJECT" --member="user:$OWNER_EMAIL" --role=roles/iam.serviceAccountUser
   gcloud iam service-accounts add-iam-policy-binding "gpu-workspace@$PROJECT.iam.gserviceaccount.com" \
     --project="$PROJECT" --member="user:$OWNER_EMAIL" --role=roles/iam.serviceAccountUser
   gcloud projects add-iam-policy-binding "$PROJECT" \
     --member="user:$OWNER_EMAIL" --role=roles/cloudtasks.taskAdmin
   gcloud projects add-iam-policy-binding "$PROJECT" \
     --member="user:$OWNER_EMAIL" --role=roles/cloudtasks.viewer
   ```

   The launcher also needs TPU create/list/get/update/stop/delete permissions (for example `roles/tpu.admin`), service-account `actAs` for the VM, and the SSH/OS Login permissions required by the project. Existing project owners may already have these. Cloud Tasks' Google-managed service agent must retain `roles/cloudtasks.serviceAgent` so it can mint the reaper's OAuth token. These permissions are **not** verified by merely creating a task; perform the disposable acceptance check below before relying on expiry. Keep the queue dedicated to `gpu`: HTTP target/auth overrides are rejected. Queue pause, deletion, IAM changes and disabled billing/APIs can all defeat expiry.
3. Merge this into `~/.config/gpu/config.json` (or its XDG location):

   ```json
   {
     "gcp": {
       "project": "gcp-sandbox-355rm",
       "zone": "us-east5-b",
       "runtime": "v2-alpha-tpuv6e",
       "service_account": "gpu-workspace@gcp-sandbox-355rm.iam.gserviceaccount.com",
       "expiry": {
         "queue": "gpu-expiry",
         "location": "us-central1",
         "service_account": "gpu-reaper@gcp-sandbox-355rm.iam.gserviceaccount.com"
       }
     }
   }
   ```

   Project is **always read from config**, never hard-coded or inferred from the active gcloud project. Zone/runtime flags override their config values. Choose an image supported by the selected type; the example runtime is for v6e, not every TPU generation. Omitting `service_account` uses Google's default Compute service account; prefer an explicitly restricted workspace identity. Never give the workspace the reaper identity or upload your local gcloud credentials.

Read-only catalog checks (substitute your configured project/zone):

```sh
gcloud compute tpus locations list --project=gcp-sandbox-355rm
gcloud compute tpus tpu-vm accelerator-types list --project=gcp-sandbox-355rm --zone=us-east5-b
gcloud compute tpus tpu-vm versions list --project=gcp-sandbox-355rm --zone=us-east5-b
```

### Rent, connect, and destroy a TPU

```sh
gpu up --provider gcp                      # terminal choices, including a mandatory expiry
gpu up --provider gcp --tpu v6e-1 --no-repo --expire 2h --spot --dry-run
gpu up --provider gcp --tpu v6e-1 --no-repo --expire 2h --spot --yes
gpu ls                                    # labelled Vast and Google Cloud sections
gpu ls --provider gcp                      # works without Vast installed/logged in
gpu ls --provider vast --json              # legacy Vast JSON, unchanged
```

Non-interactive launches require `--tpu`, `--repo` or `--no-repo`, and `--expire`; zone/runtime must be supplied through config or flags. `--on-demand` skips the capacity picker; on-demand is the non-interactive default. With GCP configured, an interactive `gpu up` first offers a numbered provider picker. Without GCP config, the existing Vast flow and its table/JSON output remain unchanged. Use `--provider vast` to skip the provider picker. GPU offer, reliability, price and disk-size flags are rejected for TPUs rather than ignored.

The returned ID is `gcp:PROJECT:ZONE:NAME`. Copy it exactly (or omit the ID to pick). Project and zone embedded in the ID prevent later config changes from redirecting a destructive command:

```sh
gpu ssh gcp:PROJECT:ZONE:NAME --exec
gpu edit gcp:PROJECT:ZONE:NAME --editor code
gpu stop gcp:PROJECT:ZONE:NAME               # on-demand, where supported; does not cancel expiry
gpu down gcp:PROJECT:ZONE:NAME --yes         # delete TPU and boot disk
```

`gpu ls` discovers TPU locations throughout the configured project, including launches with `--zone` overrides. A provider failure is reported as an incomplete listing (nonzero exit), never as an empty account. Combined JSON adds `provider` to each record; TPU records use the existing `id`, `gpu_name`, `actual_status` and `label` fields. Unknown price/uptime are not invented. Spot/preemptible TPUs cannot be stopped and restarted; destroy them instead. Separately attached disks, addresses and other independently created resources may still bill after TPU deletion.

Capacity is not guaranteed by quota or a catalog entry. A stockout leaves its safety task in place because creation may be uncertain. Inspect `gpu ls --provider gcp` before retrying with `--spot` or another `--zone`; after confirming the original TPU is absent, `gpu expire gcp:PROJECT:ZONE:NAME cancel` cleans up the old task.

Google owns SSH provisioning: `gpu ssh` prints the exact gcloud command; `--exec` runs it, using worker 0. `gpu edit` requires a READY node, probes with gcloud, and translates its resolved user/key/host into `~/.ssh/gpu_gcp_config`. It opens `/workspace` or the recorded repository directory with the same editor choices as Vast. Public repositories are cloned by a startup script; no GitHub or cloud credentials are transferred. The workspace is writable by VM users; use only trusted users on a workspace VM. No CUDA/Nsight installation or automatic TPU framework installation is performed; choose/configure the appropriate TPU runtime. Repository bootstrap failure does not disable expiry.

### TPU expiry: control-plane deletion, not a guest timer

```sh
gpu expire gcp:PROJECT:ZONE:NAME 1h30m
gpu expire gcp:PROJECT:ZONE:NAME 18:30
gpu expire gcp:PROJECT:ZONE:NAME show
gpu expire gcp:PROJECT:ZONE:NAME cancel
```

Scheduling authorizes destruction of the TPU and its boot disk. The same local-time/DST syntax as Vast applies. TPU launch **requires expiry**; `cancel` explicitly removes that protection. Deadlines must be 1 minute–29 days ahead. Flag durations start at CLI invocation; a duration entered interactively starts when submitted. Selection and provisioning consume that time, never extend it. Cloud Tasks stores the deadline rounded **down** to a whole second.

Cloud TPU v2's node `schedulingConfig` has no native max-run-duration or termination timestamp. Queued-resource `valid-until` controls provisioning validity, not a general running-node destruction deadline. Instead, `gpu` checks the dedicated Cloud Tasks queue, creates an authenticated HTTP **DELETE** task targeting the exact TPU node, and reads it back **before** sending the paid create request. Its OAuth token is minted by Google for the dedicated reaper service account at dispatch. Neither the laptop nor the TPU VM supplies credentials or needs to stay online. Expiry survives laptop sleep, VM shutdown, Spot preemption, guest crashes and missing SSH. The VM never receives the reaper's credentials.

Safety limits:

- **Not an exact-time or billing SLA.** Cloud Tasks dispatch may be delayed; IAM/API/billing failures, queue pauses, rate limiting and regional outages can prevent deletion. Queue readiness and task read-back do not prove future authorization. A successful HTTP response acknowledges a long-running TPU delete operation, not its completion: a later operation failure is not retried by the task. Independently verify disappearance and billing.
- The queue must have unlimited retries (`maxAttempts=-1`, no retry-duration limit), max backoff at most 60 seconds, no HTTP target overrides, and state RUNNING. Failed HTTP calls retry, but Google may increase backoff. Tasks have a **31-day retention limit from creation**, even with unlimited retries; the 29-day deadline cap leaves retry headroom. Missing nodes also return errors and can leave retrying tasks after a failed launch or externally performed deletion.
- Do **not reuse a TPU name** until all its deletion tasks are gone: the API DELETE targets a resource name, not an immutable instance generation. CLI launches use new randomized names. A pre-armed task remains if the laptop disconnects or creation fails; this is deliberate, not proof that a TPU exists.
- `show` reports pending tasks and queue state, not a guest heartbeat or proof of IAM health. No pending task can mean it already dispatched; inspect `gpu ls --provider gcp`. Once a delete is in flight, cancel cannot undo it.
- Replacing a deadline creates/verifies the replacement **before** deleting old tasks. Interruption, concurrent changes or cleanup failure can leave multiple tasks; the earliest wins. Inspect `show` after any error. Do not run concurrent expiry changes on the same TPU.
- The launch records its queue in TPU metadata. Keep expiry config until cleanup finishes, especially after failed launches or external deletion, when no node metadata remains. Manually created nodes without that metadata use the current configured queue.
- `gpu down` cancels tasks only **after verifying node disappearance**. Automatic deletion cannot remove your laptop's SSH alias or independently created disks. Cloud Tasks usage itself may incur small charges. The $50 project budget, if configured, is not a substitute for a tested timer.

Sources: [TPU node scheduling fields](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes#SchedulingConfig), [TPU VM create](https://cloud.google.com/sdk/gcloud/reference/compute/tpus/tpu-vm/create), [Cloud Tasks HTTP/OAuth task creation](https://cloud.google.com/sdk/gcloud/reference/tasks/create-http-task), [retry semantics](https://cloud.google.com/tasks/docs/configuring-queues), [task limits](https://cloud.google.com/tasks/docs/quotas), [TPU delete operation](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes/delete).

**Owner-only paid acceptance check, not automated tests:** after setup, launch a disposable small TPU with `--expire 5m`, verify `show`, disconnect the laptop, and independently confirm deletion after the deadline. Repeat with on-demand stop and (where available) Spot. Check each supported runtime's SSH/editor/repository bootstrap. Test replacing a deadline, cancelling twice, manually destroying, and confirming task cleanup. Inspect Cloud Tasks attempt status and the TPU delete operation on failure; never expose tokens. These live checks have not been performed by the test suite.

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

Tests use `Runner` mocks and PATH stubs, never real Vast or Google Cloud accounts. Use a temporary HOME for SSH config tests. `tests/golden/` pins serialization, numeric formatting, and other byte contracts; update prose expectations deliberately, not unrelated formats. `docs/PLAN.md` is historical implementation context.

### Workspace and local files

`src/startup.rs` generates startup scripts. Repositories clone into `/workspace/<basename>` with `.git` removed; bare names use the configured owner. `uv sync` runs only if that directory has `pyproject.toml`. No-repo launches still create `/workspace` and configure git identity and conditional `GH_TOKEN` authentication. Containers set up the entrypoint and workspace shell directory; VMs install CUDA 12.8 and Nsight Compute 2025.4.0.

`src/ssh_config.rs` manages `vast-<id>` aliases in `~/.ssh/vst_config`, included before existing Host rules in `~/.ssh/config`. `gpu down` removes the alias; remote expiry cannot clean it while the computer is offline. Successful mode, GPU, and editor picks live in `$XDG_STATE_HOME/gpu/history.json` (default `~/.local/state/gpu/history.json`). Repository names are cached under `$XDG_CACHE_HOME/gpu/repos-<owner>.json`; cached picks are checked against a fresh GitHub listing. GPU models combine the bundled list and the offer-query cache. Slow work runs in picker `Job`s, never on its input thread.

### Expiry worker and credentials

`src/expiry.rs` sends `remote/expiry_remote.sh` over SSH stdin. The Linux host needs bash, curl, flock, setsid, writable `~/.local/state/gpu-expire`, and outbound HTTPS. No init manager or package installation is needed. The detached worker's readiness handshake and `/proc` identity are verified before success. State stores the deadline and worker identity, never the key. A shared lock serializes replacement, cancellation, and destruction.

Local network calls use authenticated `gh`, `vastai`, and (for TPUs) `gcloud`. TPU expiry runs in Cloud Tasks with a dedicated service account, not on the VM. The Vast remote expiry worker is the exception: it uses only Vast-injected `CONTAINER_ID` and `CONTAINER_API_KEY`, from its environment, `/etc/environment`, or PID 1. ID mismatch or a missing scoped key fails closed. No account key, local Vast config, or create-response key is uploaded. The key reaches curl on stdin, not argv; requests use no proxy or redirects. Never print raw Vast output or remote diagnostics that could contain credentials. Neovim release archives use pinned SHA-256 digests in `src/editors.rs`.

Vast references: [instance-scoped keys](https://docs.vast.ai/api-reference/hello-world), [container environment](https://docs.vast.ai/guides/instances/docker-environment), [destroy endpoint](https://docs.vast.ai/api-reference/instances/destroy-instance), [VM environment](https://docs.vast.ai/linux-virtual-machines).

Paid, destructive acceptance checks are manual only. Use disposable container and VM instances:

1. Arm a 5-minute deadline and show its status.
2. Replace it with 10 minutes, cancel twice, and verify the instance survives the original deadline.
3. Arm again, disconnect, and verify disappearance in `gpu ls` and Vast.
4. Test a local clock deadline, then cancel it.
5. Reboot with an armed deadline and verify the timer is inactive. Never expose credentials while investigating.

Exit codes: 0 for success or declining a confirmation; 1 for errors, picker quit, or confirmation EOF; 130 for Ctrl-C; 2 for usage errors. Errors use credential-redacted `gpu: <message>` on stderr. Dry runs print JSON argument arrays, not shell commands. Preserve table and JSON formats used by scripts.
