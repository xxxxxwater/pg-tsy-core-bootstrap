use crate::{DiscoveryTree, ExperimentOutcome, NodeId, StrategyVariant, WorldError};
use std::error::Error;
use thiserror::Error;

pub type ExecutionError = Box<dyn Error + Send + Sync + 'static>;

/// Isolated experiment boundary for online discovery.
///
/// Implementations may call backtests, deterministic simulation, or shadow
/// execution. They must not call live exchange order endpoints.
pub trait ExperimentExecutor {
    fn execute(&mut self, variant: &StrategyVariant) -> Result<ExperimentOutcome, ExecutionError>;
}

#[derive(Debug)]
pub struct OnlineExplorer<E> {
    executor: E,
}

impl<E> OnlineExplorer<E>
where
    E: ExperimentExecutor,
{
    pub fn new(executor: E) -> Self {
        Self { executor }
    }

    pub fn executor(&self) -> &E {
        &self.executor
    }

    pub fn executor_mut(&mut self) -> &mut E {
        &mut self.executor
    }

    pub fn into_inner(self) -> E {
        self.executor
    }

    pub fn start(
        &mut self,
        root_variant: StrategyVariant,
    ) -> Result<DiscoveryTree, ExploreError> {
        let outcome = self
            .executor
            .execute(&root_variant)
            .map_err(ExploreError::Execution)?;
        Ok(DiscoveryTree::new(root_variant, outcome))
    }

    pub fn expand(
        &mut self,
        tree: &mut DiscoveryTree,
        parent_id: NodeId,
        variants: impl IntoIterator<Item = StrategyVariant>,
    ) -> Result<Vec<NodeId>, ExploreError> {
        let variants = variants.into_iter().collect::<Vec<_>>();
        for variant in &variants {
            tree.validate_child(parent_id, variant)?;
        }

        let mut inserted = Vec::with_capacity(variants.len());
        for variant in variants {
            let outcome = self
                .executor
                .execute(&variant)
                .map_err(ExploreError::Execution)?;
            inserted.push(tree.append(parent_id, variant, outcome)?);
        }
        Ok(inserted)
    }
}

#[derive(Debug, Error)]
pub enum ExploreError {
    #[error("experiment execution failed")]
    Execution(#[source] ExecutionError),
    #[error(transparent)]
    World(#[from] WorldError),
}
