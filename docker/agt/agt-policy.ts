// alfred × AGT 原型：pi 工具权限拦截扩展（P10/S2），单文件自包含。
//
// 接入点分析（详见 .plans/AGT评估.md）：pi 的工具调用完全发生在沙箱容器内，
// 宿主侧 Inspect 只经 sandbox_agent_bridge 看到模型代发请求，看不到工具调用
// 参数——因此唯一能拦到 pi 工具调用的层是 pi 自身，即本扩展
// （`pi.on("tool_call")`，docs/extensions.md：tool_call 可 block、event.input 可改）。
//
// AGT 不提供 pi 适配器（本地仓库只有 Claude Code/OpenCode/Copilot CLI 等），
// 本扩展以 TS 自包含镜像 AGT 策略语义在 tool_call 处做策略拦截：命中 deny
// 规则 → `{ block: true, reason }`；每次决策落审计 JSONL（AGT audit trail）。
// 单文件（无相对 import）——沙箱 pi 0.84.3 的扩展加载器不解析相对 .ts 依赖。
//
// 语义对齐微软 Agent Governance Toolkit（AGT）本地仓库：
//   - agent-governance-python/agent-mesh/src/agentmesh/governance/policy.py
//     PolicyRule：condition（简单表达式语法）+ action + priority；condition
//     求值失败按"命中"处理（fail-closed，V27）。
//   - agent-governance-claude-code/lib/policy.mjs：blockedToolCalls 的
//     commandPatterns（正则）按工具拦截。
//   - agent-governance-claude-code/config/default-policy.json：denyOnPolicyError。
// 规则文法保持 AGT 子集（and/or/==/!=/in/比较/布尔），策略文件可移植回 AGT
// Python 引擎；command_patterns 是 AGT Claude Code 钩子的对应物。
//
// 加载：`pi -e /tmp/.agt/agt-policy.ts`（沙箱容器内，策略/审计经 env 注入）。
// 策略：`AGT_POLICY_PATH`（容器内路径由 driver 注入，如 /tmp/.agt/policy.json）。
// 审计：`AGT_AUDIT_PATH`（审计子目录单独 rw 挂载，JSONL 落宿主——AGT audit
//   trail；agent 可写审计但不可改策略）。
// 属主拍板（AGT 默认启用）：三容器默认挂本扩展（executor 边界策略 / planner·
// reviewer deny-write 策略，见同目录 <role>/policy.json）；ALFRED_AGT_DISABLE=1
// 显式关闭，ALFRED_AGT_DIR 显式目录覆盖。
//
// 导出：default = pi 扩展工厂；evaluateToolCall/parsePolicy/... = 纯求值核心
// （供 node tests/e2e/agt/agt-policy.test.mjs 确定性断言，无需 pi 运行时）。

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname } from "node:path";

// ---------------------------------------------------------------------------
// 类型
// ---------------------------------------------------------------------------

/** AGT 策略文档（子集：apiVersion/name/default_action/rules + deny_on_policy_error）。 */
export interface Policy {
  apiVersion?: string;
  name?: string;
  default_action: "allow" | "deny";
  deny_on_policy_error?: boolean;
  rules: PolicyRule[];
}

export interface PolicyRule {
  name: string;
  description?: string;
  /** AGT 文法条件（and/or/==/!=/in/比较/布尔，点号嵌套）。缺省恒真。 */
  condition?: string;
  /** AGT Claude Code 式命令正则黑名单（仅对 bash 类工具的 command 生效）。 */
  command_patterns?: Array<{ source: string; flags?: string }>;
  /** 路径白名单（绝对路径前缀；宿主落盘时占位符已替换）。与 command_patterns
   *  同语义：两者都有时需同时命中（AND）；规则含 path_prefixes 时要求工具调用
   *  提取出目标路径且以任一前缀开头。 */
  path_prefixes?: string[];
  action: "allow" | "deny";
  priority?: number;
  enabled?: boolean;
}

export interface PolicyDecision {
  decision: "allow" | "deny";
  rule: string | null;
  reason: string;
  /** 规则 action；null = 无规则命中，回退 default_action。 */
  matched_rule_action: "allow" | "deny" | null;
}

