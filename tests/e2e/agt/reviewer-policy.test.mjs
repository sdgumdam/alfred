// alfred × AGT：reviewer deny-write 策略确定性求值测试（R6c，无 LLM/pi 运行时）。
//
// 用 Node 原生 type-stripping 直接 import agt-policy.ts（Node >= 22.6），
// 模拟 pi tool_call 事件断言 reviewer-policy.json 的 deny-write 语义：
//   - write/edit/... 指向 /workspace → deny（结构性拒绝写工作区，ws ro 双保险）；
//   - write 指向 /outputs → allow（verdict.json 合法产出挂载）；
//   - bash 写重定向到 /workspace → deny；sudo / rm -rf → deny；读工具 → allow。
//
// 运行：node tests/e2e/agt/reviewer-policy.test.mjs

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { evaluateToolCall, parsePolicy } from "../../../docker/agt/agt-policy.ts";

const here = path.dirname(fileURLToPath(import.meta.url));
const policy = parsePolicy(readFileSync(path.join(here, "../../../docker/agt/reviewer/policy.json"), "utf8"));

let failures = 0;
function assert(cond, label) {
  if (cond) {
    console.log(`  ok: ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL: ${label}`);
  }
}

function bash(cmd) {
  return { tool_name: "bash", args: { command: cmd } };
}
function write(p) {
  return { tool_name: "write", args: { path: p, content: "x" } };
}
function edit(p) {
  return { tool_name: "edit", args: { file_path: p } };
}
function read(p) {
  return { tool_name: "read", args: { file_path: p } };
}
function del(p) {
  return { tool_name: "delete", args: { path: p } };
}

console.log("== reviewer-policy.json 语义（deny-write） ==");

// 写 /workspace → deny（结构性拒绝，ws ro 双保险）
const d1 = evaluateToolCall(policy, write("/workspace/hello.txt"));
assert(d1.decision === "deny" && d1.rule === "deny-write-in-workspace", `write /workspace/hello.txt → deny(${d1.rule})`);

// edit /workspace → deny
const d2 = evaluateToolCall(policy, edit("/workspace/src/lib.rs"));
assert(d2.decision === "deny" && d2.rule === "deny-write-in-workspace", `edit /workspace/src/lib.rs → deny(${d2.rule})`);

// delete /workspace → deny
const d2b = evaluateToolCall(policy, del("/workspace/tmp-cleanup"));
assert(d2b.decision === "deny" && d2b.rule === "deny-write-in-workspace", `delete /workspace/tmp-cleanup → deny(${d2b.rule})`);

// 写 /outputs（verdict 合法产出挂载）→ allow
const d3 = evaluateToolCall(policy, write("/outputs/verdict.json"));
assert(d3.decision === "allow", `write /outputs/verdict.json → allow（got ${d3.decision}）`);

// bash 写重定向到 /workspace → deny
const d4 = evaluateToolCall(policy, bash("echo x > /workspace/probe.txt"));
assert(d4.decision === "deny" && d4.rule === "deny-bash-write-redirect-to-workspace", `echo > /workspace → deny(${d4.rule})`);

// bash 写重定向到 /outputs → allow
const d5 = evaluateToolCall(policy, bash("echo '{}' > /outputs/verdict.json"));
assert(d5.decision === "allow", `echo > /outputs → allow（got ${d5.decision}）`);

// sudo → deny
const d6 = evaluateToolCall(policy, bash("sudo apt-get update"));
assert(d6.decision === "deny" && d6.rule === "no-sudo", `sudo → deny(${d6.rule})`);

// rm -rf → deny
const d7 = evaluateToolCall(policy, bash("rm -rf /workspace"));
assert(d7.decision === "deny" && d7.rule === "no-recursive-delete", `rm -rf → deny(${d7.rule})`);

// 读工具（审查者全工具）→ allow
const d8 = evaluateToolCall(policy, read("/workspace/hello.txt"));
assert(d8.decision === "allow", `read /workspace/hello.txt → allow（got ${d8.decision}）`);

// bash 只读命令（ls/git diff/cat /workspace）→ allow
const d9 = evaluateToolCall(policy, bash("git -C /workspace diff --stat"));
assert(d9.decision === "allow", `git diff → allow（got ${d9.decision}）`);
const d10 = evaluateToolCall(policy, bash("ls -la"));
assert(d10.decision === "allow", `ls -la → allow（got ${d10.decision}）`);
const d11 = evaluateToolCall(policy, bash("cat /workspace/hello.txt"));
assert(d11.decision === "allow", `cat /workspace/hello.txt → allow（got ${d11.decision}）`);

if (failures > 0) {
  console.error(`\n${failures} assertion(s) failed`);
  process.exit(1);
}
console.log("\nALL PASS: reviewer deny-write 策略求值（确定性）");
