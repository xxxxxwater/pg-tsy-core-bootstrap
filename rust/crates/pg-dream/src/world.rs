use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

pub type NodeId = Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyVariant {
    pub variant_id: Uuid,
    pub parent_variant_id: Option<Uuid>,
    pub generation: u32,
    pub name: String,
    #[serde(default)]
    pub parameters: Value,
}

impl StrategyVariant {
    pub fn root(name: impl Into<String>, parameters: Value) -> Self {
        Self {
            variant_id: Uuid::new_v4(),
            parent_variant_id: None,
            generation: 0,
            name: name.into(),
            parameters,
        }
    }

    pub fn child(parent: &StrategyVariant, name: impl Into<String>, parameters: Value) -> Self {
        Self {
            variant_id: Uuid::new_v4(),
            parent_variant_id: Some(parent.variant_id),
            generation: parent.generation.saturating_add(1),
            name: name.into(),
            parameters,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentMetrics {
    pub net_return_pct: f64,
    pub sharpe: f64,
    pub max_drawdown_pct: f64,
    pub trades: u64,
    pub turnover: f64,
    pub execution_cost_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentOutcome {
    pub success: bool,
    pub metrics: ExperimentMetrics,
    pub cost_units: u64,
    #[serde(default)]
    pub diagnostics: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryNode {
    pub node_id: NodeId,
    pub parent_id: Option<NodeId>,
    pub variant: StrategyVariant,
    pub outcome: ExperimentOutcome,
    pub created_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryTree {
    pub tree_id: Uuid,
    pub root_id: NodeId,
    nodes: BTreeMap<NodeId, DiscoveryNode>,
}

impl DiscoveryTree {
    pub fn new(root_variant: StrategyVariant, root_outcome: ExperimentOutcome) -> Self {
        Self::with_id(Uuid::new_v4(), root_variant, root_outcome)
    }

    pub fn with_id(
        tree_id: Uuid,
        root_variant: StrategyVariant,
        root_outcome: ExperimentOutcome,
    ) -> Self {
        let root_id = Uuid::new_v4();
        let root = DiscoveryNode {
            node_id: root_id,
            parent_id: None,
            variant: root_variant,
            outcome: root_outcome,
            created_seq: 0,
        };
        Self {
            tree_id,
            root_id,
            nodes: BTreeMap::from([(root_id, root)]),
        }
    }

    pub fn root(&self) -> Result<&DiscoveryNode, WorldError> {
        self.node(self.root_id)
    }

    pub fn node(&self, node_id: NodeId) -> Result<&DiscoveryNode, WorldError> {
        self.nodes
            .get(&node_id)
            .ok_or(WorldError::MissingNode(node_id))
    }

    pub fn nodes(&self) -> impl Iterator<Item = &DiscoveryNode> {
        self.nodes.values()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn validate_child(
        &self,
        parent_id: NodeId,
        variant: &StrategyVariant,
    ) -> Result<(), WorldError> {
        let parent = self.node(parent_id)?;
        if variant.parent_variant_id != Some(parent.variant.variant_id) {
            return Err(WorldError::VariantParentMismatch {
                parent_node_id: parent_id,
                expected_variant_id: parent.variant.variant_id,
                actual_variant_id: variant.parent_variant_id,
            });
        }

        let expected_generation = parent.variant.generation.saturating_add(1);
        if variant.generation != expected_generation {
            return Err(WorldError::GenerationMismatch {
                parent_node_id: parent_id,
                expected: expected_generation,
                actual: variant.generation,
            });
        }
        Ok(())
    }

    pub fn append(
        &mut self,
        parent_id: NodeId,
        variant: StrategyVariant,
        outcome: ExperimentOutcome,
    ) -> Result<NodeId, WorldError> {
        self.validate_child(parent_id, &variant)?;
        let node_id = Uuid::new_v4();
        let node = DiscoveryNode {
            node_id,
            parent_id: Some(parent_id),
            variant,
            outcome,
            created_seq: self.nodes.len() as u64,
        };
        self.nodes.insert(node_id, node);
        Ok(node_id)
    }

    pub fn children_of(&self, parent_id: NodeId) -> Vec<&DiscoveryNode> {
        let mut children = self
            .nodes
            .values()
            .filter(|node| node.parent_id == Some(parent_id))
            .collect::<Vec<_>>();
        children.sort_by_key(|node| (node.created_seq, node.node_id));
        children
    }

    pub fn depth(&self, node_id: NodeId) -> Result<u32, WorldError> {
        let mut depth = 0_u32;
        let mut current = node_id;
        let mut seen = BTreeSet::new();

        loop {
            if !seen.insert(current) {
                return Err(WorldError::Cycle(current));
            }
            let node = self.node(current)?;
            match node.parent_id {
                Some(parent_id) => {
                    depth = depth.saturating_add(1);
                    current = parent_id;
                }
                None => return Ok(depth),
            }
        }
    }

    pub fn validate(&self) -> Result<(), WorldError> {
        if self.nodes.is_empty() {
            return Err(WorldError::CorruptTree("tree has no nodes".into()));
        }

        let root = self.root()?;
        if root.parent_id.is_some()
            || root.variant.parent_variant_id.is_some()
            || root.variant.generation != 0
        {
            return Err(WorldError::CorruptTree(
                "root must have no parent and generation zero".into(),
            ));
        }

        for node in self.nodes.values() {
            if node.node_id == self.root_id {
                continue;
            }
            let parent_id = node.parent_id.ok_or_else(|| {
                WorldError::CorruptTree(format!("node {} has no parent", node.node_id))
            })?;
            self.validate_child(parent_id, &node.variant)?;
            let _ = self.depth(node.node_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorldPool {
    trees: Vec<DiscoveryTree>,
}

impl WorldPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_trees(trees: Vec<DiscoveryTree>) -> Result<Self, WorldError> {
        let mut pool = Self::new();
        for tree in trees {
            pool.push(tree)?;
        }
        Ok(pool)
    }

    pub fn push(&mut self, tree: DiscoveryTree) -> Result<(), WorldError> {
        tree.validate()?;
        if self
            .trees
            .iter()
            .any(|existing| existing.tree_id == tree.tree_id)
        {
            return Err(WorldError::DuplicateTree(tree.tree_id));
        }
        self.trees.push(tree);
        Ok(())
    }

    pub fn trees(&self) -> &[DiscoveryTree] {
        &self.trees
    }

    pub fn len(&self) -> usize {
        self.trees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trees.is_empty()
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum WorldError {
    #[error("discovery node {0} does not exist")]
    MissingNode(NodeId),
    #[error(
        "variant parent mismatch at node {parent_node_id}: expected {expected_variant_id}, got {actual_variant_id:?}"
    )]
    VariantParentMismatch {
        parent_node_id: NodeId,
        expected_variant_id: Uuid,
        actual_variant_id: Option<Uuid>,
    },
    #[error(
        "variant generation mismatch at node {parent_node_id}: expected {expected}, got {actual}"
    )]
    GenerationMismatch {
        parent_node_id: NodeId,
        expected: u32,
        actual: u32,
    },
    #[error("cycle detected while walking discovery tree at node {0}")]
    Cycle(NodeId),
    #[error("duplicate discovery tree {0}")]
    DuplicateTree(Uuid),
    #[error("corrupt discovery tree: {0}")]
    CorruptTree(String),
}
