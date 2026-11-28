"""A stand-in for the gpu binary: logs each call and replies from a script.

FAKE_GPU_DIR holds responses.json, mapping "SUB CMD" or "SUB CMD ID" to
{"code": int, "envelope": {...}, "sleep": seconds}. Each call is appended to
calls.jsonl as {"argv": [...], "cwd": "..."}. It never reaches a network.
"""

import json
import os
import sys
import time
from pathlib import Path

root = Path(os.environ["FAKE_GPU_DIR"])
argv = sys.argv[1:]
with (root / "calls.jsonl").open("a") as f:
    f.write(json.dumps({"argv": argv, "cwd": os.getcwd(), "env_keys": sorted(os.environ)}) + "\n")
head = argv[: argv.index("--")] if "--" in argv else argv
words = [a for a in head if not a.startswith("-")]
if "--dry-run" in argv:
    words = [*words[:2], "dry"]
responses = json.loads((root / "responses.json").read_text())
reply = None
for n in (3, 2):
    reply = responses.get(" ".join(words[:n]))
    if reply is not None:
        break
reply = reply or {"code": 1, "envelope": {"command": " ".join(words[:2]), "ok": False, "error": "unscripted"}}
time.sleep(reply.get("sleep", 0))
print(json.dumps(reply["envelope"]))
sys.exit(reply.get("code", 0))
