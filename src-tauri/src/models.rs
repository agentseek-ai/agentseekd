//! Data models shared across the AgentSeek Desktop backend.
//!
//! This module contains all serializable structs, runtime requirement
//! definitions, and the utility functions that parse version strings.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Runtime requirements manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeRequirements {
    pub(crate) schema_version: u32,
    pub(crate) versions: RuntimeVersions,
    pub(crate) sources: RuntimeSources,
}

#[derive(Clone, Deserialize)]
pub(crate) struct RuntimeVersions {
    pub(crate) uv: DependencyVersion,
    pub(crate) node: DependencyVersion,
    pub(crate) npm: DependencyVersion,
    pub(crate) git: DependencyVersion,
    pub(crate) agentseek: DependencyVersion,
    pub(crate) nvm: DependencyVersion,
    #[serde(default)]
    pub(crate) pyseekdb: DependencyVersion,
}

#[derive(Clone, Deserialize, Default)]
pub(crate) struct DependencyVersion {
    #[serde(default)]
    pub(crate) minimum: String,
    #[serde(default)]
    pub(crate) managed: String,
    #[serde(default)]
    pub(crate) pinned: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeSources {
    pub(crate) uv_installer: String,
    #[serde(default)]
    pub(crate) uv_installer_windows: Option<String>,
    pub(crate) nvm_installer_template: String,
    pub(crate) node_distribution: String,
    pub(crate) agentseek_package_metadata: String,
}

/// Load runtime requirements from the bundled JSON manifest or an override file.
pub(crate) fn load_runtime_requirements(
    default_content: &str,
) -> Result<RuntimeRequirements, String> {
    let content = std::env::var_os("AGENTSEEK_DESKTOP_REQUIREMENTS_FILE")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|error| format!("Failed to read runtime requirements manifest: {error}"))?
        .unwrap_or_else(|| default_content.to_string());
    let requirements: RuntimeRequirements = serde_json::from_str(&content)
        .map_err(|error| format!("Runtime requirements manifest format error: {error}"))?;
    if requirements.schema_version != 1 {
        return Err(format!(
            "Unsupported runtime requirements manifest version: {}",
            requirements.schema_version
        ));
    }
    validate_runtime_requirements(&requirements)?;
    Ok(requirements)
}

pub(crate) fn validate_runtime_requirements(requirements: &RuntimeRequirements) -> Result<(), String> {
    for (field, value) in [
        ("versions.uv.minimum", &requirements.versions.uv.minimum),
        ("versions.node.minimum", &requirements.versions.node.minimum),
        ("versions.node.managed", &requirements.versions.node.managed),
        ("versions.npm.minimum", &requirements.versions.npm.minimum),
        ("versions.npm.managed", &requirements.versions.npm.managed),
        ("versions.git.minimum", &requirements.versions.git.minimum),
        (
            "versions.agentseek.minimum",
            &requirements.versions.agentseek.minimum,
        ),
        ("versions.nvm.managed", &requirements.versions.nvm.managed),
    ] {
        if numeric_version(value).is_empty() {
            return Err(format!(
                "Runtime requirements manifest field {field} is not a valid version number"
            ));
        }
    }
    // versions.pyseekdb.pinned is optional; when present it must be a valid version number.
    if !requirements.versions.pyseekdb.pinned.is_empty()
        && numeric_version(&requirements.versions.pyseekdb.pinned).is_empty()
    {
        return Err(
            "Runtime requirements manifest field versions.pyseekdb.pinned is not a valid version number"
                .to_string(),
        );
    }
    if !requirements
        .sources
        .nvm_installer_template
        .contains("{version}")
    {
        return Err(
            "Runtime requirements manifest sources.nvmInstallerTemplate must contain {version}"
                .to_string(),
        );
    }
    for (field, value) in [
        ("sources.uvInstaller", &requirements.sources.uv_installer),
        (
            "sources.nodeDistribution",
            &requirements.sources.node_distribution,
        ),
        (
            "sources.agentseekPackageMetadata",
            &requirements.sources.agentseek_package_metadata,
        ),
    ] {
        if !value.starts_with("https://") {
            return Err(format!(
                "Runtime requirements manifest field {field} must use HTTPS URL"
            ));
        }
    }
    if requirements
        .sources
        .uv_installer_windows
        .as_ref()
        .is_some_and(|value| !value.starts_with("https://"))
    {
        return Err(
            "Runtime requirements manifest field sources.uvInstallerWindows must use HTTPS URL"
                .to_string(),
        );
    }
    Ok(())
}

