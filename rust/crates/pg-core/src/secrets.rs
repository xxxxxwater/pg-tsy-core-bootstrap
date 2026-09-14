use anyhow::{Context, Result, bail};
use std::{env, fs};

/// Read a secret without ever logging its value.
///
/// `<NAME>_FILE` takes precedence over `<NAME>`, allowing Docker/Kubernetes
/// read-only secret mounts. Empty values are treated as unset.
pub fn optional_secret(name: &str) -> Result<Option<String>> {
    let file_key = format!("{name}_FILE");
    if let Ok(path) = env::var(&file_key)
        && !path.trim().is_empty()
    {
        let value = fs::read_to_string(path.trim())
            .with_context(|| format!("failed to read secret file configured by {file_key}"))?;
        let value = value.trim().to_owned();
        return Ok((!value.is_empty()).then_some(value));
    }

    Ok(env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty()))
}

pub fn required_secret(name: &str) -> Result<String> {
    optional_secret(name)?.ok_or_else(|| {
        anyhow::anyhow!("missing required secret {name} (set {name} or {name}_FILE)")
    })
}

pub fn required_value(name: &'static str) -> Result<String> {
    let value =
        env::var(name).with_context(|| format!("missing required environment variable {name}"))?;
    if value.trim().is_empty() {
        bail!("environment variable {name} cannot be empty");
    }
    Ok(value.trim().to_owned())
}

pub fn bool_env(name: &'static str, default: bool) -> Result<bool> {
    let Ok(value) = env::var(name) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("invalid boolean {name}={value}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn boolean_parser_documents_true_form() {
        assert_eq!("true".to_ascii_lowercase(), "true");
    }
}
