#!/usr/bin/env python3
"""等价 harness 规范化器：run 目录 → canonical JSON dump。

行为等价比对的规范化规则（重构方案 v2 §4）：
- state.json：剥 updated_at（时间戳）；状态机字段/attempts/verdicts/session_doc 全保留；
  run_id 保留（pre/post 两轮 run 目录同名 `run`）。
- audit.jsonl：剥每行 ts；event+data 按序全等（表断言的事件序列由此导出）。
  data 内的绝对路径统一替换为 <RUN_ROOT> 占位符（pre/post 根目录不同）。
- conversation.json：turns 剥 seq 顺序保留（seq 确定性）+ 剥 ts；role/content/source 保留。
- llm-calls/*.json：文件名序号 + 剥 ts 后记录全等。
- dagspec.json / contract.json / plan-verdicts.json / exec-verdicts.json：确定性，全等比对。

用法：normalize.py <run_dir> <repo_root>
输出：canonical JSON 到 stdout（sort_keys 稳定序列化）。
"""
import json
import os
import sys


def load_json(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def strip_keys(obj, keys):
    if isinstance(obj, dict):
        return {k: strip_keys(v, keys) for k, v in obj.items() if k not in keys}
    if isinstance(obj, list):
        return [strip_keys(v, keys) for v in obj]
    return obj


def replace_root(obj, root, run_dir):
    """递归把字符串里的绝对路径前缀替换为占位符（root 与 run_dir 两个层级）。"""
    markers = []
    if run_dir and run_dir.startswith(root):
        markers.append(run_dir)
    markers.append(root)
    def walk(v):
        if isinstance(v, dict):
            return {k: walk(x) for k, x in v.items()}
        if isinstance(v, list):
            return [walk(x) for x in v]
        if isinstance(v, str):
            for m in markers:
                if m in v:
                    v = v.replace(m, "<RUN_ROOT>")
        return v
    return walk(obj)


def main():
    run_dir = os.path.abspath(sys.argv[1])
    root = os.path.abspath(sys.argv[2])
    out = {}

    # 1) state.json（剥 updated_at）
    out["state"] = strip_keys(load_json(os.path.join(run_dir, "state.json")), ["updated_at"])

    # 2) audit.jsonl（剥 ts，event+data 按序）
    events = []
    with open(os.path.join(run_dir, "audit.jsonl"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            events.append({"event": rec["event"], "data": rec.get("data")})
    out["audit"] = replace_root(events, root, run_dir)
    out["audit_event_names"] = [e["event"] for e in events]

    # 3) conversation.json（剥每轮 ts；seq/role/content/source 保留）
    conv = load_json(os.path.join(run_dir, "conversation.json"))
    out["conversation_turns"] = [
        {"seq": t["seq"], "role": t["role"], "content": t["content"], "source": t["source"]}
        for t in conv.get("turns", [])
    ]

    # 4) llm-calls/*.json（文件名 + 剥 ts）
    calls = []
    lc = os.path.join(run_dir, "llm-calls")
    if os.path.isdir(lc):
        for name in sorted(os.listdir(lc)):
            if name.endswith(".json"):
                rec = strip_keys(load_json(os.path.join(lc, name)), ["ts"])
                calls.append({"name": name, "record": replace_root(rec, root, run_dir)})
    out["llm_calls"] = calls

    # 5) 确定性落盘产物
    for fname in ("dagspec.json", "contract.json", "plan-verdicts.json", "exec-verdicts.json"):
        p = os.path.join(run_dir, fname)
        key = fname.replace("-", "_").replace(".json", "")
        out[key] = replace_root(load_json(p), root, run_dir) if os.path.exists(p) else None

    print(json.dumps(out, ensure_ascii=False, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