/// Extract the numeric components from a version string (e.g. "v1.2.3" -> [1, 2, 3]).
pub(crate) fn numeric_version(value: &str) -> Vec<u64> {
    let start = value.find(|character: char| character.is_ascii_digit());
    let Some(start) = start else {
        return Vec::new();
    };
    value[start..]
        .split(|character: char| !character.is_ascii_digit())
        .take_while(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Domain models
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) framework: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateUpdateCheck {
    pub(crate) current_version: String,
    pub(crate) latest_version: String,
    pub(crate) has_update: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstanceRecord {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) template_id: String,
    pub(crate) status: String,
    pub(crate) deployment_mode: String,
    pub(crate) work_dir: String,
    pub(crate) env_example_path: Option<String>,
    pub(crate) env_path: Option<String>,
    pub(crate) note: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) needs_doctor: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) agent_url: Option<String>,
    pub(crate) ui_url: Option<String>,
    pub(crate) studio_url: Option<String>,
    #[serde(default)]
    pub(crate) project_name: Option<String>,
    #[serde(default)]
    pub(crate) lifecycle_version: Option<u32>,
    #[serde(default)]
    pub(crate) service_endpoints: Vec<ServiceEndpoint>,
}

#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceEndpoint {
    pub(crate) name: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) primary: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvVariable {
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) comment: String,
    pub(crate) source: String,
    pub(crate) modified: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogEntry {
    pub(crate) id: String,
    pub(crate) instance_id: Option<String>,
    pub(crate) instance_name: String,
    pub(crate) category: String,
    pub(crate) level: String,
    pub(crate) message: String,
    pub(crate) command: Option<String>,
    pub(crate) created_at: u64,
    #[serde(default)]
    pub(crate) sequence: u64,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppStore {
    pub(crate) instances: Vec<InstanceRecord>,
    pub(crate) vault: Vec<EnvVariable>,
    pub(crate) logs: Vec<LogEntry>,
}

// ---------------------------------------------------------------------------
// Storage configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageConfig {
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) host: String,
    #[serde(default = "default_database_port")]
    pub(crate) port: u16,
    #[serde(default)]
    pub(crate) tenant: String,
    #[serde(default = "default_storage_database")]
    pub(crate) database: String,
    #[serde(default = "default_storage_user")]
    pub(crate) user: String,
    #[serde(default)]
    pub(crate) password: String,
    #[serde(default = "default_runtime_log_retention_days")]
    pub(crate) runtime_log_retention_days: u32,
    #[serde(default)]
    pub(crate) setup_completed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateConfig {
    /// Git repository URL (defaults to the official agentseek-templates repo, never empty).
    pub(crate) repo_url: String,
    /// Tag, branch, or full 40-char commit SHA. Empty means auto-detect (latest release → "main").
    #[serde(default)]
    pub(crate) checkout: String,
    /// Optional custom catalog JSON URL. Empty means use the repo's built-in catalog.
    #[serde(default)]
    pub(crate) catalog_url: String,
}

impl Default for TemplateConfig {
    fn default() -> Self {
        Self {
            repo_url: "https://github.com/agentseek-ai/agentseek-templates.git".to_string(),
            checkout: String::new(),
            catalog_url: String::new(),
        }
    }
}

pub(crate) fn default_database_port() -> u16 {
    2881
}

pub(crate) fn default_storage_database() -> String {
    "agentseek_desktop".to_string()
}

pub(crate) fn default_storage_user() -> String {
    "root".to_string()
}

pub(crate) fn default_runtime_log_retention_days() -> u32 {
    super::DEFAULT_RUNTIME_LOG_RETENTION_DAYS
}

// ---------------------------------------------------------------------------
// China-region mirror URLs (centralized for maintainability)
// ---------------------------------------------------------------------------

/// npm registry mirror (npmmirror).
pub(crate) const NPM_REGISTRY_MIRROR: &str = "https://registry.npmmirror.com";
/// Node.js binary mirror (for NVM).
pub(crate) const NVM_NODEJS_MIRROR: &str = "https://cdn.npmmirror.com/binaries/node";
/// NVM install script mirror (Gitee).
pub(crate) const NVM_INSTALL_MIRROR: &str = "https://gitee.com/mirrors/nvm";
/// apt (Debian) mirror.
pub(crate) const APT_MIRROR: &str = "mirrors.aliyun.com";
/// PyPI mirror mirror.
pub(crate) const PYPI_MIRROR: &str = "https://mirrors.aliyun.com/pypi/simple/";
/// HuggingFace mirror.
pub(crate) const HF_MIRROR: &str = "https://hf-mirror.com";

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            mode: if cfg!(windows) {
                "sqlite_embedded".to_string()
            } else {
                "seekdb_embedded".to_string()
            },
            path: String::new(),
            host: String::new(),
            port: default_database_port(),
            tenant: String::new(),
            database: default_storage_database(),
            user: default_storage_user(),
            password: String::new(),
            runtime_log_retention_days: default_runtime_log_retention_days(),
            setup_completed: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalCredentials {
    #[serde(default)]
    pub(crate) storage_password: String,
}

