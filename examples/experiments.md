# External-evaluator examples

AgentLab factors are opaque labels. The following experiment shapes use ordinary factor names only to demonstrate how an external script could organize runs; AgentLab does not understand skills, layouts, models, reasoning levels, or replicates.

For a skill A/B experiment, run at least two replicates of each cell:

```bash
agentlab run --workspace . --image IMAGE --factor skill=off --factor replicate=1 -- HARNESS TASK
agentlab run --workspace . --image IMAGE --factor skill=off --factor replicate=2 -- HARNESS TASK
agentlab run --workspace . --image IMAGE --factor skill=on  --factor replicate=1 -- HARNESS TASK
agentlab run --workspace . --image IMAGE --factor skill=on  --factor replicate=2 -- HARNESS TASK
```

The same mechanism can label workspace-layout or reasoning-level comparisons:

```text
layout=flat,   replicate=1
layout=flat,   replicate=2
layout=nested, replicate=1
layout=nested, replicate=2

thinking=low,    replicate=1
thinking=low,    replicate=2
thinking=medium, replicate=1
thinking=medium, replicate=2
```

Evaluate completed run IDs with any command that implements the JSON output contract:

```bash
agentlab evaluate --name result-facts RUN_A1 RUN_A2 RUN_B1 RUN_B2 -- \
  ./examples/evaluators/result-facts.sh

agentlab report --evaluator result-facts \
  --factor skill --factor replicate \
  --score exit_zero --score portable_changes \
  RUN_A1 RUN_A2 RUN_B1 RUN_B2
```

Agent and external-service behavior can be nondeterministic even when AgentLab proves byte-identical starting inputs. Multiple replicates reveal variance; they do not remove it. The report aligns declared factors and evaluator observations but performs no aggregation, ranking, statistical significance test, or causal inference. Choosing an evaluator and interpreting its scores remain external scientific judgments.
