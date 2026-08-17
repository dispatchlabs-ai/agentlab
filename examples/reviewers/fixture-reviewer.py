#!/usr/bin/env python3
"""Deterministic conformance reviewer for AgentLab's adoption adapter contract."""

import json
import os
from pathlib import Path


request_path = Path(os.environ["AGENTLAB_ADOPTION_REQUEST_PATH"])
request = json.loads(request_path.read_text())
base = Path(os.environ["AGENTLAB_ADOPTION_BASE_DIR"])
candidate = Path(os.environ["AGENTLAB_ADOPTION_CANDIDATE_DIR"])
current = Path(os.environ["AGENTLAB_ADOPTION_CURRENT_DIR"])
machine_changes = Path(os.environ["AGENTLAB_ADOPTION_MACHINE_CHANGES_DIR"])

assert Path.cwd().resolve() == current.resolve()
assert (base / "AGENTS.md").is_file()
assert (candidate / "AGENTS.md").is_file()
assert (current / "AGENTS.md").is_file()
assert (candidate / "adopt.txt").read_text() == "candidate adoption\n"
assert (current / "conflict.txt").read_text() == "current conflict\n"
assert (machine_changes / "etc" / "agentlab-review.conf").read_text() == "environment recommendation\n"

dispositions = []
counts = {"proposed": 0, "rejected": 0, "conflicted": 0, "unresolved": 0}
for item in request["candidates"]:
    path = item["path"]
    workspace_path = item.get("workspace_path")
    disposition = {
        "path": path,
        "reason": "fixture reviewer accounted for this exact candidate",
    }
    if workspace_path == "adopt.txt":
        disposition["disposition"] = "proposed"
        disposition["workspace_operation"] = {
            "operation": "replace",
            "path": "adopt.txt",
        }
    elif workspace_path == "reject.txt":
        disposition["disposition"] = "rejected"
    elif workspace_path == "conflict.txt":
        assert item["current_relation"] == "changed_since_base"
        disposition["disposition"] = "conflicted"
    else:
        disposition["disposition"] = "unresolved"
    counts[disposition["disposition"]] += 1
    dispositions.append(disposition)

proposal = {
    "schema_version": "agentlab.adoption-proposal/v1",
    "review_id": request["review_id"],
    "anchors": request["anchors"],
    "counts": counts,
    "dispositions": dispositions,
    "summary": "Fixture proposed one safe workspace addition without applying it.",
}
print(json.dumps(proposal, separators=(",", ":")))
