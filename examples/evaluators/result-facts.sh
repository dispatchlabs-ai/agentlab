#!/bin/sh
set -eu

python3 - "$AGENTLAB_RESULT_PATH" "$AGENTLAB_DELTA_PATH" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    result = json.load(source)
with open(sys.argv[2], encoding="utf-8") as source:
    delta = json.load(source)

exit_code = result["exit_code"]
changes = len(delta["changes"])
ignored = len(delta["ignored_changes"])
print(json.dumps({
    "scores": {
        "exit_zero": 1 if exit_code == 0 else 0,
        "portable_changes": changes,
        "ignored_changes": ignored,
    },
    "observations": {
        "exit_code": exit_code,
        "result_schema": result["schema_version"],
        "delta_schema": delta["schema_version"],
    },
    "summary": f"exit={exit_code}; portable_changes={changes}; ignored_changes={ignored}",
}))
PY
