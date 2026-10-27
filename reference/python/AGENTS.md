# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Run validation with `uv run ruff check src tests && uv run pytest -q`.
- `src/gpu/cli.py` is authoritative for config defaults, generated startup scripts, and Vast CLI requests. Repository validation and the explicit no-repository representation are shared in `src/gpu/repos.py`.
- Vast CLI output can carry credentials (`instance_api_key`, tokens); never print it raw, pass it through `redact` in `src/gpu/cli.py`.
- Never exercise lifecycle commands against the real Vast account in tests; mock launch paths and use `gpu up ... --dry-run` for read-only manual verification.
- Expiry uses only Vast-injected instance-scoped credentials, never the local account key. `src/gpu/expiry.py` owns local-time/DST parsing and SSH transport; `src/gpu/expiry_remote.py` is standalone remote stdlib code. See README “Auto-destroy an instance” for its safety limits and source evidence.
- The TTY picker is one prompt_toolkit Application with a screen stack (`src/gpu/picker.py`; launch screens in `src/gpu/wizard.py`; instance selection and managed SSH hosts in `src/gpu/instances.py`; editor screens, launchers, and pinned Neovim checksums in `src/gpu/editors.py`). Never run `gh`/`vastai` or other slow work on its event loop or in a screen transition; wrap it in a `Job`. Regression tests drive real keys: pipe input in `tests/conftest.py`, a pty in `tests/test_terminal.py`. Use a temporary HOME for SSH config tests.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
