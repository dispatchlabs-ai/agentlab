use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::store::Store;

pub const CONFIG_VERSION: u32 = 1;
const DEFAULT_HARNESS_TIMEOUT_SECONDS: u64 = 600;
const MAX_HARNESS_TIMEOUT_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentLabConfig {
    pub version: u32,
    #[serde(default)]
    pub default_backend: Option<String>,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
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
            default_backend: None,
            backends: BTreeMap::new(),
            default_harness: None,
            harnesses: BTreeMap::new(),
            diff: DiffConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackendDriver {
    Docker,
    E2b,
}

impl BackendDriver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::E2b => "e2b",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub driver: BackendDriver,
    #[serde(default)]
    pub docker_context: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub ssh_alias: Option<String>,
    #[serde(default)]
    pub sdk_directory: Option<String>,
    #[serde(default)]
    pub orchestrator_directory: Option<String>,
    #[serde(default)]
    pub remote_root: Option<String>,
    #[serde(default)]
    pub expected_isolation: Option<String>,
    #[serde(default)]
    pub templates: BTreeMap<String, String>,
    #[serde(default)]
    pub template_builds: BTreeMap<String, String>,
    #[serde(default)]
    pub runtime_environments: BTreeMap<String, BTreeMap<String, String>>,
}

impl BackendConfig {
    fn local_docker() -> Self {
        Self {
            driver: BackendDriver::Docker,
            docker_context: Some("default".to_owned()),
            transport: None,
            ssh_alias: None,
            sdk_directory: None,
            orchestrator_directory: None,
            remote_root: None,
            expected_isolation: None,
            templates: BTreeMap::new(),
            template_builds: BTreeMap::new(),
            runtime_environments: BTreeMap::new(),
        }
    }

