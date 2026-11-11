# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Validate with `cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`. The remote worker's bash tests run inside `cargo test` on Linux (`tests/remote_worker.rs`).
- `src/config.rs`, `src/startup.rs` and `src/vast.rs` are authoritative for Vast config defaults, generated Vast startup scripts, and Vast CLI requests. Repository validation and the explicit no-repository value (`Repo::NoRepo`, written as `""`) live in `src/repos.rs`.
- Machine-readable values retain the Python tool's formatting inside the command JSON envelopes: format values through `src/pyfmt.rs` (`str()`, float repr, `%.3f`, `shlex.quote`) and JSON through `src/pyjson.rs`, never `{}`/serde defaults. `tests/golden/` holds tables generated from the Python reference.
- Vast CLI output can carry credentials (`instance_api_key`, tokens); never print it raw, pass it through `src/redact.rs`.
- Never exercise lifecycle commands against real Vast, Google Cloud or Modal accounts in tests. Everything external goes through the `Runner` in `src/ctx.rs`: unit tests use `testing::FakeRunner`, binary tests put logging stubs first on `PATH` (`tests/common`). Use `gpu up ... --dry-run` for read-only manual checks.
- Vast expiry uses only Vast-injected instance-scoped credentials, never the local account key; TPU expiry uses a Cloud Tasks service account, never credentials on the VM. `src/expiry.rs` owns local-time/DST parsing and the SSH transport; `remote/expiry_remote.sh` is the standalone bash worker sent over SSH stdin; `remote/expiry_guard.sh` is the `--expire` boot guard embedded in the onstart script (size-capped by `ONSTART_LIMIT` in `src/startup.rs`) and shares the worker's state and lock. Their tests point `WORKER_PATH` and `DESTROY_URL` at stubs so no test can reach the API. See README "Auto-destroy a Vast instance" for its safety limits and sources.
- The TTY picker is one loop with a screen stack (`src/picker/`: engine in `mod.rs`, `job.rs`, key decoding in `keys.rs`, terminal loop and renderer in `term.rs`; launch screens in `src/wizard.rs` and `src/gcp_wizard.rs`; instance selection in `src/instances.rs`; editor screens, launchers, and pinned Neovim checksums in `src/editors.rs`). Never run `gh`/`vastai`/`gcloud` or other slow work on the picker thread or in a screen transition; wrap it in a `Job`. Regression tests drive real keys: a pipe through `picker::testing::drive`, a pty in `tests/terminal.rs`. Use a temporary HOME for SSH config tests.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
