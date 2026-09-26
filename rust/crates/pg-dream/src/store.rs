use crate::{DiscoveryTree, WorldError, WorldPool};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub trait ExperimentStore {
    fn append(&mut self, tree: &DiscoveryTree) -> Result<(), StoreError>;
    fn load(&self) -> Result<WorldPool, StoreError>;
}

#[derive(Debug, Clone)]
pub struct JsonlExperimentStore {
    path: PathBuf,
}

impl JsonlExperimentStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ExperimentStore for JsonlExperimentStore {
    fn append(&mut self, tree: &DiscoveryTree) -> Result<(), StoreError> {
        tree.validate()?;

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, tree)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    fn load(&self) -> Result<WorldPool, StoreError> {
        if !self.path.exists() {
            return Ok(WorldPool::new());
        }

        let file = File::open(&self.path)?;
        let mut pool = WorldPool::new();

        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let tree = serde_json::from_str::<DiscoveryTree>(&line).map_err(|error| {
                StoreError::InvalidLine {
                    line: index + 1,
                    message: error.to_string(),
                }
            })?;
            pool.push(tree)?;
        }

        Ok(pool)
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryExperimentStore {
    pool: WorldPool,
}

impl MemoryExperimentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ExperimentStore for MemoryExperimentStore {
    fn append(&mut self, tree: &DiscoveryTree) -> Result<(), StoreError> {
        self.pool.push(tree.clone())?;
        Ok(())
    }

    fn load(&self) -> Result<WorldPool, StoreError> {
        Ok(self.pool.clone())
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    World(#[from] WorldError),
    #[error("invalid JSONL discovery tree at line {line}: {message}")]
    InvalidLine { line: usize, message: String },
}