/** 审计记录（AGT audit trail：每次决策一行 JSONL）。 */
export interface AuditEntry {
  ts: string;
  tool_name: string;
  tool_call_id?: string;
  command?: string;
  path?: string;
  decision: "allow" | "deny";
  rule: string | null;
  reason: string;
}

/** 默认工作区根（容器 executor 语义；未设 AGT_WORKSPACE_DIR 时的回退值）。 */
export const DEFAULT_WORKSPACE_DIR = "/workspace";

/** 工作区根（沙箱边界语义）。
 *  宿主 pi 化后由 env `AGT_WORKSPACE_DIR` 注入（planner/reviewer = 治理对象项目根；
 *  executor 容器仍为 /workspace）；env 未设时回退 /workspace（容器 executor 用法不变）。
 *  运行时读取（每次调用）：同一扩展文件可被不同 env 的进程加载。 */
export function workspaceDir(): string {
  const dir = process.env.AGT_WORKSPACE_DIR;
  return dir && dir.trim() !== "" ? dir.trim().replace(/\/+$/, "") : DEFAULT_WORKSPACE_DIR;
}

/** 只读参考卷容器内挂载点（9/3 方案②：宿主材料进路）。
 *
 *  执行容器按契约 `sandbox.volumes` 把宿主参考材料以 ro 挂到 `/workspace` 之外的
 *  独立路径（如 /references）。这些路径物理 ro（docker `:ro`），读取放行无需额外
 *  拦写；但边界判定（pathEscapesWorkspace / commandEscapesWorkspace）按"/workspace
 *  之外 = 逃逸"处理，会把 `cat /references/x.md` 误杀。此处在边界判定上把参考卷
 *  子树视为工作区内（ro 物理只读兜底，写仍由挂载层拒绝）。
 *
 *  注入：driver env `AGT_REF_VOLUMES`（冒号分隔容器内绝对路径列表，如
 *  `/references:/docs`；空/未设 = 无参考卷，语义不变）。运行时读取（每次调用），
 *  同一扩展文件可被不同 env 的进程加载。
 */
export function refVolumeDirs(): string[] {
  const raw = process.env.AGT_REF_VOLUMES;
  if (!raw || raw.trim() === "") return [];
  return raw
    .split(":")
    .map((d) => d.trim().replace(/\/+$/, ""))
    .filter((d) => d.startsWith("/"));
}

/** 路径是否落在某个只读参考卷子树内（边界判定豁免面）。 */
export function inRefVolume(p: string): boolean {
  const abs = normalizePath(p);
  return refVolumeDirs().some(
    (dir) => abs === dir || abs.startsWith(dir + "/"),
  );
}

// ---------------------------------------------------------------------------
// 路径/命令逃逸判定（沙箱边界语义）
// ---------------------------------------------------------------------------

/** 规范化路径：剥引号、解析 .、..，返回绝对路径（相对路径按工作区根解析）。 */
export function normalizePath(p: string, cwd = workspaceDir()): string {
  let s = String(p).trim();
  if ((s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))) {
    s = s.slice(1, -1);
  }
  if (s.startsWith("~")) s = "/root" + s.slice(1); // 沙箱内 HOME=/root
  if (!s.startsWith("/")) s = `${cwd}/${s}`;
  const parts: string[] = [];
  for (const seg of s.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") parts.pop();
    else parts.push(seg);
  }
  return "/" + parts.join("/");
}

/** 路径是否逃逸工作区（绝对路径不在工作区根下，或 .. 上跳出）。
 *  9/3 方案②：只读参考卷子树不视为逃逸（物理 ro，读放行；写由挂载层拒绝）。 */
export function pathEscapesWorkspace(p: string, cwd = workspaceDir()): boolean {
  const ws = workspaceDir();
  const abs = normalizePath(p, cwd);
  if (inRefVolume(abs)) return false;
  return abs !== ws && !abs.startsWith(ws + "/");
}

/** 从命令文本提取的绝对路径 token 中，是否有任何逃逸工作区。
 *  9/3 方案②：参考卷子树 token 豁免（cat /references/... 读宿主参考材料放行）。 */