// ---------------------------------------------------------------------------
// Command input/output structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareInstanceInput {
    pub(crate) name: String,
    pub(crate) template_id: String,
    pub(crate) target_dir: String,
    pub(crate) note: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareInstanceResult {
    pub(crate) instance: InstanceRecord,
    pub(crate) env: Vec<EnvVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) docker_warning: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveEnvInput {
    pub(crate) instance_id: String,
    pub(crate) entries: Vec<EnvVariable>,
    #[serde(default)]
    pub(crate) overwrite: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveEnvResult {
    pub(crate) path: String,
    pub(crate) key_count: usize,
    pub(crate) synced_count: usize,
    pub(crate) port_changes: Vec<PortChange>,
    pub(crate) entries: Vec<EnvVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) docker_warning: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportEnvInput {
    pub(crate) source_path: String,
    pub(crate) output_path: String,
    #[serde(default)]
    pub(crate) overwrite: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportEnvResult {
    pub(crate) path: String,
    pub(crate) key_count: usize,
    pub(crate) filled_count: usize,
    pub(crate) missing_count: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortChange {
    pub(crate) key: String,
    pub(crate) old_port: u16,
    pub(crate) new_port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SystemInfo {
    pub(crate) app_name: String,
    pub(crate) version: String,
    pub(crate) data_path: String,
    pub(crate) cli_strategy: String,
    pub(crate) storage: String,
    pub(crate) docker_available: bool,
    pub(crate) docker_compose_available: bool,
    pub(crate) docker_running: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct DockerStatus {
    pub(crate) cli_available: bool,
    pub(crate) compose_v2_available: bool,
    pub(crate) daemon_running: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageStatus {
    pub(crate) mode: String,
    pub(crate) effective_mode: String,
    pub(crate) path: String,
    pub(crate) default_sqlite_path: String,
    pub(crate) default_seekdb_path: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tenant: String,
    pub(crate) database: String,
    pub(crate) default_database: String,
    pub(crate) user: String,
    pub(crate) password_configured: bool,
    pub(crate) runtime_log_retention_days: u32,
    pub(crate) setup_required: bool,
    pub(crate) writable: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogSettings {
    pub(crate) runtime_retention_days: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogQuery {
    #[serde(default)]
    pub(crate) before_sequence: Option<u64>,
    #[serde(default)]
    pub(crate) after_sequence: Option<u64>,
    #[serde(default = "default_log_page_size")]
    pub(crate) limit: usize,
}

fn default_log_page_size() -> usize {
    500
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogPage {
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) has_more: bool,
    pub(crate) group_count: usize,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliStatus {
    pub(crate) platform: String,
    pub(crate) dependency_commands: HashMap<String, String>,
    pub(crate) minimum_versions: HashMap<String, String>,
    pub(crate) node_managed: bool,
    pub(crate) uv_available: bool,
    pub(crate) uv_path: String,
    pub(crate) cli_available: bool,
    pub(crate) cli_compatible: bool,
    pub(crate) cli_update_available: bool,
    pub(crate) cli_latest_version: String,
    pub(crate) cli_latest_version_checked: bool,
    pub(crate) uv_version: String,
    pub(crate) cli_version: String,
    pub(crate) node_available: bool,
    pub(crate) node_compatible: bool,
    pub(crate) node_version: String,
    pub(crate) npm_available: bool,
    pub(crate) npm_compatible: bool,
    pub(crate) npm_version: String,
    pub(crate) git_available: bool,
    pub(crate) git_compatible: bool,
    pub(crate) git_version: String,
    pub(crate) uv_compatible: bool,
    pub(crate) prerequisites_ready: bool,
    pub(crate) install_command: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeInstallPlan {
    pub(crate) task_id: String,
    pub(crate) script: String,
    pub(crate) script_path: String,
    pub(crate) install_dir: String,
    pub(crate) dependencies: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeInstallProgress {
    pub(crate) status: String,
    pub(crate) stage: String,
    pub(crate) log: String,
}

pub(crate) struct CommandResult {
    pub(crate) code: i32,
    pub(crate) output: String,
    pub(crate) command: String,
}

// ---------------------------------------------------------------------------
// Lifecycle manifest
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub(crate) struct LifecycleManifest {
    #[serde(default)]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) services: HashMap<String, LifecycleServiceSpec>,
}

#[derive(Deserialize, Default)]
pub(crate) struct LifecycleServiceSpec {
    #[serde(default)]
    pub(crate) url: String,
}

/// Truncate a string to at most `max` characters (char-boundary safe).
/// Appends "..." when the string was cut; used for trace input/output summaries.
pub(crate) fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut truncated: String = value.chars().take(max.saturating_sub(3)).collect();
        truncated.push_str("...");
        truncated
    }
}

// ---------------------------------------------------------------------------
// Trace models (ATOF)
// ---------------------------------------------------------------------------

/// Lightweight trace row for the list page.
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TraceSummary {
    pub trace_id: String,
    pub status: String,
    pub kind: String,
    pub name: String,
    pub input_summary: Option<String>,
    pub output_summary: Option<String>,
    pub start_time: Option<String>,
    pub latency_ms: Option<u64>,
    pub span_count: usize,
}

/// Full trace with span tree for the detail page.
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TraceDetail {
    pub trace_id: String,
    pub status: String,
    pub latency_ms: Option<u64>,
    pub start_time: Option<String>,
    pub spans: Vec<SpanNode>,
}

/// One node in the span execution tree.
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpanNode {
    pub span_id: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub duration_ms: Option<u64>,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub attributes: Option<serde_json::Value>,
    pub children: Vec<SpanNode>,
}

/// Paginated trace list result.
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TracePage {
    pub entries: Vec<TraceSummary>,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
}
