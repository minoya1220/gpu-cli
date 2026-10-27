# Plan: Rust rewrite of the `gpu` CLI

Status: proposal for review. No Rust code exists yet.

Source of truth for behavior: `reference/python/` (snapshot of the Python CLI at
commit b803934). Where the Python README and the Python code disagree, this plan
follows the code and its tests, and lists the disagreement in section 9.

Goal: a user of the Python tool uninstalls it, installs the Rust binary, and
keeps working with the same command name, flags, config file, history file,
caches, managed SSH config, and remote expiry state, with no migration step.

Contents:

1. Scope and hard rules
2. Command and flag mapping
3. Behavior mapping, module by module
4. Crate choices
5. Module layout
6. Test strategy and Python test mapping
7. Parity verification
8. CI
9. Deviations: unavoidable, proposed, and dropped
10. Delivery steps for phase 2

---

## 1. Scope and hard rules

- Binary name `gpu`, version string `0.1.0` (printed bare, as argparse does).
- Same files, same formats:
  - config: `$XDG_CONFIG_HOME/gpu/config.json`, default `~/.config/gpu/config.json`
  - history: `$XDG_STATE_HOME/gpu/history.json`, default `~/.local/state/gpu/history.json`
  - GPU model cache: `$XDG_CACHE_HOME/gpu/gpu_models.txt`, default `~/.cache/gpu/gpu_models.txt`
  - repo cache: `$XDG_CACHE_HOME/gpu/repos-<owner>.json`
  - managed SSH: `~/.ssh/vst_config`, plus the `Include ~/.ssh/vst_config` line prepended to `~/.ssh/config`
  - remote expiry state on the instance: `~/.local/state/gpu-expire/{deadline.json,runner.py,lock}`
- All Vast and GitHub access still goes through the user's authenticated `vastai`
  and `gh` CLIs as subprocesses. The Rust tool reads no API key and makes no HTTP
  request of its own (same as Python).
- Tests never run the real `vastai`, `gh`, `ssh`, `rsync` or GUI editors. Unit
  tests use an injected fake runner that fails on any unscripted command line.
  Integration tests put stub executables first on `PATH`; every stub logs its
  argv and exits non-zero on an unexpected call, so a missing fake can never fall
  through to a real tool installed on the machine.
- No secrets in code, fixtures, or logs. The redaction rules port unchanged.

## 2. Command and flag mapping

argparse is replaced by clap (derive). Every subcommand, flag, choice list,
metavar and default carries over. Usage errors still exit 2.

