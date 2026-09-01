# alfred

**A Least-Resistance governance skeleton for AI agents.**

alfred is a minimal, end-to-end-runnable governance skeleton for AI agents:
an owner submits a request → a planner decomposes it into a DAG → a plan
reviewer judges fidelity → an executor (pi) runs in a sandboxed container →
an execution reviewer grades the artifact → tiered routing (advance /
mechanical retry / escalate to owner) → the owner decides and the loop
resumes. Every state transition is auditable and resumable; the executor and
reviewer are isolated; the executor container has no network and holds no
real credentials.

This repository is the rewritten skeleton implementation (v0.1.0, full
rewrite; v1 code is not retained). R1–R4 are delivered; R5 closes out (AGT
evaluation + unified full-chain e2e + out-of-workspace-write automation +
documentation sync).

---

## Table of Contents

- [Architecture](#architecture)
- [Build & Test](#build--test)
- [Configuration](#configuration)
- [Library API and the alfred bin](#library-api-and-the-alfred-bin-owner-interaction-goes-through-the-codux-terminal)
- [Governance Loop](#governance-loop)
- [End-to-End Tests](#end-to-end-tests)
- [Security Boundaries](#security-boundaries)
- [Repository Layout](#repository-layout)

---

## Architecture

```
Owner (human)
  └─ alfred library (Rust; governance driver alfred-cli::governance, owner
       interaction goes through the codux terminal → feed_owner_message; the
       orchestrator state machine lives in alfred-core, called in-process)
       ├─ planner: in-container pi conversation agent (converse graph-building /
       │            maintain session-doc), calls the model through the bridge
       │            (container offline + host relays) → output → DagSpec
       ├─ execution: generate a host-side container driver script (driver.py,
       │             not an eval Task) → spawn `python3 driver.py`
       │    └─ Inspect container-management interface: starts a docker sandbox
       │            (network_mode: none + workspace-only volume)
       │         ├─ sandbox_agent_bridge: in-container localhost model proxy
       │         │            → host provider
       │         └─ in-container pi: sees only the contract prompt, calls the
       │                    model through the bridge (no keys in container)
       │    ← polls `<work>/driver.done.json` for the done record → reads the
       │                              bind-mount artifacts
       ├─ plan review / exec review: same mechanism in a dedicated reviewer
       │            container (driver.py drives in-container pi to judge DagSpec
       │            fidelity vs OwnerRequest / artifact vs acceptance) →
       │            verdict.json
       └─ persistence: run-<id>/{state.json, audit.jsonl, llm-calls/, exec-N/}
                       (driver.done.json + driver.stdout/stderr.log under exec-N/
                       are the driver evidence, replacing the old evals/)
```

### Crates (Cargo workspace, 5 crates)

| crate | responsibility |
|---|---|
| `alfred-core` | Shared cross-crate entities (single source of truth): OwnerRequest / DagSpec / GraphBuilder / Contract / TaskAssignment / ExecVerdict / PlanVerdict / SessionDoc + the **governance state machine** (`governance.rs`, §3.3 routing table in code) |
| `alfred-planner` | Planner (converse graph-building / maintain session-doc / disguise rejection); in-container pi conversation agent (bridge-relayed LLM) with `llm-calls/` on disk; `ALFRED_OFFLINE=1` deterministic bypass |
| `alfred-executor` | Execution side: generates the Inspect container-management driver (`driver.py`, not an eval Task), the sandbox compose, spawn/poll the driver (done record), artifact collection, config loading |
| `alfred-reviewer` | Review side: plan/exec review both run in a dedicated reviewer container (driver.py in-container pi, judging fidelity → PlanVerdict / acceptance → ExecVerdict) |
| `alfred-cli` | Governance-loop library driver (`governance::run_governance_loop` / `feed_owner_message`, owner interaction via codux terminal) + real `alfred` bin (codux-schedulable CLI driver: run/feed/status, consumes leading `--append-system-prompt`) |

---

## Build & Test

```bash
cargo build --workspace      # 0 error
cargo test  --workspace      # 97 passed; 0 failed (as measured in R4)
```

Dependencies: Rust (edition 2021) + Docker (sandbox image `alfred-executor:latest`;
see `docker/Dockerfile`) + Inspect AI (`inspect` CLI) + pi-coding-agent.

---

## Configuration

The single source of truth for model config is
`${XDG_CONFIG_HOME:-~/.config}/alfred/config.yml` (0600 recommended), with a
three-layer structure: `providers` (endpoint + credentials + protocol) →
`models` (id + provider + params) → `roles` (model id per role:
planner / executor / reviewer).

Environment overrides:

| env | meaning |
|---|---|
| `LLM_<ROLE>_MODEL` / `ALFRED_<ROLE>_MODEL` | model id for a role (planner/executor/reviewer) |
| `LLM_BASE_URL` / `LLM_API_KEY` | override a provider endpoint / key |
| `ALFRED_CONFIG` | config.yml path override |
| `ALFRED_INSPECT` | inspect CLI path override |
| `ALFRED_IMAGE` | sandbox image override (default `alfred-executor:latest`) |
| `ALFRED_OFFLINE=1` + `ALFRED_OFFLINE_PLAN_FILE=<dag.json>` | planner offline deterministic bypass (used by e2e) |

> config.yml holds real API keys and lives outside the repository. Keys stay on
> the host process only; the in-container models.json uses a dummy `sk-none`
> key pointing at the bridge.

---

## Library API and the alfred bin (owner interaction goes through the codux
## terminal)

alfred exposes library APIs (the orchestrator state machine) plus a real
`alfred` bin (a CLI driver schedulable by the codux terminal wrapper, per the
omp.rs pattern — no invented panel):

- `governance::run_governance_loop(&mut GovernanceRun, &GovernanceContext)`:
  initialize / advance the governance loop from the current state until a
  suspended state (PlanRejected / Escalated) or a terminal state
  (Completed / Abandoned).
- `governance::feed_owner_message(&mut run, &ctx, message, decision)`:
  owner-decision entry (called by the codux terminal): sets the owner message
  (revise replans / resumes dialogue from Planning), maintain② persists key
  conclusions, appends conversation.json, routes by suspended state and
  resumes the loop, returning the new state for the caller to display.
  `decision` ∈ retry | revise | abandon (message optional for retry/abandon).

Real `alfred` bin (`crates/alfred-cli/src/main.rs`, codux-schedulable CLI driver)
runs the governance loop:

```bash
# Run the governance loop (request → plan → plan review → execute →
#                            exec review → tiered routing → suspended/terminal)
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request <req.json> [--run-dir <dir>] [--time-limit 600] [--review-time-limit 300] \
  [--image alfred-executor:latest]

# Feed an owner decision (retry / revise / abandon); resumes from the suspended state
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir <dir> --decision retry|revise|abandon [--message <text|file>]

# Read-only governance status
cargo run --quiet -p alfred-cli --bin alfred -- status --run-dir <dir>
```

`run` / `feed` share a resumable model: `state.json` stores the state machine
(`GovernanceRun`), and `feed` is a signal, not a terminal point — `Escalated +
retry` re-enters execution, `PlanRejected + retry` replans with a disguised
message, `revise` replans with the owner's new requirement, and any decision +
`abandon` terminates.

---

## Governance Loop

```
Planning → PlanReviewing → Executing → ExecReviewing
   exits: Completed (acceptance C) / Escalated (owner, suspended) /
          PlanRejected (plan bounced, suspended) / Abandoned (terminal)
```

Tiered routing (§3.3, all six rows in code):

| ExecVerdict | failure_class | routing |
|---|---|---|
| C | — | advance |
| I/P | mechanical | retry with the same contract (budget N=2; escalate when exhausted) |
| I/P | contract_ambiguity / fidelity_dispute / disagreement | escalate to owner |
| I/P | contract_fault | escalate to owner (suggests a contract change) |

- **Mechanical-failure detection**: execution container-driver status
  (success/error/timed_out/crash) — non-success ⇒ mechanical.
- **Review errors** (unscored / driver error) escalate to the owner; never silently
  passed.
- **Disguised rejection (P7)**: the plan-review reason is rewritten into an
  owner-voice message (a forbidden-signal check rejects review/verdict/否决/打回
  etc.) before it is fed back to the planner for replanning.
- **Session document (P6)**: `maintain` updates the three-section SessionDoc
  (key_file_paths / key_conclusions / review_summary) at ① plan-review
  conclusion and ② owner-supplement; the projection fed to the planner is
  neutralized (`review_summary` → `owner_feedback`, forbidden-signal scrubbed).

---

## End-to-End Tests

`tests/e2e/` all run for real (LLM via config.yml; offline cases use
`ALFRED_OFFLINE` deterministic bypass):

| script | coverage | mode |
|---|---|---|
| `r1.sh` | execution side: in-container pi produces hello.txt on host + driver.done.json/stdout/stderr evidence archive | real container + real LLM |
| `r2.sh` | review side, two cases: exec review C / partial P (the two standalone plan-review cases are archived; equivalent coverage in r3 case3 / r6b caseA) | real LLM + offline injection |
| `r3.sh` | governance loop: happy path full loop / mechanical-escalation loop (case2b archived) / disguised-rejection loop (feed retry) / multi-turn session doc (feed revise) | real LLM + offline injection |
| `r4.sh` | owner-decision feed resume, two cases: escalated→feed abandon→Abandoned / plan_rejected→feed retry→replan→Escalated (decision-panel RPC archived) | offline injection |
| `escape.sh` | out-of-workspace-write boundary, two-way: in-container /tmp write does not land on host + workspace write lands on host (pure docker, no LLM) | pure container boundary |
| `agt/agt-policy.test.mjs` | AGT policy-eval prototype, deterministic (29 assertions) | no LLM, no container |
| `agt/demo.sh` | AGT live demo: in-sandbox pi + policy extension blocks `rm -rf` (audit deny+allow) | real container + real LLM (optional demo) |

**Unified entry:**

```bash
bash tests/e2e/skeleton.sh   # r1 → r2 → r3 → r4 → escape → agt; green only if all pass
```

The `skeleton.sh` header documents both modes honestly: real-container/real-LLM
(r1 / r2 case1·1b / r3 case1 / escape) covers real execution and review;
offline injection (r3 case2·3·4 / r4 case1·2,
`ALFRED_OFFLINE=1` + `ALFRED_OFFLINE_PLAN_FILE`) covers deterministic state-machine
paths (mechanical escalation, disguised rejection, owner decisions via feed) by bypassing
the planner LLM; `agt` is a no-LLM deterministic prototype test. Per-step logs
land in `tests/e2e/.runs/skeleton-<ts>/<step>.log`.

---

## Security Boundaries

| boundary | mechanism | verification |
|---|---|---|
| No network by default | sandbox compose `network_mode: none` (only loopback in container) | R0 experiment + carried into r1 |
| No keys in container | in-container models.json uses a dummy key; real keys stay in the host driver process (`env_clear` + allowlist) | R0 audit (`docker inspect env` zero hits) |
| Out-of-workspace write blocked | workspace-only volume; paths outside it land on the container overlay, not the host | `tests/e2e/escape.sh` (two-way PASS) |
| Review isolation | Non-claim, enforced by the mount surface: contract full text / acceptance criteria / conversation transcript are not mounted into the executor container; the reviewer container independently mounts ws full (ro) + conversation transcript for scoring; the planner is unaware of reviewer/executor | r2/r3 e2e assertions |
| Tool-level policy (prototype, off by default) | AGT-style pi extension intercepts `tool_call` (rm -rf / sudo / secret read / out-of-workspace write) | `tests/e2e/agt/` (24 deterministic assertions + live demo) — owner decides, see `.plans/AGT评估.md` |

---

## Repository Layout

```
crates/
  alfred-core/      entities + state machine + routing + GraphBuilder
  alfred-planner/   converse / maintain / disguise / container / task_gen / llm
  alfred-executor/  task_gen / compose_gen / driver / artifact / run / config + templates/executor_driver.py.tmpl
  alfred-reviewer/  plan_review / exec_review / container / task_gen / verdict
  alfred-cli/       src/{main.rs (alfred bin: run/feed/status), governance.rs, lib.rs}
docker/
  Dockerfile        sandbox image (inspect base + Node 22 + pi-coding-agent 0.84.3)
  pi-sandbox.compose.yaml   zero-mount reference base (network none)
tests/
  e2e/              r1-r4 / escape / skeleton / agt/
.plans/             implementation plan + per-phase delivery/verification reports
                    + AGT评估.md (gitignored, not in version history)
```

---

## Documentation Pointers

- `.plans/实施计划.md`: R0–R5 phase plan and the pinned-item alignment table
  (rewrite discipline, environment facts E1–E5)
- `.plans/R{1..4}交付.md` / `R{0..4}报告.md`: per-phase delivery and independent
  verification records
- `.plans/AGT评估.md`: AGT (Microsoft Agent Governance Toolkit) as a pi
  tool-permission plugin — evaluation + prototype + owner decision items (R5/P10)
- The governance documents (治理架构 / 业务架构 / 技术架构 / 限界上下文 /
  SKELETON 施工清单) live outside the repo on local disk
  (`the-path-of-least-resistance/alfred-research/docs`), not in version history

> Phase status: R1 (execution) → R2 (review) → R3 (governance loop) → R4
> (owner-decision session + codux integration) → R5 (AGT evaluation + full-chain e2e
> + out-of-workspace write + docs sync). Current implementation status is
> authoritative in `.plans/R5交付.md` (acceptance docs do not retroactively bless
> code).
