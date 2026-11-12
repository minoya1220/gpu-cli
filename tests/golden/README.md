# Pinned CLI contract fixtures

The JSON tables in this directory are pinned fixtures consumed by `tests/golden.rs`.
They cover Python-compatible numeric formatting, JSON and shell quoting, repository
validation, deadlines, prompts, labels, tables and provider requests. `pyfmt` and
`pyjson` implement the live output contract; these fixtures are not disposable
snapshots and have no automatic regeneration procedure.

For an intentional contract change, update only the affected inputs/expected
outputs and run `cargo test --locked --test golden`, followed by the full local
gate. Preserve unrelated formatting, serialized values, request shape and precision.

## Tables

| Files | Contract |
|---|---|
| `floats.json`, `float_parse.json` | Float representation, formatting and parsing |
| `shlex_quote.json` | Shell quoting |
| `json_dumps.json`, `json_errors.json` | JSON encoding and decoding |
| `redact.json` | Credential-shaped values and structures |
| `resolve_repo.json` | Repository validation |
| `startup_scripts.json`, `requests.json`, `offer_queries.json` | Startup scripts and Vast requests |
| `ls.json` | Inventory JSON shape, precision and redaction |
| `uptime.json`, `instance_labels.json`, `offer_labels.json`, `gpu_models.json` | Human labels and model categories |
| `parse_when.json` | Deadlines in fixed time zones |
| `line_prompts.json` | Line-prompt transcripts |
| `vast_gpu_models.txt` | Model-name input fixture |

## Deliberate limits

Tests do not require non-ASCII digit parsing or full Unicode case folding.
`jiff` timestamps end at 9999-12-30T22:00Z, so later date cases are excluded.
Datetime failures are compared by supported error category where internal parser
wording is not part of the contract.