    pub fn e2b_template(&self, image: &str) -> Result<&str> {
        self.templates
            .get(image)
            .map(String::as_str)
            .with_context(|| {
                let configured = if self.templates.is_empty() {
                    "none".to_owned()
                } else {
                    self.templates
                        .keys()
                        .map(|name| format!("{name:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!(
                    "E2B backend has no immutable template mapping for image {image:?}; configured images: {configured}"
                )
            })
    }

    pub fn expected_template_build(&self, image: &str) -> Option<&str> {
        self.template_builds.get(image).map(String::as_str)
    }

    pub fn runtime_environment(&self, image: &str) -> BTreeMap<String, String> {
        self.runtime_environments
            .get(image)
            .cloned()
            .unwrap_or_default()
    }

    pub fn ssh_alias(&self) -> Result<&str> {
        self.ssh_alias
            .as_deref()
            .context("E2B backend omitted ssh_alias")
    }

    pub fn sdk_directory(&self) -> Result<&str> {
        self.sdk_directory
            .as_deref()
            .context("E2B backend omitted sdk_directory")
    }

    pub fn orchestrator_directory(&self) -> Result<&str> {
        self.orchestrator_directory
            .as_deref()
            .context("E2B backend omitted orchestrator_directory")
    }

    pub fn remote_root(&self) -> Result<&str> {
        self.remote_root
            .as_deref()
            .context("E2B backend omitted remote_root")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedBackend {
    pub name: String,
    pub config: BackendConfig,
}

impl SelectedBackend {
    pub fn local() -> Self {
        Self {
            name: "local".to_owned(),
            config: BackendConfig::local_docker(),
        }
    }

    pub fn driver(&self) -> BackendDriver {
        self.config.driver
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
    pub use_agent: bool,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "enabled")]
    pub show_omitted_count: bool,
    // Accepted only so configurations written by the development build that
    // immediately preceded this interface continue to load. New
    // configurations use `use_agent`.
    #[serde(default, rename = "presentation")]
    legacy_presentation: Option<LegacyDiffPresentation>,
    #[serde(default, rename = "fallback")]
    legacy_fallback: Option<LegacyDiffFallback>,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            use_agent: false,
            harness: None,
            ignore: Vec::new(),
            show_omitted_count: true,
            legacy_presentation: None,
            legacy_fallback: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacyDiffPresentation {
    Complete,
    Important,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacyDiffFallback {
    Complete,
}

impl DiffConfig {
    pub fn use_agent(&self) -> bool {
        match self.legacy_presentation {
            Some(LegacyDiffPresentation::Complete) => false,
            Some(LegacyDiffPresentation::Important) => true,
            None => self.use_agent,
        }
    }
}

impl AgentLabConfig {
    pub fn load(store: &Store) -> Result<Self> {
        let path = config_path(store);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open AgentLab config {}", path.display()));
            }
        };
        let mut bytes = Vec::new();
        file.take(crate::process::MAX_IGNORE_RULE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read AgentLab config {}", path.display()))?;
        if bytes.len() > crate::process::MAX_IGNORE_RULE_BYTES {
            bail!(
                "AgentLab config {} exceeds the {} byte limit",
                path.display(),
                crate::process::MAX_IGNORE_RULE_BYTES
            );
        }
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

    pub fn selected_backend(&self, override_name: Option<&str>) -> Result<SelectedBackend> {
        let name = override_name
            .or(self.default_backend.as_deref())
            .unwrap_or("local");
        validate_profile_name(name, "backend")?;
        match self.backends.get(name) {
            Some(config) => Ok(SelectedBackend {
                name: name.to_owned(),
                config: config.clone(),
            }),
            None if name == "local" => Ok(SelectedBackend::local()),
            None => {
                let configured = if self.backends.is_empty() {
                    "none".to_owned()
                } else {
                    self.backends
                        .keys()
                        .map(|name| format!("{name:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                bail!(
                    "AgentLab backend {name:?} is not defined in {}; configured backends: {configured}",
                    "~/.agentlab/config.toml"
                )
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported AgentLab config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        for (name, backend) in &self.backends {
            validate_profile_name(name, "backend")?;
            validate_backend(name, backend)?;
        }
        if let Some(name) = self.default_backend.as_deref() {
            validate_profile_name(name, "backend")?;
            if name != "local" && !self.backends.contains_key(name) {
                bail!("default_backend refers to undefined AgentLab backend {name:?}");
            }
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
        for pattern in &self.diff.ignore {
            if pattern.contains(['\0', '\n', '\r']) {
                bail!(
                    "diff.ignore pattern {pattern:?} contains a NUL byte or newline; use one Git-compatible pattern per TOML string"
                );
            }
        }
        // Deserializing the sole legacy fallback value is sufficient
        // validation; touching it here also makes the compatibility field an
        // explicit part of config validation rather than inert data.
        let _legacy_fallback = self.diff.legacy_fallback;
        Ok(())
    }
}

pub fn config_path(store: &Store) -> PathBuf {
    store.root().join("config.toml")
}

fn validate_harness_name(name: &str) -> Result<()> {
    validate_profile_name(name, "harness")
}

fn validate_profile_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "invalid AgentLab {kind} name {name:?}; use letters, numbers, dot, underscore, or hyphen"
        );
    }
    Ok(())
}

fn validate_backend(name: &str, backend: &BackendConfig) -> Result<()> {
    if name == "local" && backend.driver != BackendDriver::Docker {
        bail!(
            "AgentLab backend name \"local\" is reserved for the built-in local Docker driver; use another profile name for E2B"
        );
    }
    match backend.driver {
        BackendDriver::Docker => {
            let context = backend.docker_context.as_deref().unwrap_or("default");
            if context != "default" {
                bail!(
                    "AgentLab Docker backend {name:?} requests context {context:?}; this release supports only the default Docker context"
                );
            }
            if backend.transport.is_some()
                || backend.ssh_alias.is_some()
                || backend.sdk_directory.is_some()
                || backend.orchestrator_directory.is_some()
                || backend.remote_root.is_some()
                || backend.expected_isolation.is_some()
                || !backend.templates.is_empty()
                || !backend.template_builds.is_empty()
                || !backend.runtime_environments.is_empty()
            {
                bail!("AgentLab Docker backend {name:?} contains E2B-only configuration fields");
            }
        }
        BackendDriver::E2b => {
            if backend.docker_context.is_some() {
                bail!("AgentLab E2B backend {name:?} cannot set docker_context");
            }
            if backend.transport.as_deref() != Some("ssh") {
                bail!("AgentLab E2B backend {name:?} requires transport = \"ssh\" in this release");
            }
            if backend.expected_isolation.as_deref() != Some("firecracker") {
                bail!(
                    "AgentLab E2B backend {name:?} requires expected_isolation = \"firecracker\""
                );
            }
            let alias = backend
                .ssh_alias
                .as_deref()
                .with_context(|| format!("AgentLab E2B backend {name:?} requires ssh_alias"))?;
            validate_safe_atom(alias, "ssh_alias", name)?;
            for (field, value) in [
                ("sdk_directory", backend.sdk_directory.as_deref()),
                (
                    "orchestrator_directory",
                    backend.orchestrator_directory.as_deref(),
                ),
                ("remote_root", backend.remote_root.as_deref()),
            ] {
                let value = value
                    .with_context(|| format!("AgentLab E2B backend {name:?} requires {field}"))?;
                if !value.starts_with('/') {
                    bail!("AgentLab E2B backend {name:?} {field} must be an absolute path");
                }
                validate_safe_path(value, field, name)?;
            }
            if backend.templates.is_empty() {
                bail!(
                    "AgentLab E2B backend {name:?} requires at least one immutable image-to-template mapping in templates"
                );
            }
            for (image, template) in &backend.templates {
                if image.is_empty() || image.contains(['\0', '\n', '\r']) {
                    bail!("AgentLab E2B backend {name:?} has an invalid image mapping key");
                }
                validate_safe_template(template, name)?;
                if !template.contains(':') {
                    bail!(
                        "AgentLab E2B backend {name:?} template {template:?} must include an immutable tag"
                    );
                }
                if !backend.template_builds.contains_key(image) {
                    bail!(
                        "AgentLab E2B backend {name:?} template {image:?} requires a template_builds UUID pin"
                    );
                }
            }
            for (image, build) in &backend.template_builds {
                if !backend.templates.contains_key(image) {
                    bail!(
                        "AgentLab E2B backend {name:?} template_builds contains {image:?} without a templates mapping"
                    );
                }
                uuid::Uuid::parse_str(build).with_context(|| {
                    format!(
                        "AgentLab E2B backend {name:?} template build for {image:?} is not a UUID"
                    )
                })?;
            }
            for (image, environment) in &backend.runtime_environments {
                if !backend.templates.contains_key(image) {
                    bail!(
                        "AgentLab E2B backend {name:?} runtime_environments contains {image:?} without a templates mapping"
                    );
                }
                if environment.len() > 128 {
                    bail!(
                        "AgentLab E2B backend {name:?} runtime environment for {image:?} exceeds 128 entries"
                    );
                }
                let mut total_bytes = 0usize;
                for (variable, value) in environment {
                    if variable.is_empty()
                        || !variable.bytes().enumerate().all(|(index, byte)| {
                            if index == 0 {
                                byte.is_ascii_alphabetic() || byte == b'_'
                            } else {
                                byte.is_ascii_alphanumeric() || byte == b'_'
                            }
                        })
                    {
                        bail!(
                            "AgentLab E2B backend {name:?} has invalid runtime environment variable {variable:?} for {image:?}"
                        );
                    }
                    if value.contains('\0') {
                        bail!(
                            "AgentLab E2B backend {name:?} runtime environment variable {variable:?} contains a NUL byte"
                        );
                    }
                    total_bytes = total_bytes
                        .checked_add(variable.len() + value.len())
                        .context("runtime environment size overflow")?;
                }
                if total_bytes > 256 * 1024 {
                    bail!(
                        "AgentLab E2B backend {name:?} runtime environment for {image:?} exceeds 256 KiB"
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_safe_atom(value: &str, field: &str, backend: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "AgentLab E2B backend {backend:?} has unsafe {field} {value:?}; use letters, numbers, dot, underscore, or hyphen"
        );
    }
    Ok(())
}

fn validate_safe_path(value: &str, field: &str, backend: &str) -> Result<()> {
    let components = value.strip_prefix('/').unwrap_or(value).split('/');
    if value == "/"
        || value.ends_with('/')
        || components
            .clone()
            .any(|component| component.is_empty() || component == "." || component == "..")
        || value.contains(['\0', '\n', '\r'])
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        bail!(
            "AgentLab E2B backend {backend:?} has unsafe {field} path {value:?}; spaces and shell metacharacters are not supported"
        );
    }
    Ok(())
}

fn validate_safe_template(value: &str, backend: &str) -> Result<()> {
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':')
        })
    {
        bail!("AgentLab E2B backend {backend:?} has unsafe template reference {value:?}");
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
    fn missing_config_uses_deterministic_diff_without_an_agent() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert!(!config.diff.use_agent());
        assert!(config.diff.ignore.is_empty());
        assert!(config.selected_harness(None).unwrap().is_none());
        assert_eq!(
            config.selected_backend(None).unwrap(),
            SelectedBackend::local()
        );
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
use_agent = true
harness = "careful"
ignore = ["/tmp/cache/**", "/workspace/*.lock"]
"#,
        )
        .unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert!(config.diff.use_agent());
        assert_eq!(config.diff.ignore, ["/tmp/cache/**", "/workspace/*.lock"]);
        assert_eq!(config.selected_harness(None).unwrap().unwrap().0, "careful");
        assert_eq!(
            config.selected_harness(Some("pi")).unwrap().unwrap().0,
            "pi"
        );
    }

    #[test]
    fn immediately_preceding_diff_config_remains_readable() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1

[diff]
presentation = "important"
fallback = "complete"
"#,
        )
        .unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert!(config.diff.use_agent());
    }

    #[test]
    fn config_rejects_multiline_ignore_patterns() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            "version = 1\n[diff]\nignore = [\"first\\nsecond\"]\n",
        )
        .unwrap();
        let error = AgentLabConfig::load(&store).unwrap_err().to_string();
        assert!(error.contains("one Git-compatible pattern"), "{error}");
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

    #[test]
    fn explicit_e2b_profile_selects_only_its_declared_driver_and_template() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1
default_backend = "local"

[backends.local]
driver = "docker"
docker_context = "default"

[backends.e2b-dell]
driver = "e2b"
transport = "ssh"
ssh_alias = "e2b-dell"
sdk_directory = "/home/chris/src/e2b-infra/packages/shared/scripts"
orchestrator_directory = "/home/chris/src/e2b-infra/packages/orchestrator"
remote_root = "/home/chris/.agentlab-e2b"
expected_isolation = "firecracker"
templates = { "agentlab-daily-log:dev" = "agentlab-daily-log:sha256-deadbeef" }
template_builds = { "agentlab-daily-log:dev" = "57d534fa-69f4-4dcd-a6c4-b60994c21dc1" }
runtime_environments = { "agentlab-daily-log:dev" = { PI_CODING_AGENT_SESSION_DIR = "/workspace/.pi/sessions", PI_SESSION_LOCK_PROTOCOL = "kernel-v1-drained" } }
"#,
        )
        .unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert_eq!(
            config.selected_backend(None).unwrap().driver(),
            BackendDriver::Docker
        );
        let selected = config.selected_backend(Some("e2b-dell")).unwrap();
        assert_eq!(selected.driver(), BackendDriver::E2b);
        assert_eq!(
            selected
                .config
                .e2b_template("agentlab-daily-log:dev")
                .unwrap(),
            "agentlab-daily-log:sha256-deadbeef"
        );
        assert_eq!(
            selected
                .config
                .runtime_environment("agentlab-daily-log:dev")
                .get("PI_SESSION_LOCK_PROTOCOL")
                .map(String::as_str),
            Some("kernel-v1-drained")
        );
    }

    #[test]
    fn profile_names_do_not_infer_a_backend_driver() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1

[backends.e2b-dell]
driver = "docker"
docker_context = "default"
"#,
        )
        .unwrap();
        let config = AgentLabConfig::load(&store).unwrap();
        assert_eq!(
            config.selected_backend(Some("e2b-dell")).unwrap().driver(),
            BackendDriver::Docker
        );
    }

    #[test]
    fn e2b_profiles_reject_ambiguous_or_mutable_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1

