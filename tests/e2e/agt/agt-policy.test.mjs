// alfred × AGT 原型：确定性策略求值测试（无 LLM、无 pi 运行时）。
//
// 用 Node 原生 type-stripping 直接 import agt-policy.ts（Node >= 22.6，
// `import type` 在运行时被擦除，纯求值核心可被测试引用），
// 模拟 pi tool_call 事件（tool_name + args）断言 allow/deny 决策。这是
// "原型能跑"的确定性证明；pi 扩展把同一求值核心接到 `pi.on("tool_call")`
// 上（实时拦截由 agt/demo.sh 真容器演示）。
//
// 运行：node tests/e2e/agt/agt-policy.test.mjs

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  commandEscapesWorkspace,
  evaluateToolCall,
  parsePolicy,
  pathEscapesWorkspace,
} from "../../../docker/agt/agt-policy.ts";

const here = path.dirname(fileURLToPath(import.meta.url));
const policy = parsePolicy(readFileSync(path.join(here, "../../../docker/agt/executor/policy.json"), "utf8"));

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
  return { tool_name: "read", args: { path: p } };
}

console.log("== policy.json 语义 ==");

const d1 = evaluateToolCall(policy, bash("rm -rf /workspace"));
assert(d1.decision === "deny" && d1.rule === "recursive-delete", `rm -rf /workspace → deny(${d1.rule})`);
const d1b = evaluateToolCall(policy, bash("rm -rf /tmp/junk"));
assert(d1b.decision === "deny" && d1b.rule === "recursive-delete", `rm -rf /tmp/junk → deny`);

const d2 = evaluateToolCall(policy, bash("rm -f stale.tmp"));
assert(d2.decision === "allow", `rm -f（无 -r）→ allow（got ${d2.decision}）`);

const d3 = evaluateToolCall(policy, bash("sudo apt-get update"));
assert(d3.decision === "deny" && d3.rule === "no-sudo", `sudo → deny`);

const d4 = evaluateToolCall(policy, bash("cat /workspace/src/main.rs"));
assert(d4.decision === "allow", `cat /workspace/... → allow（got ${d4.decision}）`);

const d5 = evaluateToolCall(policy, bash("cat /etc/passwd"));
assert(d5.decision === "deny" && d5.rule === "no-host-path-touch", `cat /etc/passwd → deny(${d5.rule})`);

const d6 = evaluateToolCall(policy, bash("cat .env"));
assert(d6.decision === "deny" && d6.rule === "host-secret-read", `cat .env → deny(${d6.rule})`);

const d7 = evaluateToolCall(policy, write("/etc/cron.d/x"));
assert(d7.decision === "deny" && d7.rule === "workspace-write-only", `write /etc/cron.d/x → deny`);

const d8 = evaluateToolCall(policy, write("/workspace/hello.txt"));
assert(d8.decision === "allow", `write /workspace/hello.txt → allow`);

const d9 = evaluateToolCall(policy, edit("/workspace/src/lib.rs"));
assert(d9.decision === "allow", `edit /workspace/src/lib.rs → allow`);

const d10 = evaluateToolCall(policy, bash("ls -la"));
assert(d10.decision === "allow", `bash ls → allow（default_action）`);

console.log("== 路径/命令逃逸判定 ==");
assert(pathEscapesWorkspace("/workspace/foo") === false, "pathEscapes(/workspace/foo)=false");
assert(pathEscapesWorkspace("/workspace") === false, "pathEscapes(/workspace)=false");
assert(pathEscapesWorkspace("/etc/passwd") === true, "pathEscapes(/etc/passwd)=true");
assert(pathEscapesWorkspace("/workspace/../etc/x") === true, "pathEscapes(/workspace/../etc/x)=true（.. 上跳）");
assert(pathEscapesWorkspace("hello.txt") === false, "pathEscapes(hello.txt)=false（相对→/workspace）");
assert(pathEscapesWorkspace("../etc/x") === true, "pathEscapes(../etc/x)=true（相对..逃逸）");
assert(commandEscapesWorkspace("cat /etc/passwd") === true, "commandEscapes(cat /etc/passwd)=true");
assert(commandEscapesWorkspace("ls /workspace") === false, "commandEscapes(ls /workspace)=false");
assert(commandEscapesWorkspace("node /usr/bin/foo.js") === false, "commandEscapes(/usr 白名单)=false");
assert(commandEscapesWorkspace("cat ../../etc/x") === true, "commandEscapes(cat ../../etc/x)=true");

console.log("== 坏条件：非抛错畸形表达式 → 不命中（对齐 AGT _eval_expression） ==");
const badPolicy = parsePolicy(JSON.stringify({
  default_action: "allow",
  rules: [{ name: "bad-cond", condition: "tool_name == 'bash' and broken(", action: "deny" }],
}));
const d11 = evaluateToolCall(badPolicy, bash("echo hi"));
assert(d11.decision === "allow", "坏条件（不抛错）→ 不命中 → default allow（got " + d11.decision + "）");

console.log("== 优先级（priority 高者先命中） ==");
const prioPolicy = parsePolicy(JSON.stringify({
  default_action: "allow",
  rules: [
    { name: "low-allow", condition: "tool_name == 'bash'", action: "allow", priority: 0 },
    { name: "high-deny", condition: "tool_name == 'bash'", action: "deny", priority: 10 },
  ],
}));
const d12 = evaluateToolCall(prioPolicy, bash("anything"));
assert(d12.decision === "deny" && d12.rule === "high-deny", "priority 10 deny 覆盖 priority 0 allow");

