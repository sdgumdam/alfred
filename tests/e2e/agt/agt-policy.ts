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
// 加载：`pi -e <path>/agt-policy.ts`（沙箱容器内，配合 /workspace/.agt/ 挂载）。
// 策略：`AGT_POLICY_PATH`（缺省 /workspace/.agt/policy.json）。
// 审计：`AGT_AUDIT_PATH`（缺省 /workspace/.agt/audit.jsonl，宿主可见）。
// 默认不启用；启用与否属主拍板（见 .plans/AGT评估.md 拍板项）。
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

export const WORKSPACE_DIR = "/workspace";

// ---------------------------------------------------------------------------
// 路径/命令逃逸判定（沙箱边界语义）
// ---------------------------------------------------------------------------

/** 规范化路径：剥引号、解析 .、..，返回绝对路径（相对路径按 /workspace 解析）。 */
export function normalizePath(p: string, cwd = WORKSPACE_DIR): string {
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

/** 路径是否逃逸工作区（绝对路径不在 /workspace 下，或 .. 上跳出）。 */
export function pathEscapesWorkspace(p: string, cwd = WORKSPACE_DIR): boolean {
  const abs = normalizePath(p, cwd);
  return abs !== WORKSPACE_DIR && !abs.startsWith(WORKSPACE_DIR + "/");
}

/** 从命令文本提取的绝对路径 token 中，是否有任何逃逸工作区。 */
export function commandEscapesWorkspace(command: string): boolean {
  const tokens = String(command).match(/"[^"]*"|'[^']*'|\S+/g) ?? [];
  for (const tok of tokens) {
    const t = tok.replace(/^['"]|['"]$/g, "");
    if (t.startsWith("/")) {
      if (!pathEscapesWorkspace(t)) continue;
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

export function evalCondition(expr: string, ctx: Record<string, unknown>, depth = 0): boolean {
  if (depth > MAX_EXPRESSION_DEPTH) return false;
  if (expr.length > 2000) return false;

  const orIdx = expr.indexOf(" or ");
  if (orIdx !== -1) {
    return expr.split(" or ").some((p) => evalCondition(p.trim(), ctx, depth + 1));
  }
  const andIdx = expr.indexOf(" and ");
  if (andIdx !== -1) {
    return expr.split(" and ").every((p) => evalCondition(p.trim(), ctx, depth + 1));
  }

  let m = expr.match(/^([\w.]+)\s*==\s*['"]([^'"]+)['"]$/);
  if (m) return getNested(ctx, m[1]) === m[2];
  m = expr.match(/^([\w.]+)\s*!=\s*['"]([^'"]+)['"]$/);
  if (m) return getNested(ctx, m[1]) !== m[2];
  m = expr.match(/^([\w.]+)\s+in\s+\[([^\]]*)\]$/);
  if (m) {
    const actual = getNested(ctx, m[1]);
    const items = m[2].split(",").map((s) => s.trim().replace(/^['"]|['"]$/g, ""));
    return items.includes(String(actual));
  }
  m = expr.match(/^([\w.]+)\s*(>=|<=|>|<)\s*(\d+(?:\.\d+)?)$/);
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
  m = expr.match(/^[\w.]+$/);
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
  if (path !== undefined) ctx["path"] = path;
  ctx["path_escapes_workspace"] = path !== undefined && pathEscapesWorkspace(path);
  ctx["command_escapes_workspace"] = command !== undefined && commandEscapesWorkspace(command);

  const ordered = [...policy.rules].sort((a, b) => (b.priority ?? 0) - (a.priority ?? 0));
  for (const rule of ordered) {
    if (!ruleMatches(rule, ctx)) continue;
    const reason = rule.description ?? rule.name;
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
const DEFAULT_AUDIT_PATH = "/workspace/.agt/audit.jsonl";

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
    const ctx = {
      tool_name: event.toolName,
      args: (event.input ?? {}) as Record<string, unknown>,
    };

    const pol = ensurePolicy();
    if (!pol) {
      // denyOnPolicyError 语义（AGT default-policy.json：denyOnPolicyError: true）
      const reason = `AGT policy load failed (fail-closed): ${policyLoadError}`;
      appendAudit(auditPath, {
        ts,
        tool_name: event.toolName,
        tool_call_id: event.toolCallId,
        decision: "deny",
        rule: "__policy_load_error__",
        reason,
      });
      return { block: true, reason };
    }

    const decision = evaluateToolCall(pol, ctx);
    const command = ctx.args["command"];
    appendAudit(auditPath, {
      ts,
      tool_name: event.toolName,
      tool_call_id: event.toolCallId,
      command: typeof command === "string" ? command : undefined,
      decision: decision.decision,
      rule: decision.rule,
      reason: decision.reason,
    });
    if (decision.decision === "deny") {
      return {
        block: true,
        reason: `AGT policy deny [${decision.rule}]: ${decision.reason}`,
      };
    }
    return undefined; // 放行
  });
}
