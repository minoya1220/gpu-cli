# AMD provider live-test checklist

**Not run. Requires separate captain authorization. Workers must not rent.** Both
backends remain experimental/untested until evidence below is recorded. Use only
one MI300X per provider; no multi-GPU substitute, spot, MI325X/MI355X, Vast AMD or
silent cross-provider fallback.

## Budget and preparation

- Target usage: one hour each, Runpod approximately $2.39 + 20 GB container disk
  and DigitalOcean approximately $2.59: **about $5 total usage**. Hard ceiling:
  **$15 including storage/network/tax**. Deposits are not usage; Runpod may require
  a non-refundable $10 deposit. Confirm current prices before approving funding.
- Verify account access, GPU quota, current hourly quote and region/datacenter
  stock. A catalog stock hint is not an allocation guarantee. Abort if current
  minimum rental, size or expected usage cannot fit the ceiling.
- Use a throwaway account/project where possible. Authenticate using the provider
  CLI's own config/environment. Never paste credentials into this document,
  `gpu/config.json`, logs, user-data or the guest. Do not save raw CLI responses:
  they can contain tokens/instance keys. Capture only an explicit safe field list.
- Record CLI versions, timestamp, selected datacenter/region, price, disk size,
  image digest/ID and qualified resource ID. Public SSH key only; use a disposable
  registered key. gpu's managed aliases currently use `~/.ssh/id_ed25519`.
- Keep a separate provider-console session available for emergency deletion.
  These manual launches have **no automatic deadline**. Remain online and set a
  human timer for at most one hour per resource. This is an operational check,
  not an expiry implementation or a guarantee if the operator goes offline.
- Remove `default_expire` for the manual test. Do not disable a spending guardrail
  casually: a configured `max_session_spend` intentionally rejects a manual launch
  without a supported deadline. Obtain explicit approval for this exception.

## Runpod Secure Cloud

- [ ] `gpu setup --provider runpod`: supported CLI, authenticated inventory and
  registered `~/.ssh/id_ed25519.pub`; no account key printed or read by gpu.
- [ ] `gpu offers --provider runpod`: record Secure MI300X price and exact
  datacenter stock; never use the disabled Community price as a quote.
- [ ] Set `runpod.image` to an explicitly selected ROCm image (record its digest)
  providing bash, sshd, SFTP subsystem, git and tar; set `runpod.datacenter`.
  Preview `gpu up --provider runpod --gpu MI300X --no-repo --dry-run --json`.
  Check 1 GPU, Secure, port `22/tcp`, 0 persistent volume, container disk and
  storage-inclusive hourly estimate. Confirm no NVIDIA startup command.
- [ ] Authorized operator only: launch with `--yes`, record `runpod:ID` as soon
  as create returns; never retry an uncertain create until inventory is checked.
- [ ] Measure image-pull/container-ready/direct-authenticated-SSH time separately.
  Confirm the CLI's stock hint actually allocated one MI300X. Inspect image
  entrypoint/CMD compatibility; verify `/workspace/.gpu-setup` becomes `done`.
- [ ] Prove a public TCP mapping of container port 22 and an actual sshd. Basic
  `ssh.runpod.io` proxy access alone does **not** pass this gate.
- [ ] `gpu wait runpod:ID --wait-setup`, `gpu exec ... -- PROGRAM`,
  `gpu ssh ... --exec`; test code/cursor/zed or nvim editor handoff.
- [ ] `gpu cp` upload/download a small file with spaces and a directory; compare
  hashes both ways and exercise SFTP/rsync. Do not substitute croc/proxy transfer
  for proving the shared OpenSSH transport works.
- [ ] Run the ROCm/kernel/counter checks below while still inside the budget.
- [ ] Optional within the same budget: save outputs locally before stop; confirm
  container filesystem loss and retained-storage charges. Stop is not deletion.
- [ ] `gpu down runpod:ID --yes`: verify absent in a fresh authenticated complete
  inventory and in the console. A successful delete request alone is insufficient.
  Inspect network volumes and any separately retained storage; delete only test
  resources explicitly authorized for deletion. Recheck actual final billing.

## DigitalOcean GPU Droplet