console.log("== command_patterns 非法正则 fail-closed ==");
const rePolicy = parsePolicy(JSON.stringify({
  default_action: "allow",
  rules: [{ name: "bad-re", condition: "tool_name == 'bash'", command_patterns: [{ source: "([unclosed" }], action: "deny" }],
}));
const d13 = evaluateToolCall(rePolicy, bash("echo hi"));
assert(d13.decision === "deny" && d13.rule === "bad-re", "非法正则 → 按命中 → deny（fail-closed）");

console.log("== planner-policy.json 语义（宿主 pi：不可知拦写/拦读，占位符按 run 渲染） ==");
const plannerPolicyRaw = readFileSync(path.join(here, "../../../docker/agt/planner/policy.json"), "utf8");
const plannerWs = "/home/demo/project";
const plannerRun = `${plannerWs}/.alfred/runs/run-1`;
const plannerOut = `${plannerRun}/planner/outputs`;
process.env.AGT_WORKSPACE_DIR = plannerWs;
const plannerPolicy = parsePolicy(
  plannerPolicyRaw
    .replaceAll("__WORKSPACE_DIR__", plannerWs)
    .replaceAll("__OUTPUTS_DIR__", plannerOut)
    .replaceAll("__OUTPUTS_DIR_REL__", ".alfred/runs/run-1/planner/outputs")
    .replaceAll("__RUNS_DIR__", `${plannerWs}/.alfred/runs`)
    .replaceAll("__RUNS_DIR_REL__", ".alfred/runs")
    .replaceAll("__AGT_WORK__", `${plannerRun}/planner/agt`),
);

// §2.4 两分支产出：instructions.json（Instructions 接管）与 reply.txt（Reply 对话继续）都
// 写进产出目录（宿主收割）→ allow
const p1 = evaluateToolCall(plannerPolicy, write(`${plannerOut}/instructions.json`));
assert(p1.decision === "allow" && p1.rule === "allow-write-to-outputs", `planner write outputs/instructions.json → allow(${p1.rule})`);
const p1b = evaluateToolCall(plannerPolicy, write(`${plannerOut}/reply.txt`));
assert(p1b.decision === "allow" && p1b.rule === "allow-write-to-outputs", `planner write outputs/reply.txt → allow(${p1b.rule})`);

// 写项目根（cwd=工作区）→ deny（deny-write-to-workspace：target_path.startswith(ws) 命中）
const p2 = evaluateToolCall(plannerPolicy, write(`${plannerWs}/foo.txt`));
assert(p2.decision === "deny" && p2.rule === "deny-write-to-workspace", `planner write workspace/foo.txt → deny(${p2.rule})`);

// edit 项目源码同样 deny
const p2b = evaluateToolCall(plannerPolicy, edit(`${plannerWs}/src/lib.rs`));
assert(p2b.decision === "deny" && p2b.rule === "deny-write-to-workspace", `planner edit workspace/src/lib.rs → deny(${p2b.rule})`);

// 拦读 run 治理产物（不可知核心）：verdicts/conversation/audit/审查目录 → deny，
// 拒绝反馈中性（"路径不在工作范围"，不泄露内容存在性）
for (const gp of [
  `${plannerRun}/plan-verdicts.json`,
  `${plannerRun}/exec-verdicts.json`,
  `${plannerRun}/conversation.json`,
  `${plannerRun}/audit.jsonl`,
  `${plannerRun}/plan-review/verdict.json`,
  `${plannerRun}/exec-review/verdict.json`,
]) {
  const gd = evaluateToolCall(plannerPolicy, read(gp));
  assert(gd.decision === "deny" && gd.reason === "路径不在工作范围", `planner read ${path.basename(gp)} → deny 中性(${gd.decision}/${gd.reason})`);
}

// bash 触碰 run 目录 / .agt → deny；写重定向 / sudo / rm -rf → deny
assert(evaluateToolCall(plannerPolicy, bash(`cat ${plannerRun}/plan-verdicts.json`)).decision === "deny", "planner bash cat run governance → deny");
assert(evaluateToolCall(plannerPolicy, bash("ls .alfred/runs")).decision === "deny", "planner bash ls .alfred/runs → deny");
assert(evaluateToolCall(plannerPolicy, bash("echo x > out.txt")).decision === "deny", "planner bash 写重定向 → deny");
assert(evaluateToolCall(plannerPolicy, bash("sudo ls")).decision === "deny", "planner bash sudo → deny");

// 读项目文件 / 普通命令 → allow
const p3 = evaluateToolCall(plannerPolicy, bash("cat src/main.rs"));
assert(p3.decision === "allow", `planner read（bash cat 项目文件）→ allow（got ${p3.decision}）`);
const p3b = evaluateToolCall(plannerPolicy, read(`${plannerWs}/README.md`));
assert(p3b.decision === "allow", `planner read（read 项目文件）→ allow（got ${p3b.decision}）`);

// 无 path 的 write：target_path 缺失 → .startswith 返回 false 非抛错 → 规则不命中 → default allow
const p4 = evaluateToolCall(plannerPolicy, { tool_name: "write", args: { content: "x" } });
assert(p4.decision === "allow", `planner write 无 path → allow（got ${p4.decision}）`);
delete process.env.AGT_WORKSPACE_DIR;

if (failures > 0) {
  console.error(`\n${failures} assertion(s) failed`);
  process.exit(1);
}
console.log("\nALL PASS: AGT 策略求值原型（确定性）");

if (failures > 0) {
  console.error(`\n${failures} assertion(s) failed`);
  process.exit(1);
}
console.log("\nALL PASS: AGT 策略求值原型（确定性）");
