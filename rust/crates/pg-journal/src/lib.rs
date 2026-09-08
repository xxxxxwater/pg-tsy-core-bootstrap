use anyhow::Result;
use serde::Serialize;
use std::{fs::OpenOptions, io::Write, path::{Path, PathBuf}, sync::Mutex};

pub trait Journal: Send + Sync { fn append<T: Serialize>(&self, event: &T) -> Result<()>; }

pub struct JsonlJournal { path: PathBuf, lock: Mutex<()> }

impl JsonlJournal {
    pub fn new(path: impl AsRef<Path>) -> Self { Self { path: path.as_ref().to_path_buf(), lock: Mutex::new(()) } }
}

impl Journal for JsonlJournal {
    fn append<T: Serialize>(&self, event: &T) -> Result<()> {
        let _guard = self.lock.lock().expect("journal mutex poisoned");
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        serde_json::to_writer(&mut f, event)?;
        f.write_all(b"\n")?;
        f.sync_data()?;
        Ok(())
    }
}