- [ ] `gpu setup --provider do`: supported doctl, authenticated inventory and
  registered key. Set `do.ssh_keys` to that key's ID/fingerprint, `do.region` to
  a region offering the single-MI300X size (research: `atl1`).
- [ ] `gpu offers --provider do`: exact `gpu-mi300x1-192gb`, price and region;
  verify account GPU access/quota. No eight-GPU substitute or model inference
  from a price table. Research price was $2.59/hour; current quote governs.
- [ ] Preview `gpu up --provider do --gpu MI300X --no-repo --dry-run --json`.
  Confirm `gpu-amd-base` was resolved to an image ID, one GPU, intended region/key,
  no account PAT in cloud-init, no CUDA install and no independent volume.
- [ ] Authorized operator only: launch, immediately record `do:REGION:ID`.
  Measure provisioning/cloud-init/real root SSH readiness separately. Record
  image ID, OS/kernel, ROCm/amdgpu versions; the base-image slug is rolling.
- [ ] Repeat wait/setup-marker, exec, SSH/editor and bidirectional file/directory
  copy/hash checks above against the public address. Test a public repository
  clone separately if that is the intended workspace workflow.
- [ ] Run the ROCm/kernel/counter checks below. If Docker is intended, repeat
  inside the intended ROCm container on the VM.
- [ ] Confirm documentation/billing: power-off/shutdown continues **full GPU
  billing**. Do not use shutdown as an emergency budget/expiry substitute.
- [ ] `gpu down do:REGION:ID --yes`: fresh authenticated complete inventory and
  console must confirm absence. Explicitly inspect remaining block volumes,
  snapshots/backups and other test resources; record final charges. Authentication
  failure, timeout or a stale/paginated response does not prove deletion.

## ROCm and actual hardware counters (each provider separately)

- [ ] Record GPU PCI IDs, count = 1, expected full 192 GB memory, kernel/amdgpu,
  ROCm version, image digest/ID, tenancy and effective user/capabilities. Confirm
  required `/dev/kfd` and `/dev/dri` exposure on containers.
- [ ] Compile and run a tiny deterministic HIP kernel with checked results.
  `amd-smi`, a PyTorch import or API tracing alone does not establish PMC support.
- [ ] Run `rocprofv3 --list-avail`. Pick an event actually listed on that GPU;
  collect it on the normally exiting HIP program (for example
  `rocprofv3 --pmc SQ_WAVES -- ./hip-smoke` **only if SQ_WAVES is listed**).
  Preserve nonempty, plausible hardware-counter records and exact tool versions.
- [ ] Run that installed version's documented `rocprof-compute profile` flow on
  the same replay-safe kernel; inspect derived/cache/throughput data. Record any
  required permissions and multi-pass behavior, not just a successful exit code.
- [ ] Repeat under the intended SSH user/container settings. Do not alter shared
  host clocks, security/performance policy or other tenants' state.

## Expiry proofs — remain disabled until a later reviewed implementation

These are investigation gates, **not features in this release**. Both live
attached `gpu run` and expiry changes must continue to refuse before spending.

- [ ] Runpod: obtain provider confirmation and, only on the exact authorized test
  Pod/image, prove the injected credential is Pod-scoped, bound to the current
  Pod ID and allowed to self-delete. Never copy the local account key or test
  destructive access against another resource. Do not record the credential.
- [ ] Runpod: design/review/test early boot arming before arbitrary setup, strict
  ID binding, no account-key fallback, retries, locks, cancellation/rearm/readback,
  interruption and credential injection failure; separately document guest/host
  failure and network-denial limits. A scoped-key overview is not this proof.
- [ ] DigitalOcean: investigate provider-native TTL or a genuinely resource-scoped
  deletion credential. If unavailable, document durable external deletion options
  and least-privilege/reliability requirements for a separate architecture decision.
  This release adds **no external controller**, no account PAT on the VM, no local
  sleeping process, and no guest-shutdown substitute. Do not enable attached runs
  merely because manual destruction succeeds.
- [ ] Record sanitized evidence and results for each provider independently.
  Remove the README's untested label only for a provider whose actual rental,
  SSH/copy/image/counter/deletion gates passed; do not claim safe expiry until its
  independent proof and implementation are reviewed and validated.
