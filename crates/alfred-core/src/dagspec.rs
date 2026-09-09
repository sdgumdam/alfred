//! 计划（DagSpec）：规划器产物，计划审查输入（限界上下文 §6.4 DagSpec）。
//!
//! R2 实现审查侧：计划审查容器用 `OwnerRequest + DagSpec` 判忠实度，
//! 产出 PlanVerdict。DagSpec 是规划器（R2 后续填充）的产物形态，先作为
//! 跨组件共享实体落在 alfred-core——planner / reviewer / cli 都要引用，
//! 单一真源（代码质量红线 1）。

use serde::{Deserialize, Serialize};

use crate::contract::{Contract, SandboxProfile};

/// 计划节点：一个任务节点 = 契约 + 一句话摘要。
///
/// `contract` 与 TaskAssignment.contract 同构；执行侧复用同一实体。
/// R3 增补 `sandbox`（§2.5：每个要跑 agent 的节点在图里除了契约之外，
/// 还要附带一份沙箱档案——契约管"做什么"，档案管"在什么约束下做"）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanNode {
    /// 节点 id（对应 TaskAssignment.task_id）。
    pub id: String,
    /// 节点摘要（该节点做什么，供计划审查快速对齐）。
    pub summary: String,
    /// 行为契约。
    pub contract: Contract,
    /// 沙箱档案（§2.5；缺省 = 联网拒绝、无额外挂卷——execute_run 只支持默认档案）。
    #[serde(default)]
    pub sandbox: SandboxProfile,
    /// 节点级执行时间上限（秒；可选声明）。
    ///
    /// 声明时覆盖编排器缺省（governance `exec_time_limit_secs`，缺省 600）；
    /// 未声明 = None → 编排器缺省（兼容既有契约）。写入方：planner
    /// （add_node 声明；大参考卷任务按规模折算，见 converse 提示词与
    /// `apply_large_volume_budget` 兜底）+ 编排器 timed_out 自适应放大
    /// （governance execution_step 机械重跑分支写回）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_secs: Option<u32>,
}

impl PlanNode {
    pub fn new(id: impl Into<String>, summary: impl Into<String>, contract: Contract) -> Self {
        Self {
            id: id.into(),
            summary: summary.into(),
            contract,
            sandbox: SandboxProfile::default(),
            time_limit_secs: None,
        }
    }

    /// 节点生效执行时间上限（单一真源）：契约声明优先，未声明回退编排器
    /// 缺省（fallback_secs = governance `exec_time_limit_secs`，CLI 缺省 600）。
    pub fn resolved_time_limit_secs(&self, fallback_secs: u32) -> u32 {
        self.time_limit_secs.unwrap_or(fallback_secs)
    }
}


/// 依赖边（M1 多节点）：`from` → `to`——`from` 是 `to` 的前置，
/// `from` 完成后 `to` 才可执行。
///
/// 两端为节点 id 引用，必须存在于 `DagSpec.nodes`（悬空边在
/// `topological_order` 显式拒绝）；方向语义与 `topological_order` 的
/// 依赖序一致（from 在 to 之前）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// 前置节点 id（先执行、被依赖的一方）。
    pub from: String,
    /// 后继节点 id（依赖 from 产物的一方）。
    pub to: String,
}

/// 计划：属主需求 → 任务节点的 DAG 拆解。
///
/// M1 起承载显式依赖边（`edges`）；拓扑序/环检测的单一真源是
/// [`DagSpec::topological_order`]。节点按依赖序排列，孤立节点合法，
/// 审查判"拆解是否忠实于 OwnerRequest"。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DagSpec {
    /// 被拆解的需求 id（对应 OwnerRequest.id）。
    pub request_id: String,
    /// 节点列表（按依赖拓扑序排列）。
    pub nodes: Vec<PlanNode>,
    /// 依赖边（from → to）。空 = 单节点计划/无显式依赖（M1 前的既有形态）。
    ///
    /// 旧契约兼容（红线）：无 `edges` 字段的既有 dagspec.json 反序列化为
    /// 空 vec；空 vec 序列化不落字段——与 `PlanNode::time_limit_secs` 的
    /// None 不落同一范式，旧断言面逐字节不变。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<Edge>,
}

impl DagSpec {
    pub fn new(request_id: impl Into<String>, nodes: Vec<PlanNode>) -> Self {
        Self {
            request_id: request_id.into(),
            nodes,
            edges: Vec::new(),
        }
    }
}

impl DagSpec {
    /// run 级 contract.json 投影（挂载矩阵 §1.1 第 7 行）：计划首节点契约全字段
    /// JSON 文本；无节点 → "{}" 占位（bind mount 源必须存在，E1）。
    ///
    /// 单一真源：planner 回看自己的契约（治理环 planning_step 落盘 run_dir/
    /// contract.json）与 reviewer 契约全字段输入（plan_review_inputs）共用本投影。
    pub fn contract_json(&self) -> serde_json::Result<String> {
        match self.nodes.first() {
            Some(node) => serde_json::to_string_pretty(&node.contract),
            None => Ok("{}".to_string()),
        }
    }

