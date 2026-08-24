use crate::entities::{DagSpec, EdgeSpec, NodeSpec, NodeType, RoutesSpec};
use crate::validate::{validate_dagspec, ValidationError};
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Drafting,
    Submitted,
    Abandoned,
}

#[derive(Debug)]
pub struct Draft {
    draft_id: String,
    name: String,
    status: DraftStatus,
    nodes: Vec<NodeSpec>,
    edges: Vec<EdgeSpec>,
}

impl Draft {
    pub fn draft_id(&self) -> &str {
        &self.draft_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn status(&self) -> DraftStatus {
        self.status
    }

    pub fn nodes(&self) -> &[NodeSpec] {
        &self.nodes
    }

    pub fn edges(&self) -> &[EdgeSpec] {
        &self.edges
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuilderError {
    DraftExists { draft_id: String },
    DraftNotFound { draft_id: String },
    DraftNotDrafting { draft_id: String, status: DraftStatus },
    DuplicateNodeId { draft_id: String, node_id: String },
    RoutesNodeNotFound { draft_id: String, node_id: String },
    RoutesNotAllowed { draft_id: String, node_id: String },
}

impl fmt::Display for BuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DraftExists { draft_id } => write!(f, "draft `{draft_id}` already exists"),
            Self::DraftNotFound { draft_id } => write!(f, "draft `{draft_id}` not found"),
            Self::DraftNotDrafting { draft_id, status } => {
                let rendered = serde_json::to_string(status).expect("enum serialization cannot fail");
                write!(f, "draft `{draft_id}` is {rendered}, mutations require drafting")
            }
            Self::DuplicateNodeId { draft_id, node_id } => {
                write!(f, "draft `{draft_id}` already contains node `{node_id}`")
            }
            Self::RoutesNodeNotFound { draft_id, node_id } => {
                write!(f, "draft `{draft_id}` has no node `{node_id}` to attach routes to")
            }
            Self::RoutesNotAllowed { draft_id, node_id } => {
                write!(f, "node `{node_id}` in draft `{draft_id}` is not a router, routes are only allowed on router nodes")
            }
        }
    }
}

impl std::error::Error for BuilderError {}