[backends.remote]
driver = "e2b"
transport = "ssh"
ssh_alias = "e2b-dell;touch-bad"
sdk_directory = "/tmp/sdk"
orchestrator_directory = "/tmp/orchestrator"
remote_root = "/tmp/agentlab"
expected_isolation = "firecracker"
templates = { "image" = "mutable-template" }
"#,
        )
        .unwrap();
        let error = AgentLabConfig::load(&store).unwrap_err().to_string();
        assert!(error.contains("unsafe ssh_alias"), "{error}");
    }

    #[test]
    fn e2b_profiles_require_build_pins_for_every_template() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1

[backends.remote]
driver = "e2b"
transport = "ssh"
ssh_alias = "e2b-dell"
sdk_directory = "/tmp/sdk"
orchestrator_directory = "/tmp/orchestrator"
remote_root = "/tmp/agentlab"
expected_isolation = "firecracker"
templates = { "image" = "immutable-template:sha256-deadbeef" }
"#,
        )
        .unwrap();
        let error = AgentLabConfig::load(&store).unwrap_err().to_string();
        assert!(
            error.contains("requires a template_builds UUID pin"),
            "{error}"
        );
    }

    #[test]
    fn e2b_profiles_reject_root_noncanonical_paths_and_option_like_aliases() {
        assert!(validate_safe_path("/", "remote_root", "remote").is_err());
        assert!(validate_safe_path("/tmp/", "remote_root", "remote").is_err());
        assert!(validate_safe_path("/tmp//agentlab", "remote_root", "remote").is_err());
        assert!(validate_safe_path("/tmp/./agentlab", "remote_root", "remote").is_err());
        assert!(validate_safe_path("/tmp/../agentlab", "remote_root", "remote").is_err());
        assert!(validate_safe_atom("-oProxyCommand=bad", "ssh_alias", "remote").is_err());
        assert!(validate_safe_path("/home/chris/.agentlab-e2b", "remote_root", "remote").is_ok());
    }

    #[test]
    fn the_local_profile_cannot_silently_change_backend_drivers() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(Some(temporary.path())).unwrap();
        fs::write(
            config_path(&store),
            r#"
version = 1

[backends.local]
driver = "e2b"
transport = "ssh"
ssh_alias = "e2b-dell"
sdk_directory = "/tmp/sdk"
orchestrator_directory = "/tmp/orchestrator"
remote_root = "/tmp/agentlab"
expected_isolation = "firecracker"
templates = { "image" = "template:immutable" }
template_builds = { "image" = "00000000-0000-4000-8000-000000000001" }
"#,
        )
        .unwrap();
        let error = AgentLabConfig::load(&store).unwrap_err().to_string();
        assert!(
            error.contains("reserved for the built-in local Docker"),
            "{error}"
        );
    }
}