| Python | Rust | Notes |
|---|---|---|
| `gpu -v` / `-V` / `--version` | same | Custom flag (not clap's built-in), prints `0.1.0` with no program name, exit 0. Top level only, as in argparse. |
| `gpu` (no subcommand) | same | Usage error, exit 2. |
| `gpu up` | same | Flags below. |
| `--repo REPO` / `--no-repo` | same | Mutually exclusive (clap `conflicts_with`). `--no-repo` stores the explicit "no repository" value (empty string in Python). Repeating `--repo` keeps the last value, as argparse does (`args_override_self`). |
| `--mode {container,vm}` | same | |
| `--gpu NAME` | same | |
| `--num-gpus N` (int, default 1) | same | Range check stays in launch validation (exit 1, not 2), as in Python. |
| `--max-price F` (float) | same | Kept as `f64` and rendered with Python float repr in the offer query (`--max-price 1` gives `dph<=1.0`). |
| `--interruptible` | same | |
| `--offer ID` (int) | same | |
| `--disk N` (int) | same | |
| `--yes`, `--dry-run` | same | |
| `--expire DURATION` | same | Duration only (`2h`, `1h30m`); parsed before any external call; invalid gives exit 2. |
| `--reliability F` (float) | same | |
| `--unverified` | same | |
| `gpu ls [--json]` | same | |
| `gpu ssh [ID] [--exec]` | same | `--exec` replaces the process with `ssh` (Unix `exec`). |
| `gpu cursor [ID]` | same | Alias for `edit ID --editor cursor`; ignores `editor.nvim_config`, as in Python. |
| `gpu edit [ID] [--editor {cursor,code,zed,nvim}] [--nvim-config SRC]` | same | |
| `gpu stop [ID]` | same | |
| `gpu down [ID] [--yes]` | same | |
| `gpu expire [ID] WHEN\|cancel\|show` | same | One variadic positional (`ID/WHEN`), custom usage line, description and epilog text copied. Shape validated before any external call, exit 2 on error. |

Long-option prefix matching (`--dry` for `--dry-run`) works in argparse; clap's
`infer_long_args` keeps it. IDs given to `ssh`, `stop`, `down`, `edit`, `cursor`
stay unvalidated strings, as in Python (only `expire` requires digits).

Exit status and output channel, unchanged:

| Situation | Output | Exit |
|---|---|---|
| Success | stdout | 0 |
| Typed confirmation is not `yes` (`up`, `down`) | `Cancelled.` on stdout | 0 |
| EOF at a confirmation prompt | `Cancelled.` | 1 |
| Picker quit (back twice on first screen, Ctrl-D) | `Cancelled.` | 1 |
| Picker Ctrl-C | `Cancelled.` | 130 |
| SIGINT outside the picker (line prompt, polling, subprocess) | `\nCancelled.` | 130 |
| Handled error | `gpu: <redacted message>` on stderr | 1 |
| `expire ... show` with inactive worker | summary on stdout, `gpu: Timer is NOT running...` on stderr | 1 |
| Usage error | clap usage message on stderr | 2 |

## 3. Behavior mapping, module by module

Python module to Rust module, with the behavior that must carry over exactly.
"Exact" means byte-for-byte output or argv, checked by tests.

### 3.1 `cli.py` config, history, redaction, subprocess (Rust: `config`, `history`, `redact`, `process`)

- `DEFAULTS` dict and shallow merge of `config.json` over it. The config stays a
  JSON object (`serde_json::Map`), not a typed struct, because Python accepts any
  JSON types there and formats them with `str()` (for example `"disk": "30"`
  works). Non-object config is an error: `Config <path> must contain a JSON object`.
- `history.json`: same load rules (missing, unreadable, invalid, or non-object
  file means empty), `recent_picks` keeps only string entries, `record_picks`
  prepends and truncates to 100 per step, keeps other keys and their order,
  writes `json.dumps(indent=2)` plus newline. Only `mode` and `gpu` after a
  successful `up`, and `editor` after a successful editor handoff, are recorded
  (code and tests; the README claims `repo` too, see 9.2).
- `by_frequent_recency`: score `sum(0.85**age)`, stable sort descending.
- `redact`: same key regex, same assignment regex (Rust `regex` uses the same
  leftmost-first semantics as Python `re` for these patterns), same `[name,
  value]` pair rule, applied to the same places: `ls --json`, `ssh` URL output,
  `created_summary` failure reason, `run()` failure text, and the top-level
  error printer.
- `run(argv, timeout)`: stdin from `/dev/null`, stdout and stderr captured
  (read on threads so large `search offers` output cannot deadlock), universal
  newline translation, `strip()`, kill on timeout. Errors keep their text:
  `Timed out running X`, `Required command not found: X`, redacted stderr, else
  stdout, else `Command failed: X`.
- `Vast` adapter: `search offers` argv, `create instance` argv including
  `CONTAINER_ENV`, `--bid_price` (Python float repr), `--label`, `--raw`; exact.
- `startup_script`: exact bytes for both modes, with Python `shlex.quote`
  reimplemented exactly (the `shlex` crate quotes differently).
- `choose` / `choose_gpu` line prompts (non-TTY): same text, paging (12 per
  page), filters, unique-match shortcut, default selection, `filter_across`
  recursion, EOF messages. Prompts flush stdout before reading, as `input()` does.
- `validate_launch_args`, `select_offer`, `prompt_for_picks`, `_launch`,
  `created_summary`, `render_instances`, `wait_for_lifecycle`, `wait_for_ssh`,
  `edit_instance`, `expire_instance`, `main` dispatch: same order of checks,
  same messages, same polling intervals (2 s lifecycle, 3 s SSH, 60 s and 120 s
  limits, 5 s SSH probe connect timeout capped at 6 s), same stdout/stderr split.

Python formatting details that need explicit Rust helpers (module `pyfmt`):

- `str()` of JSON values for display and argv: `None`, `True`/`False`, Python
  float repr (`0.1`, `1.0`, `1e-05`, `1e+16`, `inf`, `nan`), and Python `repr`
  for lists and dicts. Used in the `ls` table, `instance_label`, offer queries,
  `--disk`, `--bid_price`, `created_summary`.
- Truthiness (`item.get('gpu_name') or '-'`).
- `float()` on strings (whitespace, underscores, `inf`/`nan`) for `format_uptime`.
- `%.3f` and `6.1%` formatting. Checked: Rust `{:.3}` rounds exactly like
  Python's `%.3f`, including exact ties (`0.0625` gives `0.062` in both).
- `str.isprintable`, `str.isspace`, `casefold` (see 9.3 for the approximation).

JSON (module `pyjson`, on top of `serde_json` with `preserve_order`):

- `loads` accepts what Python's `json` accepts and `serde_json` does not:
  bare `NaN`, `Infinity`, `-Infinity`. A pre-pass maps them to reserved sentinel
  strings that the numeric helpers and the writer understand, so `ls --json`
  prints them back as Python would.
- `dumps` matches Python output: `indent=1` (`ls --json`), `indent=2` (dry-run
  request, history), default compact separators `", "` and `": "` (repo cache),
  `ensure_ascii` escaping (everything outside `0x20..0x7e`, surrogate pairs for
  astral code points), Python float repr.

### 3.2 `picker.py` (Rust: `picker::{mod, keys, term, job}`)

prompt_toolkit's Application becomes a small purpose-built picker. The engine is
a state machine with no I/O, so tests drive it directly:

- `Screen<V, R>`: title, `options` closure (re-evaluated on every redraw),
  `select` closure returning the next screen or the flow result, optional
  `search`, `load` job, `loading`, `empty`, `prices` job, `quick_select`,
  `load_required`, `text_input`, `validate`, `on_settled`, `notice`,
  `wait_initial`. One value enum per flow (`V`) keeps option values typed.
- View stack with per-view filter, cursor and error; same `ready`, `entries`,
  `current`, `quick_select` rules.
- Keys, exactly as bound in Python: Up/Down move with wrap-around; Enter and
  Right select (or validate and submit text input, or retry a failed required
  load); Left and backtick clear the filter, else pop, else arm quit, else quit
  with status 1; Backspace edits; Ctrl-R retries the load; F2 starts the prices
  job; Ctrl-C quits with 130, Ctrl-D with 1, from any screen even with a quit
  pending; bracketed paste appends stripped printable text; digits 1-9
  quick-select only on short unfiltered lists; every key other than backtick and
  Left disarms the pending quit, including ignored keys and Escape; Escape and
  all other special keys are ignored and never reach the filter.
- `Job<T>`: idle, running, done, failed; listeners appended while running and
  invoked in registration order on the picker thread; restart only from idle or
  failed; work runs on a detached `std::thread` so quitting never waits on a slow
  `gh` or `vastai`. Settle order matters for the repository re-validation flow
  (section 3.3), and is ported as is.
- Rendering (header, body, footer) reproduces the Python text: title, `Filter:`
  or `Repository:` line, `› N. label` selection marker with right-aligned
  numbers, notice line, loading and error notes, `No matches`, footer hints
  joined by two spaces (`` ↑/↓ move  Enter/→ select  1-9 select  `/← quit (twice) ``
  and the rest). Styles: title bold, filter cyan, selected bold reverse, note
  italic, error red.

Terminal layer (`picker::term`), matching prompt_toolkit's non-fullscreen mode:

- Raw mode, hidden cursor, bracketed paste on; restored on every exit path,
  including panics (panic hook) and SIGINT.
- Draws in place below the cursor, grows to at most the terminal height, wraps
  long lines by display width, keeps the selected row visible with one row of
  scroll margin, redraws on SIGWINCH, erases itself on exit (`erase_when_done`).
- Input is read from fd 0 (as Python reads `sys.stdin`), multiplexed with job
  completions and SIGWINCH through `poll(2)` on stdin plus a wake pipe. No
  reader thread outlives the picker, so a following `Type 'yes'` prompt gets
  every keystroke.
- Key decoding is our own (`picker::keys`), not crossterm's event reader.
  Reason: prompt_toolkit holds a lone `ESC` for 0.5 s (`ttimeoutlen`) before
  treating it as Escape, so an arrow key split across reads still moves
  (Python test `test_arrow_key_split_across_reads_still_moves`). crossterm
  reports a lone `ESC` immediately and the rest would leak into the filter. The
  decoder is pure (`feed(bytes, now)`, `flush(now)`) and covers CSI and SS3
  arrows, modifier forms, F-keys, Home/End/Insert/Delete/PageUp/PageDown,
  bracketed paste, and control bytes. Unknown CSI sequences are consumed whole
  (see 9.3).

### 3.3 `wizard.py` (Rust: `wizard`)

Direct port of `LaunchWizard` and `Picks`: screen order repo, mode, GPU,
category, offer; the URL entry screen with inline validation; cached repository
names shown while `gh repo list` runs; first run waits for the load; the
"Checking repositories" screen before returning a cached pick; reset to the
repository screen with `Repository not available or deleted: NAME` or the
GitHub error as notice; F2 price job shared per mode; recent GPUs first
(top five that are still known models); GPU search across all models while
filtering; offer search re-run on every visit, top ten. State shared by the
screen closures lives in one `Rc<RefCell<WizardState>>` on the picker thread;
jobs only carry `Send` work.

### 3.4 `offers.py` (Rust: `offers`)

`OfferSearch` query string (exact), price and deverified filtering, stable sort
by price, `gpu_stats`, `offer_label`, `offer_rows` (aligned columns),
`model_label`, `category_label`, `plural`, `gpu_category` rules and
`CATEGORY_ORDER`, `known_gpu_models` (bundled list via `include_str!` merged
with the cache, sorted), `valid_gpu_name`, GPU cache read and atomic write
(temp file in the same directory, mode 0600 as `mkstemp` gives, rename, global
lock because several picker jobs can search at once).

### 3.5 `repos.py` (Rust: `repos`)

`NO_REPO` becomes an explicit enum variant internally but is still written as
`""` wherever Python writes it. `resolve_repo` keeps every accept and reject
rule, including Python `urlsplit` behavior that matters here (netloc ends at
the first `/`, `?` or `#`; an empty `?` or `#` is not rejected; the netloc
regex rejects userinfo and IPv6 brackets). Cache path sanitizing, name
filtering, compact JSON write via temp file and rename. `repo_label`.

### 3.6 `instances.py` (Rust: `instances`, `ssh_config`)

`repo_name` (label first, then the `git clone ... /workspace/NAME` fallback;
Python `(?:\s|$)` becomes `(?:\s|\z)`), `format_uptime`, `instance_label`,
`select_instance` (title `Instance to <command>`, selection required even for
one instance, `No instances to select.` for none). SSH config: `Include` line
detection on raw bytes with the same case-insensitive regex and `splitlines`
semantics, prepend only when missing, chmod 0600 when written; `~/.ssh` created
0700; `vast-<id>` stanza text, replacement and removal regex, chmod 0600; the
`ssh_host` and `ssh_port` checks and their messages.

### 3.7 `editors.py` (Rust: `editors`)

`EDITORS` table and order, `editor_screen`, `after_up_screen` (labels and
default handling), `check_editor` with the same hints (PATH lookup like
`shutil.which`), GUI argv for cursor, code and zed (zed path percent-encoded
like `urllib.parse.quote(..., safe='/')`), handoff waits for the CLI only and a
non-zero exit is an error. Neovim: `nvim_bootstrap` script text (pinned
v0.11.5, both SHA-256 digests, arch detection, git config clone or
`--ff-only` pull), `nvim_session` text, the three-step ssh/rsync/ssh sequence,
`~` expansion and canonicalization for local config directories, the rsync
presence check, failures reported by exit code only, the final `ssh -t` owns
the terminal.

### 3.8 `expiry.py` (Rust: `expiry`)

- `parse_duration`, `parse_expire_arguments`, `parse_when`, `parse_deadline`
  with the same regexes, messages and edge rules.
- Time zones via `jiff`: elapsed durations are exact seconds; clock times pick
  the next local occurrence over up to three days, skip spring-forward gaps,
  and take the first future instant in a fall-back fold; timezone-less ISO
  times reject gaps and folds with the same message; aware ISO times convert
  to UTC; past or present deadlines are rejected. The system zone comes from
  `TZ` or `/etc/localtime`, as Python's `astimezone()` does.
- ISO parsing follows Python 3.11+ `datetime.fromisoformat` (the version range
  the tool runs on in practice): extended and basic dates, ISO week dates,
  any single separator character after the `T`/space pre-check, `HH`,
  `HH:MM`, `HH:MM:SS`, fractional seconds, `Z`, `+HH:MM`, `+HHMM`. A table of
  cases generated from Python checks it (section 7).
- `remote_command` (alias and ID validation, `python3 - <action> <id>
  [ceil(deadline)]`, `BatchMode=yes`, `ConnectTimeout=10`), `manage_expiry`
  (45 s timeout, never echoes stderr or remote text, same error table and
  status checks), `expiry_summary` (local ISO time with offset, time zone
  abbreviation from `jiff`, `in Xh Ym Zs`).

### 3.9 `expiry_remote.py` (stays Python)

This script runs on the rented instance with the instance's own `python3`;
the README documents that requirement. It stays a Python file, byte-identical,
moved to `remote/expiry_remote.py` and embedded with `include_str!`. The
transport sends `SOURCE = <python string literal>` followed by the source, as
Python does with `repr()`; the Rust side writes an equivalent literal (exact
`repr()` output is not needed, only a literal that evaluates to the same
text, which a test checks with the local `python3`). Keeping it unchanged
means timers armed by the Python CLI can be shown, cancelled and re-armed by
the Rust CLI, and the remote requirements do not change.

### 3.10 Signals, terminal and process exit

- SIGINT outside the picker: handler restores the terminal if needed, flushes
  stdout, prints `\nCancelled.`, exits 130 (Python's `KeyboardInterrupt` path).
- Broken pipe on stdout (`gpu ls | head`) becomes a handled error, exit 1, like
  Python's `OSError` path, never a panic.
- No panics on bad input data: missing keys give `gpu: 'key'` like Python's
  `KeyError`, type mismatches give an error and exit 1.

## 4. Crate choices

All current, widely used, and maintained. Exact versions get pinned in
`Cargo.lock` (committed, since this is a binary). Versions below are the
current releases as of writing.

| Crate | Version | Use | Why this one |
|---|---|---|---|
| `clap` (derive) | 4.6 | Argument parsing | Standard; supports prefix inference, conflicts, last-wins repeats, custom usage and epilog text, exit code 2 on usage errors. |
| `serde`, `serde_json` (`preserve_order`) | 1.0 | All JSON | Standard. `preserve_order` keeps key order like Python dicts. Python-compatible formatting is a custom `Formatter`. |
| `regex` | 1.13 | Redaction, repo validation, SSH config edits, expiry parsing | Same leftmost-first semantics as Python `re` for these patterns; `regex::bytes` for the raw `~/.ssh/config` bytes. |
| `crossterm` | 0.29 | Raw mode, terminal size, cursor movement, clearing, styles | The standard terminal backend. Used for output and modes only; input decoding is ours (3.2). |
| `rustix` (`event`, `pipe`; `pty` in dev) | 1.1 | `poll(2)` on stdin plus wake pipe; pseudo-terminals in tests | Safe syscall wrappers, no libc boilerplate. |
| `signal-hook` | 0.4 | SIGWINCH redraw, SIGINT cancel | Standard, works with a self-pipe. |
| `jiff` | 0.2 | Local time zone, DST gaps and folds, abbreviations | Reads `TZ` and the system tz database, gives zone abbreviations (`chrono` cannot: its `%Z` prints the offset), explicit gap and fold handling. |
| `shlex` | 2.0 | Splitting the `vastai ssh-url` output for `ssh --exec` | Matches Python `shlex.split` for this input. Quoting is done by our own exact port of Python `shlex.quote`. |
| `tempfile` | 3.27 | Atomic cache writes (0600 temp file, rename); tests | Same file mode as Python's `mkstemp`. |
| `unicode-width` | 0.2 | Line wrapping in the picker | prompt_toolkit wraps by display width too. |
| `which` | 8.0 | `shutil.which` for editor CLIs and rsync | Standard PATH lookup. |

Dev-only: `vt100` 0.18 (terminal emulator for the pty tests, replaces `pyte`),
`sha2` 0.11 (checksums for the Neovim install test fixture).

Considered and not used:

- An HTTP client (`reqwest`, `ureq`): the local CLI makes no HTTP requests.
  Vast and GitHub calls go through `vastai` and `gh` so the tool never holds an
  account credential. Adding a client would mean reading the user's Vast API
  key, which the Python design avoids on purpose. If you want the tool to call
  the Vast API directly, that is a design change, not part of parity.
- `ratatui`: its inline viewport has a fixed height chosen at construction
  (`Viewport::Inline(n)`), so a two-item Mode screen would reserve the same
  block as a hundred-repo list. prompt_toolkit's non-fullscreen mode grows and
  shrinks with the content. The picker draws three regions (header, list,
  footer); a direct crossterm renderer of about 200 lines matches the Python
  look exactly. If you prefer ratatui anyway, the cost is the fixed-height block.
- `tokio` or any async runtime: one blocking poll loop plus threads for jobs is
  enough.
- `chrono`: no time zone abbreviations (see `jiff`).
- `dirs`: only `HOME` and the `XDG_*` variables are needed.

## 5. Module layout

```
Cargo.toml                 package gpu-cli, [lib] gpu, [[bin]] gpu
Cargo.lock
assets/gpu_models.txt      bundled GPU names (moved from src/gpu/gpu_models.txt)
remote/expiry_remote.py    remote worker, byte-identical to the Python one
remote/test_expiry_remote.py  its tests, ported to stdlib unittest
src/
  main.rs                  calls gpu::main_with(argv, Ctx::real()) and exits
  lib.rs                   module tree, main_with (dispatch, error to exit code)
  cli.rs                   clap definitions
  ctx.rs                   Ctx: env snapshot, Runner, Clock, Console, Ui
  config.rs                defaults, paths, load_config
  history.rs               history.json, recency ranking
  pyfmt.rs                 str/repr/float repr, truthiness, float(), quote, isprintable
  pyjson.rs                Python-compatible loads/dumps
  redact.rs
  process.rs               run() with timeout, Runner trait, exec
  vast.rs                  Vast adapter, list_instances, lifecycle argv
  startup.rs               startup_script, CONTAINER_ENV
  repos.rs
  offers.rs
  instances.rs
  ssh_config.rs
  editors.rs
  expiry.rs                parsing, ISO grammar, transport, summary
  prompt.rs                line-based choose, choose_gpu, confirm
  launch.rs                up: validate, picks, offers, request, confirm, create, expire, editor
  commands.rs              ls, ssh, stop, down, edit, cursor, expire; wait_for_ssh/lifecycle
  picker/mod.rs            Screen, View, Picker state machine, Snapshot
  picker/job.rs            Job<T>
  picker/keys.rs           byte-to-key decoder
  picker/term.rs           raw mode, poll loop, renderer
  wizard.rs
  tests/                   in-crate tests, one file per Python test file (section 6)
tests/
  common/mod.rs            stub executables, temp HOME and XDG dirs, run the binary
  terminal.rs              pty end-to-end tests (Python test_terminal.py)
  cli_flows.rs             non-TTY end-to-end runs of the real binary
  golden.rs                data-driven parity tables from tests/golden/
  remote_worker.rs         runs python3 -m unittest on remote/test_expiry_remote.py
  golden/*.json            cases generated from the Python reference
.github/workflows/ci.yml
```

`Ctx` is the seam that replaces Python's monkeypatching. Rust tests run in
parallel threads, so no test mutates process environment variables; every path
is computed from `Ctx.env`, a snapshot of `HOME`, `XDG_*`, `PATH`, `TZ`.

- `Runner`: executes argv with options; real one spawns processes, fake one
  matches scripted argv and panics on anything else.
- `Clock`: monotonic now, sleep, wall now; fake one advances on sleep (the
  Python tests' fake `time.monotonic`/`time.sleep`).
- `Console`: stdin lines, stdout, stderr, `isatty` flags.
- `Ui`: runs a picker flow either on the real terminal or on a scripted key
  source. Tests that monkeypatched `pick` in Python instead send real key bytes
  through the real picker state machine.

## 6. Test strategy and Python test mapping

Layout rule: each Python file `tests/test_X.py` maps to `src/tests/X.rs`,
and each test function keeps its name. Exceptions: `test_terminal.py` goes to
`tests/terminal.rs` (it needs the built binary), and the remote worker tests in
`test_expiry.py` go to `remote/test_expiry_remote.py`. Parametrized cases
become table-driven loops that name the failing case, or separate tests where
the setup differs.

Drivers that replace the Python fixtures:

- `conftest.isolate_user_directories`: every test builds a `Ctx` over fresh
  temp `HOME` and `XDG_*` directories; end-to-end tests set them only on the
  child process.
- `conftest.run_picker` / `Keyboard` (pipe input, `press`, `type`, `wait`,
  `on`): `picker::testing::Driver` runs the real event loop on a socket pair
  for input and a sink for output, in a thread, and exposes a `Snapshot`
  (title, filter, labels, footer, notice, loading text, done) updated after
  every redraw. Latency checks (`< 0.5 s`) keep the same thresholds.
- `test_terminal.py` (`pyte` + fake `gh`/`vastai` Python script): `rustix`
  pty, `vt100` screen, POSIX `sh` stubs that log calls, sleep 0.3 s, and
  answer the same JSON.
- `gated` loaders (`threading.Event`): `std::sync::Barrier`/channel gates.

### test_cli.py → src/tests/cli.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_container_script_uses_selected_repo_directory_only | same | Exact `EXPECTED_CONTAINER_SCRIPT` constant copied. |
| test_generated_startup_script_is_valid_bash[container,vm] | same | Runs local `bash -n`. |
| test_vm_script_preserves_toolchain_and_does_not_inject_secret | same | |
| test_launch_request_exact_image_env_script_and_offer | same | Exact argv for container and interruptible VM. |
| test_adapter_search_generates_exact_read_only_cli_call | same | Fake runner records argv. |
| test_up_validates_before_network_calls | same | Fake runner panics on any call. |
| test_dry_run_launch_path_uses_mocked_api_and_exact_request | same | Asserts exact query string and request; output has no em dash. |
| test_yes_without_offer_auto_selects_cheapest_without_prompt | same | Console panics on any read. |
| test_interactive_offer_selection_does_not_print_second_list | same | |
| test_ls_table_and_json_output | same | |
| test_gpu_cache_honors_xdg_and_rejects_invalid_names | same | |
| test_live_vast_gpu_fixture_has_few_other_models | same | Reads `tests/golden/vast_gpu_models.txt` (copied fixture). |
| test_gpu_default_hint_only_appears_when_recent_pick_exists | same | |
| test_picker_selects_unique_text_match_without_second_enter | same | Line-based `choose`. |
| test_interactive_gpu_stats_and_offers_share_unverified_filter | same | |
| test_picker_filters_and_paginates | same | |
| test_picker_recents_use_frequency_weighted_recency_and_latest_default | same | |
| test_history_is_written_only_after_successful_confirmed_launch | same | |
| test_unverified_flag_removes_verified_filter_and_lists_all_hosts | same | |
| test_ctrl_c_at_a_line_prompt_cancels_without_traceback | `tests/cli_flows.rs` | Real binary on a pipe, SIGINT sent while it waits at the prompt; stdout ends with `\nCancelled.\n`, exit 130. |
| test_up_never_prints_instance_api_key[raw-json,unparseable] | same | |
| test_up_errors_never_print_instance_api_key[unsuccessful,command-failed] | same | |
| test_run_redacts_secrets_from_failed_command_output | `src/tests/cli.rs` + `process.rs` unit | Real `run()` against a stub script that fails with a secret on stderr. |
| test_ls_ssh_and_stop_redact_secrets | same | |
| test_dry_run_request_contains_no_credentials | same | |

### test_picker.py → src/tests/picker.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_typing_filters_and_enter_selects_the_highlighted_match | same | |
| test_digits_quick_select_only_short_unfiltered_lists | same | |
| test_search_list_replaces_options_while_filtering | same | |
| test_back_clears_filter_then_goes_back_one_screen_then_quits_on_first_twice[backtick,left] | same | |
| test_going_back_restores_the_previous_screen_as_it_was_left | same | |
| test_right_selects_like_enter[enter,right] | same | |
| test_one_back_then_another_key_on_the_first_screen_does_not_quit[7 keys] | same | |
| test_backtick_then_left_quits_the_first_screen | same | |
| test_special_keys_never_leak_into_the_filter_or_url_entry | same | Same 24 escape sequences. |
| test_key_hints_leave_out_ctrl_c | same | |
| test_ctrl_c_and_ctrl_d_quit_at_once_even_with_a_quit_pending[4 cases] | same | |
| test_ctrl_c_and_ctrl_d_quit_from_a_deep_screen[2 cases] | same | |
| test_escape_is_ignored_and_never_leaks_into_navigation | same | |
| test_arrow_key_split_across_reads_still_moves | same | Plus pure decoder unit tests with a fake clock in `picker/keys.rs`. |
| test_loading_screen_stays_responsive_while_its_loader_blocks | same | |
| test_failed_load_shows_error_and_enter_retries | same | |
| test_f2_loads_prices_in_the_background_once | same | |
| test_quitting_does_not_wait_for_a_running_loader | same | |

### test_wizard.py → src/tests/wizard.rs

| Python test | Rust test |
|---|---|
| test_back_walks_every_screen_without_refetching_or_quitting | same |
| test_backtick_twice_on_the_first_real_screen_quits | same |
| test_flags_for_every_pick_skip_the_picker | same |
| test_offer_search_never_blocks_navigation | same |
| test_f2_prices_are_shared_by_gpu_screens_and_loaded_once_per_mode | same |
| test_recent_gpus_come_first_and_digits_filter_gpu_names | same |

### test_terminal.py → tests/terminal.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_backtick_from_the_deepest_screen_steps_back_to_the_first_then_quits | same | Same screen-line assertions and call log check (`gh`, `vastai` once each). |
| test_keyboard_dry_run_launch_after_going_back | same | |
| test_new_repository_choices_dry_run_in_terminal[None, URL] | same | Bracketed paste. |
| test_arrow_keys_navigate_and_special_keys_never_reach_the_filter | same | |
| test_ctrl_c_and_ctrl_d_quit_from_the_first_screen[2 cases] | same | |
| test_ctrl_c_quits_while_an_offer_search_is_still_running | same | |

### test_expiry.py → src/tests/expiry.rs and remote/test_expiry_remote.py

| Python test | Rust test | Notes |
|---|---|---|
| no_live_calls (autouse fixture) | fake runner and `Ui` that panic | |
| test_duration[6] | same | |
| test_invalid_duration[9] | same | |
| test_times[10] | same | UTC zone, fixed now. |
| test_invalid_times[11] | same | |
| test_dst_elapsed_duration_is_not_wall_time | same | `America/New_York` via jiff. |
| test_dst_gap_and_fold | same | |
| test_system_local_timezone_uses_future_rules | same | Re-runs the test binary as a child with `TZ=America/New_York` (no in-process env mutation). |
| test_argument_shape[4] | same | |
| test_ambiguous_arguments[5] | same | |
| test_command_has_no_user_shell_input | same | |
| test_transport_embeds_standalone_source_without_secrets | same | Executes the payload with local `python3` (as `__name__ != "__main__"`) and compares `SOURCE` with the embedded file. |
| test_transport_errors_do_not_leak[4] | same | |
| test_timeout_marks_arming_unverified | same | |
| test_confirmation_local_timezone_and_remaining | same | |
| test_arm_show_replace_cancel | `remote/test_expiry_remote.py` | Same test, stdlib `unittest` and `unittest.mock`. |
| test_tick_only_destroys_at_deadline_and_retries | remote | |
| test_failed_rearm_preserves_existing_deadline | remote | |
| test_post_arm_verification_failure_rolls_back[2] | remote | |
| test_cancel_serializes_with_inflight_destroy | remote | |
| test_show_detects_dead_worker | remote | |
| test_worker_is_detached_and_startup_verified | remote | |
| test_process_identity_check | remote | |
| test_scoped_credentials_sources[3] | remote | |
| test_missing_scoped_credentials_never_falls_back_to_account | remote | |
| test_exact_destroy_request_mocked | remote | |
| test_cli_dispatch[4] | same | |
| test_cli_usage_errors_before_external_calls[8] | same | Exit 2, runner never called. |
| test_cli_show_inactive_is_nonzero | same | |
| test_up_expire_invalid_fails_before_launch | same | |

`tests/remote_worker.rs` runs `python3 -m unittest remote/test_expiry_remote.py`
so `cargo test` stays the single gate. CI runners have `python3`.

### test_expiry_launch.py → src/tests/expiry_launch.rs

| Python test | Rust test |
|---|---|
| test_up_arms_after_creation | same (deadline within 7199 to 7201 s of invocation) |
| test_up_dry_run_does_not_arm | same |
| test_up_arming_failure_leaves_instance_and_warns | same |
| test_up_without_returned_id_cannot_claim_expiry | same |

### test_editors.py → src/tests/editors.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_gui_commands[cursor,code,zed] | same | |
| test_missing_cli[3] | same | |
| test_nvim_commands[None, https, git@, local] | same | |
| test_safe_config_and_directory_quoting | same | |
| test_history_only_after_success[2] | same | |
| test_no_history_noninteractive | same | |
| test_after_up_real_keys[5] | same | Real keys through the picker driver. |
| test_editor_real_keys[4] | same | |
| test_edit_flag_config_override_and_alias | same | |
| test_nvim_session_shell[tmux, no tmux] | same | Executes generated shell with stub binaries. |
| test_existing_nvim_skips_install | same | |
| test_missing_nvim_installs_only_verified_archive[3] | same | Tarball built with local `tar`, digest via `sha2`, stub `curl`/`uname`/`sha256sum` on a restricted PATH; no network. |
| test_git_config_shell_is_quoted_and_updates[2] | same | Stub `git` logs argv as JSON lines. |
| test_invalid_local_config_does_not_run_ssh | same | |

### test_instances.py → src/tests/instances.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_picker_zero_one_many[0,1,2] | same | Picker driven by scripted keys (select last) instead of a patched `pick`. |
| test_instance_picker_real_keys[1,2] | same | |
| test_explicit_id_skips_picker | same | Scripted `Ui` panics if a picker opens. |
| test_down_selection_still_confirms_and_cancel_is_safe | same | |
| test_cursor_repo_and_fallback | same | |
| test_managed_ssh_config_preserves_user_bytes | same | CRLF user config bytes preserved. |
| test_ssh_wait_timeout | same | Fake clock. |
| test_ssh_probe_failure_times_out | same | |
| test_onstart_repo_and_untrusted_label | same | |
| test_up_offers_editor_on_success[code,none] | same | |

### test_lifecycle.py → src/tests/lifecycle.rs

| Python test | Rust test |
|---|---|
| test_lifecycle_verifies_before_reporting[2 actions x 4 outcomes] | same |
| test_stop_disappearance_is_not_stopped | same |

### test_repo_cache.py → src/tests/repo_cache.rs

| Python test | Rust test |
|---|---|
| test_cached_display_and_wait_before_launch | same |
| test_invalid_cache_resets_with_message_and_never_launches | same |
| test_sync_error_resets_without_launch | same |
| test_first_run_waits_for_sync | same |
| test_sync_rewrites_cache_with_names_only | same |

### test_repos.py → src/tests/repos.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_resolve_repo[7] | same | |
| test_unsafe_or_credential_bearing_repos_are_rejected[19] | same | |
| test_generated_script_new_choices[4 repos x 2 modes] | same | Includes `bash -n`. |
| test_cli_dry_run_skips_github_and_prompts[5] | same | |
| test_flag_conflict_and_invalid_repo_fail_before_network | same | Exit 2 for the conflict, 1 for the invalid repo. |
| test_no_repo_usable_while_github_is_blocked | same | |
| test_url_entered_inline_and_back_preserves_choices[3] | same | |
| test_github_failure_does_not_hide_static_choices | same | |
| test_cache_contains_only_repo_names_and_no_recent_rows | same | |
| test_paste_url_in_same_application | same | |
| test_line_picker_new_choices_without_github[2] | same | |
| test_optional_repository_load_can_retry | same | |

### test_uptime.py → src/tests/uptime.rs

| Python test | Rust test | Notes |
|---|---|---|
| test_format_uptime[23] | same | Inputs given as JSON values (`true`, `{}`, strings, NaN sentinel). |
| test_label_and_table_share_uptime[4] | same | |

New tests with no Python counterpart: key decoder units, `pyfmt` and `pyjson`
units, renderer snapshots (wrapping, scrolling, footer truncation), terminal
restore on panic, broken pipe handling.

## 7. Parity verification

Four layers, all offline, none touching Vast:

1. Ported tests (section 6). Every Python assertion has a Rust counterpart.
2. Golden tables generated from the Python reference before it is deleted, and
   committed as data in `tests/golden/`, checked by `tests/golden.rs`:
   - `startup_script` for a matrix of repos and both modes (exact bytes);
   - `Vast.request` argv for a matrix of flags and offers;
   - `ls` table and `ls --json` text for payloads with odd types (null label,
     float and string durations, nested values, non-ASCII, NaN);
   - `format_uptime`, `offer_label`, `offer_rows`, `model_label`,
     `category_label`, `gpu_category` over the bundled model list;
   - `redact` over a corpus of secret-shaped strings and structures;
   - `resolve_repo` accept and reject table;
   - `parse_when` over a table of inputs, fixed "now" values and zones
     (UTC, America/New_York, Europe/London, Australia/Lord_Howe for its 30-minute
     DST shift), including ISO grammar cases;
   - line-prompt transcripts (stdin script in, stdout out) for `choose` and
     `choose_gpu`;
   - Python float repr, `json.dumps` formatting, `shlex.quote` tables.
   The generator is a one-off script run from outside the repo against
   `reference/python`; `tests/golden/README.md` records the reference commit and
   how the data was made, so the data stays auditable after `reference/` goes.
3. Differential runs during development (not committed): the Python CLI
   (`uv run gpu`) and the Rust binary run side by side under identical stub
   `gh`/`vastai`/`ssh`, temp `HOME` and `XDG_*`, over a scripted list of
   non-TTY invocations (all subcommands, every flag, error paths, dry runs).
   Compared: exit status, stdout, stderr, and the files written (history,
   caches, SSH config). The same harness drives both in a pty for the picker
   flows and compares the final screens. Every difference must be either fixed
   or listed in section 9. The PR description reports the result.
4. Manual checks with `--dry-run` only, as the Python AGENTS.md prescribes.

`reference/` is deleted in the phase 2 PR only after layers 1 to 3 are clean.

## 8. CI

`.github/workflows/ci.yml`, on push and pull request:

- `fmt`: `cargo fmt --all --check` (ubuntu-latest)
- `clippy`: `cargo clippy --all-targets --locked -- -D warnings` (ubuntu-latest)
- `test`: `cargo test --locked` on ubuntu-latest and macos-latest. The tool's
  users are on macOS (the README talks about "your Mac"), so tests run there
  too. Both runner images ship `python3`, `bash`, `tar`.

Actions pinned to release tags of `actions/checkout` and
`dtolnay/rust-toolchain`; `Swatinem/rust-cache` for build caching. No secrets
in the workflow.

## 9. Deviations

### 9.1 Unavoidable

1. Help and usage text: clap formats `--help` and usage errors differently from
   argparse. The help strings, metavars, `expire` usage line, description and
   epilog are copied; exit codes match (0 for help, 2 for usage errors).
2. Error text that comes from Python internals rather than from the tool:
   JSON decode errors, `OSError` strings such as `[Errno 2] No such file or
   directory: '...'`, `datetime` messages such as `hour must be in 0..23`,
   `urlsplit`'s `Invalid IPv6 URL`. These get plain Rust-written equivalents
   with the same exit codes. Every message the Python tool writes itself is
   kept verbatim.
3. The remote worker stays Python (section 3.9). Its tests move from pytest to stdlib
   `unittest` so they run without installing anything, and `cargo test` invokes
   them.

### 9.2 README and code disagree (plan follows the code)

1. The Python README says successful launches record the repository choice in
   history and offer recent repositories in the TTY picker. The code records
   only `mode` and `gpu` (and `editor` after an editor opens), and its test
   asserts exactly that. The repository screen shows the cached `gh` list, not
   history. The Rust README will describe the actual behavior.

### 9.3 Small behavior differences I propose to accept

Each is an edge case; say if you want any of them matched exactly.

1. Unknown terminal escape sequences: prompt_toolkit emits `Escape` and then
   types the rest of an unknown sequence into the filter (for example
   `\x1b[99~` adds `[99~`). The Rust decoder consumes any complete CSI or SS3
   sequence. Known sequences behave the same.
2. Non-ASCII digits: Python's `\d` and `int()` accept digits such as `٣`
   (Arabic-Indic) in expiry durations, instance IDs and ports. Rust accepts
   ASCII digits only.
3. Numeric flags: Python accepts `1_000` and surrounding spaces in `--disk`,
   `--num-gpus`, `--offer`, `--max-price`, `--reliability`. Rust accepts the
   plain forms only (`+1`, `1e3`, `inf` still work for floats).
4. Case-insensitive filtering uses Unicode lowercase, not Python's full case
   folding. Differs only for characters such as `ß` and ligatures.
5. `str.isprintable` is approximated: control, format and separator characters
   (other than space) are rejected as in Python; unassigned and private-use code
   points are accepted.
6. `--nvim-config ~user/dir` (another user's home) is not expanded; `~` and
   `~/...` are.
7. `format_uptime` for values above 2^64 seconds prints `-`; JSON integers above
   64 bits are read as floats.

### 9.4 Proposed fixes (default: keep Python behavior unless you approve)

1. Three expiry error messages tell the user to run `gpu expire ID --show` (or
   `run --show`), a flag that does not exist (the README says so). Proposed
   text: `gpu expire ID show`.
2. An empty `XDG_CONFIG_HOME` or `XDG_STATE_HOME`, or an empty `XDG_CACHE_HOME`
   for the repo cache, makes Python use a path relative to the current
   directory (`gpu/history.json`). The GPU model cache already treats empty as
   unset. Proposed: treat empty as unset everywhere, per the XDG spec.

### 9.5 Dropped

1. Windows. The Rust build is Unix only (macOS, Linux): it uses `poll(2)`,
   `exec`, and Unix file modes. The Python tool was not tested on Windows
   either, and its README targets macOS.
2. Python packaging files (`pyproject.toml`, `uv.lock`, `.gitignore` entries
   for Python) go away with `reference/`.

### 9.6 Additions

1. Root `AGENTS.md` for the Rust project, carrying over the still-relevant
   notes from the Python one (validation command, redaction rule, no live Vast
   calls in tests, where the picker and expiry code live).
2. README install section: `cargo install --path . --locked`, and a note to
   run `uv tool uninstall gpu` first so the old `gpu` does not shadow the new
   one on `PATH`. Config, history, caches, SSH aliases and armed timers need no
   migration.

## 10. Delivery steps for phase 2

One branch `rust-rewrite`, one PR. Order of work:

1. Cargo project, `Ctx`, `process`, `pyfmt`, `pyjson`, `redact`, `config`,
   `history`.
2. `repos`, `startup`, `offers`, `vast`, `instances`, `ssh_config` with their
   tests.
3. `expiry` (parsing, ISO grammar, transport) and the remote worker move.
4. Picker engine, key decoder, terminal layer, picker tests.
5. `wizard`, `editors`, `prompt`, `launch`, `commands`, CLI dispatch, the
   remaining ported tests.
6. pty tests, golden tables, differential runs; fix every difference or list
   it here.
7. README for the Rust tool, `AGENTS.md`, CI workflow, delete `reference/`.
8. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test` all green locally, then open the PR.