#[derive(Debug, Clone, PartialEq)]
pub enum CommitError {
    Builder(BuilderError),
    Validation(ValidationError),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builder(err) => write!(f, "{err}"),
            Self::Validation(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CommitError {}

#[derive(Debug, Default)]
pub struct GraphBuilder {
    drafts: Vec<Draft>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&mut self, draft_id: impl Into<String>, name: impl Into<String>) -> Result<(), BuilderError> {
        let draft_id = draft_id.into();
        if self.drafts.iter().any(|d| d.draft_id == draft_id) {
            return Err(BuilderError::DraftExists { draft_id });
        }
        self.drafts.push(Draft {
            draft_id,
            name: name.into(),
            status: DraftStatus::Drafting,
            nodes: Vec::new(),
            edges: Vec::new(),
        });
        Ok(())
    }

    pub fn add_node(&mut self, draft_id: &str, node: NodeSpec) -> Result<(), BuilderError> {
        let draft = self.drafting_draft_mut(draft_id)?;
        if draft.nodes.iter().any(|n| n.node_id == node.node_id) {
            return Err(BuilderError::DuplicateNodeId {
                draft_id: draft_id.to_string(),
                node_id: node.node_id,
            });
        }
        draft.nodes.push(node);
        Ok(())
    }

    pub fn add_edge(&mut self, draft_id: &str, edge: EdgeSpec) -> Result<(), BuilderError> {
        let draft = self.drafting_draft_mut(draft_id)?;
        draft.edges.push(edge);
        Ok(())
    }

    pub fn set_routes(&mut self, draft_id: &str, node_id: &str, routes: RoutesSpec) -> Result<(), BuilderError> {
        let draft = self.drafting_draft_mut(draft_id)?;
        let target = draft
            .nodes
            .iter_mut()
            .find(|n| n.node_id == node_id)
            .ok_or_else(|| BuilderError::RoutesNodeNotFound {
                draft_id: draft_id.to_string(),
                node_id: node_id.to_string(),
            })?;
        if target.node_type != NodeType::Router {
            return Err(BuilderError::RoutesNotAllowed {
                draft_id: draft_id.to_string(),
                node_id: node_id.to_string(),
            });
        }
        target.routes = Some(routes);
        Ok(())
    }

    pub fn commit(&mut self, draft_id: &str, entrypoint: &str) -> Result<DagSpec, CommitError> {
        let draft = self.drafting_draft_mut(draft_id).map_err(CommitError::Builder)?;
        let spec = DagSpec {
            name: draft.name.clone(),
            version: 1,
            entrypoint: entrypoint.to_string(),
            nodes: draft.nodes.clone(),
            edges: draft.edges.clone(),
        };
        validate_dagspec(&spec).map_err(CommitError::Validation)?;
        draft.status = DraftStatus::Submitted;
        Ok(spec)
    }

    pub fn abandon(&mut self, draft_id: &str) -> Result<(), BuilderError> {
        let draft = self.draft_mut(draft_id)?;
        if draft.status != DraftStatus::Drafting {
            return Err(BuilderError::DraftNotDrafting {
                draft_id: draft_id.to_string(),
                status: draft.status,
            });
        }
        draft.status = DraftStatus::Abandoned;
        Ok(())
    }

    pub fn draft_status(&self, draft_id: &str) -> Option<DraftStatus> {
        self.drafts.iter().find(|d| d.draft_id == draft_id).map(|d| d.status)
    }

    fn draft_mut(&mut self, draft_id: &str) -> Result<&mut Draft, BuilderError> {
        self.drafts
            .iter_mut()
            .find(|d| d.draft_id == draft_id)
            .ok_or_else(|| BuilderError::DraftNotFound { draft_id: draft_id.to_string() })
    }

    fn drafting_draft_mut(&mut self, draft_id: &str) -> Result<&mut Draft, BuilderError> {
        let draft = self.draft_mut(draft_id)?;
        if draft.status != DraftStatus::Drafting {
            return Err(BuilderError::DraftNotDrafting {
                draft_id: draft_id.to_string(),
                status: draft.status,
            });
        }
        Ok(draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{Contract, NodeType};
    use crate::validate::ValidationIssue;
    use std::collections::BTreeMap;

    fn step_node(id: &str) -> NodeSpec {
        NodeSpec {
            node_id: id.to_string(),
            node_type: NodeType::Step,
            contract: Contract {
                prompt: "do the work".into(),
                acceptance_criteria: "work is done".into(),
                reviewer_models: vec!["reviewer-a".into()],
            },
            params: serde_json::Map::new(),
            input_schema: BTreeMap::new(),
            routes: None,
        }
    }

    fn two_step_draft(builder: &mut GraphBuilder) {
        builder.begin("draft-1", "greeting").expect("begin succeeds");
        builder.add_node("draft-1", step_node("step-a")).expect("first node");
        builder.add_node("draft-1", step_node("step-b")).expect("second node");
    }

    #[test]
    fn commit_produces_validated_dagspec_and_marks_submitted() {
        let mut builder = GraphBuilder::new();
        two_step_draft(&mut builder);
        builder
            .add_edge("draft-1", EdgeSpec { id: "e1".into(), from: "step-a".into(), to: "step-b".into() })
            .expect("edge accepted");

        let spec = builder.commit("draft-1", "step-a").expect("valid graph commits");

        assert_eq!(spec.name, "greeting");
        assert_eq!(spec.version, 1);
        assert_eq!(spec.entrypoint, "step-a");
        assert_eq!(spec.nodes.len(), 2);
        assert_eq!(spec.edges.len(), 1);
        assert_eq!(builder.draft_status("draft-1"), Some(DraftStatus::Submitted));
    }

    #[test]
    fn begin_rejects_existing_draft() {
        let mut builder = GraphBuilder::new();
        builder.begin("draft-1", "first").expect("begin succeeds");
        let err = builder.begin("draft-1", "second").expect_err("duplicate draft id refused");
        assert_eq!(err, BuilderError::DraftExists { draft_id: "draft-1".into() });
    }

    #[test]
    fn operations_on_unknown_draft_fail() {
        let mut builder = GraphBuilder::new();
        let err = builder.add_node("missing", step_node("a")).expect_err("unknown draft refused");
        assert_eq!(err, BuilderError::DraftNotFound { draft_id: "missing".into() });
    }

    #[test]
    fn add_node_rejects_duplicate_node_id() {
        let mut builder = GraphBuilder::new();
        builder.begin("draft-1", "dup").expect("begin succeeds");
        builder.add_node("draft-1", step_node("step-a")).expect("first node");
        let err = builder.add_node("draft-1", step_node("step-a")).expect_err("duplicate node refused");
        assert_eq!(
            err,
            BuilderError::DuplicateNodeId { draft_id: "draft-1".into(), node_id: "step-a".into() }
        );
    }

    #[test]
    fn set_routes_requires_router_node() {
        let mut builder = GraphBuilder::new();
        builder.begin("draft-1", "routes").expect("begin succeeds");
        builder.add_node("draft-1", step_node("step-a")).expect("node added");

        let routes = RoutesSpec { choices: BTreeMap::from([("pass".into(), "__end__".into())]) };
        let err = builder.set_routes("draft-1", "ghost", routes.clone()).expect_err("unknown node refused");
        assert_eq!(err, BuilderError::RoutesNodeNotFound { draft_id: "draft-1".into(), node_id: "ghost".into() });

        let err = builder.set_routes("draft-1", "step-a", routes.clone()).expect_err("non-router refused");
        assert_eq!(err, BuilderError::RoutesNotAllowed { draft_id: "draft-1".into(), node_id: "step-a".into() });

        let router = NodeSpec { node_type: NodeType::Router, ..step_node("judge") };
        builder.add_node("draft-1", router).expect("router added");
        builder.set_routes("draft-1", "judge", routes).expect("router accepts routes");
        assert!(
            builder.add_edge("draft-1", EdgeSpec { id: "e1".into(), from: "step-a".into(), to: "judge".into() }).is_ok()
        );
        let spec = builder.commit("draft-1", "step-a").expect("commits with routed router");
        assert!(spec.nodes.iter().find(|n| n.node_id == "judge").unwrap().routes.is_some());
    }

    #[test]
    fn failed_commit_keeps_draft_editable_until_valid() {
        let mut builder = GraphBuilder::new();
        two_step_draft(&mut builder);

        let err = builder.commit("draft-1", "step-a").expect_err("disconnected second node rejected");
        match err {
            CommitError::Validation(validation) => {
                assert!(validation.issues.contains(&ValidationIssue::UnreachableNode { node_id: "step-b".into() }))
            }
            other => panic!("expected validation failure, got {other:?}"),
        }
        assert_eq!(builder.draft_status("draft-1"), Some(DraftStatus::Drafting));

        builder
            .add_edge("draft-1", EdgeSpec { id: "e1".into(), from: "step-a".into(), to: "step-b".into() })
            .expect("repair edge accepted");
        builder.commit("draft-1", "step-a").expect("repaired draft commits");
    }

    #[test]
    fn submitted_and_abandoned_drafts_reject_mutations() {
        let mut builder = GraphBuilder::new();
        two_step_draft(&mut builder);
        builder
            .add_edge("draft-1", EdgeSpec { id: "e1".into(), from: "step-a".into(), to: "step-b".into() })
            .expect("edge accepted");
        builder.commit("draft-1", "step-a").expect("commits");
        let err = builder.add_node("draft-1", step_node("step-c")).expect_err("submitted draft frozen");
        assert_eq!(
            err,
            BuilderError::DraftNotDrafting { draft_id: "draft-1".into(), status: DraftStatus::Submitted }
        );

        builder.begin("draft-2", "abandon-me").expect("begin succeeds");
        builder.abandon("draft-2").expect("drafting draft can be abandoned");
        let err = builder.add_node("draft-2", step_node("step-a")).expect_err("abandoned draft frozen");
        assert_eq!(
            err,
            BuilderError::DraftNotDrafting { draft_id: "draft-2".into(), status: DraftStatus::Abandoned }
        );
        let err = builder.abandon("draft-2").expect_err("already-abandoned draft refuses second abandon");
        assert_eq!(
            err,
            BuilderError::DraftNotDrafting { draft_id: "draft-2".into(), status: DraftStatus::Abandoned }
        );
    }
}
