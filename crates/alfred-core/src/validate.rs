use crate::entities::{DagSpec, NodeType, END_NODE};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationIssue {
    EmptyGraph,
    UnknownEntrypoint { entrypoint: String },
    DuplicateNodeId { node_id: String },
    EmptyReviewerModels { node_id: String },
    RouterWithoutRoutes { node_id: String },
    EmptyRoutes { node_id: String },
    NonRouterWithRoutes { node_id: String },
    RouteTargetUnknown { node_id: String, choice: String, target: String },
    EdgeUnknownNode { edge_id: String, endpoint: String },
    EdgeSelfLoop { edge_id: String, node_id: String },
    DuplicateEdgeId { edge_id: String },
    UnreachableNode { node_id: String },
    CyclicGraph { node_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub issues: Vec<ValidationIssue>,
    pub raw_json: Option<Value>,
}

impl ValidationError {
    pub fn single(issue: ValidationIssue) -> Self {
        Self { issues: vec![issue], raw_json: None }
    }

    pub fn to_report(&self) -> Report {
        Report { issues: self.issues.iter().map(ValidationIssue::report).collect() }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let report = serde_json::to_string(&self.to_report()).expect("report serialization cannot fail");
        write!(f, "{}", report)
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.report().describe())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub issues: Vec<IssueReport>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct IssueReport {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl IssueReport {
    fn describe(&self) -> String {
        let mut location = Vec::new();
        if let Some(node) = &self.node_id {
            location.push(format!("node `{node}`"));
        }
        if let Some(edge) = &self.edge_id {
            location.push(format!("edge `{edge}`"));
        }
        let where_clause = match location.is_empty() {
            true => "".to_string(),
            false => format!(" at {}", location.join(" / ")),
        };
        let field_hint = self.field.map(|f| format!(" (field `{f}`)")).unwrap_or_default();
        format!("{}{}: {}{}", self.code, where_clause, self.message, field_hint)
    }
}

impl ValidationIssue {
    pub fn report(&self) -> IssueReport {
        match self {
            Self::EmptyGraph => IssueReport {
                code: "EMPTY_GRAPH",
                message: "DagSpec has no nodes".into(),
                node_id: None,
                edge_id: None,
                field: Some("nodes"),
                detail: None,
            },
            Self::UnknownEntrypoint { entrypoint } => IssueReport {
                code: "UNKNOWN_ENTRYPOINT",
                message: format!("entrypoint `{entrypoint}` does not match any node"),
                node_id: Some(entrypoint.clone()),
                edge_id: None,
                field: Some("entrypoint"),
                detail: None,
            },
            Self::DuplicateNodeId { node_id } => IssueReport {
                code: "DUPLICATE_NODE_ID",
                message: format!("node_id `{node_id}` declared more than once"),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("node_id"),
                detail: None,
            },
            Self::EmptyReviewerModels { node_id } => IssueReport {
                code: "CONTRACT_INCOMPLETE",
                message: "reviewer_models must list at least one reviewer model".into(),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("contract.reviewer_models"),
                detail: None,
            },
            Self::RouterWithoutRoutes { node_id } => IssueReport {
                code: "ROUTER_WITHOUT_ROUTES",
                message: "router node must declare routes".into(),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("routes"),
                detail: None,
            },
            Self::EmptyRoutes { node_id } => IssueReport {
                code: "EMPTY_ROUTES",
                message: "routes.choices must contain at least one branch".into(),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("routes.choices"),
                detail: None,
            },
            Self::NonRouterWithRoutes { node_id } => IssueReport {
                code: "NON_ROUTER_WITH_ROUTES",
                message: "only router nodes may declare routes".into(),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("routes"),
                detail: None,
            },
            Self::RouteTargetUnknown { node_id, choice, target } => IssueReport {
                code: "ROUTE_TARGET_UNKNOWN",
                message: format!("choice `{choice}` targets unknown node `{target}`"),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: Some("routes.choices"),
                detail: None,
            },
            Self::EdgeUnknownNode { edge_id, endpoint } => IssueReport {
                code: "EDGE_UNKNOWN_NODE",
                message: format!("edge references unknown node `{endpoint}`"),
                node_id: Some(endpoint.clone()),
                edge_id: Some(edge_id.clone()),
                field: None,
                detail: None,
            },
            Self::EdgeSelfLoop { edge_id, node_id } => IssueReport {
                code: "EDGE_SELF_LOOP",
                message: format!("edge from `{node_id}` to itself"),
                node_id: Some(node_id.clone()),
                edge_id: Some(edge_id.clone()),
                field: None,
                detail: None,
            },
            Self::DuplicateEdgeId { edge_id } => IssueReport {
                code: "DUPLICATE_EDGE_ID",
                message: format!("edge id `{edge_id}` declared more than once"),
                node_id: None,
                edge_id: Some(edge_id.clone()),
                field: Some("id"),
                detail: None,
            },
            Self::UnreachableNode { node_id } => IssueReport {
                code: "UNREACHABLE_NODE",
                message: format!("node `{node_id}` is not reachable from the entrypoint"),
                node_id: Some(node_id.clone()),
                edge_id: None,
                field: None,
                detail: None,
            },
            Self::CyclicGraph { node_ids } => IssueReport {
                code: "CYCLIC_GRAPH",
                message: format!("cycle detected through nodes [{}]", node_ids.join(", ")),
                node_id: None,
                edge_id: None,
                field: None,
                detail: None,
            },
        }
    }
}

pub fn validate_dagspec(spec: &DagSpec) -> Result<(), ValidationError> {
    let mut issues = Vec::new();

    if spec.nodes.is_empty() {
        issues.push(ValidationIssue::EmptyGraph);
        return Err(ValidationError { issues, raw_json: None });
    }

    check_node_declarations(spec, &mut issues);
    check_contracts(spec, &mut issues);
    check_routes(spec, &mut issues);
    check_edges(spec, &mut issues);

    if issues.is_empty() {
        check_graph_topology(spec, &mut issues);
    }

    match issues.is_empty() {
        true => Ok(()),
        false => Err(ValidationError { issues, raw_json: None }),
    }
}

fn check_node_declarations(spec: &DagSpec, issues: &mut Vec<ValidationIssue>) {
    if !spec.nodes.iter().any(|n| n.node_id == spec.entrypoint) {
        issues.push(ValidationIssue::UnknownEntrypoint { entrypoint: spec.entrypoint.clone() });
    }
    let mut seen = std::collections::HashSet::new();
    for node in &spec.nodes {
        if !seen.insert(node.node_id.clone()) {
            issues.push(ValidationIssue::DuplicateNodeId { node_id: node.node_id.clone() });
        }
    }
}

fn check_contracts(spec: &DagSpec, issues: &mut Vec<ValidationIssue>) {
    for node in &spec.nodes {
        if node.contract.reviewer_models.is_empty() {
            issues.push(ValidationIssue::EmptyReviewerModels { node_id: node.node_id.clone() });
        }
    }
}

fn check_routes(spec: &DagSpec, issues: &mut Vec<ValidationIssue>) {
    let node_ids: std::collections::HashSet<&str> =
        spec.nodes.iter().map(|n| n.node_id.as_str()).collect();
    for node in &spec.nodes {
        match (node.node_type, &node.routes) {
            (NodeType::Router, None) => {
                issues.push(ValidationIssue::RouterWithoutRoutes { node_id: node.node_id.clone() })
            }
            (NodeType::Router, Some(routes)) if routes.choices.is_empty() => {
                issues.push(ValidationIssue::EmptyRoutes { node_id: node.node_id.clone() })
            }
            (NodeType::Router, Some(routes)) => {
                for (choice, target) in &routes.choices {
                    if target != END_NODE && !node_ids.contains(target.as_str()) {
                        issues.push(ValidationIssue::RouteTargetUnknown {
                            node_id: node.node_id.clone(),
                            choice: choice.clone(),
                            target: target.clone(),
                        });
                    }
                }
            }
            (_, Some(_)) => {
                issues.push(ValidationIssue::NonRouterWithRoutes { node_id: node.node_id.clone() })
            }
            _ => {}
        }
    }
}

fn check_edges(spec: &DagSpec, issues: &mut Vec<ValidationIssue>) {
    let node_ids: std::collections::HashSet<&str> =
        spec.nodes.iter().map(|n| n.node_id.as_str()).collect();
    let mut seen_edge_ids = std::collections::HashSet::new();
    for edge in &spec.edges {
        if !seen_edge_ids.insert(edge.id.clone()) {
            issues.push(ValidationIssue::DuplicateEdgeId { edge_id: edge.id.clone() });
        }
        if !node_ids.contains(edge.from.as_str()) {
            issues.push(ValidationIssue::EdgeUnknownNode { edge_id: edge.id.clone(), endpoint: edge.from.clone() });
        }
        if !node_ids.contains(edge.to.as_str()) && edge.to != END_NODE {
            issues.push(ValidationIssue::EdgeUnknownNode { edge_id: edge.id.clone(), endpoint: edge.to.clone() });
        }
        if edge.from == edge.to {
            issues.push(ValidationIssue::EdgeSelfLoop { edge_id: edge.id.clone(), node_id: edge.from.clone() });
        }
    }
}

fn check_graph_topology(spec: &DagSpec, issues: &mut Vec<ValidationIssue>) {
    let index_of: HashMap<&str, usize> = spec.nodes.iter().enumerate().map(|(i, n)| (n.node_id.as_str(), i)).collect();

    let adjacency = build_adjacency(spec, &index_of);
    if let Some(entry_index) = index_of.get(spec.entrypoint.as_str()) {
        let reachable = reachability_from(*entry_index, &adjacency);
        for (i, node) in spec.nodes.iter().enumerate() {
            if node.node_type != NodeType::End && !reachable[i] {
                issues.push(ValidationIssue::UnreachableNode { node_id: node.node_id.clone() });
            }
        }
    }

    if let Some(cycle) = find_cycle(&adjacency) {
        let cycle_ids: Vec<String> = cycle.into_iter().map(|i| spec.nodes[i].node_id.clone()).collect();
        issues.push(ValidationIssue::CyclicGraph { node_ids: cycle_ids });
    }
}

fn build_adjacency(spec: &DagSpec, index_of: &HashMap<&str, usize>) -> Vec<Vec<usize>> {
    let mut adjacency = vec![Vec::new(); spec.nodes.len()];
    for edge in &spec.edges {
        let (Some(from), Some(to)) = (index_of.get(edge.from.as_str()), index_of.get(edge.to.as_str())) else {
            continue;
        };
        adjacency[*from].push(*to);
    }
    for node in &spec.nodes {
        if node.node_type != NodeType::Router {
            continue;
        }
        let Some(from) = index_of.get(node.node_id.as_str()) else { continue };
        if let Some(routes) = &node.routes {
            for target in routes.choices.values() {
                if let Some(to) = index_of.get(target.as_str()) {
                    adjacency[*from].push(*to);
                }
            }
        }
    }
    adjacency
}

fn reachability_from(start: usize, adjacency: &[Vec<usize>]) -> Vec<bool> {
    let mut visited = vec![false; adjacency.len()];
    let mut stack = vec![start];
    visited[start] = true;
    while let Some(current) = stack.pop() {
        for &next in &adjacency[current] {
            if !visited[next] {
                visited[next] = true;
                stack.push(next);
            }
        }
    }
    visited
}

fn find_cycle(adjacency: &[Vec<usize>]) -> Option<Vec<usize>> {
    const UNVISITED: u8 = 0;
    const ON_STACK: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![UNVISITED; adjacency.len()];
    let mut path = Vec::new();

    fn visit(
        node: usize,
        adjacency: &[Vec<usize>],
        state: &mut [u8],
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        state[node] = ON_STACK;
        path.push(node);
        for &next in &adjacency[node] {
            match state[next] {
                UNVISITED => {
                    if let Some(cycle) = visit(next, adjacency, state, path) {
                        return Some(cycle);
                    }
                }
                ON_STACK => {
                    let start = path.iter().position(|&p| p == next).expect("node marked ON_STACK must be on the current path");
                    return Some(path[start..].to_vec());
                }
                DONE => {}
                _ => unreachable!("state values are limited to UNVISITED/ON_STACK/DONE"),
            }
        }
        path.pop();
        state[node] = DONE;
        None
    }

    for start in 0..adjacency.len() {
        if state[start] == UNVISITED {
            if let Some(cycle) = visit(start, adjacency, &mut state, &mut path) {
                return Some(cycle);
            }
        }
    }
    None
}