    /// 拓扑排序（Kahn，单一真源）：返回节点 id 的依赖执行序——任一条边的
    /// `from` 必出现在 `to` 之前；同时就绪的节点取声明序最前者（稳定序）。
    ///
    /// 显式 Err（不静默容忍坏图）：
    /// - 重复节点 id：同 id 节点多于一个（id 键控的执行记账无法区分）；
    /// - 悬空边：`from`/`to` 引用了 `nodes` 中不存在的节点 id；
    /// - 重复边：同一 `(from, to)` 出现多次；
    /// - 环（含自环）：报出环路径，如 `a -> b -> a`。
    ///
    /// 孤立节点（无入边无出边）合法，照常出现在序里。
    pub fn topological_order(&self) -> Result<Vec<String>, String> {
        let id_index: std::collections::HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.id.as_str(), i))
            .collect();
        // 重复节点 id：id 键控的执行推进/完成记账（M3 `next_pending_node` /
        // GovernanceRun.completed_nodes）无法区分同 id 节点——结构坏图显式
        // 拒绝（builder add_node 已拒，此处兜底离线注入/旧 run 路径）。
        if id_index.len() != self.nodes.len() {
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let dup = self
                .nodes
                .iter()
                .find(|n| !seen.insert(n.id.as_str()))
                .expect("id_index shorter than nodes implies a duplicate id");
            return Err(format!("dagspec: duplicate node id '{}'", dup.id));
        }
        for edge in &self.edges {
            for endpoint in [&edge.from, &edge.to] {
                if !id_index.contains_key(endpoint.as_str()) {
                    return Err(format!(
                        "dagspec: edge '{} -> {}' references unknown node '{}'",
                        edge.from, edge.to, endpoint
                    ));
                }
            }
        }
        let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
        for edge in &self.edges {
            if !seen.insert((edge.from.as_str(), edge.to.as_str())) {
                return Err(format!(
                    "dagspec: duplicate edge '{} -> {}'",
                    edge.from, edge.to
                ));
            }
        }
        // Kahn：入度归零即就绪；就绪集按声明序取最小（稳定拓扑序）。
        let n = self.nodes.len();
        let mut indegree = vec![0usize; n];
        let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &self.edges {
            let from = id_index[edge.from.as_str()];
            let to = id_index[edge.to.as_str()];
            indegree[to] += 1;
            successors[from].push(to);
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..n)
            .filter(|&i| indegree[i] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order: Vec<usize> = Vec::with_capacity(n);
        while let Some(std::cmp::Reverse(i)) = ready.pop() {
            order.push(i);
            for &to in &successors[i] {
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.push(std::cmp::Reverse(to));
                }
            }
        }
        if order.len() == n {
            return Ok(order
                .into_iter()
                .map(|i| self.nodes[i].id.clone())
                .collect());
        }
        // 有环：剩余节点沿“剩余前驱”回溯必回到自身（每个剩余节点必有
        // 剩余前驱，否则早被就绪弹出），回溯环反转即边方向的真实路径。
        let mut emitted = vec![false; n];
        for &i in &order {
            emitted[i] = true;
        }
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
        for edge in &self.edges {
            predecessors[id_index[edge.to.as_str()]].push(id_index[edge.from.as_str()]);
        }
        let start = (0..n)
            .find(|&i| !emitted[i])
            .expect("stalled Kahn implies unemitted nodes");
        let mut path: Vec<usize> = Vec::new();
        let mut position: Vec<Option<usize>> = vec![None; n];
        let mut current = start;
        let cycle_head = loop {
            if let Some(head) = position[current] {
                break head;
            }
            position[current] = Some(path.len());
            path.push(current);
            current = *predecessors[current]
                .iter()
                .find(|&&pred| !emitted[pred])
                .expect("remaining node has a remaining predecessor");
        };
        let cycle: Vec<&str> = path[cycle_head..]
            .iter()
            .rev()
            .map(|&i| self.nodes[i].id.as_str())
            .collect();
        Err(format!(
            "dagspec: cycle detected: {} -> {}",
            cycle.join(" -> "),
            cycle[0]
        ))
    }

    /// M3：依赖执行序中首个未完成节点（执行推进单一真源）。
    ///
    /// `completed` = 已完成节点 id 集；`Ok(None)` = 全图已完成。拓扑序保证
    /// 所选节点的前置全部已完成——任一条边的 `from` 先于 `to`，若有前置
    /// 未完成，它排得更早、才是"首个未完成"——调用方无需再查前置满足性。
    pub fn next_pending_node(&self, completed: &[String]) -> Result<Option<&PlanNode>, String> {
        let order = self.topological_order()?;
        let by_id: std::collections::HashMap<&str, &PlanNode> =
            self.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        Ok(order
            .iter()
            .map(|id| by_id[id.as_str()])
            .find(|n| !completed.contains(&n.id)))
    }
}
