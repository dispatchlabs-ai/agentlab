# External-evaluator examples

AgentLab compares real immutable inputs; it does not accept labels that merely claim which treatment was used. Prepare the host workspace exactly as the agent should see it, snapshot that state once, and reuse the digest for every repetition in that cell.

For a skill A/B experiment:

```bash
# A: current workspace without the skill
A=$(agentlab snapshot --workspace /path/to/workspace --json | jq -r .digest)

# B: make the actual treatment change, then freeze it
mkdir -p /path/to/workspace/skills/review
cp /path/to/candidate/SKILL.md /path/to/workspace/skills/review/SKILL.md
B=$(agentlab snapshot --workspace /path/to/workspace --json | jq -r .digest)

agentlab run --snapshot "$A" --image IMAGE -- HARNESS TASK  # RUN_A1
agentlab run --snapshot "$A" --image IMAGE -- HARNESS TASK  # RUN_A2
agentlab run --snapshot "$B" --image IMAGE -- HARNESS TASK  # RUN_B1
agentlab run --snapshot "$B" --image IMAGE -- HARNESS TASK  # RUN_B2
```

The four commands may be launched concurrently. `RUN_A1` and `RUN_A2` have the same derived run-input identity and should compare as an independent repetition. `RUN_A1` and `RUN_B1` have different workspace snapshots, so AgentLab reports `different_inputs`; no `skill=on` assertion is needed or trusted.

A workspace-layout treatment is prepared the same way: arrange the files one way, snapshot it, rearrange them, then snapshot again. A model or reasoning-level treatment must be a real command argument, harness configuration file, environment input supported by the harness, or image change. AgentLab records whichever of those inputs it actually controls; it does not infer model semantics.

For a treatment outside the workspace, prepare a new immutable backend base. With Docker today, make the change in a disposable container, commit it to a new image, and use that image for the relevant runs. AgentLab resolves the tag to a content digest before execution. A future VM backend can use an equivalent VM snapshot.

Evaluate completed run IDs with any trusted host command that implements the JSON output contract:

```bash
agentlab evaluate --name result-facts RUN_A1 RUN_A2 RUN_B1 RUN_B2 -- \
  ./examples/evaluators/result-facts.sh

agentlab report --evaluator result-facts \
  --score exit_zero --score portable_changes \
  RUN_A1 RUN_A2 RUN_B1 RUN_B2
```

The report exposes run-input, workspace, image, and portable-base identities beside evaluator observations. Agent and external-service behavior can be nondeterministic even when AgentLab proves byte-identical starting inputs. Repeated runs reveal variance; they do not remove it. AgentLab performs no aggregation, ranking, statistical significance test, or causal inference. Choosing an evaluator and interpreting its scores remain external scientific judgments.