export function commandEscapesWorkspace(command: string): boolean {
  const tokens = String(command).match(/"[^"]*"|'[^']*'|\S+/g) ?? [];
  for (const tok of tokens) {
    const t = tok.replace(/^['"]|['"]$/g, "");
    if (t.startsWith("/")) {
      if (!pathEscapesWorkspace(t)) continue;
      if (inRefVolume(t)) continue;
      // 排除常见无害的只读系统工具参数（白名单）
      if (/^\/(usr|bin|sbin|opt)\//.test(t)) continue;
      return true;
    }
    if (t.startsWith("..") || t.includes("/..")) return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// AGT 条件表达式求值（镜像 PolicyRule._eval_expression 文法，fail-closed）
// ---------------------------------------------------------------------------

const MAX_EXPRESSION_DEPTH = 20;

function getNested(obj: Record<string, unknown>, path: string): unknown {
  let cur: unknown = obj;
  for (const part of path.split(".")) {
    if (cur !== null && typeof cur === "object") {
      cur = (cur as Record<string, unknown>)[part];
    } else {
      return undefined;
    }
  }
  return cur;
}

/** 剥掉平衡的外层括号（`(A) and B` 的 `(A)` → `A`；整体 `(A and B)` → `A and B`）。 */
function stripOuterParens(expr: string): string {
  let t = expr.trim();
  while (t.startsWith("(") && t.endsWith(")")) {
    let depth = 0;
    let outerPair = true;
    for (let i = 0; i < t.length; i++) {
      if (t[i] === "(") depth++;
      else if (t[i] === ")") {
        depth--;
        if (depth === 0 && i !== t.length - 1) {
          outerPair = false;
          break;
        }
      }
    }
    if (!outerPair || depth !== 0) break;
    t = t.slice(1, -1).trim();
  }
  return t;
}

/** 顶层（paren depth 0）的 op 拆分；无顶层 op → null。支持 `(A or B) and C` 分组。 */
function splitTopLevel(expr: string, op: string): string[] | null {
  const parts: string[] = [];
  let depth = 0;
  let start = 0;
  let found = false;
  for (let i = 0; i < expr.length; i++) {
    const c = expr[i];
    if (c === "(") depth++;
    else if (c === ")") depth--;
    if (depth === 0 && expr.startsWith(op, i)) {
      parts.push(expr.slice(start, i));
      start = i + op.length;
      i += op.length - 1;
      found = true;
    }
  }
  if (!found) return null;
  parts.push(expr.slice(start));
  return parts;
}

export function evalCondition(expr: string, ctx: Record<string, unknown>, depth = 0): boolean {
  if (depth > MAX_EXPRESSION_DEPTH) return false;
  if (expr.length > 2000) return false;

  const e = stripOuterParens(expr);

  // 顶层（不在括号内）的 or / and 才拆分——支持 (A or B) and C 分组语义。
  const orParts = splitTopLevel(e, " or ");
  if (orParts) return orParts.some((p) => evalCondition(p.trim(), ctx, depth + 1));
  const andParts = splitTopLevel(e, " and ");
  if (andParts) return andParts.every((p) => evalCondition(p.trim(), ctx, depth + 1));

  // 字符串前缀方法（planner-policy 的 target_path.startswith('/workspace')）。
  // 非字符串（含缺失）返回 false 而非抛错——无 path 的 write 不因此 fail-closed deny。
  let m = e.match(/^([\w.]+)\.startswith\(\s*['"]([^'"]+)['"]\s*\)$/);
  if (m) {
    const actual = getNested(ctx, m[1]);
    return typeof actual === "string" && actual.startsWith(m[2]);
  }

  m = e.match(/^([\w.]+)\s*==\s*['"]([^'"]+)['"]$/);
  if (m) return getNested(ctx, m[1]) === m[2];
  m = e.match(/^([\w.]+)\s*!=\s*['"]([^'"]+)['"]$/);
  if (m) return getNested(ctx, m[1]) !== m[2];
  m = e.match(/^([\w.]+)\s+in\s+\[([^\]]*)\]$/);
  if (m) {
    const actual = getNested(ctx, m[1]);
    const items = m[2].split(",").map((s) => s.trim().replace(/^['"]|['"]$/g, ""));
    return items.includes(String(actual));
  }
  m = e.match(/^([\w.]+)\s*(>=|<=|>|<)\s*(\d+(?:\.\d+)?)$/);
  if (m) {
    const actual = Number(getNested(ctx, m[1]));
    const target = Number(m[3]);
    if (Number.isNaN(actual)) return false;
    switch (m[2]) {
      case ">": return actual > target;
      case "<": return actual < target;
      case ">=": return actual >= target;
      case "<=": return actual <= target;
    }
  }
  m = e.match(/^[\w.]+$/);
  if (m) return Boolean(getNested(ctx, m[0]));
  return false;
}

/** 规则条件求值，失败按"命中"处理（fail-closed，对齐 AGT V27）。 */
function ruleMatches(rule: PolicyRule, ctx: Record<string, unknown>): boolean {
  if (rule.enabled === false) return false;
  let cond = true;
  if (rule.condition !== undefined) {
    try {
      cond = evalCondition(rule.condition, ctx);
    } catch {
      cond = true; // fail-closed
    }
  }
  if (!cond) return false;
  if (rule.command_patterns && rule.command_patterns.length > 0) {
    const cmd = String(ctx["command"] ?? "");
    if (!cmd) return false;
    const anyMatch = rule.command_patterns.some((pat) => {
      try {
        return new RegExp(pat.source, pat.flags ?? "i").test(cmd);
      } catch {
        return true; // fail-closed：非法正则按命中（保守拒绝）
      }
    });
    if (!anyMatch) return false;
  }
  if (rule.path_prefixes && rule.path_prefixes.length > 0) {
    const target = String(ctx["target_path"] ?? ctx["path"] ?? "");
    if (!target) return false;
    const normalized = normalizePath(target);
    const anyPrefix = rule.path_prefixes.some((prefix) => {
      const p = normalizePath(prefix);
      return normalized === p || normalized.startsWith(p + "/");
    });
    if (!anyPrefix) return false;
  }
  return true;
}

function extractCommand(toolName: string, args: Record<string, unknown>): string | undefined {
  const t = toolName.toLowerCase();
  if (t === "bash" || t === "shell" || t === "sh" || t === "terminal" || t === "execute") {
    const c = args["command"] ?? args["cmd"] ?? args["code"];
    return c !== undefined ? String(c) : undefined;
  }
  return undefined;
}

function extractPath(toolName: string, args: Record<string, unknown>): string | undefined {
  const t = toolName.toLowerCase();
  if (t === "bash" || t === "shell" || t === "sh" || t === "terminal") return undefined;
  for (const key of ["path", "file_path", "filePath", "target", "destination", "src", "source"]) {
    const v = args[key];
    if (v !== undefined && typeof v === "string") return v;
  }
  return undefined;
}

/** 求值一次工具调用。规则按 priority 降序，首个命中规则决定 action；否则 default_action。 */
export function evaluateToolCall(
  policy: Policy,
  event: { tool_name: string; args: Record<string, unknown> },
): PolicyDecision {
  const ctx: Record<string, unknown> = {
    tool_name: event.tool_name,
    args: event.args,
  };
  const command = extractCommand(event.tool_name, event.args);
  const path = extractPath(event.tool_name, event.args);
  if (command !== undefined) ctx["command"] = command;
  if (path !== undefined) {
    ctx["path"] = path;
    ctx["target_path"] = path; // 策略条件用 target_path（与 AGT 语义一致）
  }
  ctx["path_escapes_workspace"] = path !== undefined && pathEscapesWorkspace(path);
  ctx["command_escapes_workspace"] = command !== undefined && commandEscapesWorkspace(command);

  const ordered = [...policy.rules].sort((a, b) => (b.priority ?? 0) - (a.priority ?? 0));
  for (const rule of ordered) {
    if (!ruleMatches(rule, ctx)) continue;
    const reason = rule.description ?? "操作被策略拒绝";
    if (rule.action === "allow") {
      return { decision: "allow", rule: rule.name, reason, matched_rule_action: "allow" };
    }
    return { decision: "deny", rule: rule.name, reason, matched_rule_action: "deny" };
  }
  return {
    decision: policy.default_action,
    rule: null,
    reason: `default_action=${policy.default_action}`,
    matched_rule_action: null,
  };
}

// ---------------------------------------------------------------------------
// 策略加载与审计
// ---------------------------------------------------------------------------

export function parsePolicy(text: string): Policy {
  const data = JSON.parse(text);
  if (!data || typeof data !== "object") throw new Error("policy must be a JSON object");
  if (!Array.isArray(data.rules)) throw new Error("policy.rules must be an array");
  if (data.default_action !== "allow" && data.default_action !== "deny") {
    throw new Error("policy.default_action must be 'allow'|'deny'");
  }
  return data as Policy;
}

export function loadPolicyFromFile(path: string): Policy {
  return parsePolicy(readFileSync(path, "utf8"));
}

/** 追加一条审计决策（AGT audit trail 语义：每次决策落盘）。
 *  审计失败不改变强制决策（denyOnPolicyError 只管策略加载）；目录缺失自动创建，
 *  写入异常吞掉并向 stderr 报告，避免"审计故障阻断合法工具调用"。 */
export function appendAudit(path: string, entry: AuditEntry): void {
  try {
    mkdirSync(dirname(path), { recursive: true });
    appendFileSync(path, JSON.stringify(entry) + "\n", "utf8");
  } catch (e) {
    process.stderr.write(
      `[agt-policy] audit append failed (${path}): ${e instanceof Error ? e.message : String(e)}\n`,
    );
  }
}

// ---------------------------------------------------------------------------
// pi 扩展工厂（tool_call 拦截）
// ---------------------------------------------------------------------------

const DEFAULT_POLICY_PATH = "/workspace/.agt/policy.json";
const DEFAULT_AUDIT_PATH = "/workspace/.agt/audit/audit.jsonl";

export default function (pi: ExtensionAPI) {
  let policy: Policy | null = null;
  let policyLoadError: string | null = null;

  function ensurePolicy(): Policy | null {
    if (policy) return policy;
    const policyPath = process.env.AGT_POLICY_PATH || DEFAULT_POLICY_PATH;
    try {
      policy = loadPolicyFromFile(policyPath);
      policyLoadError = null;
    } catch (e) {
      policyLoadError = e instanceof Error ? e.message : String(e);
      return null;
    }
    return policy;
  }

  pi.on("tool_call", async (event) => {
    const auditPath = process.env.AGT_AUDIT_PATH || DEFAULT_AUDIT_PATH;
    const ts = new Date().toISOString();
    const args = (event.input ?? {}) as Record<string, unknown>;
    const command = args["command"];
    const path = extractPath(event.toolName, args);
    const ctx = { tool_name: event.toolName, args };

    const pol = ensurePolicy();
    if (!pol) {
      // denyOnPolicyError 语义（AGT default-policy.json：denyOnPolicyError: true）
      // A1 脱敏（DebtInventory）：block reason 保持中性——policyLoadError 含策略
      // 文件完整路径 + run 布局，planner 直面即泄露；planner 只见中性拒绝，
      // 完整错误原文只落审计 JSONL（AGT_AUDIT_PATH 宿主侧，agent 不可读）。
      const reason = "策略不可用，操作被拒绝";
      appendAudit(auditPath, {
        ts,
        tool_name: event.toolName,
        tool_call_id: event.toolCallId,
        // 审计带 path（P0 契约修复）：key_file_paths 真实数据源依赖
        // allow read 行的 path 提取（host.rs extract_allow_read_paths）；
        // deny 行同样带 path，但提取侧按 decision==allow 过滤，无泄露。
        path,
        decision: "deny",
        rule: "__policy_load_error__",
        // 错误原文唯一出口（生产者-消费者契约：审计行 = 宿主排障真源）。
        reason: `AGT policy load failed (fail-closed): ${policyLoadError}`,
      });
      return { block: true, reason };
    }

    const decision = evaluateToolCall(pol, ctx);
    appendAudit(auditPath, {
      ts,
      tool_name: event.toolName,
      tool_call_id: event.toolCallId,
      command: typeof command === "string" ? command : undefined,
      path,
      decision: decision.decision,
      rule: decision.rule,
      reason: decision.reason,
    });
    if (decision.decision === "deny") {
      return {
        block: true,
        reason: `操作被策略拒绝：${decision.reason}`,
      };
    }
    return undefined; // 放行
  });
}
