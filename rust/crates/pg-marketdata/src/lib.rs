use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SequenceError {
    #[error("market data gap: expected {expected}, got {actual}")]
    Gap { expected: u64, actual: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SequenceTracker { last: Option<u64> }

impl SequenceTracker {
    pub fn apply(&mut self, sequence: u64) -> Result<(), SequenceError> {
        if let Some(last) = self.last {
            let expected = last + 1;
            if sequence != expected { return Err(SequenceError::Gap { expected, actual: sequence }); }
        }
        self.last = Some(sequence);
        Ok(())
    }

    pub fn reset(&mut self, snapshot_sequence: u64) { self.last = Some(snapshot_sequence); }
}
