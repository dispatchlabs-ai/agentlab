use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::store::Store;

pub const CONFIG_VERSION: u32 = 1;
const DEFAULT_HARNESS_TIMEOUT_SECONDS: u64 = 600;
const MAX_HARNESS_TIMEOUT_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentLabConfig {
    pub version: u32,
    #[serde(default)]
    pub default_harness: Option<String>,
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessConfig>,
    #[serde(default)]
    pub diff: DiffConfig,
}

impl Default for AgentLabConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            default_harness: None,
            harnesses: BTreeMap::new(),
            diff: DiffConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    pub command: Vec<String>,
    #[serde(default = "stdin_input")]
    pub input: String,
    #[serde(default = "default_harness_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiffConfig {
    #[serde(default)]
    pub presentation: DiffPresentation,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default = "enabled")]
    pub show_omitted_count: bool,
    #[serde(default)]
    pub fallback: DiffFallback,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            presentation: DiffPresentation::Complete,
            harness: None,
            show_omitted_count: true,
            fallback: DiffFallback::Complete,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffPresentation {
    #[default]
    Complete,
    Important,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffFallback {
    #[default]
    Complete,
}

impl AgentLabConfig {
    pub fn load(store: &Store) -> Result<Self> {
        let path = config_path(store);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read AgentLab config {}", path.display()));
            }
        };
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("AgentLab config {} is not UTF-8", path.display()))?;
        let config: Self = toml::from_str(text)
            .with_context(|| format!("decode AgentLab config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn selected_harness<'a>(
        &'a self,
        override_name: Option<&'a str>,
    ) -> Result<Option<(&'a str, &'a HarnessConfig)>> {
        let name = override_name
            .or(self.diff.harness.as_deref())
            .or(self.default_harness.as_deref());
        let Some(name) = name else {
            return Ok(None);
        };
        let harness = self
            .harnesses
            .get(name)
            .with_context(|| format!("AgentLab harness {name:?} is not defined"))?;
        Ok(Some((name, harness)))
    }

    fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported AgentLab config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        for (name, harness) in &self.harnesses {
            validate_harness_name(name)?;
            if harness.command.is_empty() || harness.command[0].trim().is_empty() {
                bail!("AgentLab harness {name:?} requires a non-empty command");
            }
            if harness
                .command
                .iter()
                .any(|argument| argument.contains('\0'))
            {
                bail!("AgentLab harness {name:?} command contains a NUL byte");
            }
            if harness.input != "stdin" {
                bail!(
                    "AgentLab harness {name:?} has unsupported input {:?}; expected \"stdin\"",
                    harness.input
                );
            }
            if !(1..=MAX_HARNESS_TIMEOUT_SECONDS).contains(&harness.timeout_seconds) {
                bail!(
                    "AgentLab harness {name:?} timeout_seconds must be between 1 and {MAX_HARNESS_TIMEOUT_SECONDS}"
                );
            }
        }
        for (source, name) in [
            ("default_harness", self.default_harness.as_deref()),
            ("diff.harness", self.diff.harness.as_deref()),
        ] {
            if let Some(name) = name {
                validate_harness_name(name)?;
                if !self.harnesses.contains_key(name) {
                    bail!("{source} refers to undefined AgentLab harness {name:?}");
                }
            }
        }
        Ok(())
    }
}

pub fn config_path(store: &Store) -> PathBuf {
    store.root().join("config.toml")
}

fn validate_harness_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "invalid AgentLab harness name {name:?}; use letters, numbers, dot, underscore, or hyphen"
        );
    }
    Ok(())
}

fn stdin_input() -> String {
    "stdin".to_owned()
}

fn default_harness_timeout_seconds() -> u64 {
    DEFAULT_HARNESS_TIMEOUT_SECONDS
}

fn enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_uses_complete_deterministic_diff() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert_eq!(config.diff.presentation, DiffPresentation::Complete);
        assert!(config.selected_harness(None).unwrap().is_none());
    }

    #[test]
    fn trusted_config_selects_default_and_feature_harnesses() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1
default_harness = "pi"

[harnesses.pi]
command = ["pi", "--no-tools", "-p"]

[harnesses.careful]
command = ["claude", "--print"]
timeout_seconds = 1200

[diff]
presentation = "important"
harness = "careful"
"#,
        )
        .unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert_eq!(config.diff.presentation, DiffPresentation::Important);
        assert_eq!(config.selected_harness(None).unwrap().unwrap().0, "careful");
        assert_eq!(
            config.selected_harness(Some("pi")).unwrap().unwrap().0,
            "pi"
        );
    }

    #[test]
    fn config_rejects_shell_strings_and_undefined_harnesses() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1
default_harness = "missing"
"#,
        )
        .unwrap();
        let error = AgentLabConfig::load(&store).unwrap_err().to_string();
        assert!(error.contains("undefined AgentLab harness"), "{error}");
    }
}
