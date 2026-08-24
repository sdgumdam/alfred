#!/usr/bin/env bash
set -euo pipefail
# plan 的确定性直通路径只接受 dag_spec；强制离线模式，e2e 不依赖 LLM 环境变量。
export ALFRED_OFFLINE=1

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO_ROOT/target/debug/alfred"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

pass=0
fail=0

check() {
    local name="$1"; shift
    if "$@"; then
        echo "PASS: $name"
        pass=$((pass + 1))
    else
        echo "FAIL: $name"
        fail=$((fail + 1))
    fi
}

expect_exit() {
    local expected="$1"; shift
    local actual=0
    "$@" >/dev/null 2>&1 || actual=$?
    [ "$actual" -eq "$expected" ]
}

capture_stderr() {
    local stderr_file="$1"; shift
    "$@" >/dev/null 2>"$stderr_file" || true
}

expect_grep() {
    local pattern="$1" file="$2"
    grep -q -- "$pattern" "$file"
}

json_field() {
    python3 - "$1" "$2" <<'PYEOF'
import json, sys
doc = json.load(open(sys.argv[1]))
cursor = doc
for key in sys.argv[2].split('.'):
    cursor = cursor[int(key)] if key.isdigit() else cursor[key]
print(json.dumps(cursor))
PYEOF
}

echo "--- S0 e2e: alfred plan (black-box, via CLI only) ---"
echo "binary: $BIN"

# case 1: valid single-node OwnerRequest plans to a legal DagSpec
cat > "$WORK_DIR/valid.json" <<'EOF'
{
  "request_id": "req-e2e-1",
  "requirement": "Write a haiku about compilers",
  "acceptance_criteria": "Haiku exists in output file, 5-7-5 syllables",
  "dag_spec": {
    "name": "haiku",
    "version": 1,
    "entrypoint": "write-haiku",
    "nodes": [
      {
        "node_id": "write-haiku",
        "node_type": "step",
        "contract": {
          "prompt": "Write a haiku about compilers",
          "acceptance_criteria": "5-7-5 syllable haiku delivered in haiku.md",
          "reviewer_models": ["seed-1.5-review", "another-family-judge"]
        },
        "params": {},
        "input_schema": {}
      }
    ],
    "edges": []
  }
}
EOF
check "valid request exits 0" expect_exit 0 "$BIN" plan "$WORK_DIR/valid.json" --out-dir "$WORK_DIR/run-a"
check "dagspec written" test -f "$WORK_DIR/run-a/dagspec.json"
check "dagspec entrypoint" test "$(json_field "$WORK_DIR/run-a/dagspec.json" entrypoint | tr -d '"')" = "write-haiku"
check "dagspec node contract intact" test "$(json_field "$WORK_DIR/run-a/dagspec.json" nodes.0.contract.reviewer_models)" = '["seed-1.5-review", "another-family-judge"]'

# case 2: multi-node graph through builder API validation
cat > "$WORK_DIR/two-nodes.json" <<'EOF'
{
  "request_id": "req-e2e-2",
  "requirement": "Draft then review",
  "acceptance_criteria": "Both steps done",
  "dag_spec": {
    "name": "draft-review",
    "version": 1,
    "entrypoint": "draft",
    "nodes": [
      {"node_id": "draft", "node_type": "start", "contract": {"prompt": "draft it", "acceptance_criteria": "draft exists", "reviewer_models": ["m1"]}, "params": {}, "input_schema": {}},
      {"node_id": "review", "node_type": "step", "contract": {"prompt": "review it", "acceptance_criteria": "review verdict exists", "reviewer_models": ["m1", "m2"]}, "params": {}, "input_schema": {}}
    ],
    "edges": [{"id": "e1", "from": "draft", "to": "review"}]
  }
}
EOF
check "two-node graph exits 0" expect_exit 0 "$BIN" plan "$WORK_DIR/two-nodes.json" --out-dir "$WORK_DIR/run-b"

# case 3: contract missing acceptance_criteria -> serde rejection -> exit 1 + error says which field
cat > "$WORK_DIR/missing-contract-field.json" <<'EOF'
{
  "request_id": "req-e2e-3",
  "requirement": "broken contract",
  "acceptance_criteria": "n/a",
  "dag_spec": {
    "name": "broken",
    "version": 1,
    "entrypoint": "only-node",
    "nodes": [
      {"node_id": "only-node", "node_type": "step", "contract": {"prompt": "do it", "reviewer_models": ["m1"]}, "params": {}, "input_schema": {}}
    ],
    "edges": []
  }
}
EOF
capture_stderr "$WORK_DIR/stderr-c.txt" "$BIN" plan "$WORK_DIR/missing-contract-field.json" --out-dir "$WORK_DIR/run-c"
check "missing contract field exits 1" expect_exit 1 "$BIN" plan "$WORK_DIR/missing-contract-field.json" --out-dir "$WORK_DIR/run-c"
check "error names missing field" expect_grep "acceptance_criteria" "$WORK_DIR/stderr-c.txt"
check "error names the contract" expect_grep "contract" "$WORK_DIR/stderr-c.txt"

