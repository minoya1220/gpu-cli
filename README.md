# gpu

Run `gpu up` to rent a Vast.ai GPU workspace, with or without a git repository. Choose a container for general work or a VM for CUDA/Nsight profiling. Choose **TPU (Google Cloud)** on the GPU screen to rent a TPU VM instead, or use `--tpu` / `--provider gcp`; see [Google Cloud TPUs](#google-cloud-tpus) for the required one-time setup and offline expiry.

## Install

1. Install a [Rust toolchain](https://rustup.rs/), the [`vastai` CLI](https://github.com/vast-ai/vast-python), and [GitHub CLI](https://cli.github.com/). macOS and Linux are supported.
2. Authenticate `vastai` and run `gh auth login` for repository listing.
3. From this checkout, install and launch:

   ```sh
   cargo install --path . --locked
   gpu up
   ```

Launching rents a paid instance. The Vast confirmation shows the hourly price before creating it. TPU confirmation shows the current public USD list-price estimate from Google's Billing Catalog, or an explicit “price unavailable” reason if it cannot be read. To preview without renting:

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

- Type to filter, use ↑/↓ to move, and → or Enter to select. Backtick does nothing.
- Press ← to clear the filter, then go back. On the first screen, press twice to quit. Ctrl-C or Ctrl-D quits from any screen; Esc does nothing.
- The GPU screen shows at most **three Recent** rows, GPU categories, and **TPU (Google Cloud)**. Type `tpu` to find the TPU category, `v5e` for a generation, or `v6e-1` for a full TPU type. TPU discovery is visible even before Google Cloud is configured. Before Google Cloud setup, selecting it keeps a notice pointing to `gpu setup` on screen, including the exact config file path; no Google Cloud command runs and no TPU is created. Press ← to return to the GPU choices, or Ctrl-C to quit.
- GPU/TPU price and TPU catalog reads start when the launch picker opens, while you choose Repository/Mode, without blocking input. TPU rows need `gpu setup`; cached estimates show their age. Choose a TPU generation, size and priced capacity, then its area and priced zone. Digits filter names; on short, unfiltered lists, 1-9 select directly.
- On the Repository screen, **1–7** select the first seven repos, **8** opens **All repositories...** (when there are more than seven), **9** opens **Clone from URL...**, and **0** selects **No repository** at the bottom. Type to filter across all repos, including those not initially shown.
- Choose **No repository** or **Clone from URL...** without waiting for GitHub. Paste a URL or `owner/name`, then Enter. Backspace edits; arrows select or go back, not move the text cursor.
- Press → to retry a failed offer load; Ctrl-R retries repositories. Enter submits repository URLs and TPU durations. Paid launch approval needs **yes + Enter**; → never authorizes it. Press ← to restore the previous page, even after typing. Terminal text prompts decode arrow keys without echoing escape codes. Piped input keeps numbered line prompts.

GPU and TPU rows share aligned name, price, Spot and offer-count columns. `-` means unquoted, not unsupported; Vast's Spot column is not quoted yet. Offers are sorted cheapest first and show hourly price, reliability, verification, ID, and location. Up to ten are shown. Launch confirmation stays on the same screen stack, so ← restores the previous picker page with its filter and cursor. Launch checks run in background Jobs before approval. Recent successful choices appear first; cancellations and dry runs do not change history.

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

Omit the ID to pick an instance, even if only one exists. `gpu ls` shows one aligned table with provider, type, lowercase status, hourly price and uptime. TPU IDs are shortened when unambiguous. `gpu ls --long` keeps full IDs; `gpu ls --json` prints credential-redacted JSON with full IDs and original numeric precision. `gpu cursor ID` is an alias for `gpu edit ID --editor cursor`.

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

Both commands verify Vast's state for up to 60 seconds. TPU commands wait for the Google Cloud operation, then verify the node's state or disappearance. TPU creation (including retries), stopping and deletion show an elapsed timer on terminal stderr, or one progress line when redirected. Failed or unverified completion exits nonzero and does not claim success.

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

```sh
gpu setup                                # select Google Cloud and project
gpu up --tpu v6e-1 --no-repo --expire 2h --spot --dry-run
gpu up --tpu v6e-1 --no-repo --expire 2h --spot --yes
gpu ls --provider gcp
```

### Set up once

1. Install [gcloud](https://cloud.google.com/sdk/docs/install) and run `gcloud auth login`.
   Use an **existing disposable project** with billing, TPU quota, a default
   VPC/subnet and SSH firewall access. GPU quota does not cover TPUs.
2. Run `gpu setup`, select Google Cloud, then review the project-scoped plan.
   Type the project ID to approve. No TPU or project is created.
3. Keep the undo journal in `~/.config/gpu/config.json` (macOS/Linux), or
   `$XDG_CONFIG_HOME/gpu/config.json`. Other config stays intact.

**No ordinary command invokes gcloud until setup succeeds.** The offline GPU
screen still shows TPU choices; without setup, their price says `needs gpu setup`.
A hand-written legacy `gcp` section is not consent. Project always comes from
config, never gcloud's ambient project.

Setup enables missing Compute, TPU, Cloud Tasks and Cloud Billing APIs. Its
**only explicit resource is the `gpu-expiry` queue**: unlimited retries, backoff
at most 60s, no HTTP target overrides. Compatible existing queues are reused
unchanged; paused/conflicting queues are refused. It does not create identities,
roles, IAM grants, networks or firewalls, or change billing/quotas. API enablement
may cause Google to provision default identities, networks and service agents;
undo leaves these Google-managed defaults alone.

Both VM and expiry use the existing default Compute identity,
`PROJECT_NUMBER-compute@developer.gserviceaccount.com`. **VM users inherit all
its roles**, potentially including project-wide TPU deletion and other access.
Use trusted workloads/users only. No local credentials or service-account keys
are uploaded.

```sh
gpu setup --provider gcp --project PROJECT --location us-central1 --dry-run
gpu setup --provider gcp --project PROJECT --location us-central1 --confirm-project PROJECT
```

`--location` is only the Cloud Tasks queue region. Setup does not ask for, save,
or validate a TPU zone: that belongs to launch. Existing explicit `gcp.zone`
settings remain compatible; setup's old `--zone` flag now points to launch.

At launch, choose **generation → size → capacity → area → zone**, then the
deadline. Generation and size are remembered per project. Aligned size rows show
prices for the displayed reference region; `-` means no price is available.
Spot/on-demand estimates load without blocking input. v5e/v6e suffixes count **chips**; v2/v3/v4/v5p suffixes count
**TensorCores** (two per chip), not pods. Rows show both counts where applicable.
Full `--tpu v6e-1` / `--type v6e-1` names still work and skip generation/size;
capacity flags skip their screen. Areas include only zones offering that
type; counts are matching zones, not stock. → opens that area's zones,
each priced for the chosen type and Spot/on-demand mode. There are no per-zone
type badges. Typing a full zone name on the area screen searches matching zones
directly. ← returns one screen at a time, including from the TPU screens to the
GPU picker that opened them. A direct `--provider gcp` invocation has no earlier GPU page.

The last chosen zone is remembered per project in local history. Its area and
zone are highlighted when they still offer the selected type, without changing
nearest-first list order. Otherwise the nearest valid choice is highlighted.
Legacy configured zones remain a fallback. Zone history is separate from setup
and cannot change the queue region.

Zones sort approximately nearest-first from the local IANA timezone and region
city coordinates (not measured latency). There are no network probes/geolocation
services. `TZ` overrides cross-platform timezone discovery; embedded public-domain
`zone1970.tab` supplies timezone coordinates. Unknown regions sort last; unknown
timezones keep default/saved ordering.

<details>
<summary>Permissions, partial setup and undo</summary>

Setup reads the identity and project IAM policy. It requires a recognized
unconditional project `roles/tpu.admin`, `roles/editor` or `roles/owner` grant.
Conditional, inherited and custom grants need administrator review; deny policies
can still block expiry. Setup never repairs IAM. The launcher also needs TPU
lifecycle, Cloud Tasks create/list/get/delete, service-account `actAs`, and SSH/
OS Login permissions. Google's Cloud Tasks service agent needs its existing
`roles/cloudtasks.serviceAgent` grant for OAuth dispatch. Queue/task read-back
does not prove future authorization.

Dry runs only read; non-interactive setup requires `--confirm-project` to write.
Intent is journaled **before** mutations. Failed steps retain earlier changes;
rerun setup to resume. Keep the journal and avoid concurrent setup/cloud edits.
Remove setup before switching project or queue region. Do not copy completion
markers between projects.

To undo, choose **Remove gpu setup**, or run:

```sh
gpu setup --provider gcp --remove --project PROJECT --confirm-project PROJECT
```

Removal checks one complete cross-location TPU inventory and all pending queue
tasks. Owned TPUs or any pending task block removal: destroy the TPU, or verify
absence and run `gpu expire ID cancel`. Read failures and disabled APIs never
prove absence; restore access/API first. Removal deletes only an empty queue
**it created**, disables only APIs **it enabled**, and removes local `gcp` config.
Pre-existing APIs/queues, other config, projects, IAM, networks, billing and
quotas stay untouched; dependent APIs are never force-disabled. Interrupted
removal keeps its journal and blocks ordinary cloud commands until resumed.
`--remove --dry-run` previews this. Deleting the journal is not undo.
Google can reserve a deleted queue name for **seven days**, delaying re-setup.

</details>

### Rent, connect, destroy

`gpu up` keeps the repository → mode → GPU flow, with TPU choices alongside
Vast GPUs. `--provider gcp` skips to TPU choices; TPU flags imply that provider.
The UI's container/VM choice is ignored for a TPU, but explicit Vast-only flags
are rejected. `--provider vast` rejects TPU flags. Non-interactive TPU launches
need `--tpu`, `--repo` or `--no-repo`, `--expire`, and `--zone` (or a previous
project-zone pick / legacy configured zone).
On-demand is the default; `--on-demand` skips the capacity picker.

Real launches verify the exact type/zone **live before confirmation or arming**.
Fully specified dry runs are offline; interactive pickers may read catalogs/prices.
Stock/quota failures can still occur. Runtime is automatic:
v2/v3/v4 `tpu-ubuntu2204-base`, v5e `v2-alpha-tpuv5-lite`, v5p `v2-alpha-tpuv5`,
v6e `v2-alpha-tpuv6e`. `v5e-N` aliases `v5litepod-N`. Use `--runtime IMAGE` to
override or support a new generation; old saved `gcp.runtime` is ignored.

Short TPU IDs from `gpu ls`, such as `gpu-uwhcp7`, work with `ssh`, `edit`,
`cursor`, `stop`, `down` and `expire`. They resolve against a fresh inventory of
the **configured project**. Ambiguous IDs fail without acting; the table falls
back to a longer, unique name or full ID. Full `gcp:PROJECT:ZONE:NAME` IDs from
`gpu ls --long` or `--json` retain their embedded scope after config changes and
skip this lookup. Omit IDs to pick interactively.

```sh
gpu ssh gcp:PROJECT:ZONE:NAME --exec
gpu edit gcp:PROJECT:ZONE:NAME --editor code
gpu stop gcp:PROJECT:ZONE:NAME             # on-demand only; expiry stays armed
gpu down gcp:PROJECT:ZONE:NAME --yes       # deletes node + boot disk
```

`gpu ls` uses one cross-location node list, including zone overrides. Queue
inventory runs concurrently with the same in-memory OAuth token; orphan cleanup
reuses that node snapshot, not a GET per task. Partial/failed reads exit nonzero,
not “empty account.” Combined JSON adds `provider`; Vast-only JSON stays unchanged.
TPU prices are cached compute estimates for the node's type and capacity mode,
with no extra price requests during listing. TPU uptime is age since creation,
including stopped time, not billable runtime. Missing prices/timestamps show `-`.
Separate disks/addresses may still bill.

A known create rejection plus authenticated not-found removes its timer;
ambiguous/time-out results keep it. `ls`, TPU `up`, `down` and `expire` reconcile
only owned tasks whose exact node is absent from a complete authenticated
inventory. Failed checks, existing nodes, unrelated tasks and tasks armed within
60s are kept. After uncertain creation, inspect `gpu ls --provider gcp`.

After a verified capacity rejection and timer cleanup, interactive launches offer
the next-nearest zone with the same type and capacity mode. Its price is shown;
one key approves the retry, or choose another zone. Previously tried zones are
omitted. Retries keep the original deadline, runtime and repo; a changed price
requires confirmation again. Non-interactive errors name verified alternative
zones and a different command with the original absolute deadline. Unverified
creates or timer cleanup never offer another paid launch.

Worker **0** handles SSH/editing, including multi-worker types. Successful
interactive launches offer the same editor picker as Vast. `gpu ssh` prints the
command; `--exec` runs it. Editing waits up to 120 seconds for CREATING → READY
and SSH, with progress on stderr. It retries boot/key-propagation failures;
stopped, deleted, or preempted nodes and IAM errors fail without starting a VM
or changing its expiry. gcloud provisions SSH keys and resolves the user; the
editor then reuses that connection through `~/.ssh/gpu_gcp_config`.
IAP-only/private SSH is unsupported. Public repos clone
into `/workspace/NAME`; no GitHub/cloud credentials transfer. VM users share a
writable workspace. Install your own TPU framework; no CUDA/Nsight setup runs.
Repo bootstrap failure does not disable expiry. Distributed orchestration,
Google GPUs, reservations and queued-resource provisioning are unsupported;
queued-resource-owned nodes cannot be stopped, destroyed or re-armed here.

### Prices and cached facts

Vast and TPU prices load **automatically in background Jobs**. Choices accept
input immediately; `…` becomes a price or `price unavailable`. Ctrl-R retries
failures; revisiting a screen reuses its Job/cache. There is no F2 step.

`src/data/tpu_facts.json` bundles **no dollar amounts**: zone/type mappings and
SKU metadata for on-demand/Spot usage. Generations are derived from SKU metadata;
reviewed region coordinates live in `src/data/gcp_region_locations.csv`.
Live caches extend types and override known zones. Project catalogs refresh
in the background after setup (15-minute cache); SKU metadata lasts 30 days.
A stale snapshot can never bypass live paid-launch preflight.

Dollar values come from Google's authenticated [public SKU price API](https://cloud.google.com/billing/docs/reference/pricing-api/rest/v1beta/skus.price/get),
using known [Catalog SKU IDs](https://cloud.google.com/billing/v1/how-tos/catalog-api).
Latest prices are fetched in parallel, cached **per project/SKU for 10 minutes**,
and reused across regions sharing a SKU. Cached estimates show their age.
Quotes cover the whole slice, distinguish on-demand/Spot and match its region.
Cloud TPU VM-hours and Compute Engine TPU chip-hours are normalized separately.
All are USD list rates: storage, network, taxes and account discounts excluded.
Missing, ambiguous, tiered, inaccessible or unsupported-unit rates are unavailable,
never guessed; an explicitly confirmed paid launch may proceed without a quote.

Needs local curl **7.66+** (`--parallel`). OAuth tokens stay in memory and go to
curl on stdin, never argv, logs or caches. User curl config, proxies and redirects
are disabled. Rerun old setup to explicitly enable Cloud Billing; ordinary
commands never enable it or grant access silently.

To regenerate facts (read-only; requires completed setup and Python 3):

```sh
python3 tools/update_tpu_facts.py --project PROJECT
```

The script queries TPU/Billing metadata, strips prices/account details and unused
fields, and writes one compact record per line. Offline input
is supported with `--input FILE`. It uses v1 location discovery (matching gcloud;
v2 omits the v4-only zone), then v2 accelerator types. Review the diff before
committing. A refresh failure leaves the existing snapshot usable.

### Expiry: works with laptop and VM offline

```sh
gpu expire gcp:PROJECT:ZONE:NAME 1h30m
gpu expire gcp:PROJECT:ZONE:NAME 18:30
gpu expire gcp:PROJECT:ZONE:NAME show
gpu expire gcp:PROJECT:ZONE:NAME cancel
```

Launch **requires** expiry, 1 minute–29 days ahead; scheduling authorizes node
and boot-disk deletion. Local-time/DST rules match Vast. Flag durations start at
invocation; typed durations start on submission. Selection/boot consume that
time, never extend it; Cloud Tasks rounds down to a second. Cancel removes
protection but cannot stop deletion already in flight.

Before paid creation, gpu creates and verifies a Cloud Tasks OAuth **DELETE**
task for the exact node. Google mints its dispatch token; neither laptop nor
guest credentials are needed. Stops, Spot preemption, missing SSH and guest
crashes do not disarm it. Cloud TPU nodes have no native running-time deadline;
queued-resource `valid-until` only limits provisioning.

<details>
<summary>Expiry safety limits: verify deletion independently</summary>

- **Not an exact-time or billing SLA.** Pauses, IAM/API/billing failures, rate
  limits and regional outages can delay/prevent deletion. HTTP success only
  acknowledges a long-running delete operation; later operation failure is not
  retried. Independently check disappearance and billing. A project budget is
  only a backstop; reporting may lag hours.
- Unlimited retries still have **31-day retention from task creation**; the
  29-day deadline cap leaves headroom. Google may increase backoff. Missing-node
  DELETE returns 404, but Cloud Tasks needs 2xx: external deletion may leave a
  retrying task until reconciliation/retention expiry. These retries usually
  cost little and are not a live TPU. No hosted handler is added.
- **Never reuse a node name while its tasks remain.** DELETE addresses a name,
  not an immutable generation. Launches use randomized names. A pre-armed task
  after disconnection/unverified create is deliberate, not proof a node exists.
- `show` reports pending tasks/queue state, not guest or IAM health. No pending
  task may mean it dispatched. Replacements arm/verify first, then remove old
  tasks. Cleanup failure or concurrent expiry edits can leave several; the earliest
  wins. Inspect `show` after errors; avoid concurrent expiry edits.
- Keep expiry config until cleanup, especially after failed creation/external
  deletion. Node metadata records its queue; manual nodes fall back to config.
  `gpu down` cancels tasks only after verified disappearance. Remote deletion
  cannot remove laptop SSH aliases or independent disks. Cloud Tasks can bill
  small usage charges.

Sources: [node scheduling](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes#SchedulingConfig),
[OAuth tasks](https://cloud.google.com/sdk/gcloud/reference/tasks/create-http-task),
[retries](https://cloud.google.com/tasks/docs/configuring-queues),
[limits](https://cloud.google.com/tasks/docs/quotas),
[delete operations](https://cloud.google.com/tpu/docs/reference/rest/v2/projects.locations.nodes/delete).

**Owner-only paid acceptance test (not run by automated tests):** launch a
small disposable TPU with `--expire 5m`, verify `show`, disconnect, and independently
confirm deletion. Repeat stopped/on-demand and Spot cases, each runtime's
SSH/editor/bootstrap, deadline replacement, double cancellation, manual deletion
and task cleanup. Inspect dispatch/delete-operation status on failures; never
expose tokens.

</details>

## Configuration

Edit `~/.config/gpu/config.json` on macOS or Linux, or `$XDG_CONFIG_HOME/gpu/config.json` when `XDG_CONFIG_HOME` is nonempty. An unset or empty XDG variable falls back to `$HOME/.config`; only explicit `gpu setup` creates the config file. Only supplied keys override defaults:

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

Local network calls use authenticated `gh`, `vastai`, and (for TPUs) `gcloud`, plus authenticated, read-only curl calls to Google's TPU, Cloud Tasks and public pricing APIs. TPU expiry runs in Cloud Tasks using the project's default Compute identity, not in a guest worker. The Vast remote expiry worker is the exception: it uses only Vast-injected `CONTAINER_ID` and `CONTAINER_API_KEY`, from its environment, `/etc/environment`, or PID 1. ID mismatch or a missing scoped key fails closed. No account key, local Vast config, or create-response key is uploaded. The key reaches curl on stdin, not argv; requests use no proxy or redirects. Never print raw Vast output or remote diagnostics that could contain credentials. Neovim release archives use pinned SHA-256 digests in `src/editors.rs`.

Vast references: [instance-scoped keys](https://docs.vast.ai/api-reference/hello-world), [container environment](https://docs.vast.ai/guides/instances/docker-environment), [destroy endpoint](https://docs.vast.ai/api-reference/instances/destroy-instance), [VM environment](https://docs.vast.ai/linux-virtual-machines).

Paid, destructive acceptance checks are manual only. Use disposable container and VM instances:

1. Arm a 5-minute deadline and show its status.
2. Replace it with 10 minutes, cancel twice, and verify the instance survives the original deadline.
3. Arm again, disconnect, and verify disappearance in `gpu ls` and Vast.
4. Test a local clock deadline, then cancel it.
5. Reboot with an armed deadline and verify the timer is inactive. Never expose credentials while investigating.

Exit codes: 0 for success or declining a confirmation; 1 for errors, picker quit, or confirmation EOF; 130 for Ctrl-C; 2 for usage errors. Errors use credential-redacted `gpu: <message>` on stderr. Dry runs print JSON argument arrays, not shell commands. Preserve table and JSON formats used by scripts.
