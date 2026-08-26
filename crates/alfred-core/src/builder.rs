//! 建图接口（施工清单 §五 S0 迭代"builder API 模式"）。
//!
//! 规划器的 LLM 不直接吐完整 DagSpec JSON，而是输出**建图指令序列**——一组
//! 原子操作（begin/add_node/add_edge/set_routes/commit），alfred 拿到指令后
//! 逐条驱动 `GraphBuilder` 草稿状态机执行。每条指令粒度小、语义单一，错了
//! 知道是哪一步错的；schema 变更只改 builder 不改 prompt。
//!
//! 结构检查（施工清单 §3.2 环节 2"节点格式对不对、图有没有环、契约字段
//! 全不全"）在此落地：begin 只能一次且必须最先、节点 id 唯一、边两端存在、
//! 无环、契约字段非空、commit 后才可 build。

use serde::{Deserialize, Serialize};

use crate::contract::{Contract, SandboxProfile};
use crate::dagspec::{DagSpec, PlanNode};

/// 建图指令（LLM 输出或离线注入的原子操作）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BuildInstruction {
    /// 开始建图，声明需求 id。
    Begin {
        request_id: String,
    },
    /// 添加一个任务节点（契约 + 沙箱档案）。
    AddNode {
        id: String,
        summary: String,
        contract: Contract,
        #[serde(default)]
        sandbox: SandboxProfile,
    },
    /// 添加一条依赖边（from 依赖 to 之前的节点——语义：from 在 to 之后执行）。
    AddEdge {
        from: String,
        to: String,
    },
    /// 声明起始节点（入度为 0 的节点集合；骨架单节点可缺省）。
    SetRoutes {
        start: Vec<String>,
    },
    /// 结束建图，冻结草稿。
    Commit,
}

/// 建图草稿状态机。
///
/// 状态：未开始 → 已 begin → 收集中 → 已 commit。非法指令显式报错（返回
/// `Err(String)` 说明哪一步错、为什么错），不静默忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphBuilder {
    request_id: Option<String>,
    nodes: Vec<PlanNode>,
    edges: Vec<(String, String)>,
    start: Vec<String>,
    committed: bool,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已收到 begin。
    pub fn begun(&self) -> bool {
        self.request_id.is_some()
    }

    /// 是否已 commit。
    pub fn committed(&self) -> bool {
        self.committed
    }

    /// 当前草稿节点数。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// 逐条应用指令；非法指令返回 Err（说明哪一步错）。
    pub fn apply(&mut self, inst: BuildInstruction) -> Result<(), String> {
        match inst {
            BuildInstruction::Begin { request_id } => self.begin(request_id),
            BuildInstruction::AddNode {
                id,
                summary,
                contract,
                sandbox,
            } => self.add_node(id, summary, contract, sandbox),
            BuildInstruction::AddEdge { from, to } => self.add_edge(from, to),
            BuildInstruction::SetRoutes { start } => self.set_routes(start),
            BuildInstruction::Commit => self.commit(),
        }
    }

    fn begin(&mut self, request_id: String) -> Result<(), String> {
        if self.begun() {
            return Err("builder: begin already called (only once)".into());
        }
        if !self.nodes.is_empty() {
            return Err("builder: begin must be the first instruction".into());
        }
        if request_id.trim().is_empty() {
            return Err("builder: begin requires non-empty request_id".into());
        }
        self.request_id = Some(request_id);
        Ok(())
    }

    fn add_node(
        &mut self,
        id: String,
        summary: String,
        contract: Contract,
        sandbox: SandboxProfile,
    ) -> Result<(), String> {
        if !self.begun() {
            return Err("builder: add_node before begin".into());
        }
        if self.committed {
            return Err("builder: add_node after commit".into());
        }
        if id.trim().is_empty() {
            return Err("builder: add_node requires non-empty id".into());
        }
        if self.nodes.iter().any(|n| n.id == id) {
            return Err(format!("builder: duplicate node id '{id}'"));
        }
        if contract.prompt.trim().is_empty() {
            return Err(format!("builder: add_node '{id}' contract.prompt is empty"));
        }
        if contract.acceptance_criteria.trim().is_empty() {
            return Err(format!(
                "builder: add_node '{id}' contract.acceptance_criteria is empty"
            ));
        }
        self.nodes.push(PlanNode {
            id,
            summary,
            contract,
            sandbox,
        });
        Ok(())
    }

    fn add_edge(&mut self, from: String, to: String) -> Result<(), String> {
        if !self.begun() {
            return Err("builder: add_edge before begin".into());
        }
        if self.committed {
            return Err("builder: add_edge after commit".into());
        }
        if from == to {
            return Err(format!("builder: self-loop edge '{from} -> {to}'"));
        }
        if !self.nodes.iter().any(|n| n.id == from) {
            return Err(format!("builder: add_edge from unknown node '{from}'"));
        }
        if !self.nodes.iter().any(|n| n.id == to) {
            return Err(format!("builder: add_edge to unknown node '{to}'"));
        }
        if self.edges.iter().any(|(f, t)| f == &from && t == &to) {
            return Err(format!("builder: duplicate edge '{from} -> {to}'"));
        }
        // 无环检查：加入 (from -> to) 后不得成环。
        let mut test = self.edges.clone();
        test.push((from.clone(), to.clone()));
        if has_cycle(&test) {
            return Err(format!("builder: edge '{from} -> {to}' would create a cycle"));
        }
        self.edges.push((from, to));
        Ok(())
    }

    fn set_routes(&mut self, start: Vec<String>) -> Result<(), String> {
        if !self.begun() {
            return Err("builder: set_routes before begin".into());
        }
        if self.committed {
            return Err("builder: set_routes after commit".into());
        }
        for s in &start {
            if !self.nodes.iter().any(|n| n.id == *s) {
                return Err(format!("builder: set_routes references unknown node '{s}'"));
            }
        }
        self.start = start;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), String> {
        if !self.begun() {
            return Err("builder: commit before begin".into());
        }
        if self.committed {
            return Err("builder: commit already called".into());
        }
        if self.nodes.is_empty() {
            return Err("builder: commit requires at least one node".into());
        }
        self.committed = true;
        Ok(())
    }

    /// 冻结草稿 → DagSpec（节点按依赖拓扑序排列）。
    pub fn build(self) -> Result<DagSpec, String> {
        if !self.committed {
            return Err("builder: build before commit".into());
        }
        let request_id = self
            .request_id
            .clone()
            .ok_or_else(|| "builder: build without request_id".to_string())?;
        let ordered = topological_order(&self.nodes, &self.edges)?;
        Ok(DagSpec {
            request_id,
            nodes: ordered,
        })
    }
}

