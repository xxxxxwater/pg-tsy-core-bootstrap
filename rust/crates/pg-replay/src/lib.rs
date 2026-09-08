use anyhow::Result;
use serde_json::Value;
use std::{fs::File, io::{BufRead, BufReader}, path::Path};

pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<Value>> {
    let file = File::open(path)?;
    BufReader::new(file).lines().map(|line| Ok(serde_json::from_str(&line?)?)).collect()
}
