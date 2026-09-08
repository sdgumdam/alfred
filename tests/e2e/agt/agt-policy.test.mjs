// alfred × AGT 原型：确定性策略求值测试（无 LLM、无 pi 运行时）。
//
// 用 Node 原生 type-stripping 直接 import agt-policy.ts（Node >= 22.6，
// `import type` 在运行时被擦除，纯求值核心可被测试引用），
// 模拟 pi tool_call 事件（tool_name + args）断言 allow/deny 决策。这是
// "原型能跑"的确定性证明；pi 扩展把同一求值核心接到 `pi.on("tool_call")`
// 上（实时拦截由 agt/demo.sh 真容器演示）。
//
// 运行：node tests/e2e/agt/agt-policy.test.mjs

import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  appendAudit,
  commandEscapesWorkspace,
  evaluateToolCall,
  inRefVolume,
  parsePolicy,
  pathEscapesWorkspace,
  refVolumeDirs,
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

/** 临时目录（确定性测试：真实 run 目录模拟不落仓库）。 */
function makeTempDir(label) {
  return mkdtempSync(path.join(os.tmpdir(), label));
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

console.log("== 9/3 方案②：只读参考卷边界豁免（AGT_REF_VOLUMES） ==");
// 9/3 方案②：executor 宿主材料进路——参考卷以 ro 挂到 /workspace 之外
// （如 /references），边界判定必须放行读取（物理 ro 兜底，写由挂载层拒绝）。
assert(refVolumeDirs().length === 0, "未设 AGT_REF_VOLUMES → 空参考卷集（语义不变）");

process.env.AGT_REF_VOLUMES = "/references:/design-docs/";
assert(
  JSON.stringify(refVolumeDirs()) === JSON.stringify(["/references", "/design-docs"]),
  `refVolumeDirs 解析冒号分隔 + 剥尾斜杠（got ${JSON.stringify(refVolumeDirs())}）`
);

// 逃逸判定：参考卷子树不视为逃逸
assert(pathEscapesWorkspace("/references/docs/治理架构.md") === false, "pathEscapes(/references/…) = false（豁免面）");
assert(inRefVolume("/references") === true, "inRefVolume(/references) = true");
assert(inRefVolume("/references/a/b.md") === true, "inRefVolume 子树 = true");
assert(inRefVolume("/references/a/../b.md") === true, "inRefVolume 归一化子树（.. 内部解析）= true");
assert(inRefVolume("/referenceE") === false, "前缀相似路径不误命中");
assert(inRefVolume("/workspace/x") === false, "工作区内路径不属参考卷");

// 命令级：cat /references/... 不再命中 no-host-path-touch
const rv1 = evaluateToolCall(policy, bash("cat /references/治理架构.md"));
assert(rv1.decision === "allow", `cat /references/治理架构.md → allow（got ${rv1.decision}/${rv1.rule}）`);
const rv2 = evaluateToolCall(policy, bash("ls -la /references && head -20 /design-docs/README.md"));
assert(rv2.decision === "allow", `ls/head 参考卷 → allow（got ${rv2.decision}/${rv2.rule}）`);
const rv3 = evaluateToolCall(policy, bash("grep -rn 治理 /references/ | wc -l"));
assert(rv3.decision === "allow", `grep 参考卷 → allow（got ${rv3.decision}/${rv3.rule}）`);

// read 工具直读参考卷（path_escapes_workspace 上下文位放行 → default allow）
const rv4 = evaluateToolCall(policy, read("/references/docs/限界上下文.md"));
assert(rv4.decision === "allow", `read /references/... → allow（got ${rv4.decision}/${rv4.rule}）`);

// 豁免面外仍拦：非参考卷宿主路径
const rv5 = evaluateToolCall(policy, bash("cat /etc/passwd"));
assert(rv5.decision === "deny" && rv5.rule === "no-host-path-touch", `cat /etc/passwd 仍 deny（got ${rv5.decision}/${rv5.rule}）`);
// 相对路径 .. 穿越仍拦（豁免面不看相对形态——normalize 后落参考卷才豁免）
assert(commandEscapesWorkspace("cat ../../etc/x") === true, "相对 .. 穿越仍 true");

// 写参考卷：策略层与 /workspace 内写同语义（不命中 deny 规则 → default allow）——
// 参考卷的写防护是**物理 ro 挂载**（docker :ro，写入直接 EROFS），策略层不重复拦
// （9/3 方案②原话：ro 物理只读无需额外拦写）。策略面只保证"读放行"。
const rv6 = evaluateToolCall(policy, write("/references/hacked.md"));
assert(rv6.decision === "allow", `write 参考卷 → allow（写防护由物理 ro 挂载承担，got ${rv6.decision}/${rv6.rule}）`);
// 对照：写参考卷外宿主路径仍被 workspace-write-only 拦（豁免面没有放大写边界）
const rv7 = evaluateToolCall(policy, write("/etc/hacked.md"));
assert(rv7.decision === "deny" && rv7.rule === "workspace-write-only", `write /etc → 仍 deny（got ${rv7.decision}/${rv7.rule}）`);

delete process.env.AGT_REF_VOLUMES;
assert(refVolumeDirs().length === 0, "delete env → 参考卷集清空");

// 恢复无参考卷基线语义（后续段落不受豁免影响）
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

// ============================================================================
// planner-policy.json 语义（宿主 pi：不可知隔离——占位符由 host.rs
// render_planner_policy 按**真实 run 目录**渲染；本节复刻同一替换，
// run 目录用临时目录模拟 `$ALFRED_STATE_DIR` 布局——run 根在项目根之外，
// 旧写死 `<ws>/.alfred/runs` 前缀从不命中，HostPiAudit P0 修复的负向断言）。
// ============================================================================
const plannerPolicyRaw = readFileSync(path.join(here, "../../../docker/agt/planner/policy.json"), "utf8");
const plannerWs = makeTempDir("alfred-planner-ws-");
const plannerState = makeTempDir("alfred-state-"); // 模拟 $ALFRED_STATE_DIR（~/.local/state/alfred 布局：run 根在项目根外）
const plannerRun = path.join(plannerState, "runs", "run-1");
const plannerOut = path.join(plannerRun, "planner", "outputs");
const plannerAgtWork = path.join(plannerRun, "planner", "agt");
process.env.AGT_WORKSPACE_DIR = plannerWs;
const regexEscape = (s) => s.replace(/[\\.+*?()|[\]{}^$]/g, "\\$&");
const jsonInner = (s) => JSON.stringify(s).slice(1, -1);
// 复刻体语义 = host.rs render_planner_policy（复审⑤对齐）：相对形态 =
// strip_prefix(ws + "/")，剥不动（run 根在项目根外，state 布局）退化为绝对串
// （恒不命中，安全侧：绝对前缀规则仍覆盖）——path.relative 恒产生 ../ 形态，与
// host.rs 不同，state 布局下 ../ 负向断言认证的是运行时不存在的能力。
const stripPrefix = (abs) => {
  const p = abs.startsWith(plannerWs + "/") ? abs.slice(plannerWs.length + 1) : abs;
  return p;
};
const plannerRunRel = stripPrefix(plannerRun);
const plannerOutRel = stripPrefix(plannerOut);
const outputsRedirectAllow =
  `(?:^|[;|&\\s])(?:>>?|tee\\s+(?:-a\\s+)?)\\s*(?:${regexEscape(plannerOut)}|${regexEscape(plannerOutRel)})(?:/[^\\s|;&<>]*)?(?=[\\s]|$)`;
// planner/outputs 子树负向断言（carve）：命中 run 字面前缀、其后无 planner/outputs
// 子树才拦——合法写只内嵌 run 前缀（<run>/planner/outputs），plan-review /
// exec-review 的 outputs 子树仍被拦（UnknowFinalAudit P1：审查结论不泄露）。
const outputsCarve = `(?!(?:/|\\\\)(?:[^/\\\\]*(?:/|\\\\))*planner(?:/|\\\\)outputs(?:/|\\\\|$))`;
const runDirPattern =
  `(?:${regexEscape(plannerRun)}|${regexEscape(plannerRunRel)}|\\.\\.[/\\\\]|~/|\\$(?:HOME\\b|\\{HOME\\})|\\.alfred/runs)${outputsCarve}`;
const agtWorkPattern = `(?:${regexEscape(plannerAgtWork)}|\\.agt/)`;
// 优先级同 docker/agt/planner/policy.json（policy.json 为单一真源，此处仅复刻）。
const PLANNER_PRIORITY = {
  "allow-write-to-outputs": 40,
  "deny-bash-governance-paths": 38,
  "deny-governance-files": 36,
  "allow-bash-write-outputs": 34,
  "deny-bash-write-family": 32,
  "deny-write-to-workspace": 20,
};
const renderedPlannerPolicy = plannerPolicyRaw
  .replace("{outputs_redirect_allow}", jsonInner(outputsRedirectAllow))
  .replace("{run_dir_pattern}", jsonInner(runDirPattern))
  .replace("{agt_work_pattern}", jsonInner(agtWorkPattern))
  .replace("{run_dir_rel}", jsonInner(plannerRunRel))
  .replace("{run_dir}", jsonInner(plannerRun))
  .replace("{outputs_dir_rel}", jsonInner(plannerOutRel))
  .replace("{outputs_dir}", jsonInner(plannerOut))
  .replace("{agt_work}", jsonInner(plannerAgtWork))
  .replace("{workspace_dir}", jsonInner(plannerWs));
// 复刻体逐规则对齐 policy.json 优先级（真断言防漂移：policy.json 数字改动即红——
// 静默覆写会让组合遮蔽回归假绿，UnknowFinalAudit P2）。
const stageDoc = JSON.parse(renderedPlannerPolicy);
for (const rule of stageDoc.rules) {
  assert(rule.name in PLANNER_PRIORITY && rule.priority === PLANNER_PRIORITY[rule.name],
    `优先级防漂移 ${rule.name}=${rule.priority} === PLANNER_PRIORITY`);
}
const plannerPolicy = parsePolicy(JSON.stringify(stageDoc));

// §2.4 两分支产出：instructions.json（Instructions 接管）与 reply.txt（Reply 对话继续）都
// 写进产出目录（宿主收割）→ allow
const p1 = evaluateToolCall(plannerPolicy, write(`${plannerOut}/instructions.json`));
assert(p1.decision === "allow" && p1.rule === "allow-write-to-outputs", `planner write outputs/instructions.json → allow(${p1.rule})`);
const p1b = evaluateToolCall(plannerPolicy, write(`${plannerOut}/reply.txt`));
assert(p1b.decision === "allow" && p1b.rule === "allow-write-to-outputs", `planner write outputs/reply.txt → allow(${p1b.rule})`);

// bash 写重定向 / tee 指向产出目录 → allow（写族放行的唯一出口）
const p1c = evaluateToolCall(plannerPolicy, bash(`echo '{}' > ${plannerOut}/instructions.json`));
assert(p1c.decision === "allow" && p1c.rule === "allow-bash-write-outputs", `planner bash > outputs → allow(${p1c.rule}/${p1c.decision})`);
const p1d = evaluateToolCall(plannerPolicy, bash(`echo x | tee -a ${plannerOut}/reply.txt`));
assert(p1d.decision === "allow" && p1d.rule === "allow-bash-write-outputs", `planner bash tee -a outputs → allow(${p1d.rule}/${p1d.decision})`);

// 写项目根（cwd=工作区）→ deny（deny-write-to-workspace：target_path.startswith(ws) 命中）
const p2 = evaluateToolCall(plannerPolicy, write(`${plannerWs}/foo.txt`));
assert(p2.decision === "deny" && p2.rule === "deny-write-to-workspace", `planner write workspace/foo.txt → deny(${p2.rule})`);

// edit 项目源码同样 deny
const p2b = evaluateToolCall(plannerPolicy, edit(`${plannerWs}/src/lib.rs`));
assert(p2b.decision === "deny" && p2b.rule === "deny-write-to-workspace", `planner edit workspace/src/lib.rs → deny(${p2b.rule})`);

// P0 负向断言：**真实 run 目录**（项目根外，$ALFRED_STATE_DIR 布局）下拦读 run
// 治理产物：verdicts/conversation/audit/审查目录 → deny + 中性反馈
// （"路径不在允许的工作范围"，不泄露规则名/内容存在性——HostPiAudit P1）
for (const gp of [
  `${plannerRun}/plan-verdicts.json`,
  `${plannerRun}/exec-verdicts.json`,
  `${plannerRun}/conversation.json`,
  `${plannerRun}/audit.jsonl`,
  `${plannerRun}/plan-review/verdict.json`,
  `${plannerRun}/exec-review/verdict.json`,
]) {
  const gd = evaluateToolCall(plannerPolicy, read(gp));
  assert(gd.decision === "deny" && gd.reason === "路径不在允许的工作范围", `planner read ${path.basename(gp)} → deny 中性(${gd.decision}/${gd.reason})`);
}

// UnknowFinalAudit P1 对抗探针（review-outputs 读）：carve 收窄到 planner/outputs
// 子树后，plan-review/outputs 与 exec-review/outputs（各含 verdict.json = 审查
// 结论全文）必须 deny——点名与不点名文件的内容原语（grep -r / head / cat 通配 /
// ls / find）逐条封堵，堵"grep -r pass <run>/plan-review/outputs/"读自家审查结论。
for (const cmd of [
  `grep -r pass ${plannerRun}/plan-review/outputs/`,
  `grep -ri verdict ${plannerRun}/exec-review/outputs/`,
  `head -5 ${plannerRun}/plan-review/outputs/verdict.json`,
  `tail -20 ${plannerRun}/exec-review/outputs/verdict.json`,
  `cat ${plannerRun}/plan-review/outputs/v*`,
  `cat ${plannerRun}/exec-review/outputs/*`,
  `less ${plannerRun}/plan-review/outputs/verdict.json`,
  `wc -l ${plannerRun}/exec-review/outputs/verdict.json`,
  `ls ${plannerRun}/plan-review/outputs/`,
  `ls -la ${plannerRun}/exec-review/outputs`,
  `find ${plannerRun}/plan-review/outputs -name '*.json'`,
  `stat ${plannerRun}/exec-review/outputs/verdict.json`,
  `file ${plannerRun}/plan-review/outputs/verdict.json`,
  `cat "${plannerRun}/plan-review/outputs/verdict.json"`,
  `cat '${plannerRun}/exec-review/outputs/verdict.json'`,
]) {
  const rd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(rd.decision === "deny" && rd.rule === "deny-bash-governance-paths",
    `对抗探针⑥ review-outputs 读 ${cmd.slice(0, 52)}… → deny（got ${rd.decision}/${rd.rule}）`);
}
// read 工具直读 review-outputs → deny（deny-governance-files 前缀命中）
for (const gp of [
  `${plannerRun}/plan-review/outputs/verdict.json`,
  `${plannerRun}/exec-review/outputs/verdict.json`,
]) {
  const gd = evaluateToolCall(plannerPolicy, read(gp));
  assert(gd.decision === "deny" && gd.rule === "deny-governance-files",
    `对抗探针⑥ read review-outputs ${gp.slice(gp.indexOf("/runs/") + 1)} → deny（got ${gd.decision}/${gd.rule}）`);
}

// P1 负向断言：outputs 白名单无穿越——相对路径与 ../ 穿越必须落回真实 run 前缀
// （normalizePath 归一化 + 绝对前缀双形态），逃逸不成立 → deny
const relOut = path.relative(plannerWs, plannerOut);
for (const tp of [
  `${relOut}/../../plan-verdicts.json`, // ../ 穿越出产出目录 → run 治理产物
  `${plannerOut}/../conversation.json`, // 同上（直接上跳一级）
]) {
  const td = evaluateToolCall(plannerPolicy, write(tp));
  assert(td.decision === "deny", `planner write 穿越形态 ${tp} → deny（got ${td.decision}/${td.rule}）`);
}

// bash 触碰 run 目录 / .agt → deny；写族（重定向到白名单外 / tee / dd / cp / mv /
// ln / mkdir / rm / sed -i / sudo）→ deny（HostPiAudit P1：bash 写通道封堵）
const bashDenyCases = [
  `cat ${plannerRun}/plan-verdicts.json`,
  `cat ../${path.basename(plannerState)}/runs/run-1/plan-verdicts.json`,
  "ls .alfred/runs",
  "cat planner/../.agt/policy.json",
  "echo x > out.txt",
  "echo x | tee out.txt",
  "dd if=/dev/zero of=out.txt bs=1 count=1",
  `cp /etc/passwd ${plannerRun}/stolen.json`,
  `mv src/lib.rs ${plannerRun}/lib.rs`,
  "ln -s /etc/passwd pwn",
  "mkdir rogue-dir",
  "rm -rf ws",
  "rm stale.tmp",
  "sed -i 's/a/b/' src/lib.rs",
  "sudo ls",
];
for (const cmd of bashDenyCases) {
  const bd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(bd.decision === "deny" && bd.reason === "路径不在允许的工作范围", `planner bash ${cmd} → deny 中性（got ${bd.decision}/${bd.rule}/${bd.reason}）`);
}

// ============================================================================
// 第四轮对抗探针（UnknowRecheck 复审 5 点——组合遮蔽 / 非字面形态 / fd 与无空格
// 重定向 / block reason 真实反馈面）。全部负向：逐条 deny + 中性 reason。
// ============================================================================
console.log("== 第四轮对抗探针（组合遮蔽 / 非字面形态 / fd·无空格重定向） ==");
// ① 优先级遮蔽回归：治理读 + 产出写组合命令曾被 allow-bash-write-outputs
//    整体放行（含 > outputs/../plan-verdicts.json 覆写治理文件）——governance
//    deny(38) 先于 bash allow(34) 后必须逐条 deny。
const comboProbes = [
  `cat ${plannerRun}/plan-verdicts.json > ${plannerOut}/leak.txt`, // 治理读+产出写
  `cat ${plannerRun}/plan-verdicts.json > ${plannerOut}/../plan-verdicts.json`, // ../ 覆写治理文件（回归）
  `cat ${plannerRun}/plan-verdicts.json > ${plannerOut}/../conversation.json`, // ../ 覆写同级治理文件
];
for (const cmd of comboProbes) {
  const cd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(cd.decision === "deny" && cd.reason === "路径不在允许的工作范围",
    `对抗探针① 组合遮蔽 ${cmd.slice(0, 44)}… → deny（got ${cd.decision}/${cd.rule}）`);
}
// ② 非字面形态：../ 穿越串 / ~ / $HOME / ${HOME} / .alfred/runs / 治理关键词，
//    均不命中字面前缀（host.rs 与 policy.json 静态层双单源同形态）。
const literalEscapeProbes = [
  `cat ../../${path.basename(plannerState)}/runs/run-1/plan-verdicts.json`,
  `cat ../../${path.basename(plannerState)}/runs/run-1/../../runs/run-1/plan-verdicts.json`,
  "ls ~/.local/state/alfred/runs",
  "cat $HOME/.local/state/alfred/runs/run-1/plan-verdicts.json",
  "cat ${HOME}/.local/state/alfred/runs/run-1/conversation.json",
  "ls .alfred/runs/run-1",
  "cat .alfred/runs/run-1/plan-verdicts.json",
  "ls run-dir-somewhere-else",
  "cat plan-verdicts.json",
  "cat exec-verdicts.json",
  "tail -5 conversation.json",
  "wc -l audit.jsonl",
];
for (const cmd of literalEscapeProbes) {
  const ld = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(ld.decision === "deny" && ld.reason === "路径不在允许的工作范围",
    `对抗探针② 非字面形态 ${cmd} → deny（got ${ld.decision}/${ld.rule}）`);
}
// ③ 合法写不被 governance 层误杀（carve 精确性——run 字面前缀 + planner/outputs
//    子树命中时 deny-governance-paths 让路，allow-bash-write-outputs(34) 接住放行）。
const legitOutWrites = [
  `echo '{}' > ${plannerOut}/instructions.json`,
  `echo x > ${plannerOut}/sub/deep/reply.txt`,
  `echo x >> ${plannerOut}/reply.txt`,
  `echo x | tee -a ${plannerOut}/reply.txt`,
  `echo x | tee -a ${plannerOutRel}/reply.txt`, // 相对形态同放行
];
for (const cmd of legitOutWrites) {
  const gd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(gd.decision === "allow" && gd.rule === "allow-bash-write-outputs",
    `对抗探针③ 合法 outputs 写不误杀 ${cmd.slice(0, 40)}… → allow（got ${gd.decision}/${gd.rule}）`);
}
// ③-2 carve 收窄负向：重定向指向 review-outputs 子树不因 outputs 段放行（P1 回归
//    面——旧 carve"任意 outputs 段"会整体放行含写）。
const reviewRedirectProbes = [
  `echo x > ${plannerRun}/plan-review/outputs/verdict.json`,
  `echo '{"decision":"deny"}' > ${plannerRun}/exec-review/outputs/verdict.json`,
  `cat src/x.md >> ${plannerRun}/plan-review/outputs/verdict.json`,
];
for (const cmd of reviewRedirectProbes) {
  const rd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(rd.decision === "deny" && rd.rule === "deny-bash-governance-paths",
    `对抗探针③-2 review-outputs 重定向写 ${cmd.slice(0, 48)}… → deny（got ${rd.decision}/${rd.rule}）`);
}
// ④ 写族漏形态：无空格 `>`（>file）与 fd 限定 `2>` / `1>>` / `&>>` → deny；
//    fd 丢弃（`2>/dev/null` / `1>/dev/null`）→ allow（stderr/stdout 丢弃无害只读习惯，
//    非写文件——DevnullFix 修复误杀：planner 标准探查 `ls ... 2>/dev/null | head` 整条被拦复发）。
const redirectFormProbes = [
  "echo x >file.txt",
  "echo x >>file.txt",
  "echo x 2>err.txt",
  "echo x 1>>out.txt",
  "echo x &>>all.txt",
  "cmd1 2> /dev/null; echo x > out.txt",
];
for (const cmd of redirectFormProbes) {
  const rd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(rd.decision === "deny", `对抗探针④ 写族漏形态 ${cmd} → deny（got ${rd.decision}/${rd.rule}）`);
}
// ④-2 fd 丢弃豁免（devnull-fix 回归面）：`2>/dev/null` / `1>/dev/null` 整条 allow；
//    fd 指向真实文件（`2>file` / `2>>file`）仍 deny——负向先行只豁免 /dev/null 目标。
const devnullAllowProbes = [
  "ls src 2>/dev/null | head -30",
  "find src 2>/dev/null | head -5",
  "grep -rn todo src 2>/dev/null",
  "cmd 1>/dev/null",
];
for (const cmd of devnullAllowProbes) {
  const rd = evaluateToolCall(plannerPolicy, bash(cmd));
  assert(rd.decision === "allow", `fd 丢弃豁免④-2 ${cmd} → allow（got ${rd.decision}/${rd.rule}）`);
}
// ⑤ block reason 中性（复审③：真实反馈面 = 扩展层 agt-policy.ts 的 block 输出，
//    非 engine reason）——模拟扩展层拼装，断言拼装产物不含规则名/治理语义词。
const extensionBlock = (d) => `操作被策略拒绝：${d.reason}`;
for (const d of [
  evaluateToolCall(plannerPolicy, read(`${plannerRun}/plan-verdicts.json`)),
  evaluateToolCall(plannerPolicy, bash(`cat ${plannerRun}/plan-verdicts.json > ${plannerOut}/leak.txt`)),
  evaluateToolCall(plannerPolicy, bash("cat plan-verdicts.json")),
]) {
  const msg = extensionBlock(d);
  assert(!msg.includes(d.rule ?? "") && !/\[(?:deny|allow)[-\]a-z]*/i.test(msg),
    `对抗探针⑤ block reason 无规则名（got ${msg}）`);
  assert(!/governance|verdict|conversation|audit/i.test(msg.replace("操作被策略拒绝：", "")),
    `对抗探针⑤ block reason 无治理语义词（got ${msg}）`);
}
// ⑤b 扩展层真实源码级断言：agt-policy.ts 的 deny 分支模板串不得拼 rule 名。
{
  const extSource = readFileSync(path.join(here, "../../../docker/agt/agt-policy.ts"), "utf8");
  const denyBranch = extSource.slice(extSource.indexOf('decision.decision === "deny"'));
  assert(denyBranch.includes("`操作被策略拒绝：${decision.reason}`"),
    "对抗探针⑤b agt-policy.ts deny 分支用中性模板（源码级）");
  assert(!denyBranch.includes("${decision.rule}"),
    "对抗探针⑤b agt-policy.ts deny 分支不含规则名插值（源码级）");
}

// 读项目文件 / 普通只读命令 → allow（隔离面只封治理产物与写通道，不误伤）
const p3 = evaluateToolCall(plannerPolicy, bash("cat src/main.rs"));
assert(p3.decision === "allow", `planner read（bash cat 项目文件）→ allow（got ${p3.decision}）`);
const p3b = evaluateToolCall(plannerPolicy, read(`${plannerWs}/README.md`));
assert(p3b.decision === "allow", `planner read（read 项目文件）→ allow（got ${p3b.decision}）`);
const p3c = evaluateToolCall(plannerPolicy, bash("git status --porcelain"));
assert(p3c.decision === "allow", `planner bash git status → allow（got ${p3c.decision}/${p3c.rule}）`);
const p3d = evaluateToolCall(plannerPolicy, bash("ls src"));
assert(p3d.decision === "allow", `planner bash ls src → allow（got ${p3d.decision}）`);
const p3e = evaluateToolCall(plannerPolicy, bash("grep -rn todo src | wc -l"));
assert(p3e.decision === "allow", `planner bash grep|wc → allow（got ${p3e.decision}）`);
// 无 path 的 write：target_path 缺失 → .startswith 返回 false 非抛错 → 规则不命中 → default allow
const p4 = evaluateToolCall(plannerPolicy, { tool_name: "write", args: { content: "x" } });
assert(p4.decision === "allow", `planner write 无 path → allow（got ${p4.decision}）`);
delete process.env.AGT_WORKSPACE_DIR;
for (const d of [plannerWs, plannerState]) rmSync(d, { recursive: true, force: true });

// ============================================================================
// 审计 path 契约（MaintainerAudit P0）：appendAudit 条目形状 = host.rs
// AgtAuditLine 消费契约钉死——每次决策行必须携带 path（key_file_paths 真实
// 数据源 = allow read 行的 path 提取；deny 行也带 path，提取侧按
// decision==allow 过滤，无泄露）。此前 appendAudit 从不写 path，生产审计
// 恒缺 path——README 宣称的"真实数据源"不存在。
// ============================================================================
console.log("== 审计 path 契约（key_file_paths 数据源 = allow read 行 path） ==");
{
  const auditDir = makeTempDir("alfred-audit-contract-");
  const auditFile = path.join(auditDir, "audit.jsonl");
  process.env.AGT_AUDIT_PATH = auditFile;
  // read 工具 allow → path 必须落盘（extractPath 的 file_path 形态也覆盖）。
  for (const ev of [
    { toolName: "read", input: { path: "/workspace/src/main.rs" } },
    { toolName: "edit", input: { file_path: "/workspace/src/lib.rs" } },
    { toolName: "bash", input: { command: "cat /etc/passwd" } }, // deny；bash 无 path
    { toolName: "write", input: { content: "x" } }, // 无路径参数 → path 缺省
  ]) {
    const d = evaluateToolCall(policy, { tool_name: ev.toolName, args: ev.input });
    appendAudit(auditFile, {
      ts: "2026-09-07T00:00:00.000Z",
      tool_name: ev.toolName,
      command: typeof ev.input.command === "string" ? ev.input.command : undefined,
      path: ev.input.path ?? ev.input.file_path,
      decision: d.decision,
      rule: d.rule,
      reason: d.reason,
    });
  }
  const lines = readFileSync(auditFile, "utf8").trim().split("\n").map((l) => JSON.parse(l));
  assert(lines.length === 4, `审计 4 行 JSONL（got ${lines.length}）`);
  for (const l of lines) {
    assert(
      typeof l.ts === "string" && typeof l.tool_name === "string"
        && (l.decision === "allow" || l.decision === "deny")
        && "rule" in l && typeof l.reason === "string",
      `审计行含 host.rs 消费必需字段 ts/tool_name/decision/rule/reason：${JSON.stringify(l)}`,
    );
  }
  const readAllow = lines[0];
  assert(readAllow.decision === "allow" && readAllow.tool_name === "read"
    && readAllow.path === "/workspace/src/main.rs",
    `allow read 行携带 path（key_file_paths 数据源）：${JSON.stringify(readAllow)}`);
  const editAllow = lines[1];
  assert(editAllow.decision === "allow" && editAllow.path === "/workspace/src/lib.rs",
    `allow edit 行携带 file_path 形态 path：${JSON.stringify(editAllow)}`);
  const bashDeny = lines[2];
  assert(bashDeny.decision === "deny" && bashDeny.path === undefined,
    `bash 无 path（extractPath 对 bash 返 undefined，不可伪造）：${JSON.stringify(bashDeny)}`);
  // 消费侧契约演练：host.rs extract_allow_read_paths 同款过滤（decision==allow
  // 且 tool_name==read 且 path 非空）在真实审计行上提取出且仅提取出 allow read 路径
  // ——deny 行的 path 不会被提取（无治理面泄露）。
  const extracted = [...new Set(lines
    .filter((l) => l.decision === "allow" && l.tool_name === "read" && l.path)
    .map((l) => l.path))].sort();
  assert(extracted.length === 1 && extracted[0] === "/workspace/src/main.rs",
    `提取器契约：allow read 过滤 → ${JSON.stringify(extracted)}`);
  delete process.env.AGT_AUDIT_PATH;
  rmSync(auditDir, { recursive: true, force: true });
}
// 扩展工厂源码级断言（⑤b 同款）：pi.on("tool_call") 两处 appendAudit 都必须
// 携带 path 字段（生产者侧真源——改掉即红，防止回退成"生产恒空"契约断裂）。
{
  const extSource = readFileSync(path.join(here, "../../../docker/agt/agt-policy.ts"), "utf8");
  const handler = extSource.slice(extSource.indexOf('pi.on("tool_call"'));
  const auditCalls = handler.split("appendAudit(auditPath").length - 1;
  assert(auditCalls === 2, `工厂恰好两处 appendAudit（got ${auditCalls}）`);
  for (const seg of handler.split("appendAudit(auditPath").slice(1)) {
    const entry = seg.slice(0, seg.indexOf("});"));
    assert(/\bpath\b/.test(entry), `appendAudit 条目携带 path：${entry.replace(/\s+/g, " ").trim()}`);
  }
}
// ============================================================================
// fail-closed reason 脱敏契约（A1/DebtInventory）：策略加载失败 → planner 直面
// 的 block reason 中性（零路径 / 零错误原文），完整错误只落审计 JSONL（宿主侧
// AGT_AUDIT_PATH，agent 不可读）。真工厂驱动（default export + 桩 pi 收集
// handler）：block 面（planner 可见）与审计面（JSONL 行）两侧各自钉死——
// 生产者-消费者契约：脱敏的是模型反馈面，不是审计排障面。
// ============================================================================
console.log("== fail-closed reason 脱敏（block 中性 + 错误原文仅入审计） ==");
{
  const { default: agtFactory } = await import("../../../docker/agt/agt-policy.ts");
  const auditDir = makeTempDir("alfred-agt-failclosed-");
  const auditFile = path.join(auditDir, "audit.jsonl");
  process.env.AGT_AUDIT_PATH = auditFile;
  process.env.AGT_POLICY_PATH = path.join(auditDir, "missing-policy.json"); // 必然加载失败
  const handlers = {};
  agtFactory({ on: (ev, fn) => { handlers[ev] = fn; } });

  // planner 直面①：block reason 恰为中性句——零路径 / 零错误原文 / 零实现细节
  const block = await handlers["tool_call"]({
    toolName: "bash", toolCallId: "fc-bash", input: { command: "echo hi" },
  });
  assert(block && block.block === true,
    `fail-closed → block=true（got ${JSON.stringify(block)}）`);
  assert(block.reason === "策略不可用，操作被拒绝",
    `fail-closed block reason 恰为中性句（got ${JSON.stringify(block.reason)}）`);

  // planner 直面②：write 事件的 block 同样中性（策略加载失败对所有工具一致脱敏）
  const blockW = await handlers["tool_call"]({
    toolName: "write", toolCallId: "fc-write", input: { path: "/tmp/x.txt", content: "x" },
  });
  assert(blockW && blockW.block === true && blockW.reason === "策略不可用，操作被拒绝",
    `fail-closed write block 同样中性（got ${JSON.stringify(blockW)}）`);

  // 审计面：完整错误原文（含策略路径）落 JSONL——宿主排障真源；block 面丢的
  // 信息这里必须找得回（生产者-消费者契约的消费侧验证）。
  const lines = readFileSync(auditFile, "utf8").trim().split("\n").map((l) => JSON.parse(l));
  assert(lines.length === 2, `fail-closed 审计 2 行（got ${lines.length}）`);
  for (const line of lines) {
    assert(line.decision === "deny" && line.rule === "__policy_load_error__",
      `审计行 deny/__policy_load_error__（got ${line.decision}/${line.rule}）`);
    assert(line.reason.includes("AGT policy load failed (fail-closed)")
      && line.reason.includes("missing-policy.json"),
      `审计行含完整错误原文（策略路径在列）：${line.reason}`);
    assert(!line.reason.includes('"策略不可用"'),
      "审计行不回填中性句（错误原文与中性反馈各走各的面）");
  }
  assert(lines[1].path === "/tmp/x.txt",
    `fail-closed 审计行携带 path（P0 契约延续）：${JSON.stringify(lines[1])}`);

  // 对照组：正常加载路径不受影响——engine deny 的 block 模板不变、allow 放行。
  process.env.AGT_POLICY_PATH = path.join(here, "../../../docker/agt/executor/policy.json");
  const handlersOk = {};
  agtFactory({ on: (ev, fn) => { handlersOk[ev] = fn; } });
  const denyBlock = await handlersOk["tool_call"]({
    toolName: "bash", toolCallId: "ok-deny", input: { command: "rm -rf /workspace" },
  });
  assert(denyBlock && denyBlock.block === true && denyBlock.reason.startsWith("操作被策略拒绝："),
    `正常加载 engine deny block 模板不变（got ${JSON.stringify(denyBlock?.reason)}）`);
  const allowPass = await handlersOk["tool_call"]({
    toolName: "bash", toolCallId: "ok-allow", input: { command: "ls -la" },
  });
  assert(allowPass === undefined, `正常加载 allow 放行不 block（got ${JSON.stringify(allowPass)}）`);

  delete process.env.AGT_POLICY_PATH;
  delete process.env.AGT_AUDIT_PATH;
  rmSync(auditDir, { recursive: true, force: true });
}
// 源码级钉（⑤b 同款，防回归）：fail-closed 分支 block reason 不得插值
// policyLoadError——脱敏后回退（reason 拼 `${policyLoadError}`）即红。
{
  const extSource = readFileSync(path.join(here, "../../../docker/agt/agt-policy.ts"), "utf8");
  const fcBranch = extSource.slice(
    extSource.indexOf("if (!pol) {"),
    extSource.indexOf("const decision = evaluateToolCall"),
  );
  assert(fcBranch.includes('const reason = "策略不可用，操作被拒绝";'),
    "源码级：fail-closed block reason 中性字面量");
  assert(fcBranch.includes('reason: `AGT policy load failed (fail-closed): ${policyLoadError}`')
    && !fcBranch.includes("return { block: true, reason: `"),
    "源码级：错误原文仅在审计条目，block return 无插值");
}
if (failures > 0) {
  console.error(`\n${failures} assertion(s) failed`);
  process.exit(1);
}
console.log("\nALL PASS: AGT 策略求值原型（确定性）");