/// 拓扑排序（Kahn）：有环返回 Err；无环返回依赖序（from 在 to 之前）。
fn topological_order(
    nodes: &[PlanNode],
    edges: &[(String, String)],
) -> Result<Vec<PlanNode>, String> {
    if edges.is_empty() {
        return Ok(nodes.to_vec());
    }
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut indegree: std::collections::HashMap<&str, usize> =
        ids.iter().map(|id| (*id, 0usize)).collect();
    let mut adj: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (f, t) in edges {
        *indegree.get_mut(t.as_str()).unwrap() += 1;
        adj.entry(f.as_str()).or_default().push(t.as_str());
    }
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    queue.sort_unstable();
    let mut order: Vec<&str> = Vec::new();
    while let Some(id) = queue.pop() {
        order.push(id);
        if let Some(nexts) = adj.get(id) {
            for n in nexts {
                let d = indegree.get_mut(n).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push(n);
                }
            }
        }
    }
    if order.len() != nodes.len() {
        return Err("builder: cycle detected in DAG".into());
    }
    let by_id: std::collections::HashMap<&str, &PlanNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    Ok(order
        .into_iter()
        .filter_map(|id| by_id.get(id).map(|n| (*n).clone()))
        .collect())
}

/// 有向图是否有环（DFS 三色标记）。
fn has_cycle(edges: &[(String, String)]) -> bool {
    let mut adj: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (f, t) in edges {
        adj.entry(f.as_str()).or_default().push(t.as_str());
    }
    fn visit<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        state: &mut std::collections::HashMap<&'a str, u8>,
    ) -> bool {
        match state.get(node) {
            Some(1) => return true, // 正在访问 → 环
            Some(2) => return false,
            _ => {}
        }
        state.insert(node, 1);
        if let Some(nexts) = adj.get(node) {
            for n in nexts {
                if visit(n, adj, state) {
                    return true;
                }
            }
        }
        state.insert(node, 2);
        false
    }
    let mut state = std::collections::HashMap::new();
    let keys: Vec<&str> = adj.keys().copied().collect();
    for k in keys {
        if visit(k, &adj, &mut state) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(prompt: &str, ac: &str) -> Contract {
        Contract {
            prompt: prompt.into(),
            acceptance_criteria: ac.into(),
            reviewer_models: vec![],
        }
    }

    #[test]
    fn single_node_build() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "req-1".into(),
        })
        .unwrap();
        b.apply(BuildInstruction::AddNode {
            id: "task-1".into(),
            summary: "create hello.txt".into(),
            contract: contract("create hello.txt", "hello.txt exists"),
            sandbox: SandboxProfile::default(),
        })
        .unwrap();
        b.apply(BuildInstruction::Commit).unwrap();
        let dag = b.build().unwrap();
        assert_eq!(dag.request_id, "req-1");
        assert_eq!(dag.nodes.len(), 1);
        assert_eq!(dag.nodes[0].id, "task-1");
    }

    #[test]
    fn begin_must_be_first_and_once() {
        let mut b = GraphBuilder::new();
        assert!(b
            .apply(BuildInstruction::AddNode {
                id: "t".into(),
                summary: "s".into(),
                contract: contract("p", "a"),
                sandbox: SandboxProfile::default(),
            })
            .is_err());
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        assert!(b
            .apply(BuildInstruction::Begin {
                request_id: "r2".into(),
            })
            .is_err());
    }

    #[test]
    fn duplicate_node_rejected() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        let node = BuildInstruction::AddNode {
            id: "t".into(),
            summary: "s".into(),
            contract: contract("p", "a"),
            sandbox: SandboxProfile::default(),
        };
        b.apply(node.clone()).unwrap();
        assert!(b.apply(node).is_err());
    }

    #[test]
    fn empty_contract_rejected() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        assert!(b
            .apply(BuildInstruction::AddNode {
                id: "t".into(),
                summary: "s".into(),
                contract: contract("", "a"),
                sandbox: SandboxProfile::default(),
            })
            .is_err());
    }

    #[test]
    fn cycle_edge_rejected() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        for id in ["a", "b", "c"] {
            b.apply(BuildInstruction::AddNode {
                id: id.into(),
                summary: "s".into(),
                contract: contract("p", "a"),
                sandbox: SandboxProfile::default(),
            })
            .unwrap();
        }
        b.apply(BuildInstruction::AddEdge {
            from: "a".into(),
            to: "b".into(),
        })
        .unwrap();
        b.apply(BuildInstruction::AddEdge {
            from: "b".into(),
            to: "c".into(),
        })
        .unwrap();
        // c -> a 成环
        assert!(b
            .apply(BuildInstruction::AddEdge {
                from: "c".into(),
                to: "a".into(),
            })
            .is_err());
    }

    #[test]
    fn topological_order_sorts_by_edges() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        // 故意逆序插入：b 依赖 a，a 依赖 base
        for id in ["c", "b", "a", "base"] {
            b.apply(BuildInstruction::AddNode {
                id: id.into(),
                summary: "s".into(),
                contract: contract("p", "a"),
                sandbox: SandboxProfile::default(),
            })
            .unwrap();
        }
        b.apply(BuildInstruction::AddEdge {
            from: "base".into(),
            to: "a".into(),
        })
        .unwrap();
        b.apply(BuildInstruction::AddEdge {
            from: "a".into(),
            to: "b".into(),
        })
        .unwrap();
        b.apply(BuildInstruction::AddEdge {
            from: "b".into(),
            to: "c".into(),
        })
        .unwrap();
        b.apply(BuildInstruction::Commit).unwrap();
        let dag = b.build().unwrap();
        let order: Vec<&str> = dag.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, vec!["base", "a", "b", "c"]);
    }

    #[test]
    fn commit_requires_node() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        assert!(b.apply(BuildInstruction::Commit).is_err());
    }

    #[test]
    fn build_before_commit_rejected() {
        let mut b = GraphBuilder::new();
        b.apply(BuildInstruction::Begin {
            request_id: "r".into(),
        })
        .unwrap();
        assert!(b.build().is_err());
    }
}
