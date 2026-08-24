# alfred

Minimal working skeleton of the Workboss governance architecture: owner submits requirement → planner decomposes into execution plan → reviewer audits plan → executor runs pi in Docker container → reviewer audits artifacts → failure-graded routing → escalate to owner for final decision.

[中文](README.md)

## Prerequisites

| Dependency | Version | Check |
|---|---|---|
| Rust | 1.97+ | `cargo --version` |
| Docker | 29+ | `docker info` |
| pi (TS) | 0.80+ | `pi --version` |

## Install

```bash
git clone https://github.com/sdgumdam/alfred.git
cd alfred
cargo build
```

## Configure

Config file: `~/.config/alfred/config.yml` (YAML, three-layer structure)

```yaml
providers:
  kuaizi:
    base_url: "https://your-gateway/v1"
    api_key: "your-key"
    protocol: openai-compatible

models:
  - id: qwen3.8-max
    provider: kuaizi
    contextWindow: 131072
    maxTokens: 8192
  - id: glm-5.2
    provider: kuaizi
    contextWindow: 131072
    maxTokens: 16384
    thinking: high

roles:
  planner: qwen3.8-max
  executor: qwen3.8-max
  reviewer: glm-5.2
```

**Requirement**: reviewer and executor must use different providers (heterogeneous review).

## Run

### Submit a requirement

```bash
./target/debug/alfred run my-request.json
```

`my-request.json` format:

```json
{
  "request_id": "req-1",
  "requirement": "Create hello.txt in /tmp/demo containing Hello Alfred.",
  "acceptance_criteria": "File /tmp/demo/hello.txt exists with content Hello Alfred"
}
```

### Check status

```bash
./target/debug/alfred status run-req-1
```

### Decide on escalation

When escalated (requirement unfulfillable, review failed, retry budget exhausted), alfred spawns a pi TUI session showing a three-button card (retry / revise contract / abandon). Select with keyboard.

Or manually:

```bash
./target/debug/alfred decide run-req-1 retry
./target/debug/alfred decide run-req-1 revise-contract
./target/debug/alfred decide run-req-1 abandon
```

Skip pi card (for CI):

```bash
ALFRED_NO_ASK_PANEL=1 ./target/debug/alfred run my-request.json
```

## Test

```bash
# Unit tests (126)
cargo test

# S0 black-box e2e (17 assertions, offline, no LLM needed)
ALFRED_OFFLINE=1 bash tests/e2e/s0.sh

# S3 end-to-end e2e (46 assertions, offline, four case types)
bash tests/e2e/skeleton.sh
```

## Verify

```bash
# Static: LSP (rust-analyzer) zero diagnostics
# Build: cargo build 0 errors 0 warnings
# Behavior: 126 unit tests + s0 17 + skeleton 46 all green
```

## Architecture

| Role | Implementation | Notes |
|---|---|---|
| Owner | Human | Decides at terminal |
| Planner | alfred-planner | Rust crate, builder API decomposition |
| Orchestrator | alfred-core | Deterministic state machine, no LLM |
| Executor | pi in Docker container | SandboxProfile: volumes/network/provider/runtime |
| Reviewer | alfred-reviewer | Heterogeneous model review |
| Persistence | run-<id>/ | state.json + verdicts.jsonl + audit.jsonl |

## Docs

Knowledge base (never in repo): `the-path-of-least-resistance/alfred-research/docs/`

- `SKELETON施工清单.md` — sole authoritative construction spec
- `治理架构.md` — architecture principles (LeastR paper)
- `业务架构.md` — roles and permissions
- `技术架构.md` — technical implementation
- `限界上下文.md` — schema single source

## Repos

- `sdgumdam/alfred` (public): code
- `alfred-research` (local, physically isolated): docs, papers
