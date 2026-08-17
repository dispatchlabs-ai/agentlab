#!/usr/bin/env python3
"""Deterministic conformance reviewer for AgentLab's review adapter contract."""

import json
import os
from pathlib import Path


request_path = Path(os.environ["AGENTLAB_REVIEW_REQUEST_PATH"])
request = json.loads(request_path.read_text())

invalid_first = (
    os.environ.get("AGENTLAB_FIXTURE_ALWAYS_INVALID") == "1"
    or (
        os.environ.get("AGENTLAB_FIXTURE_INVALID_FIRST") == "1"
        and os.environ.get("AGENTLAB_REVIEW_REPAIR") != "1"
    )
)
if invalid_first:
    print(json.dumps({
        "schema_version": "agentlab.review-proposal/v1",
        "review_id": request["review_id"],
        "anchors": request["anchors"],
        "counts": {"proposed": 0, "rejected": 0, "conflicted": 0, "unresolved": 0},
        "summary": "Intentionally missing dispositions to exercise one correction.",
    }))
    raise SystemExit(0)

if os.environ.get("AGENTLAB_REVIEW_REPAIR") == "1":
    previous = Path(os.environ["AGENTLAB_REVIEW_PREVIOUS_STDOUT_PATH"])
    validation_error = Path(os.environ["AGENTLAB_REVIEW_VALIDATION_ERROR_PATH"])
    assert previous.is_file()
    assert "dispositions" in validation_error.read_text()

base = Path(os.environ["AGENTLAB_REVIEW_BASE_DIR"])
candidate = Path(os.environ["AGENTLAB_REVIEW_CANDIDATE_DIR"])
current = Path(os.environ["AGENTLAB_REVIEW_CURRENT_DIR"])
machine_changes = Path(os.environ["AGENTLAB_REVIEW_MACHINE_CHANGES_DIR"])
run_stdout = Path(os.environ["AGENTLAB_REVIEW_RUN_STDOUT_PATH"])
run_stderr = Path(os.environ["AGENTLAB_REVIEW_RUN_STDERR_PATH"])
evaluations = Path(os.environ["AGENTLAB_REVIEW_EVALUATIONS_PATH"])

assert Path.cwd().resolve() == current.resolve()
assert (base / "AGENTS.md").is_file()
assert (candidate / "AGENTS.md").is_file()
assert (current / "AGENTS.md").is_file()
assert (candidate / "accepted.txt").read_text() == "candidate accepted\n"
assert (current / "conflict.txt").read_text() == "current conflict\n"
assert (machine_changes / "etc" / "agentlab-review.conf").read_text() == "environment recommendation\n"
assert run_stdout.is_file()
assert run_stderr.is_file()
assert json.loads(evaluations.read_text()) == []

dispositions = []
counts = {"proposed": 0, "rejected": 0, "conflicted": 0, "unresolved": 0}
for item in request["candidates"]:
    path = item["path"]
    workspace_path = item.get("workspace_path")
    disposition = {
        "path": path,
        "reason": "fixture reviewer accounted for this exact candidate",
    }
    if workspace_path == "accepted.txt":
        disposition["disposition"] = "proposed"
        disposition["workspace_operation"] = {
            "operation": "replace",
            "path": "accepted.txt",
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
    "schema_version": "agentlab.review-proposal/v1",
    "review_id": request["review_id"],
    "anchors": request["anchors"],
    "counts": counts,
    "dispositions": dispositions,
    "recommendations": [],
    "summary": "Fixture proposed one safe workspace addition without applying it.",
}
print(json.dumps(proposal, separators=(",", ":")))
