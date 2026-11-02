# Golden parity tables

Each `*.json` file here was generated once from the Python reference implementation and is checked by `tests/golden.rs`. They pin behavior that is easy to get subtly wrong in a port: Python number formatting, `json.dumps`, `shlex.quote`, URL validation, time zone handling, and the exact text of prompts and tables.

## Provenance

- Reference: the Python `gpu` CLI at commit b803934, snapshotted as `reference/python/` on `main` (last present at commit 5767d70, removed by the Rust rewrite).
- Interpreter: CPython 3.13.17 with the reference's locked dependencies (`uv sync`), `TZ=UTC`.
- Method: a one-off script, run from outside the repository, imported `reference/python/src/gpu` and called the functions below directly with the inputs listed, writing each result (or the exception text) next to its input. Lists are written one row per line.

To regenerate the original reference data, check out 5767d70, run the reference with `uv`, and call the same functions with the inputs stored in each file. Human-copy expectations now deliberately differ: line prompts name defaults and recovery flags, empty instance lists point to `gpu up`, startup warns how to set `GH_TOKEN`, missing fields and malformed responses name their cause, and expiry errors give supported inputs. Preserve these copy changes when regenerating; numeric formats, serialized values, tables, and request structure remain pinned.

## Tables

| File | Python source | Contents |
|---|---|---|
| `floats.json` | `repr`, f-strings | IEEE bits of edge-case floats with `repr`, `.3f`, `.1f`, `.1%`, `6.1%`, `g` |
| `float_parse.json` | `float(str)` | strings, parsed value or error text |
| `shlex_quote.json` | `shlex.quote` | strings and quoted forms |
| `json_dumps.json` | `json.loads`/`json.dumps` | documents with compact, `indent=1`, `indent=2` output |
| `json_errors.json` | `json.loads` | invalid documents and the decode error text |
| `redact.json` | `cli.redact` | secret-shaped strings and structures |
| `resolve_repo.json` | `repos.resolve_repo` | accepted and rejected repository inputs |
| `startup_scripts.json` | `cli.startup_script` | repositories x modes x two configs, exact scripts |
| `requests.json` | `cli.Vast.request` | offers x modes x bid x disk x configs (script replaced by a marker; checked against `startup_scripts`) |
| `offer_queries.json` | `offers.OfferSearch.query` | flag combinations x mode x GPU |
| `ls.json` | `cli.render_instances`, `ls --json` | payloads with odd types, table text and redacted JSON |
| `uptime.json`, `instance_labels.json`, `offer_labels.json`, `gpu_models.json` | `instances`, `offers` | labels, aligned rows, categories over the bundled model list |
| `parse_when.json` | `expiry.parse_when` | inputs x fixed "now" values x zones (UTC, America/New_York, Europe/London, Australia/Lord_Howe), result or error |
| `line_prompts.json` | `cli.choose`, `cli.choose_gpu` | stdin answers and history in, stdout transcript and result out |
| `vast_gpu_models.txt` | `tests/fixtures/` | GPU names seen in a live Vast search (read by `src/tests/cli.rs`) |

## Rows the Rust side skips or relaxes

Each is listed in `docs/PLAN.md` section 9 or 11:

- Non-ASCII digits in `float()` and `valid_gpu_name` (plan 9.3.2) and full case folding of `ß` (9.3.4).
- `parse_when` results on 9999-12-31: `jiff` timestamps end at 9999-12-30T22:00Z (section 11).
- Datetime parser errors (now `Invalid datetime`, UTC offset and year range messages) allow equivalent error categories rather than exact internal wording (plan 9.1.2).
