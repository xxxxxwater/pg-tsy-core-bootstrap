use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlScriptDefinition {
    pub schema_version: String,
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub actions: Vec<ControlAction>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlAction {
    Status,
    Latency,
    Logs { lines: u16 },
    Reconcile { venue: Option<String> },
    ReloadStrategies,
    SafeHold { venue: Option<String>, asset: Option<String> },
    EmergencyFlattenOwned { venue: Option<String>, asset: Option<String> },
    Halt,
}

impl ControlAction {
    pub fn mutates_runtime(&self) -> bool {
        matches!(
            self,
            Self::Reconcile { .. }
                | Self::ReloadStrategies
                | Self::SafeHold { .. }
                | Self::EmergencyFlattenOwned { .. }
                | Self::Halt
        )
    }
}

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("control script IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("control script TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported control script schema: {0}")]
    Schema(String),
    #[error("invalid control script name: {0}")]
    InvalidName(String),
    #[error("control script has no actions: {0}")]
    Empty(String),
    #[error("invalid log line count in script {0}")]
    InvalidLogLines(String),
    #[error("duplicate control script name: {0}")]
    Duplicate(String),
}

impl ControlScriptDefinition {
    pub fn from_toml_str(input: &str) -> Result<Self, ScriptError> {
        let definition: Self = toml::from_str(input)?;
        definition.validate()?;
        Ok(definition)
    }

    pub fn validate(&self) -> Result<(), ScriptError> {
        if self.schema_version != "control-script.v1" {
            return Err(ScriptError::Schema(self.schema_version.clone()));
        }
        if !valid_script_name(&self.name) {
            return Err(ScriptError::InvalidName(self.name.clone()));
        }
        if self.actions.is_empty() {
            return Err(ScriptError::Empty(self.name.clone()));
        }
        for action in &self.actions {
            if let ControlAction::Logs { lines } = action
                && !(1..=200).contains(lines)
            {
                return Err(ScriptError::InvalidLogLines(self.name.clone()));
            }
        }
        Ok(())
    }

    pub fn mutates_runtime(&self) -> bool {
        self.actions.iter().any(ControlAction::mutates_runtime)
    }
}

#[derive(Debug, Default)]
pub struct ControlScriptRegistry {
    scripts: BTreeMap<String, ControlScriptDefinition>,
    sources: BTreeMap<PathBuf, String>,
}

impl ControlScriptRegistry {
    pub fn load_dir(path: impl AsRef<Path>) -> Result<Self, ScriptError> {
        let path = path.as_ref();
        let mut entries = fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("toml"))
            .collect::<Vec<_>>();
        entries.sort();

        let mut registry = Self::default();
        for path in entries {
            registry.load_file(&path)?;
        }
        Ok(registry)
    }

    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), ScriptError> {
        let path = path.as_ref().to_path_buf();
        let definition = ControlScriptDefinition::from_toml_str(&fs::read_to_string(&path)?)?;
        if self.scripts.contains_key(&definition.name) {
            return Err(ScriptError::Duplicate(definition.name));
        }
        let name = definition.name.clone();
        self.scripts.insert(name.clone(), definition);
        self.sources.insert(path, name);
        Ok(())
    }

    pub fn reload_file(&mut self, path: impl AsRef<Path>) -> Result<String, ScriptError> {
        let path = path.as_ref().to_path_buf();
        let definition = ControlScriptDefinition::from_toml_str(&fs::read_to_string(&path)?)?;
        if let Some(old_name) = self.sources.get(&path)
            && old_name != &definition.name
        {
            self.scripts.remove(old_name);
        }
        if let Some(existing) = self.scripts.get(&definition.name)
            && self.sources.get(&path).map(String::as_str) != Some(existing.name.as_str())
        {
            return Err(ScriptError::Duplicate(definition.name));
        }
        let name = definition.name.clone();
        self.scripts.insert(name.clone(), definition);
        self.sources.insert(path, name.clone());
        Ok(name)
    }

    pub fn get(&self, name: &str) -> Option<&ControlScriptDefinition> {
        self.scripts.get(name).filter(|script| script.enabled)
    }

    pub fn names(&self) -> Vec<String> {
        self.scripts
            .values()
            .filter(|script| script.enabled)
            .map(|script| script.name.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.scripts.values().filter(|script| script.enabled).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_safe_multi_action_script() {
        let script = ControlScriptDefinition::from_toml_str(
            r#"
            schema_version = "control-script.v1"
            name = "status-snapshot"
            description = "Read-only status bundle"

            [[actions]]
            type = "status"

            [[actions]]
            type = "latency"

            [[actions]]
            type = "logs"
            lines = 50
            "#,
        )
        .unwrap();
        assert_eq!(script.actions.len(), 3);
        assert!(!script.mutates_runtime());
    }

    #[test]
    fn arbitrary_shell_is_not_part_of_the_schema() {
        let result = ControlScriptDefinition::from_toml_str(
            r#"
            schema_version = "control-script.v1"
            name = "bad"
            [[actions]]
            type = "shell"
            command = "rm -rf /"
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn emergency_flatten_is_marked_mutating() {
        let script = ControlScriptDefinition::from_toml_str(
            r#"
            schema_version = "control-script.v1"
            name = "emergency-owned"
            [[actions]]
            type = "emergency_flatten_owned"
            venue = "HYPERLIQUID"
            "#,
        )
        .unwrap();
        assert!(script.mutates_runtime());
    }
}
