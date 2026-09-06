// alfred × AGT：reviewer 策略确定性求值测试（宿主 pi 化后语义，无 LLM/pi 运行时）。
//
// 用 Node 原生 type-stripping 直接 import agt-policy.ts（Node >= 22.6），
// 模拟 pi tool_call 事件断言 reviewer-policy.json 的"只拦写 + 产出白名单"语义
// （宿主形态：reviewer 读全放，写一律拦，唯一例外 = 审查产出目录）。
//
// 占位符：策略文件里的 {outputs_dir} / {outputs_redirect_allow} 由宿主落盘时
// 替换（host.rs render_reviewer_policy）。本测试复刻同一替换（JSON 转义规则
// 一致），断言替换后的求值语义：
//   - write/edit 指向产出目录 → allow（verdict 合法产出）；
//   - write/edit/delete/... 指向其他任何路径 → deny（deny-all-writes）；
//   - bash 写重定向到产出目录 → allow；重定向到别处 → deny（write 被拒后的
//     绕过通道，实测模型 fallback 行为）；
//   - sudo / rm -rf → deny；读工具（read/bash 只读）→ allow（读全放）。
//
// 运行：node tests/e2e/agt/reviewer-policy.test.mjs

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { evaluateToolCall, parsePolicy } from "../../../docker/agt/agt-policy.ts";

const here = path.dirname(fileURLToPath(import.meta.url));

// 占位符替换值（与 host.rs render_reviewer_policy 同构）：
const OUTPUTS = "/tmp/alfred-rvtest/plan-review/outputs";
const regexEscape = (s) => s.replace(/[\\.+*?()|[\]{}^$]/g, "\\$&");
const redirectAllow =
  `(?:^|[;|&\\s])(?:>>?|tee\\s+(?:-a\\s+)?)\\s*${regexEscape(OUTPUTS)}(?:/[^\\s|;&<>]*)?(?=[\\s]|$)`;

let raw = readFileSync(path.join(here, "../../../docker/agt/reviewer/policy.json"), "utf8");
// JSON 字符串值内替换：正则 source 的反斜杠需再转义（JSON.stringify 去引号）。
raw = raw
  .replace("{outputs_redirect_allow}", JSON.stringify(redirectAllow).slice(1, -1))
  .replaceAll("{outputs_dir}", JSON.stringify(OUTPUTS).slice(1, -1));
const policy = parsePolicy(raw);

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

console.log("== reviewer-policy.json 语义（只拦写 + 产出白名单） ==");

// —— 写工具：产出目录白名单 + 其余全拒 ——
const a1 = evaluateToolCall(policy, write(`${OUTPUTS}/verdict.json`));
assert(a1.decision === "allow" && a1.rule === "allow-write-verdict-output",
  `write outputs/verdict.json → allow（got ${a1.decision}/${a1.rule}）`);

const d1 = evaluateToolCall(policy, write("/workspace/hello.txt"));
assert(d1.decision === "deny" && d1.rule === "deny-all-writes",
  `write /workspace/hello.txt → deny（got ${d1.decision}/${d1.rule}）`);

const d2 = evaluateToolCall(policy, edit("/workspace/src/lib.rs"));
assert(d2.decision === "deny" && d2.rule === "deny-all-writes",
  `edit /workspace/src/lib.rs → deny（got ${d2.decision}/${d2.rule}）`);

const d2b = evaluateToolCall(policy, del("/workspace/tmp-cleanup"));
assert(d2b.decision === "deny" && d2b.rule === "deny-all-writes",
  `delete /workspace/tmp-cleanup → deny（got ${d2b.decision}/${d2b.rule}）`);

// 写 run 治理产物（非产出目录）→ deny
const d2c = evaluateToolCall(policy, write("/tmp/alfred-rvtest/plan-verdicts.json"));
assert(d2c.decision === "deny" && d2c.rule === "deny-all-writes",
  `write plan-verdicts.json → deny（got ${d2c.decision}/${d2c.rule}）`);

// —— bash 写重定向：产出目录放行、其余拦（write 被拒后的绕过通道）——
const a2 = evaluateToolCall(policy, bash(`echo '{}' > ${OUTPUTS}/verdict.json`));
assert(a2.decision === "allow" && a2.rule === "allow-write-redirect-verdict-output",
  `echo > outputs/verdict.json → allow（got ${a2.decision}/${a2.rule}）`);

const a2b = evaluateToolCall(policy, bash(`echo x > ${OUTPUTS}`));
assert(a2b.decision === "allow", `echo > outputs（目录本身）→ allow（got ${a2b.decision}/${a2b.rule}）`);

const d4 = evaluateToolCall(policy, bash("echo x > /workspace/probe.txt"));
assert(d4.decision === "deny" && d4.rule === "deny-bash-write-redirect",
  `echo > /workspace → deny（got ${d4.decision}/${d4.rule}）`);

const d4b = evaluateToolCall(policy, bash("echo x | tee /etc/contraband"));
assert(d4b.decision === "deny" && d4b.rule === "deny-bash-write-redirect",
  `tee /etc/contraband → deny（got ${d4b.decision}/${d4b.rule}）`);

// —— sudo / rm -rf ——
const d6 = evaluateToolCall(policy, bash("sudo apt-get update"));
assert(d6.decision === "deny" && d6.rule === "no-sudo", `sudo → deny(${d6.rule})`);

const d7 = evaluateToolCall(policy, bash("rm -rf /workspace"));
assert(d7.decision === "deny" && d7.rule === "no-recursive-delete", `rm -rf → deny(${d7.rule})`);

// —— 读工具（审查者全可见）——
const d8 = evaluateToolCall(policy, read("/workspace/hello.txt"));
assert(d8.decision === "allow", `read /workspace/hello.txt → allow（got ${d8.decision}）`);

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
console.log("\nALL PASS: reviewer 策略求值（只拦写 + 产出白名单，确定性）");