# case 4: unknown field in request -> strict schema rejection -> exit 1
cat > "$WORK_DIR/unknown-field.json" <<'EOF'
{
  "request_id": "req-e2e-4",
  "requirement": "x",
  "acceptance_criteria": "x",
  "dag_spec": {
    "name": "strict",
    "version": 1,
    "entrypoint": "only-node",
    "nodes": [
      {"node_id": "only-node", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"], "hallucinated_field": 1}, "params": {}, "input_schema": {}}
    ],
    "edges": []
  }
}
EOF
capture_stderr "$WORK_DIR/stderr-d.txt" "$BIN" plan "$WORK_DIR/unknown-field.json" --out-dir "$WORK_DIR/run-d"
check "unknown field exits 1" expect_exit 1 "$BIN" plan "$WORK_DIR/unknown-field.json" --out-dir "$WORK_DIR/run-d"
check "error names hallucinated field" expect_grep "hallucinated_field" "$WORK_DIR/stderr-d.txt"

# case 5: missing top-level OwnerRequest field -> exit 1
cat > "$WORK_DIR/missing-owner-field.json" <<'EOF'
{
  "requirement": "no request_id here",
  "acceptance_criteria": "x",
  "dag_spec": {
    "name": "orphan",
    "version": 1,
    "entrypoint": "n",
    "nodes": [{"node_id": "n", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}}],
    "edges": []
  }
}
EOF
capture_stderr "$WORK_DIR/stderr-e.txt" "$BIN" plan "$WORK_DIR/missing-owner-field.json" --out-dir "$WORK_DIR/run-e"
check "missing OwnerRequest field exits 1" expect_exit 1 "$BIN" plan "$WORK_DIR/missing-owner-field.json" --out-dir "$WORK_DIR/run-e"
check "error names request_id" expect_grep "request_id" "$WORK_DIR/stderr-e.txt"

# case 6: structural validation - unreachable node -> exit 1 + structured issue
cat > "$WORK_DIR/unreachable.json" <<'EOF'
{
  "request_id": "req-e2e-6",
  "requirement": "disconnected",
  "acceptance_criteria": "x",
  "dag_spec": {
    "name": "disconnected",
    "version": 1,
    "entrypoint": "a",
    "nodes": [
      {"node_id": "a", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}},
      {"node_id": "b", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}}
    ],
    "edges": []
  }
}
EOF
capture_stderr "$WORK_DIR/stderr-f.txt" "$BIN" plan "$WORK_DIR/unreachable.json" --out-dir "$WORK_DIR/run-f"
check "unreachable node exits 1" expect_exit 1 "$BIN" plan "$WORK_DIR/unreachable.json" --out-dir "$WORK_DIR/run-f"
check "error is structured with issue code" expect_grep "UNREACHABLE_NODE" "$WORK_DIR/stderr-f.txt"
expect_grep "b" "$WORK_DIR/stderr-f.txt"

# case 7: cycle detection -> exit 1
cat > "$WORK_DIR/cycle.json" <<'EOF'
{
  "request_id": "req-e2e-7",
  "requirement": "loop",
  "acceptance_criteria": "x",
  "dag_spec": {
    "name": "loop",
    "version": 1,
    "entrypoint": "a",
    "nodes": [
      {"node_id": "a", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}},
      {"node_id": "b", "node_type": "step", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}}
    ],
    "edges": [
      {"id": "e1", "from": "a", "to": "b"},
      {"id": "e2", "from": "b", "to": "a"}
    ]
  }
}
EOF
check "cycle exits 1" expect_exit 1 "$BIN" plan "$WORK_DIR/cycle.json" --out-dir "$WORK_DIR/run-g"

# case 8: router with routes + route targets -> exit 0 (multi-node routing supported)
cat > "$WORK_DIR/router.json" <<'EOF'
{
  "request_id": "req-e2e-8",
  "requirement": "route",
  "acceptance_criteria": "x",
  "dag_spec": {
    "name": "routed",
    "version": 1,
    "entrypoint": "judge",
    "nodes": [
      {"node_id": "judge", "node_type": "router", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {},
       "routes": {"choices": {"pass": "done", "fail": "__end__"}}},
      {"node_id": "done", "node_type": "end", "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": ["m"]}, "params": {}, "input_schema": {}}
    ],
    "edges": []
  }
}
EOF
check "router graph exits 0" expect_exit 0 "$BIN" plan "$WORK_DIR/router.json" --out-dir "$WORK_DIR/run-h"

# case 9: malformed JSON file -> usage exit 2
printf '{ this is not json' > "$WORK_DIR/malformed.json"
check "malformed JSON exits 2" expect_exit 2 "$BIN" plan "$WORK_DIR/malformed.json" --out-dir "$WORK_DIR/run-i"

echo ""
echo "--- result: $pass passed, $fail failed ---"
[ "$fail" -eq 0 ]
