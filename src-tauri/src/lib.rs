use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{Manager, State};

const DEFAULT_RUNTIME_REQUIREMENTS: &str = include_str!("../../src/runtime-requirements.json");
const SEEKDB_STORAGE_HELPER: &str = include_str!("seekdb_storage.py");
const MAX_LOG_ENTRIES: usize = 100_000;
const DEFAULT_RUNTIME_LOG_RETENTION_DAYS: u32 = 7;
const DELETED_INSTANCE_LOG_RETENTION_DAYS: u32 = 7;
const SECONDS_PER_DAY: u64 = 86_400;
const LOG_CLEANUP_BATCH_SIZE: usize = 1_000;
const MAX_PENDING_LOG_ENTRIES: usize = 1_000;
const MAX_LOG_TEXT_BYTES: usize = 64 * 1024;
const RUNTIME_LOG_SPOOL_DIRECTORY: &str = "runtime-logs";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeRequirements {
    schema_version: u32,
    versions: RuntimeVersions,
    sources: RuntimeSources,
}

#[derive(Clone, Deserialize)]
struct RuntimeVersions {
    uv: DependencyVersion,
    node: DependencyVersion,
    npm: DependencyVersion,
    git: DependencyVersion,
    agentseek: DependencyVersion,
    nvm: DependencyVersion,
}

#[derive(Clone, Deserialize)]
struct DependencyVersion {
    #[serde(default)]
    minimum: String,
    #[serde(default)]
    managed: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSources {
    uv_installer: String,
    #[serde(default)]
    uv_installer_windows: Option<String>,
    nvm_installer_template: String,
    node_distribution: String,
    agentseek_package_metadata: String,
}

fn load_runtime_requirements() -> Result<RuntimeRequirements, String> {
    let content = env::var_os("AGENTSEEK_DESKTOP_REQUIREMENTS_FILE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .map(fs::read_to_string)
        .transpose()
        .map_err(|error| format!("Failed to read runtime requirements manifest: {error}"))?
        .unwrap_or_else(|| DEFAULT_RUNTIME_REQUIREMENTS.to_string());
    let requirements: RuntimeRequirements =
        serde_json::from_str(&content).map_err(|error| format!("Runtime requirements manifest format error: {error}"))?;
    if requirements.schema_version != 1 {
        return Err(format!(
            "Unsupported runtime requirements manifest version: {}",
            requirements.schema_version
        ));
    }
    validate_runtime_requirements(&requirements)?;
    Ok(requirements)
}

fn validate_runtime_requirements(requirements: &RuntimeRequirements) -> Result<(), String> {
    let required_versions = [
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
    ];
    for (field, value) in required_versions {
        if numeric_version(value).is_empty() {
            return Err(format!("Runtime requirements manifest field {field} is not a valid version number"));
        }
    }
    if !requirements
        .sources
        .nvm_installer_template
        .contains("{version}")
    {
        return Err("Runtime requirements manifest sources.nvmInstallerTemplate must contain {version}".to_string());
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
            return Err(format!("Runtime requirements manifest field {field} must use HTTPS URL"));
        }
    }
    if requirements
        .sources
        .uv_installer_windows
        .as_ref()
        .is_some_and(|value| !value.starts_with("https://"))
    {
        return Err("Runtime requirements manifest field sources.uvInstallerWindows must use HTTPS URL".to_string());
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TemplateInfo {
    id: String,
    name: String,
    description: String,
    framework: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceRecord {
    id: String,
    name: String,
    template_id: String,
    status: String,
    deployment_mode: String,
    work_dir: String,
    env_example_path: Option<String>,
    env_path: Option<String>,
    note: String,
    created_at: u64,
    updated_at: u64,
    needs_doctor: bool,
    pid: Option<u32>,
    agent_url: Option<String>,
    ui_url: Option<String>,
    studio_url: Option<String>,
    #[serde(default)]
    project_name: Option<String>,
    #[serde(default)]
    lifecycle_version: Option<u32>,
    #[serde(default)]
    service_endpoints: Vec<ServiceEndpoint>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ServiceEndpoint {
    name: String,
    url: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    primary: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EnvVariable {
    key: String,
    value: String,
    comment: String,
    source: String,
    modified: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    id: String,
    instance_id: Option<String>,
    instance_name: String,
    category: String,
    level: String,
    message: String,
    command: Option<String>,
    created_at: u64,
    #[serde(default)]
    sequence: u64,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AppStore {
    instances: Vec<InstanceRecord>,
    vault: Vec<EnvVariable>,
    logs: Vec<LogEntry>,
}

fn is_deployment_completed_log(log: &LogEntry) -> bool {
    log.category == "install" && log.level == "success" && log.message == "Instance deployment completed"
}

fn repair_predeployment_restart_statuses(data: &mut AppStore) -> bool {
    let deployed = data
        .logs
        .iter()
        .filter(|log| is_deployment_completed_log(log))
        .filter_map(|log| log.instance_id.clone())
        .collect::<HashSet<_>>();
    let mut changed = false;
    for instance in &mut data.instances {
        if instance.status == "needs-restart"
            && instance.pid.is_none()
            && !deployed.contains(&instance.id)
        {
            instance.status = if instance.env_path.is_some() {
                "ready-to-install".to_string()
            } else {
                "configuring".to_string()
            };
            instance.needs_doctor = false;
            instance.updated_at = timestamp();
            changed = true;
        }
    }
    changed
}

fn is_desktop_lifecycle_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    message == "Instance stopped"
        || message.starts_with("Stopped instance process tree")
        || message.starts_with("Instance associated processes stopped")
        || message.starts_with("Doctor passed; instance restarted")
        || message.contains("Delete instance")
        || message.contains("Instance record deleted")
        || message.contains("Instance processes, working directory, and record deleted")
        || message.starts_with("Instance deletion completed")
        || lower == "instance stopped"
        || lower.contains("instance restarted")
        || lower.contains("delete instance")
        || (lower.contains("instance") && lower.contains("deleted"))
}

fn repair_lifecycle_log_categories(data: &mut AppStore) -> bool {
    let mut changed = false;
    for log in &mut data.logs {
        if log.category == "runtime" && is_desktop_lifecycle_message(&log.message) {
            log.category = "install".to_string();
            changed = true;
        }
    }
    changed
}

fn repair_log_sequences(data: &mut AppStore) -> bool {
    let mut changed = false;
    for (sequence, log) in data.logs.iter_mut().enumerate() {
        let sequence = sequence as u64;
        if log.sequence != sequence {
            log.sequence = sequence;
            changed = true;
        }
    }
    changed
}

fn instance_has_completed_deployment(
    state: &DesktopState,
    instance: &InstanceRecord,
) -> Result<bool, String> {
    if instance.pid.is_some() {
        return Ok(true);
    }
    state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .has_completed_deployment(&instance.id)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageConfig {
    mode: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    host: String,
    #[serde(default = "default_database_port")]
    port: u16,
    #[serde(default)]
    tenant: String,
    #[serde(default = "default_storage_database")]
    database: String,
    #[serde(default = "default_storage_user")]
    user: String,
    #[serde(default)]
    password: String,
    #[serde(default = "default_runtime_log_retention_days")]
    runtime_log_retention_days: u32,
    #[serde(default)]
    setup_completed: bool,
}

fn default_database_port() -> u16 {
    2881
}

fn default_storage_database() -> String {
    "agentseek_desktop".to_string()
}

fn default_storage_user() -> String {
    "root".to_string()
}

fn default_runtime_log_retention_days() -> u32 {
    DEFAULT_RUNTIME_LOG_RETENTION_DAYS
}

fn normalize_storage_database(config: &mut StorageConfig) {
    if matches!(config.mode.as_str(), "sqlite_embedded" | "seekdb_embedded")
        || config.database.trim().is_empty()
    {
        config.database = default_storage_database();
    }
}

fn sqlite_storage_directory(data_dir: &Path, config: &StorageConfig) -> PathBuf {
    if config.path.trim().is_empty() {
        data_dir.to_path_buf()
    } else {
        PathBuf::from(config.path.trim())
    }
}

fn sqlite_database_path(data_dir: &Path, config: &StorageConfig) -> PathBuf {
    sqlite_storage_directory(data_dir, config).join("agentseek-desktop.sqlite3")
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            mode: "seekdb_embedded".to_string(),
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
struct LocalCredentials {
    #[serde(default)]
    storage_password: String,
}

fn read_local_credentials(path: &Path) -> Result<LocalCredentials, String> {
    match fs::read_to_string(path) {
        Ok(value) => serde_json::from_str(&value)
            .map_err(|error| format!("Application private credentials file format error: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(LocalCredentials::default())
        }
        Err(error) => Err(format!("Failed to read application private credentials file: {error}")),
    }
}

fn write_local_credentials(path: &Path, credentials: &LocalCredentials) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Failed to write application private credentials file: {error}"))?;
    file.write_all(
        serde_json::to_string_pretty(credentials)
            .map_err(|error| error.to_string())?
            .as_bytes(),
    )
    .map_err(|error| format!("Failed to write application private credentials file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Failed to flush application private credentials file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to set application private credentials permissions: {error}"))?;
    }
    Ok(())
}

fn write_storage_config(path: &Path, config: &StorageConfig) -> Result<(), String> {
    let mut persisted = config.clone();
    persisted.password.clear();
    fs::write(
        path,
        serde_json::to_string_pretty(&persisted).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn sanitized_store(data: &AppStore) -> AppStore {
    let mut sanitized = data.clone();
    for entry in &mut sanitized.vault {
        entry.value.clear();
        entry.modified = false;
    }
    sanitized
}

fn write_storage_backup(data_dir: &Path, data: &AppStore) -> Result<(), String> {
    let backup_dir = data_dir.join("storage-backups");
    fs::create_dir_all(&backup_dir).map_err(|error| error.to_string())?;
    fs::write(
        backup_dir.join(format!("before-switch-{}.json", unique_stamp())),
        serde_json::to_string_pretty(data).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut backups = fs::read_dir(&backup_dir)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("before-switch-") && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    backups.sort();
    let remove_count = backups.len().saturating_sub(5);
    for backup in backups.into_iter().take(remove_count) {
        fs::remove_file(backup).map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct SeekDbBridge {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SeekDbBridge {
    fn open(config_path: &Path, data_dir: &Path) -> Result<Self, String> {
        let runtime = data_dir.join("runtime/seekdb-python");
        let python = if cfg!(windows) {
            runtime.join("Scripts/python.exe")
        } else {
            runtime.join("bin/python")
        };
        if !python.is_file() {
            return Err("SeekDB private runtime not yet installed".to_string());
        }
        let helper = data_dir.join("runtime/seekdb_storage.py");
        fs::write(&helper, SEEKDB_STORAGE_HELPER).map_err(|error| error.to_string())?;
        let mut child = Command::new(&python)
            .arg(&helper)
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Failed to start SeekDB storage runtime: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to connect SeekDB input stream".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to connect SeekDB output stream".to_string())?;
        let mut bridge = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let ready = bridge.read_response()?;
        if !ready
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ready
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("SeekDB initialization failed")
                .to_string());
        }
        Ok(bridge)
    }

    fn read_response(&mut self) -> Result<serde_json::Value, String> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            return Err("SeekDB storage runtime exited unexpectedly".to_string());
        }
        serde_json::from_str(&line).map_err(|error| format!("SeekDB response format error: {error}"))
    }

    fn request(&mut self, request: serde_json::Value) -> Result<serde_json::Value, String> {
        serde_json::to_writer(&mut self.stdin, &request).map_err(|error| error.to_string())?;
        self.stdin
            .write_all(b"\n")
            .map_err(|error| error.to_string())?;
        self.stdin.flush().map_err(|error| error.to_string())?;
        let response = self.read_response()?;
        if response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            Ok(response)
        } else {
            Err(response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("SeekDB operation failed")
                .to_string())
        }
    }
}

enum StorageEngine {
    Pending,
    Sqlite(PathBuf),
    SeekDb(SeekDbBridge),
}

fn storage_not_initialized() -> String {
    "Desktop storage has not been initialized".to_string()
}

fn open_sqlite(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn truncate_log_text(mut value: String) -> String {
    if value.len() <= MAX_LOG_TEXT_BYTES {
        return value;
    }
    let mut end = MAX_LOG_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n...[log content truncated]");
    value
}

fn is_secret_env_key(key: &str) -> bool {
    let normalized = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| {
            normalized == *marker
                || normalized.starts_with(&format!("{marker}_"))
                || normalized.ends_with(&format!("_{marker}"))
                || normalized.contains(&format!("_{marker}_"))
        })
}

fn prune_logs(data: &mut AppStore, runtime_retention_days: u32, now: u64) -> Vec<String> {
    let active_instances = data
        .instances
        .iter()
        .map(|instance| instance.id.as_str())
        .collect::<HashSet<_>>();
    let runtime_cutoff = now
        .saturating_sub(u64::from(runtime_retention_days.max(1)).saturating_mul(SECONDS_PER_DAY));
    let deleted_cutoff = now.saturating_sub(
        u64::from(DELETED_INSTANCE_LOG_RETENTION_DAYS).saturating_mul(SECONDS_PER_DAY),
    );
    let mut removed = Vec::new();
    data.logs.retain(|log| {
        let deleted_instance = log
            .instance_id
            .as_deref()
            .is_some_and(|instance_id| !active_instances.contains(instance_id));
        let keep = if deleted_instance && log.created_at < deleted_cutoff {
            false
        } else if log.category == "runtime" {
            log.created_at >= runtime_cutoff
        } else {
            true
        };
        if !keep {
            removed.push(log.id.clone());
        }
        keep
    });
    if data.logs.len() > MAX_LOG_ENTRIES {
        let remove = data.logs.len() - MAX_LOG_ENTRIES + LOG_CLEANUP_BATCH_SIZE;
        let removable = data
            .logs
            .iter()
            .filter(|log| {
                log.category == "runtime"
                    || log
                        .instance_id
                        .as_deref()
                        .is_some_and(|id| !active_instances.contains(id))
            })
            .take(remove)
            .map(|log| log.id.clone())
            .collect::<HashSet<_>>();
        data.logs.retain(|log| {
            if removable.contains(&log.id) {
                removed.push(log.id.clone());
                false
            } else {
                true
            }
        });
    }
    removed
}

fn initialize_sqlite_schema(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS instances (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                template_id TEXT NOT NULL,
                status TEXT NOT NULL,
                deployment_mode TEXT NOT NULL,
                work_dir TEXT NOT NULL,
                env_example_path TEXT,
                env_path TEXT,
                note TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                needs_doctor INTEGER NOT NULL,
                pid INTEGER,
                agent_url TEXT,
                ui_url TEXT,
                studio_url TEXT,
                project_name TEXT,
                lifecycle_version INTEGER,
                service_endpoints TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS env_vault (
                position INTEGER PRIMARY KEY,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                comment TEXT NOT NULL,
                source TEXT NOT NULL,
                modified INTEGER NOT NULL
            );
            DELETE FROM env_vault
             WHERE position NOT IN (SELECT MIN(position) FROM env_vault GROUP BY key);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_env_vault_key ON env_vault(key);
            CREATE TABLE IF NOT EXISTS logs (
                id TEXT PRIMARY KEY,
                instance_id TEXT,
                instance_name TEXT NOT NULL,
                category TEXT NOT NULL,
                level TEXT NOT NULL,
                message TEXT NOT NULL,
                command TEXT,
                created_at INTEGER NOT NULL,
                sequence INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_logs_instance ON logs(instance_id, sequence);
            CREATE INDEX IF NOT EXISTS idx_logs_category ON logs(category, sequence);
            CREATE INDEX IF NOT EXISTS idx_logs_created_at ON logs(created_at);
            CREATE INDEX IF NOT EXISTS idx_logs_sequence ON logs(sequence);
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
                VALUES (2, strftime('%s', 'now'));
            PRAGMA user_version = 2;",
        )
        .map_err(|error| error.to_string())
}

fn replace_sqlite_store(connection: &mut Connection, data: &AppStore) -> Result<(), String> {
    initialize_sqlite_schema(connection)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch("DELETE FROM instances; DELETE FROM env_vault; DELETE FROM logs;")
        .map_err(|error| error.to_string())?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO instances (
                    id, name, template_id, status, deployment_mode, work_dir,
                    env_example_path, env_path, note, created_at, updated_at,
                    needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                    lifecycle_version, service_endpoints
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19
                )",
            )
            .map_err(|error| error.to_string())?;
        for instance in &data.instances {
            let endpoints = serde_json::to_string(&instance.service_endpoints)
                .map_err(|error| error.to_string())?;
            statement
                .execute(params![
                    instance.id,
                    instance.name,
                    instance.template_id,
                    instance.status,
                    instance.deployment_mode,
                    instance.work_dir,
                    instance.env_example_path,
                    instance.env_path,
                    instance.note,
                    instance.created_at as i64,
                    instance.updated_at as i64,
                    instance.needs_doctor,
                    instance.pid.map(i64::from),
                    instance.agent_url,
                    instance.ui_url,
                    instance.studio_url,
                    instance.project_name,
                    instance.lifecycle_version.map(i64::from),
                    endpoints,
                ])
                .map_err(|error| error.to_string())?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO env_vault (position, key, value, comment, source, modified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| error.to_string())?;
        for (position, entry) in data.vault.iter().enumerate() {
            statement
                .execute(params![
                    position as i64,
                    entry.key,
                    entry.value,
                    entry.comment,
                    entry.source,
                    entry.modified,
                ])
                .map_err(|error| error.to_string())?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO logs (
                    id, instance_id, instance_name, category, level, message,
                    command, created_at, sequence
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(|error| error.to_string())?;
        for log in &data.logs {
            statement
                .execute(params![
                    log.id,
                    log.instance_id,
                    log.instance_name,
                    log.category,
                    log.level,
                    log.message,
                    log.command,
                    log.created_at as i64,
                    log.sequence as i64,
                ])
                .map_err(|error| error.to_string())?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn read_sqlite_store(connection: &Connection) -> Result<Option<AppStore>, String> {
    let mut instances = Vec::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT id, name, template_id, status, deployment_mode, work_dir,
                        env_example_path, env_path, note, created_at, updated_at,
                        needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                        lifecycle_version, service_endpoints
                 FROM instances ORDER BY created_at, id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let endpoints: String = row.get(18)?;
                Ok(InstanceRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    template_id: row.get(2)?,
                    status: row.get(3)?,
                    deployment_mode: row.get(4)?,
                    work_dir: row.get(5)?,
                    env_example_path: row.get(6)?,
                    env_path: row.get(7)?,
                    note: row.get(8)?,
                    created_at: row.get::<_, i64>(9)? as u64,
                    updated_at: row.get::<_, i64>(10)? as u64,
                    needs_doctor: row.get(11)?,
                    pid: row.get::<_, Option<i64>>(12)?.map(|value| value as u32),
                    agent_url: row.get(13)?,
                    ui_url: row.get(14)?,
                    studio_url: row.get(15)?,
                    project_name: row.get(16)?,
                    lifecycle_version: row.get::<_, Option<i64>>(17)?.map(|value| value as u32),
                    service_endpoints: serde_json::from_str(&endpoints).unwrap_or_default(),
                })
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            instances.push(row.map_err(|error| error.to_string())?);
        }
    }
    let mut vault = Vec::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT key, value, comment, source, modified
                 FROM env_vault ORDER BY position",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(EnvVariable {
                    key: row.get(0)?,
                    value: row.get(1)?,
                    comment: row.get(2)?,
                    source: row.get(3)?,
                    modified: row.get(4)?,
                })
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            vault.push(row.map_err(|error| error.to_string())?);
        }
    }
    let log_count = connection
        .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?;
    if instances.is_empty() && vault.is_empty() && log_count == 0 {
        Ok(None)
    } else {
        Ok(Some(AppStore {
            instances,
            vault,
            logs: Vec::new(),
        }))
    }
}

fn load_sqlite_store(path: &Path) -> Result<Option<AppStore>, String> {
    let mut connection = open_sqlite(path)?;
    initialize_sqlite_schema(&connection)?;
    let legacy_exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'app_state')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    let existing = read_sqlite_store(&connection)?;
    if legacy_exists {
        let legacy_payload = connection
            .query_row("SELECT payload FROM app_state WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(|error| error.to_string())?;
        if existing.is_none() {
            if let Some(payload) = legacy_payload {
                let legacy: AppStore =
                    serde_json::from_str(&payload).map_err(|error| error.to_string())?;
                replace_sqlite_store(&mut connection, &legacy)?;
            }
        }
        connection
            .execute("DROP TABLE app_state", [])
            .map_err(|error| error.to_string())?;
    }
    read_sqlite_store(&connection)
}

fn replace_sqlite_core(connection: &mut Connection, data: &AppStore) -> Result<(), String> {
    initialize_sqlite_schema(connection)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch("DELETE FROM instances; DELETE FROM env_vault;")
        .map_err(|error| error.to_string())?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO instances (
                    id, name, template_id, status, deployment_mode, work_dir,
                    env_example_path, env_path, note, created_at, updated_at,
                    needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                    lifecycle_version, service_endpoints
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19
                )",
            )
            .map_err(|error| error.to_string())?;
        for instance in &data.instances {
            let endpoints = serde_json::to_string(&instance.service_endpoints)
                .map_err(|error| error.to_string())?;
            statement
                .execute(params![
                    instance.id,
                    instance.name,
                    instance.template_id,
                    instance.status,
                    instance.deployment_mode,
                    instance.work_dir,
                    instance.env_example_path,
                    instance.env_path,
                    instance.note,
                    instance.created_at as i64,
                    instance.updated_at as i64,
                    instance.needs_doctor,
                    instance.pid.map(i64::from),
                    instance.agent_url,
                    instance.ui_url,
                    instance.studio_url,
                    instance.project_name,
                    instance.lifecycle_version.map(i64::from),
                    endpoints,
                ])
                .map_err(|error| error.to_string())?;
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO env_vault (position, key, value, comment, source, modified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| error.to_string())?;
        for (position, entry) in data.vault.iter().enumerate() {
            statement
                .execute(params![
                    position as i64,
                    entry.key,
                    entry.value,
                    entry.comment,
                    entry.source,
                    entry.modified,
                ])
                .map_err(|error| error.to_string())?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn sqlite_log_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: row.get(0)?,
        instance_id: row.get(1)?,
        instance_name: row.get(2)?,
        category: row.get(3)?,
        level: row.get(4)?,
        message: row.get(5)?,
        command: row.get(6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        sequence: row.get::<_, i64>(8)? as u64,
    })
}

impl StorageEngine {
    fn load(&mut self) -> Result<Option<AppStore>, String> {
        let payload = match self {
            Self::Pending => return Ok(None),
            Self::Sqlite(path) => return load_sqlite_store(path),
            Self::SeekDb(bridge) => bridge
                .request(serde_json::json!({"op": "load_core"}))?
                .get("payload")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        };
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(|error| error.to_string()))
            .transpose()
    }

    fn save_core(&mut self, data: &AppStore) -> Result<(), String> {
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let mut connection = open_sqlite(path)?;
                replace_sqlite_core(&mut connection, data)?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "save_core",
                    "payload": serde_json::to_string(data).map_err(|error| error.to_string())?,
                }))?;
            }
        }
        Ok(())
    }

    fn query_logs(&mut self, query: &LogQuery) -> Result<LogPage, String> {
        let limit = query.limit.clamp(1, 1_000);
        match self {
            Self::Pending => Ok(LogPage {
                entries: Vec::new(),
                has_more: false,
                group_count: 0,
            }),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let order = if query.after_sequence.is_some() {
                    "ASC"
                } else {
                    "DESC"
                };
                let sql = format!(
                    "SELECT id, instance_id, instance_name, category, level, message,
                            command, created_at, sequence
                     FROM logs
                     WHERE (?1 IS NULL OR sequence < ?1)
                       AND (?2 IS NULL OR sequence > ?2)
                     ORDER BY sequence {order}
                     LIMIT ?3"
                );
                let mut statement = connection
                    .prepare(&sql)
                    .map_err(|error| error.to_string())?;
                let rows = statement
                    .query_map(
                        params![
                            query.before_sequence.map(|value| value as i64),
                            query.after_sequence.map(|value| value as i64),
                            (limit + 1) as i64,
                        ],
                        sqlite_log_from_row,
                    )
                    .map_err(|error| error.to_string())?;
                let mut entries = rows
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?;
                let has_more = entries.len() > limit;
                entries.truncate(limit);
                let group_count = connection
                    .query_row(
                        "SELECT COUNT(DISTINCT COALESCE(instance_id, 'name:' || instance_name)) FROM logs",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(|error| error.to_string())? as usize;
                Ok(LogPage {
                    entries,
                    has_more,
                    group_count,
                })
            }
            Self::SeekDb(bridge) => {
                let response = bridge.request(serde_json::json!({
                    "op": "query_logs",
                    "query": query,
                }))?;
                serde_json::from_value(
                    response
                        .get("page")
                        .cloned()
                        .ok_or_else(|| "SeekDB log pagination response missing page".to_string())?,
                )
                .map_err(|error| error.to_string())
            }
        }
    }

    fn max_log_sequence(&mut self) -> Result<u64, String> {
        match self {
            Self::Pending => Ok(0),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .query_row("SELECT COALESCE(MAX(sequence), 0) FROM logs", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map(|value| value as u64)
                    .map_err(|error| error.to_string())
            }
            Self::SeekDb(bridge) => bridge
                .request(serde_json::json!({"op": "max_log_sequence"}))?
                .get("sequence")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "SeekDB log sequence response invalid".to_string()),
        }
    }

    fn log_count(&mut self) -> Result<usize, String> {
        match self {
            Self::Pending => Ok(0),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
                    .map(|value| value as usize)
                    .map_err(|error| error.to_string())
            }
            Self::SeekDb(bridge) => bridge
                .request(serde_json::json!({"op": "log_count"}))?
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as usize)
                .ok_or_else(|| "SeekDB log count response invalid".to_string()),
        }
    }

    fn has_completed_deployment(&mut self, instance_id: &str) -> Result<bool, String> {
        match self {
            Self::Pending => Ok(false),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM logs
                            WHERE instance_id = ?1
                              AND category = 'install'
                              AND level = 'success'
                              AND message = 'Instance deployment completed'
                         )",
                        [instance_id],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())
            }
            Self::SeekDb(bridge) => bridge
                .request(serde_json::json!({
                    "op": "has_completed_deployment",
                    "instanceId": instance_id,
                }))?
                .get("completed")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| "SeekDB deployment status response invalid".to_string()),
        }
    }

    fn cleanup_logs(&mut self, runtime_retention_days: u32, now: u64) -> Result<usize, String> {
        match self {
            Self::Pending => Ok(0),
            Self::Sqlite(path) => {
                let mut connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let runtime_cutoff = now.saturating_sub(
                    u64::from(runtime_retention_days.max(1)).saturating_mul(SECONDS_PER_DAY),
                );
                let deleted_cutoff = now.saturating_sub(
                    u64::from(DELETED_INSTANCE_LOG_RETENTION_DAYS).saturating_mul(SECONDS_PER_DAY),
                );
                let transaction = connection
                    .transaction()
                    .map_err(|error| error.to_string())?;
                let mut removed = transaction
                    .execute(
                        "DELETE FROM logs WHERE category = 'runtime' AND created_at < ?1",
                        [runtime_cutoff as i64],
                    )
                    .map_err(|error| error.to_string())?;
                removed += transaction
                    .execute(
                        "DELETE FROM logs
                         WHERE instance_id IS NOT NULL
                           AND created_at < ?1
                           AND NOT EXISTS (
                               SELECT 1 FROM instances WHERE instances.id = logs.instance_id
                           )",
                        [deleted_cutoff as i64],
                    )
                    .map_err(|error| error.to_string())?;
                let count = transaction
                    .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
                    .map_err(|error| error.to_string())? as usize;
                if count > MAX_LOG_ENTRIES {
                    let remove_limit = count - MAX_LOG_ENTRIES + LOG_CLEANUP_BATCH_SIZE;
                    removed += transaction
                        .execute(
                            "DELETE FROM logs WHERE id IN (
                                SELECT logs.id FROM logs
                                WHERE category = 'runtime'
                                   OR (instance_id IS NOT NULL AND NOT EXISTS (
                                       SELECT 1 FROM instances WHERE instances.id = logs.instance_id
                                   ))
                                ORDER BY sequence ASC
                                LIMIT ?1
                             )",
                            [remove_limit as i64],
                        )
                        .map_err(|error| error.to_string())?;
                }
                transaction.commit().map_err(|error| error.to_string())?;
                Ok(removed)
            }
            Self::SeekDb(bridge) => bridge
                .request(serde_json::json!({
                    "op": "cleanup_logs",
                    "runtimeRetentionDays": runtime_retention_days,
                    "now": now,
                    "maxEntries": MAX_LOG_ENTRIES,
                    "batchSize": LOG_CLEANUP_BATCH_SIZE,
                    "deletedRetentionDays": DELETED_INSTANCE_LOG_RETENTION_DAYS,
                }))?
                .get("removed")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as usize)
                .ok_or_else(|| "SeekDB log cleanup response invalid".to_string()),
        }
    }

    fn clear_logs(&mut self) -> Result<(), String> {
        match self {
            Self::Pending => return Ok(()),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .execute("DELETE FROM logs", [])
                    .map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({"op": "clear_logs"}))?;
            }
        }
        Ok(())
    }

    fn delete_runtime_logs(&mut self, instance_id: &str) -> Result<(), String> {
        match self {
            Self::Pending => return Ok(()),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .execute(
                        "DELETE FROM logs WHERE instance_id = ?1 AND category = 'runtime'",
                        params![instance_id],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "delete_runtime_logs",
                    "instance_id": instance_id,
                }))?;
            }
        }
        Ok(())
    }

    fn append_logs(&mut self, logs: &[LogEntry]) -> Result<(), String> {
        if logs.is_empty() {
            return Ok(());
        }
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let mut connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let transaction = connection
                    .transaction()
                    .map_err(|error| error.to_string())?;
                {
                    let mut statement = transaction
                        .prepare(
                            "INSERT OR REPLACE INTO logs (
                                id, instance_id, instance_name, category, level, message,
                                command, created_at, sequence
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        )
                        .map_err(|error| error.to_string())?;
                    for log in logs {
                        statement
                            .execute(params![
                                log.id,
                                log.instance_id,
                                log.instance_name,
                                log.category,
                                log.level,
                                log.message,
                                log.command,
                                log.created_at as i64,
                                log.sequence as i64,
                            ])
                            .map_err(|error| error.to_string())?;
                    }
                }
                transaction.commit().map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "append_logs",
                    "entries": logs,
                }))?;
            }
        }
        Ok(())
    }

    fn append_log(&mut self, log: &LogEntry, removed_ids: &[String]) -> Result<(), String> {
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let mut connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let transaction = connection
                    .transaction()
                    .map_err(|error| error.to_string())?;
                transaction
                    .execute(
                        "INSERT OR REPLACE INTO logs (
                            id, instance_id, instance_name, category, level, message,
                            command, created_at, sequence
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            log.id,
                            log.instance_id,
                            log.instance_name,
                            log.category,
                            log.level,
                            log.message,
                            log.command,
                            log.created_at as i64,
                            log.sequence as i64,
                        ],
                    )
                    .map_err(|error| error.to_string())?;
                if !removed_ids.is_empty() {
                    let mut statement = transaction
                        .prepare("DELETE FROM logs WHERE id = ?1")
                        .map_err(|error| error.to_string())?;
                    for id in removed_ids {
                        statement.execute([id]).map_err(|error| error.to_string())?;
                    }
                }
                transaction.commit().map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "append_log",
                    "entry": log,
                    "removedIds": removed_ids,
                }))?;
            }
        }
        Ok(())
    }

    fn upsert_instance(&mut self, instance: &InstanceRecord) -> Result<(), String> {
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let endpoints = serde_json::to_string(&instance.service_endpoints)
                    .map_err(|error| error.to_string())?;
                connection
                    .execute(
                        "INSERT OR REPLACE INTO instances (
                            id, name, template_id, status, deployment_mode, work_dir,
                            env_example_path, env_path, note, created_at, updated_at,
                            needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                            lifecycle_version, service_endpoints
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                            ?13, ?14, ?15, ?16, ?17, ?18, ?19
                         )",
                        params![
                            instance.id,
                            instance.name,
                            instance.template_id,
                            instance.status,
                            instance.deployment_mode,
                            instance.work_dir,
                            instance.env_example_path,
                            instance.env_path,
                            instance.note,
                            instance.created_at as i64,
                            instance.updated_at as i64,
                            instance.needs_doctor,
                            instance.pid.map(i64::from),
                            instance.agent_url,
                            instance.ui_url,
                            instance.studio_url,
                            instance.project_name,
                            instance.lifecycle_version.map(i64::from),
                            endpoints,
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "upsert_instance",
                    "instance": instance,
                }))?;
            }
        }
        Ok(())
    }

    fn delete_instance(&mut self, instance_id: &str) -> Result<(), String> {
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .execute("DELETE FROM instances WHERE id = ?1", [instance_id])
                    .map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "delete_instance",
                    "instanceId": instance_id,
                }))?;
            }
        }
        Ok(())
    }

    fn replace_vault(&mut self, entries: &[EnvVariable]) -> Result<(), String> {
        match self {
            Self::Pending => return Err(storage_not_initialized()),
            Self::Sqlite(path) => {
                let mut connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                let transaction = connection
                    .transaction()
                    .map_err(|error| error.to_string())?;
                transaction
                    .execute("DELETE FROM env_vault", [])
                    .map_err(|error| error.to_string())?;
                {
                    let mut statement = transaction
                        .prepare(
                            "INSERT INTO env_vault (position, key, value, comment, source, modified)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        )
                        .map_err(|error| error.to_string())?;
                    for (position, entry) in entries.iter().enumerate() {
                        statement
                            .execute(params![
                                position as i64,
                                entry.key,
                                entry.value,
                                entry.comment,
                                entry.source,
                                entry.modified,
                            ])
                            .map_err(|error| error.to_string())?;
                    }
                }
                transaction.commit().map_err(|error| error.to_string())?;
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({
                    "op": "replace_vault",
                    "entries": entries,
                }))?;
            }
        }
        Ok(())
    }

    fn maintain(&mut self, aggressive: bool) -> Result<(), String> {
        if let Self::Sqlite(path) = self {
            let connection = open_sqlite(path)?;
            connection
                .execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;")
                .map_err(|error| error.to_string())?;
            if aggressive {
                connection
                    .execute_batch("VACUUM;")
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct DesktopState {
    data_dir: PathBuf,
    config_path: PathBuf,
    credentials_path: PathBuf,
    storage_config: Arc<Mutex<StorageConfig>>,
    storage: Arc<Mutex<StorageEngine>>,
    storage_error: Arc<Mutex<Option<String>>>,
    storage_ready: Arc<Mutex<bool>>,
    storage_setup_required: Arc<Mutex<bool>>,
    effective_storage_mode: Arc<Mutex<String>>,
    data: Arc<Mutex<AppStore>>,
    next_log_sequence: Arc<Mutex<u64>>,
    runtime_log_sync: Arc<Mutex<()>>,
    deployment_stages: Arc<Mutex<HashMap<String, String>>>,
}

impl DesktopState {
    fn load(data_dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&data_dir);
        let mut config_ready = true;
        let mut startup_errors = Vec::new();
        let config_path = data_dir.join("storage.json");
        let config_file_exists = config_path.is_file();
        let mut config: StorageConfig = match fs::read_to_string(&config_path) {
            Ok(value) => match serde_json::from_str(&value) {
                Ok(config) => config,
                Err(error) => {
                    config_ready = false;
                    startup_errors
                        .push(format!("Storage config file format error, entered read-only protection mode: {error}"));
                    StorageConfig::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StorageConfig::default(),
            Err(error) => {
                config_ready = false;
                startup_errors.push(format!("Failed to read storage config file, entered read-only protection mode: {error}"));
                StorageConfig::default()
            }
        };
        normalize_storage_database(&mut config);
        // Legacy files have no completion marker and must still show the first-run storage choice.
        let storage_setup_required = !config_file_exists || !config.setup_completed;
        let default_sqlite_path = data_dir.clone();
        let default_seekdb_path = data_dir.join("seekdb");
        let legacy_seekdb_path = default_seekdb_path.join("desktop");
        if config.mode == "sqlite_embedded" {
            let legacy_misassigned_path = Path::new(&config.path) == default_seekdb_path
                && !default_seekdb_path
                    .join("agentseek-desktop.sqlite3")
                    .is_file();
            if config.path.is_empty() || legacy_misassigned_path {
                config.path = default_sqlite_path.to_string_lossy().to_string();
            }
            let _ = fs::create_dir_all(&config.path);
        } else if config.path.is_empty()
            || (Path::new(&config.path) == legacy_seekdb_path && !legacy_seekdb_path.exists())
        {
            config.path = default_seekdb_path.to_string_lossy().to_string();
        }
        let credentials_path = data_dir.join("credentials.json");
        let mut credentials_ready = true;
        let mut credentials = match read_local_credentials(&credentials_path) {
            Ok(credentials) => credentials,
            Err(error) => {
                credentials_ready = false;
                startup_errors.push(error);
                LocalCredentials::default()
            }
        };
        if config.password.is_empty() {
            config.password = credentials.storage_password.clone();
        } else {
            credentials.storage_password = config.password.clone();
            if let Err(error) = write_local_credentials(&credentials_path, &credentials) {
                credentials_ready = false;
                startup_errors.push(error);
            }
        }
        // Do not persist the default before the user confirms the first-run selection.
        if config_ready && !storage_setup_required {
            if let Err(error) = write_storage_config(&config_path, &config) {
                config_ready = false;
                startup_errors.push(format!("Failed to save storage config: {error}"));
            }
        }
        // SeekDB startup failures use the app-level SQLite file, never the SeekDB data directory.
        let sqlite_path = if config.mode == "sqlite_embedded" {
            sqlite_database_path(&data_dir, &config)
        } else {
            data_dir.join("agentseek-desktop.sqlite3")
        };
        // A truly fresh launch stays database-free until the user confirms a storage backend.
        let configured_engine = if !config_file_exists {
            Ok(StorageEngine::Pending)
        } else if config.mode == "sqlite_embedded" {
            Ok(StorageEngine::Sqlite(sqlite_path.clone()))
        } else {
            let pending = data_dir.join("storage.startup.json");
            let bridge = fs::write(
                &pending,
                serde_json::to_string_pretty(&config).unwrap_or_default(),
            )
            .map_err(|error| error.to_string())
            .and_then(|_| SeekDbBridge::open(&pending, &data_dir));
            let _ = fs::remove_file(&pending);
            bridge.map(StorageEngine::SeekDb)
        };
        let (mut engine, effective_storage_mode) = match configured_engine {
            Ok(engine) => (engine, config.mode.clone()),
            Err(error) => {
                startup_errors.push(format!(
                    "Configured {} storage unavailable, degraded to embedded SQLite: {error}",
                    config.mode
                ));
                (
                    StorageEngine::Sqlite(sqlite_path.clone()),
                    "sqlite_embedded".to_string(),
                )
            }
        };
        let legacy_path = data_dir.join("state.json");
        let legacy_exists = legacy_path.is_file();
        let legacy_data = fs::read_to_string(&legacy_path)
            .ok()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        let (database_data, database_ready) = match engine.load() {
            Ok(data) => (data, true),
            Err(error) => {
                startup_errors.push(format!("Failed to read desktop database, entered read-only protection mode: {error}"));
                (None, false)
            }
        };
        let migrating_legacy = database_data.is_none() && legacy_exists;
        let mut data = database_data.unwrap_or(legacy_data);
        let credentials_required =
            matches!(config.mode.as_str(), "seekdb_server" | "oceanbase_server");
        let mut storage_ready = config_file_exists
            && database_ready
            && config_ready
            && (!credentials_required || credentials_ready);
        let repaired_statuses = if migrating_legacy {
            repair_predeployment_restart_statuses(&mut data)
        } else {
            let mut changed = false;
            for instance in &mut data.instances {
                let deployed = instance.pid.is_some()
                    || engine
                        .has_completed_deployment(&instance.id)
                        .unwrap_or(false);
                if instance.status == "needs-restart" && !deployed {
                    instance.status = if instance.env_path.is_some() {
                        "ready-to-install".to_string()
                    } else {
                        "configuring".to_string()
                    };
                    instance.needs_doctor = false;
                    instance.updated_at = timestamp();
                    changed = true;
                }
            }
            changed
        };
        if migrating_legacy {
            repair_lifecycle_log_categories(&mut data);
            repair_log_sequences(&mut data);
            data.logs.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.sequence.cmp(&right.sequence))
                    .then_with(|| left.id.cmp(&right.id))
            });
            prune_logs(&mut data, config.runtime_log_retention_days, timestamp());
        }
        if storage_ready {
            let persist_result = (|| -> Result<(), String> {
                if migrating_legacy {
                    let legacy_logs = data.logs.clone();
                    engine.save_core(&data)?;
                    engine.clear_logs()?;
                    for chunk in legacy_logs.chunks(LOG_CLEANUP_BATCH_SIZE) {
                        engine.append_logs(chunk)?;
                    }
                } else if repaired_statuses {
                    engine.save_core(&data)?;
                }
                engine.cleanup_logs(config.runtime_log_retention_days, timestamp())?;
                Ok(())
            })();
            if let Err(error) = persist_result {
                storage_ready = false;
                startup_errors.push(format!("Failed to initialize desktop storage, entered read-only protection mode: {error}"));
            }
        }
        let next_log_sequence = if storage_ready {
            match engine.max_log_sequence() {
                Ok(sequence) => sequence,
                Err(error) => {
                    storage_ready = false;
                    startup_errors.push(format!("Failed to read log sequence, entered read-only protection mode: {error}"));
                    0
                }
            }
        } else {
            data.logs
                .iter()
                .map(|log| log.sequence)
                .max()
                .unwrap_or_default()
        };
        if storage_ready {
            data.logs.clear();
        } else if data.logs.len() > MAX_PENDING_LOG_ENTRIES {
            data.logs.drain(..data.logs.len() - MAX_PENDING_LOG_ENTRIES);
        }
        Self {
            data_dir,
            config_path,
            credentials_path,
            storage_config: Arc::new(Mutex::new(config)),
            storage: Arc::new(Mutex::new(engine)),
            storage_error: Arc::new(Mutex::new(if startup_errors.is_empty() {
                None
            } else {
                Some(startup_errors.join("\n"))
            })),
            storage_ready: Arc::new(Mutex::new(storage_ready)),
            storage_setup_required: Arc::new(Mutex::new(storage_setup_required)),
            effective_storage_mode: Arc::new(Mutex::new(effective_storage_mode)),
            data: Arc::new(Mutex::new(data)),
            next_log_sequence: Arc::new(Mutex::new(next_log_sequence)),
            runtime_log_sync: Arc::new(Mutex::new(())),
            deployment_stages: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn set_deployment_stage(&self, instance_id: &str, stage: &str) {
        if let Ok(mut stages) = self.deployment_stages.lock() {
            stages.insert(instance_id.to_string(), stage.to_string());
        }
    }

    fn ensure_storage_ready(&self) -> Result<(), String> {
        if *self
            .storage_ready
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?
        {
            Ok(())
        } else {
            Err(self
                .storage_error
                .lock()
                .ok()
                .and_then(|error| error.clone())
                .unwrap_or_else(|| "Desktop storage not writable, please fix storage connection first".to_string()))
        }
    }

    fn ensure_storage_configurable(&self) -> Result<(), String> {
        let setup_required = *self
            .storage_setup_required
            .lock()
            .map_err(|_| "Storage setup state lock is poisoned".to_string())?;
        if setup_required {
            Ok(())
        } else {
            self.ensure_storage_ready()
        }
    }

    fn persist_instance(&self, instance: &InstanceRecord) -> Result<(), String> {
        self.ensure_storage_ready()?;
        self.storage
            .lock()
            .map_err(|_| "Storage lock is poisoned".to_string())?
            .upsert_instance(instance)
    }

    fn remove_persisted_instance(&self, instance_id: &str) -> Result<(), String> {
        self.ensure_storage_ready()?;
        self.storage
            .lock()
            .map_err(|_| "Storage lock is poisoned".to_string())?
            .delete_instance(instance_id)
    }

    fn replace_vault_entries(&self, mut entries: Vec<EnvVariable>) -> Result<(), String> {
        self.ensure_storage_ready()?;
        let mut seen = HashSet::new();
        for entry in &mut entries {
            entry.key = entry.key.trim().to_string();
            if entry.key.is_empty() {
                return Err("Environment variable name cannot be empty".to_string());
            }
            if !seen.insert(entry.key.clone()) {
                return Err(format!("Duplicate environment variable name: {}", entry.key));
            }
            entry.modified = false;
        }
        self.storage
            .lock()
            .map_err(|_| "Storage lock is poisoned".to_string())?
            .replace_vault(&entries)?;
        self.data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?
            .vault = entries;
        Ok(())
    }

    fn persist_current_vault(&self) -> Result<(), String> {
        let entries = self
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?
            .vault
            .clone();
        self.replace_vault_entries(entries)
    }

    fn cleanup_logs(&self) -> Result<usize, String> {
        self.ensure_storage_ready()?;
        let retention_days = self
            .storage_config
            .lock()
            .map_err(|_| "Storage config lock is poisoned".to_string())?
            .runtime_log_retention_days;
        let mut storage = self
            .storage
            .lock()
            .map_err(|_| "Storage lock is poisoned".to_string())?;
        let removed = storage.cleanup_logs(retention_days, timestamp())?;
        storage.maintain(removed >= 10_000)?;
        Ok(removed)
    }

    fn redact_log_text(&self, value: String) -> String {
        let secrets = self
            .data
            .lock()
            .map(|data| {
                data.vault
                    .iter()
                    .filter(|entry| is_secret_env_key(&entry.key) && entry.value.len() >= 4)
                    .map(|entry| entry.value.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        truncate_log_text(secrets.into_iter().fold(value, |redacted, secret| {
            redacted.replace(&secret, "******")
        }))
    }

    fn log(
        &self,
        instance_id: Option<&str>,
        instance_name: &str,
        category: &str,
        level: &str,
        message: impl Into<String>,
        command: Option<String>,
    ) -> bool {
        let message = self.redact_log_text(message.into());
        let command = command.map(|command| self.redact_log_text(command));
        let now = timestamp();
        let sequence = match self.next_log_sequence.lock() {
            Ok(mut sequence) => {
                *sequence = sequence.saturating_add(1);
                *sequence
            }
            Err(_) => return false,
        };
        let log = LogEntry {
            id: format!("log-{now}-{sequence}"),
            instance_id: instance_id.map(str::to_string),
            instance_name: instance_name.to_string(),
            category: category.to_string(),
            level: level.to_string(),
            message,
            command,
            created_at: now,
            sequence,
        };
        let persist_result = self.ensure_storage_ready().and_then(|_| {
            self.storage
                .lock()
                .map_err(|_| "Storage lock is poisoned".to_string())?
                .append_log(&log, &[])
        });
        if persist_result.is_err() {
            if let Ok(mut data) = self.data.lock() {
                let storage_ready = self
                    .storage_ready
                    .lock()
                    .map(|ready| *ready)
                    .unwrap_or(false);
                if storage_ready {
                    drop(data);
                    if self
                        .storage
                        .lock()
                        .map_err(|_| "Storage lock is poisoned".to_string())
                        .and_then(|mut storage| storage.append_log(&log, &[]))
                        .is_ok()
                    {
                        return true;
                    }
                    data = match self.data.lock() {
                        Ok(data) => data,
                        Err(_) => return false,
                    };
                }
                data.logs.push(log);
                if data.logs.len() > MAX_PENDING_LOG_ENTRIES {
                    let remove = data.logs.len() - MAX_PENDING_LOG_ENTRIES;
                    data.logs.drain(..remove);
                }
            }
            false
        } else if sequence % 100 == 0 {
            let _ = self.cleanup_logs();
            true
        } else {
            true
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareInstanceInput {
    name: String,
    template_id: String,
    target_dir: String,
    note: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrepareInstanceResult {
    instance: InstanceRecord,
    env: Vec<EnvVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docker_warning: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveEnvInput {
    instance_id: String,
    entries: Vec<EnvVariable>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveEnvResult {
    path: String,
    key_count: usize,
    synced_count: usize,
    port_changes: Vec<PortChange>,
    entries: Vec<EnvVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docker_warning: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportEnvInput {
    source_path: String,
    output_path: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportEnvResult {
    path: String,
    key_count: usize,
    filled_count: usize,
    missing_count: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PortChange {
    key: String,
    old_port: u16,
    new_port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemInfo {
    app_name: String,
    version: String,
    data_path: String,
    cli_strategy: String,
    storage: String,
    docker_available: bool,
    docker_compose_available: bool,
    docker_running: bool,
}

#[derive(Clone, Copy)]
struct DockerStatus {
    cli_available: bool,
    compose_v2_available: bool,
    daemon_running: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageStatus {
    mode: String,
    effective_mode: String,
    path: String,
    default_sqlite_path: String,
    default_seekdb_path: String,
    host: String,
    port: u16,
    tenant: String,
    database: String,
    default_database: String,
    user: String,
    password_configured: bool,
    runtime_log_retention_days: u32,
    setup_required: bool,
    writable: bool,
    error: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogSettings {
    runtime_retention_days: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogQuery {
    #[serde(default)]
    before_sequence: Option<u64>,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default = "default_log_page_size")]
    limit: usize,
}

fn default_log_page_size() -> usize {
    500
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogPage {
    entries: Vec<LogEntry>,
    has_more: bool,
    group_count: usize,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliStatus {
    platform: String,
    dependency_commands: HashMap<String, String>,
    minimum_versions: HashMap<String, String>,
    node_managed: bool,
    uv_available: bool,
    uv_path: String,
    cli_available: bool,
    cli_compatible: bool,
    cli_update_available: bool,
    cli_latest_version: String,
    cli_latest_version_checked: bool,
    uv_version: String,
    cli_version: String,
    node_available: bool,
    node_compatible: bool,
    node_version: String,
    npm_available: bool,
    npm_compatible: bool,
    npm_version: String,
    git_available: bool,
    git_compatible: bool,
    git_version: String,
    uv_compatible: bool,
    prerequisites_ready: bool,
    install_command: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeInstallPlan {
    task_id: String,
    script: String,
    script_path: String,
    install_dir: String,
    dependencies: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeInstallProgress {
    status: String,
    stage: String,
    log: String,
}

struct CommandResult {
    code: i32,
    output: String,
    command: String,
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn display_name(template_id: &str) -> String {
    template_id
        .split('/')
        .next_back()
        .unwrap_or(template_id)
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn runtime_path() -> std::ffi::OsString {
    let mut paths = Vec::new();
    if let Some(runtime_root) = env::var_os("AGENTSEEK_DESKTOP_RUNTIME_DIR") {
        let versions_dir = PathBuf::from(runtime_root).join("nvm/versions/node");
        let mut managed_bins = fs::read_dir(versions_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("bin/node").is_file())
            .collect::<Vec<_>>();
        managed_bins.sort_by_key(|path| {
            path.file_name()
                .map(|name| numeric_version(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        paths.extend(managed_bins.into_iter().rev().map(|path| path.join("bin")));
    }
    if let Some(managed_node_bin) = env::var_os("AGENTSEEK_DESKTOP_NODE_BIN") {
        paths.push(PathBuf::from(managed_node_bin));
    }
    if let Some(home) = env::var_os("HOME") {
        paths.push(PathBuf::from(&home).join(".local/bin"));
        paths.push(PathBuf::from(&home).join(".cargo/bin"));
        paths.push(PathBuf::from(&home).join(".pyenv/shims"));
        paths.push(PathBuf::from(home).join(".pyenv/bin"));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.9/bin"),
    ]);
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap_or_default()
}

fn managed_runtime_root() -> Option<PathBuf> {
    env::var_os("AGENTSEEK_DESKTOP_RUNTIME_DIR").map(PathBuf::from)
}

fn managed_node_bin(runtime_root: &Path, node_version: &str) -> PathBuf {
    if cfg!(windows) {
        let architecture = if cfg!(target_arch = "aarch64") {
            "win-arm64"
        } else {
            "win-x64"
        };
        runtime_root.join(format!("node-v{node_version}-{architecture}"))
    } else {
        let versions_dir = runtime_root.join("nvm").join("versions").join("node");
        let exact = versions_dir.join(format!("v{node_version}")).join("bin");
        if exact.join("node").is_file() {
            return exact;
        }
        let major = numeric_version(node_version).first().copied();
        let mut candidates = fs::read_dir(&versions_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.join("bin/node").is_file()
                    && path
                        .file_name()
                        .map(|name| {
                            numeric_version(&name.to_string_lossy()).first().copied() == major
                        })
                        .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|path| {
            path.file_name()
                .map(|name| numeric_version(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        candidates
            .pop()
            .map(|path| path.join("bin"))
            .unwrap_or(exact)
    }
}

fn configured_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("PATH", runtime_path());
    // Prevent Python from importing a local agentseek source tree
    // that may shadow the installed package when CWD contains agentseek/.
    command.current_dir(std::env::temp_dir());
    // Clear Python env vars that may leak from conda/venv and cause agentseek
    // to import from the wrong environment.
    command.env_remove("PYTHONPATH");
    command.env_remove("PYTHONHOME");
    command.env_remove("VIRTUAL_ENV");
    command.env_remove("CONDA_PREFIX");
    command.env_remove("CONDA_DEFAULT_ENV");
    command.env_remove("CONDA_PROMPT_MODIFIER");
    command
}

fn curl_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    }
}

fn command_version(program: &str, arg: &str) -> Option<String> {
    let output = configured_command(program).arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    combined
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

fn numeric_version(value: &str) -> Vec<u64> {
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

fn version_at_least(value: &str, minimum: &[u64]) -> bool {
    let current = numeric_version(value);
    if current.is_empty() {
        return false;
    }
    for index in 0..minimum.len().max(current.len()) {
        let current_part = current.get(index).copied().unwrap_or(0);
        let minimum_part = minimum.get(index).copied().unwrap_or(0);
        if current_part != minimum_part {
            return current_part > minimum_part;
        }
    }
    true
}

fn meets_requirement(value: &str, minimum: &str) -> bool {
    version_at_least(value, &numeric_version(minimum))
}

fn platform_id() -> String {
    if cfg!(target_os = "macos") {
        return "macos".to_string();
    }
    if cfg!(target_os = "windows") {
        return "windows".to_string();
    }
    if cfg!(target_os = "linux") {
        let distribution = fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    line.strip_prefix("ID=")
                        .map(|value| value.trim_matches(['\"', '\'']).to_lowercase())
                })
            })
            .unwrap_or_else(|| "linux".to_string());
        return match distribution.as_str() {
            "ubuntu" | "debian" | "linuxmint" | "pop" => "debian".to_string(),
            "centos" | "rhel" | "fedora" | "rocky" | "almalinux" | "ol" => "rhel".to_string(),
            _ => "linux".to_string(),
        };
    }
    "unknown".to_string()
}

fn dependency_commands(
    requirements: &RuntimeRequirements,
    platform: &str,
    managed_runtime_root: Option<&Path>,
    uv_available: bool,
    git_available: bool,
) -> HashMap<String, String> {
    let mut commands = HashMap::new();
    let node_version = &requirements.versions.node.managed;
    let node_major = numeric_version(node_version)
        .first()
        .copied()
        .unwrap_or_default();
    let nvm_version = &requirements.versions.nvm.managed;
    let nvm_installer = requirements
        .sources
        .nvm_installer_template
        .replace("{version}", nvm_version);
    commands.insert(
        "uv".to_string(),
        if uv_available {
            "uv self update".to_string()
        } else {
            format!("curl -LsSf {} | sh", requirements.sources.uv_installer)
        },
    );
    let managed_root = managed_runtime_root
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "<AgentSeek data>/runtime".to_string());
    let managed_nvm = PathBuf::from(&managed_root).join("nvm");
    let managed_node_command = match platform {
        "macos" | "debian" | "rhel" | "linux" => format!(
            "unset npm_config_prefix && export NVM_DIR=\"{}\" PROFILE=/dev/null && curl -o- {} | bash && . \"{}/nvm.sh\" && nvm install {} && node --version && npm --version",
            managed_nvm.to_string_lossy(),
            nvm_installer,
            managed_nvm.to_string_lossy(),
            node_major,
        ),
        "windows" => format!(
            "Downloading Node.js {} official ZIP to {} (for AgentSeek Desktop only)",
            node_version, managed_root
        ),
        _ => format!(
            "Installing Node.js {} to AgentSeek Desktop private runtime directory {}",
            node_version, managed_root
        ),
    };
    let git = match platform {
        "macos" => {
            if git_available {
                "brew upgrade git"
            } else {
                "brew install git"
            }
        }
        "debian" => "sudo apt-get update && sudo apt-get install -y git",
        "rhel" => "sudo dnf install -y git",
        "windows" => "winget install --id Git.Git",
        _ => "Please install the required Git version using your system package manager",
    };
    commands.insert("node".to_string(), managed_node_command.clone());
    commands.insert("npm".to_string(), managed_node_command);
    commands.insert("git".to_string(), git.to_string());
    commands.insert(
        "agentseek".to_string(),
        "uv tool install --upgrade agentseek".to_string(),
    );
    commands
}

fn program_from_login_shell(program: &str) -> Option<String> {
    if program.is_empty()
        || !program.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }
    #[cfg(windows)]
    let output = Command::new("where.exe").arg(program).output().ok()?;
    #[cfg(not(windows))]
    let output = {
        let shell = env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .unwrap_or_else(|| {
                if cfg!(target_os = "macos") {
                    PathBuf::from("/bin/zsh")
                } else {
                    PathBuf::from("/bin/bash")
                }
            });
        Command::new(shell)
            .args(["-lic", &format!("command -v {program}")])
            .stderr(Stdio::null())
            .output()
            .ok()?
    };
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| Path::new(line).is_file())
        .map(str::to_string)
}

fn resolved_program(program: &str, version_arg: &str) -> Option<String> {
    if command_version(program, version_arg).is_some() {
        return Some(program.to_string());
    }
    let resolved = program_from_login_shell(program)?;
    command_version(&resolved, version_arg).map(|_| resolved)
}

fn uv_program() -> Option<String> {
    if let Ok(program) = env::var("AGENTSEEK_DESKTOP_UV") {
        if command_version(&program, "--version").is_some() {
            return Some(program);
        }
    }
    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/bin/uv"));
        candidates.push(PathBuf::from(&home).join(".cargo/bin/uv"));
        candidates.push(PathBuf::from(home).join(".pyenv/shims/uv"));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/uv"),
        PathBuf::from("/usr/local/bin/uv"),
        PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.9/bin/uv"),
    ]);
    let located = candidates
        .into_iter()
        .find(|candidate| command_version(&candidate.to_string_lossy(), "--version").is_some())
        .map(|candidate| candidate.to_string_lossy().to_string());
    if located.is_some() {
        return located;
    }
    resolved_program("uv", "--version")
}

fn uv_tool_bin_dir() -> String {
    uv_program()
        .and_then(|uv| {
            configured_command(&uv)
                .args(["tool", "dir", "--bin"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_default()
}

fn agentseek_program() -> String {
    if let Ok(program) = env::var("AGENTSEEK_CLI") {
        return program;
    }
    if let Some(uv) = uv_program() {
        let output = configured_command(uv)
            .args(["tool", "dir", "--bin"])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let bin_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !bin_dir.is_empty() {
                    let executable = if cfg!(windows) {
                        "agentseek.exe"
                    } else {
                        "agentseek"
                    };
                    let candidate = PathBuf::from(bin_dir).join(executable);
                    if candidate.is_file() {
                        return candidate.to_string_lossy().to_string();
                    }
                }
            }
        }
    }
    if let Some(program) = resolved_program("agentseek", "--help") {
        return program;
    }
    "agentseek".to_string()
}

fn parse_uv_tool_version(content: &str, tool_name: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let name = parts.next()?;
        let version = parts.next()?;
        if name == tool_name && version.starts_with('v') {
            Some(format!("{tool_name} {}", version.trim_start_matches('v')))
        } else {
            None
        }
    })
}

fn parse_agentseek_version(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        let normalized = trimmed.to_ascii_lowercase();
        if normalized.starts_with("agentseek v") && !numeric_version(trimmed).is_empty() {
            Some(trimmed.to_string())
        } else {
            None
        }
    })
}

fn agentseek_command_version(program: &str) -> Option<String> {
    let output = configured_command(program).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let content = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_agentseek_version(&content)
}

fn uv_tool_version(tool_name: &str) -> Option<String> {
    let uv = uv_program()?;
    let output = configured_command(uv)
        .args(["tool", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let content = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_uv_tool_version(&content, tool_name)
}

fn parse_agentseek_package_version(content: &[u8]) -> Result<String, String> {
    let metadata: serde_json::Value = serde_json::from_slice(content)
        .map_err(|error| format!("AgentSeek package metadata format error: {error}"))?;
    let version = metadata
        .get("info")
        .and_then(|info| info.get("version"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if numeric_version(&version).is_empty() {
        Err("AgentSeek package metadata has no valid latest version".to_string())
    } else {
        Ok(version)
    }
}

fn latest_agentseek_version(requirements: &RuntimeRequirements) -> Result<String, String> {
    if let Ok(version) = env::var("AGENTSEEK_DESKTOP_AGENTSEEK_LATEST_VERSION") {
        if !numeric_version(&version).is_empty() {
            return Ok(version);
        }
    }
    let output = configured_command(curl_program())
        .args([
            "-fsSL",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--retry",
            "2",
            &requirements.sources.agentseek_package_metadata,
        ])
        .output()
        .map_err(|error| format!("Failed to query AgentSeek latest version: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by system".to_string());
        return Err(if stderr.is_empty() {
            format!("Failed to query AgentSeek latest version (curl exit status: {status})")
        } else {
            format!("Failed to query AgentSeek latest version (curl exit status: {status})：{stderr}")
        });
    }
    parse_agentseek_package_version(&output.stdout)
}

fn cli_parts() -> (String, Vec<String>) {
    (agentseek_program(), Vec::new())
}

fn agentseek_update_available(installed: &str, latest: Option<&str>, available: bool) -> bool {
    available && latest.is_some_and(|latest_version| !meets_requirement(installed, latest_version))
}

fn current_cli_status(check_latest: bool) -> Result<CliStatus, String> {
    let requirements = load_runtime_requirements()?;
    let uv = uv_program();
    let uv_version = uv
        .as_deref()
        .and_then(|program| command_version(program, "--version"))
        .unwrap_or_default();
    let uv_path = uv.clone().unwrap_or_default();
    let program = agentseek_program();
    let cli_version = agentseek_command_version(&program)
        .or_else(|| command_version(&program, "--version"))
        .or_else(|| uv_tool_version("agentseek"))
        .unwrap_or_default();
    let cli_available = command_version(&program, "--help").is_some();
    let node_version = resolved_program("node", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let npm_version = resolved_program("npm", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let git_version = resolved_program("git", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let uv_compatible = meets_requirement(&uv_version, &requirements.versions.uv.minimum);
    let node_compatible = meets_requirement(&node_version, &requirements.versions.node.minimum);
    let npm_compatible = meets_requirement(&npm_version, &requirements.versions.npm.minimum);
    let git_compatible = meets_requirement(&git_version, &requirements.versions.git.minimum);
    let cli_latest_version = check_latest
        .then(|| latest_agentseek_version(&requirements).ok())
        .flatten();
    let cli_latest_version_checked = cli_latest_version.is_some();
    let cli_compatible = meets_requirement(&cli_version, &requirements.versions.agentseek.minimum);
    let cli_update_available =
        agentseek_update_available(&cli_version, cli_latest_version.as_deref(), cli_available);
    let platform = platform_id();
    let runtime_root = managed_runtime_root();
    let dependency_commands = dependency_commands(
        &requirements,
        &platform,
        runtime_root.as_deref(),
        !uv_version.is_empty(),
        !git_version.is_empty(),
    );
    let node_managed = runtime_root
        .as_deref()
        .map(|root| managed_node_bin(root, &requirements.versions.node.managed))
        .map(|bin| {
            bin.join(if cfg!(windows) { "node.exe" } else { "node" })
                .is_file()
        })
        .unwrap_or(false);
    let prerequisites_ready = uv_compatible && node_compatible && npm_compatible && cli_compatible;
    let minimum_versions = [
        ("uv", requirements.versions.uv.minimum.as_str()),
        ("node", requirements.versions.node.minimum.as_str()),
        ("npm", requirements.versions.npm.minimum.as_str()),
        ("git", requirements.versions.git.minimum.as_str()),
        (
            "agentseek",
            requirements.versions.agentseek.minimum.as_str(),
        ),
    ]
    .into_iter()
    .map(|(name, version)| (name.to_string(), version.to_string()))
    .collect();
    Ok(CliStatus {
        platform,
        dependency_commands,
        minimum_versions,
        node_managed,
        uv_available: !uv_version.is_empty(),
        uv_path,
        cli_available,
        cli_compatible,
        cli_update_available,
        cli_latest_version: cli_latest_version.unwrap_or_default(),
        cli_latest_version_checked,
        uv_version,
        cli_version,
        node_available: !node_version.is_empty(),
        node_compatible,
        node_version,
        npm_available: !npm_version.is_empty(),
        npm_compatible,
        npm_version,
        git_available: !git_version.is_empty(),
        git_compatible,
        git_version,
        uv_compatible,
        prerequisites_ready,
        install_command: "uv tool install agentseek".to_string(),
    })
}

fn run_cli(args: &[&str], cwd: Option<&Path>) -> Result<CommandResult, String> {
    let (program, prefix) = cli_parts();
    let mut command = configured_command(&program);
    command.args(&prefix).args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let output = command
        .output()
        .map_err(|error| format!("Unable to execute {printable}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(CommandResult {
        code: output.status.code().unwrap_or(1),
        output: combined.trim().to_string(),
        command: printable,
    })
}

fn parse_templates(output: &str) -> Vec<TemplateInfo> {
    let mut templates = Vec::new();
    let mut current: Option<usize> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        let is_template = trimmed.contains('/')
            && !trimmed.contains(' ')
            && !trimmed.starts_with("http")
            && trimmed.split('/').count() == 2;
        if is_template {
            let framework = trimmed.split('/').next().unwrap_or_default().to_string();
            templates.push(TemplateInfo {
                id: trimmed.to_string(),
                name: display_name(trimmed),
                description: String::new(),
                framework,
            });
            current = Some(templates.len() - 1);
        } else if let Some(index) = current {
            if !trimmed.is_empty()
                && !trimmed.chars().all(|character| character == '─')
                && !trimmed.contains("templates)")
            {
                templates[index].description = trimmed.to_string();
                current = None;
            }
        }
    }
    templates
}

fn validate_target(target: &Path) -> Result<(), String> {
    if target.exists() {
        if !target.is_dir() {
            return Err("Target path is not a directory".to_string());
        }
        if target
            .read_dir()
            .map_err(|error| error.to_string())?
            .next()
            .is_some()
        {
            return Err("Target directory must be empty".to_string());
        }
    }
    Ok(())
}

fn instance_target_path(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Instance name cannot be empty".to_string());
    }
    if matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        return Err("Instance name cannot contain path separators".to_string());
    }
    Ok(parent.join(name))
}

fn find_env_example(root: &Path) -> Option<PathBuf> {
    let direct = root.join(".env.example");
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path();
            path.is_dir()
                .then(|| path.join(".env.example"))
                .filter(|candidate| candidate.is_file())
        })
}

fn parse_env(content: &str) -> Vec<EnvVariable> {
    let mut entries = Vec::new();
    let mut comments = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            comments.push(trimmed.trim_start_matches('#').trim().to_string());
            continue;
        }
        if trimmed.is_empty() {
            if !comments.is_empty() {
                comments.push(String::new());
            }
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((key, raw_value)) = assignment.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                let (value, inline_comment) = split_env_value(raw_value.trim());
                if let Some(inline_comment) = inline_comment {
                    comments.push(inline_comment);
                }
                entries.push(EnvVariable {
                    key: key.to_string(),
                    value,
                    comment: comments.join("\n").trim().to_string(),
                    source: "template".to_string(),
                    modified: false,
                });
                comments.clear();
            }
        }
    }
    entries
}

fn split_env_value(raw: &str) -> (String, Option<String>) {
    let mut quote = None;
    let mut escaped = false;
    let mut previous = None;
    for (index, character) in raw.char_indices() {
        if escaped {
            escaped = false;
            previous = Some(character);
            continue;
        }
        if character == '\\' && quote == Some('"') {
            escaped = true;
            previous = Some(character);
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
        } else if character == '#' && quote.is_none() && previous.is_some_and(char::is_whitespace) {
            let value = raw[..index].trim_end().to_string();
            let comment = raw[index + 1..].trim();
            return (value, (!comment.is_empty()).then(|| comment.to_string()));
        }
        previous = Some(character);
    }
    (raw.to_string(), None)
}

#[derive(Deserialize, Default)]
struct LifecycleManifest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    services: HashMap<String, LifecycleServiceSpec>,
}

#[derive(Deserialize, Default)]
struct LifecycleServiceSpec {
    #[serde(default)]
    url: String,
}

fn service_display_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "app" | "frontend" | "web" => "Frontend".to_string(),
        "gateway" | "agent" => "Agent / Gateway".to_string(),
        "copilotkit" => "CopilotKit Runtime".to_string(),
        "ctx" | "contextseek" => "ContextSeek API".to_string(),
        "studio" | "langsmith" => "LangSmith Studio".to_string(),
        _ => name.to_string(),
    }
}

fn service_kind(name: &str) -> (&'static str, bool) {
    match name.to_ascii_lowercase().as_str() {
        "app" | "frontend" | "web" => ("web", true),
        "studio" | "langsmith" | "phoenix" => ("web", false),
        "gateway" | "agent" => ("protocol", false),
        "copilotkit" | "backend" | "langgraph" | "ctx" | "contextseek" => ("api", false),
        "database" | "db" | "seekdb" | "oceanbase" => ("database", false),
        _ => ("other", false),
    }
}

fn replace_url_port(url: &str, port: u16) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let remainder = &url[authority_start..];
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let host_end = if authority.starts_with('[') {
        authority.find(']').map(|index| index + 1)
    } else {
        authority.rfind(':').or(Some(authority.len()))
    };
    let Some(host_end) = host_end else {
        return url.to_string();
    };
    let host = &authority[..host_end];
    if host.is_empty() {
        return url.to_string();
    }
    format!(
        "{}{}:{}{}",
        &url[..authority_start],
        host,
        port,
        &remainder[authority_end..]
    )
}

fn service_env_port(name: &str, env: &HashMap<String, String>) -> Option<u16> {
    let normalized = name.to_ascii_lowercase();
    let mut candidates: Vec<String> = match normalized.as_str() {
        "app" | "frontend" | "web" => vec!["FRONTEND_PORT", "APP_PORT", "WEB_PORT"],
        "gateway" | "agent" => vec![
            "BUB_AG_UI_PORT",
            "AG_UI_PORT",
            "GATEWAY_PORT",
            "AGENT_PORT",
            "BACKEND_PORT",
        ],
        "copilotkit" | "runtime" => vec!["COPILOTKIT_PORT", "RUNTIME_PORT"],
        "backend" | "langgraph" => vec!["BACKEND_PORT", "LANGGRAPH_PORT", "API_PORT"],
        "ctx" | "contextseek" => vec!["CTX_SERVER_PORT", "CONTEXTSEEK_PORT"],
        "studio" | "langsmith" => vec!["STUDIO_PORT", "LANGSMITH_PORT"],
        "phoenix" => vec!["PHOENIX_PORT"],
        _ => Vec::new(),
    }
    .into_iter()
    .map(str::to_string)
    .collect();
    candidates.push(format!("{}_PORT", name.to_ascii_uppercase()));
    candidates.into_iter().find_map(|key| {
        env.get(&key)
            .and_then(|value| value.trim().parse::<u16>().ok())
            .filter(|port| *port > 0)
    })
}

fn enrich_service_endpoints(instance: &mut InstanceRecord) {
    let path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let Some(manifest) = fs::read_to_string(path)
        .ok()
        .and_then(|content| toml::from_str::<LifecycleManifest>(&content).ok())
    else {
        return;
    };
    if instance.project_name.is_none() && !manifest.name.trim().is_empty() {
        instance.project_name = Some(manifest.name.clone());
    }
    instance.lifecycle_version = (manifest.version > 0).then_some(manifest.version);
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    let env = fs::read_to_string(env_path)
        .ok()
        .map(|content| {
            parse_env(&content)
                .into_iter()
                .map(|entry| (entry.key, entry.value))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut services = manifest
        .services
        .into_iter()
        .filter(|(_, service)| !service.url.trim().is_empty())
        .map(|(name, service)| {
            let url = service_env_port(&name, &env)
                .map(|port| replace_url_port(&service.url, port))
                .unwrap_or(service.url);
            (name, url)
        })
        .collect::<Vec<_>>();
    services.sort_by_key(|(name, _)| match name.to_ascii_lowercase().as_str() {
        "gateway" | "agent" => 0,
        "app" | "frontend" | "web" => 1,
        "copilotkit" | "runtime" => 2,
        "studio" | "langsmith" => 3,
        _ => 4,
    });
    instance.service_endpoints = services
        .iter()
        .map(|(name, url)| {
            let (kind, primary) = service_kind(name);
            ServiceEndpoint {
                name: service_display_name(name),
                url: url.clone(),
                kind: kind.to_string(),
                primary,
            }
        })
        .collect();
    for (name, url) in services {
        match name.to_ascii_lowercase().as_str() {
            "gateway" | "agent" => instance.agent_url = Some(url),
            "app" | "frontend" | "web" => instance.ui_url = Some(url),
            "studio" | "langsmith" => instance.studio_url = Some(url),
            _ => {}
        }
    }
}

fn render_env(entries: &[EnvVariable]) -> String {
    let mut output = String::new();
    for entry in entries {
        if !entry.comment.trim().is_empty() {
            for line in entry.comment.lines() {
                output.push_str("# ");
                output.push_str(line.trim());
                output.push('\n');
            }
        }
        output.push_str(&entry.key);
        output.push('=');
        output.push_str(&entry.value);
        output.push_str("\n\n");
    }
    output
}

fn is_local_service_port_key(key: &str) -> bool {
    let normalized = key.to_ascii_uppercase();
    if normalized != "PORT" && !normalized.ends_with("_PORT") {
        return false;
    }
    ![
        "DB",
        "DATABASE",
        "MYSQL",
        "POSTGRES",
        "POSTGRESQL",
        "REDIS",
        "SEEKDB",
        "OCEANBASE",
        "OBSERVER",
        "MONGO",
        "ELASTIC",
        "QDRANT",
    ]
    .iter()
    .any(|external| normalized.contains(external))
}

fn port_is_available(port: u16) -> bool {
    let timeout = Duration::from_millis(150);
    let ipv4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let ipv6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&ipv4, timeout).is_ok()
        || TcpStream::connect_timeout(&ipv6, timeout).is_ok()
    {
        return false;
    }
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        return false;
    }

    // Node commonly listens on an IPv6 wildcard socket that also serves localhost.
    // Skip the IPv6 target check only on systems where IPv6 loopback is unavailable.
    if TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err() {
        return true;
    }
    TcpListener::bind((Ipv6Addr::LOCALHOST, port)).is_ok()
}

fn available_ephemeral_port() -> Result<u16, String> {
    for _ in 0..64 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("Failed to allocate free port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("Failed to read free port: {error}"))?
            .port();
        drop(listener);
        if port_is_available(port) {
            return Ok(port);
        }
    }
    Err("Failed to find a free port available for both IPv4 and IPv6".to_string())
}

fn collect_assigned_ports(state: &DesktopState, exclude_instance_id: Option<&str>) -> HashSet<u16> {
    // Collect lifecycle paths under a brief lock, then read lifecycle.toml outside the lock.
    let lifecycle_paths: Vec<PathBuf> = {
        state
            .data
            .lock()
            .ok()
            .map(|data| {
                data.instances
                    .iter()
                    .filter(|i| Some(i.id.as_str()) != exclude_instance_id)
                    .map(|i| Path::new(&i.work_dir).join(".agentseek/lifecycle.toml"))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut ports = HashSet::new();
    for lifecycle_path in &lifecycle_paths {
        if let Ok(content) = fs::read_to_string(lifecycle_path) {
            if let Ok(manifest) = toml::from_str::<LifecycleManifest>(&content) {
                for (_, service) in &manifest.services {
                    if let Some(port) = extract_url_port(&service.url) {
                        if port > 0 {
                            ports.insert(port);
                        }
                    }
                }
            }
        }
    }
    ports
}

fn extract_url_port(url: &str) -> Option<u16> {
    url.split("://").nth(1).and_then(|rest| {
        rest.split('/').next().and_then(|host_port| {
            host_port
                .rsplit(':')
                .next()
                .and_then(|port_str| port_str.parse().ok())
        })
    })
}

fn resolve_port_conflicts(entries: &mut [EnvVariable]) -> Result<Vec<PortChange>, String> {
    let mut changes = Vec::new();
    let mut selected = HashSet::new();
    for entry in entries.iter_mut() {
        if !is_local_service_port_key(&entry.key) {
            continue;
        }
        let Ok(port) = entry.value.trim().parse::<u16>() else {
            continue;
        };
        if port > 0 && port_is_available(port) && selected.insert(port) {
            continue;
        }
        let mut replacement = available_ephemeral_port()?;
        while selected.contains(&replacement) {
            replacement = available_ephemeral_port()?;
        }
        selected.insert(replacement);
        entry.value = replacement.to_string();
        entry.modified = true;
        changes.push(PortChange {
            key: entry.key.clone(),
            old_port: port,
            new_port: replacement,
        });
    }
    for change in &changes {
        let key_prefix = change.key.trim_end_matches("_PORT");
        let duplicate_old_port = changes
            .iter()
            .filter(|candidate| candidate.old_port == change.old_port)
            .count()
            > 1;
        for entry in entries.iter_mut().filter(|entry| {
            let key = entry.key.to_ascii_uppercase();
            key.contains("URL") && (!duplicate_old_port || key.contains(key_prefix))
        }) {
            let mut updated = entry.value.clone();
            for host in ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"] {
                updated = updated.replace(
                    &format!("{host}:{}", change.old_port),
                    &format!("{host}:{}", change.new_port),
                );
            }
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
        }
    }
    let local_ports = entries
        .iter()
        .filter(|entry| is_local_service_port_key(&entry.key))
        .filter_map(|entry| {
            entry
                .value
                .trim()
                .parse::<u16>()
                .ok()
                .map(|port| (entry.key.trim_end_matches("_PORT").to_string(), port))
        })
        .collect::<Vec<_>>();
    for entry in entries.iter_mut().filter(|entry| {
        entry.key.to_ascii_uppercase().contains("URL")
            && [
                "http://127.0.0.1",
                "https://127.0.0.1",
                "http://localhost",
                "https://localhost",
                "http://0.0.0.0",
                "https://0.0.0.0",
                "http://[::1]",
                "https://[::1]",
            ]
            .iter()
            .any(|prefix| entry.value.starts_with(prefix))
    }) {
        let normalized_key = entry.key.to_ascii_uppercase();
        let Some((_, port)) = local_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        else {
            continue;
        };
        let updated = replace_url_port(&entry.value, *port);
        if updated != entry.value {
            entry.value = updated;
            entry.modified = true;
        }
    }
    Ok(changes)
}

/// Sync `*_URL` env variables with ports resolved in lifecycle.toml so that
/// URLs like `LANGGRAPH_URL` stay in sync even when the `.env` file has no
/// corresponding `*_PORT` variable.
fn sync_env_urls_from_lifecycle(work_dir: &str, entries: &mut [EnvVariable]) {
    let lifecycle_path = Path::new(work_dir).join(".agentseek/lifecycle.toml");
    let Ok(content) = fs::read_to_string(&lifecycle_path) else {
        return;
    };
    let Ok(manifest) = toml::from_str::<LifecycleManifest>(&content) else {
        return;
    };
    // Build a map of service-name-prefix -> port from lifecycle.toml service URLs.
    let lifecycle_ports: Vec<(String, u16)> = manifest
        .services
        .iter()
        .filter_map(|(name, service)| {
            extract_url_port(&service.url).map(|port| (name.to_ascii_uppercase(), port))
        })
        .collect();
    if lifecycle_ports.is_empty() {
        return;
    }
    for entry in entries.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if !normalized_key.contains("URL") {
            continue;
        }
        if ![
            "http://127.0.0.1",
            "https://127.0.0.1",
            "http://localhost",
            "https://localhost",
            "http://0.0.0.0",
            "https://0.0.0.0",
            "http://[::1]",
            "https://[::1]",
        ]
        .iter()
        .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        let Some((_, port)) = lifecycle_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        else {
            continue;
        };
        let updated = replace_url_port(&entry.value, *port);
        if updated != entry.value {
            entry.value = updated;
            entry.modified = true;
        }
    }
}

fn local_env_port_values(entries: &[EnvVariable]) -> Vec<(String, u16)> {
    entries
        .iter()
        .filter(|entry| is_local_service_port_key(&entry.key))
        .filter_map(|entry| {
            entry.value.trim().parse::<u16>().ok().map(|port| {
                (
                    entry
                        .key
                        .to_ascii_uppercase()
                        .trim_end_matches("_PORT")
                        .to_string(),
                    port,
                )
            })
        })
        .collect()
}

fn synchronize_env_entries(target: &mut [EnvVariable], root: &[EnvVariable]) {
    let root_by_key = root
        .iter()
        .map(|entry| (entry.key.to_ascii_uppercase(), entry))
        .collect::<HashMap<_, _>>();
    let local_ports = local_env_port_values(root);
    for entry in target.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if let Some(source) = root_by_key.get(&normalized_key) {
            entry.value = source.value.clone();
            continue;
        }
        if ![
            "http://127.0.0.1",
            "https://127.0.0.1",
            "http://localhost",
            "https://localhost",
            "http://0.0.0.0",
            "https://0.0.0.0",
            "http://[::1]",
            "https://[::1]",
        ]
        .iter()
        .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        if let Some((_, port)) = local_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        {
            entry.value = replace_url_port(&entry.value, *port);
        }
    }
}

fn lifecycle_section_service(header: &str) -> Option<String> {
    let header = header.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (group, service) = header.split_once('.')?;
    matches!(group.trim(), "services" | "checks").then(|| {
        service
            .trim()
            .trim_matches(|character| matches!(character, '\'' | '"'))
            .to_string()
    })
}

fn lifecycle_section_env_key(header: &str) -> Option<String> {
    let header = header.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (group, key) = header.split_once('.')?;
    (group.trim() == "env").then(|| {
        key.trim()
            .trim_matches(|character| matches!(character, '\'' | '"'))
            .to_string()
    })
}

fn replace_toml_string_line(
    line: &str,
    keys: &[&str],
    update: impl FnOnce(&str) -> String,
) -> String {
    let Some(equals) = line.find('=') else {
        return line.to_string();
    };
    if !keys.contains(&line[..equals].trim()) {
        return line.to_string();
    }
    let value = &line[equals + 1..];
    let Some(quote_start) = value.find(['\'', '"']) else {
        return line.to_string();
    };
    let quote = value.as_bytes()[quote_start];
    let Some(quote_end_offset) = value.as_bytes()[quote_start + 1..]
        .iter()
        .position(|candidate| *candidate == quote)
    else {
        return line.to_string();
    };
    let value_start = equals + 1 + quote_start + 1;
    let value_end = value_start + quote_end_offset;
    let current = &line[value_start..value_end];
    let updated = update(current);
    if updated == current {
        return line.to_string();
    }
    format!("{}{}{}", &line[..value_start], updated, &line[value_end..])
}

fn replace_lifecycle_url_line(line: &str, port: u16) -> String {
    replace_toml_string_line(line, &["url", "target"], |url| {
        if url.contains("${") {
            return url.to_string();
        }
        replace_url_port(url, port)
    })
}

fn toml_basic_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn replace_lifecycle_name_line(line: &str, project_name: &str) -> String {
    let (body, line_ending) = line
        .strip_suffix("\r\n")
        .map(|body| (body, "\r\n"))
        .or_else(|| line.strip_suffix('\n').map(|body| (body, "\n")))
        .unwrap_or((line, ""));
    let Some(equals) = body.find('=') else {
        return line.to_string();
    };
    if body[..equals].trim() != "name" {
        return line.to_string();
    }

    let value = &body[equals + 1..];
    let mut quote = None;
    let mut escaped = false;
    let mut comment_start = None;
    for (index, character) in value.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(active) if character == active => quote = None,
            Some(_) => {}
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character == '#' => {
                comment_start = Some(index);
                break;
            }
            None => {}
        }
    }

    let value_end = comment_start.unwrap_or(value.len());
    let old_value = &value[..value_end];
    let leading_len = old_value.len() - old_value.trim_start().len();
    let trailing_start = old_value.trim_end().len();
    let leading = &old_value[..leading_len];
    let trailing = &old_value[trailing_start..];
    let comment = &value[value_end..];
    format!(
        "{}{}{}{}{}{}",
        &body[..=equals],
        leading,
        toml_basic_string(project_name),
        trailing,
        comment,
        line_ending
    )
}

fn synchronize_lifecycle_project_name_content(content: &str, project_name: &str) -> String {
    let mut in_root = true;
    let mut found = false;
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if in_root && trimmed.starts_with('[') {
            in_root = false;
        }
        if in_root && !found {
            let updated = replace_lifecycle_name_line(line, project_name);
            found = updated != line
                || line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "name");
            output.push_str(&updated);
        } else {
            output.push_str(line);
        }
    }
    output
}

fn replace_project_name_in_directory(
    dir: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current)
            .map_err(|error| format!("Failed to read directory {}: {error}", current.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if !path.to_string_lossy().contains("/.git") {
                    stack.push(path);
                }
            } else if path.is_file() {
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                if content.contains(old_name) {
                    let updated = content.replace(old_name, new_name);
                    fs::write(&path, updated)
                        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
                }
            }
        }
    }
    Ok(())
}

/// Rewrites hardcoded port mappings in docker-compose.yml to `${ENV_KEY:-ORIGINAL}:CONTAINER` variable references.
///
/// Template docker-compose.yml typically hardcodes ports (e.g. `"127.0.0.1:2881:2881"`);
/// this function builds a variable reference `${SEEKDB_PORT:-2881}:2881` from the **original** port number,
/// so docker-compose automatically reads `SEEKDB_PORT` from .env.
///
/// Advantages:
/// - No need to re-sync docker-compose.yml when ports change
/// - Original port preserved as fallback; works even if .env is missing
/// - Idempotent: lines already using `${...}` syntax are not modified again
fn sync_docker_compose_port_mappings(content: &str, entries: &[EnvVariable]) -> String {
    let env_keys: HashSet<String> = entries
        .iter()
        .map(|e| e.key.to_ascii_uppercase())
        .collect();

    let mut updated = content.to_string();
    let mut current_service: Option<String> = None;

    for line in content.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        // Detect service definition: 2-space indent + `name:`
        if indent == 2 && trimmed.ends_with(':') && !trimmed.starts_with('-') {
            let name = trimmed.trim_end_matches(':');
            if name != "services" {
                current_service = Some(name.to_string());
            } else {
                current_service = None;
            }
        }

        // Detect port mapping line: `      - "127.0.0.1:HOST:CONTAINER"`
        if trimmed.starts_with("- ") && trimmed.contains(':') {
            if let Some(ref service_name) = current_service {
                let env_key = format!("{}_PORT", service_name.to_ascii_uppercase());
                if !env_keys.contains(&env_key) {
                    continue;
                }
                // Extract host:container from port mapping
                // Supported formats: "127.0.0.1:2881:2881" or "2881:2881"
                let mapping = trimmed
                    .trim_start_matches("- ")
                    .trim()
                    .trim_matches('"');
                // Already using variable reference syntax; skip (idempotent)
                if mapping.contains("${") {
                    continue;
                }
                if let Some(colon_pos) = mapping.rfind(':') {
                    let container_port = &mapping[colon_pos + 1..];
                    // container_port must be pure digits; otherwise it is a volume mapping, not a port mapping
                    if !container_port.chars().all(|c| c.is_ascii_digit()) {
                        continue;
                    }
                    let host_part = &mapping[..colon_pos];
                    // host_part may be "127.0.0.1:2881" or "2881"
                    if let Some(host_colon) = host_part.rfind(':') {
                        let original_host_port = &host_part[host_colon + 1..];
                        // host_port must be pure digits
                        if !original_host_port.chars().all(|c| c.is_ascii_digit()) {
                            continue;
                        }
                        let prefix = &host_part[..host_colon];
                        let old_mapping = format!(
                            "{}:{}:{}",
                            prefix, original_host_port, container_port
                        );
                        let new_mapping = format!(
                            "{}:${{{}:-{}}}:{}",
                            prefix, env_key, original_host_port, container_port
                        );
                        updated = updated.replace(&old_mapping, &new_mapping);
                    } else if host_part.chars().all(|c| c.is_ascii_digit()) {
                        // Format: "2881:2881" (no IP prefix)
                        let old_mapping = format!("{}:{}", host_part, container_port);
                        let new_mapping = format!(
                            "${{{}:-{}}}:{}",
                            env_key, host_part, container_port
                        );
                        updated = updated.replace(&old_mapping, &new_mapping);
                    }
                }
            }
        }
    }

    updated
}

fn synchronize_instance_project_name(
    root: &Path,
    project_name: &str,
) -> Result<Option<PathBuf>, String> {
    let lifecycle_path = root.join(".agentseek/lifecycle.toml");
    if !lifecycle_path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(&lifecycle_path)
        .map_err(|error| format!("Unable to read {}: {error}", lifecycle_path.display()))?;
    let updated = synchronize_lifecycle_project_name_content(&content, project_name);
    if updated == content {
        return Ok(None);
    }
    fs::write(&lifecycle_path, updated)
        .map_err(|error| format!("Unable to write {}: {error}", lifecycle_path.display()))?;
    Ok(Some(lifecycle_path))
}

fn synchronize_lifecycle_content(content: &str, root: &[EnvVariable]) -> String {
    let env = root
        .iter()
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<HashMap<_, _>>();
    let mut service = None;
    let mut env_key = None;
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            service = lifecycle_section_service(trimmed);
            env_key = lifecycle_section_env_key(trimmed);
        }
        if let Some(port) = service
            .as_deref()
            .and_then(|name| service_env_port(name, &env))
        {
            output.push_str(&replace_lifecycle_url_line(line, port));
        } else if let Some(value) = env_key
            .as_deref()
            .filter(|key| is_local_service_port_key(key))
            .and_then(|key| env.get(key))
        {
            output.push_str(&replace_toml_string_line(line, &["default"], |_| {
                value.clone()
            }));
        } else {
            output.push_str(line);
        }
    }
    output
}

/// Extracts ports from [services.*] in lifecycle.toml content.
/// Returns a list of (service name uppercase, port) tuples.
fn extract_lifecycle_service_ports(content: &str) -> Vec<(String, u16)> {
    let mut result = Vec::new();
    let mut current_service: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            current_service = None;
        }
        if let Some(service) = trimmed
            .strip_prefix("[services.")
            .and_then(|r| r.strip_suffix(']'))
        {
            current_service = Some(service.to_string());
            continue;
        }
        if let Some(ref service) = current_service {
            if let Some(eq_pos) = trimmed.find('=') {
                let after_eq = trimmed[eq_pos + 1..].trim();
                let url = after_eq.trim_matches('"');
                if let Some(port) = extract_url_port(url) {
                    result.push((service.to_ascii_uppercase(), port));
                }
            }
        }
    }
    result
}

/// Inserts --port argument into process command.
/// Supports two TOML formats:
///   - Array: ["langgraph", "dev"] -> ["langgraph", "dev", "--port", "59064"]
///   - String: "langgraph dev" -> "langgraph dev --port 59064"
fn insert_command_port(line: &str, port: u16) -> Option<String> {
    // Array format: insert ", "--port", "PORT"" before the last ]
    if let Some(bracket) = line.rfind(']') {
        let before = &line[..bracket];
        let port_arg = format!(", \"--port\", \"{port}\"");
        return Some(format!("{before}{port_arg}]"));
    }
    // String format: insert " --port PORT" before the last "
    if let Some(quote) = line.rfind('"') {
        let before = &line[..quote];
        let after = &line[quote..];
        return Some(format!("{before} --port {port}{after}"));
    }
    None
}

/// Extracts command token list from `command = ...` line for command type detection.
/// Supports two TOML formats: array `["npm", "install"]` and string `"npm install"`.
fn command_tokens(line: &str) -> Vec<String> {
    let after_eq = line.split_once('=').map(|(_, rest)| rest).unwrap_or(line);
    let mut quoted: Vec<String> = Vec::new();
    let mut in_quote = false;
    let mut cur = String::new();
    for ch in after_eq.chars() {
        if ch == '"' {
            if in_quote {
                quoted.push(std::mem::take(&mut cur));
            }
            in_quote = !in_quote;
        } else if in_quote {
            cur.push(ch);
        }
    }
    match quoted.len() {
        0 => Vec::new(),
        // String format: split entire quoted content by whitespace
        1 => quoted[0]
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        // Array format: each quoted string is a token
        _ => quoted,
    }
}

/// Determines whether a command accepts the `--port` argument.
/// Only commands known to support `--port` (e.g. `langgraph dev`, `vite`, `uvicorn`)
/// will have `--port` injected or replaced; other commands (e.g. `npm run dev`, `docker compose`,
/// `uv sync`, `sh -lc`, etc.) neither inject nor keep existing `--port`.
fn accepts_port_flag(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|t| matches!(t.to_ascii_lowercase().as_str(), "langgraph" | "vite" | "uvicorn"))
}

/// Removes `--port` and its port value from a process command line,
/// used to clean up port arguments erroneously injected into install commands.
/// Supports both TOML array and string formats; returns the cleaned line.
fn remove_command_port(line: &str) -> Option<String> {
    let flag_pos = line.find("--port")?;
    let after_flag = &line[flag_pos..];
    // Array format: "--port", "PORT"
    let after_comma = after_flag.find(',').unwrap_or(after_flag.len());
    let after_comma_s = &after_flag[after_comma..];
    if let Some(q) = after_comma_s.find('"') {
        let rest = &after_comma_s[q + 1..];
        if let Some(q2) = rest.find('"') {
            let port_str = &rest[..q2];
            if port_str.parse::<u16>().is_ok() {
                let target = format!(", \"--port\", \"{}\"", port_str);
                let cleaned = line.replace(&target, "");
                if cleaned != line {
                    return Some(cleaned);
                }
            }
        }
    }
    // String format: "--port PORT"
    let after_flag_trimmed = after_flag.strip_prefix("--port").unwrap_or(after_flag);
    let after_space = after_flag_trimmed.trim_start();
    let num_end = after_space
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_space.len());
    if num_end > 0 {
        let port_str = &after_space[..num_end];
        if port_str.parse::<u16>().is_ok() {
            let target = format!(" --port {}", port_str);
            let cleaned = line.replace(&target, "");
            if cleaned != line {
                return Some(cleaned);
            }
        }
    }
    None
}

fn sync_process_command_ports(content: &str, entries: &[EnvVariable]) -> String {
    // Find process commands with --port flags and replace port values to match .env entries.
    // When .env has no *_PORT, extract port from lifecycle.toml [services.*] URL.
    let lifecycle_ports = extract_lifecycle_service_ports(content);
    let mut updated = content.to_string();
    let mut current_process: Option<String> = None;
    let mut seen_processes: HashSet<String> = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[processes.") {
            current_process = trimmed
                .strip_prefix("[processes.")
                .and_then(|r| r.strip_suffix(']'))
                .map(|n| n.to_string());
            if let Some(ref name) = current_process {
                seen_processes.insert(name.to_ascii_uppercase());
            }
        } else if trimmed.starts_with('[') {
            // Leaving [processes.*] section: reset current_process,
            // prevent command lines in [tasks.*] and later sections from being injected with --port.
            current_process = None;
        } else if let Some(ref proc_name) = current_process {
            if !trimmed.starts_with("command") {
                continue;
            }
            let port_key = format!("{}_PORT", proc_name.to_ascii_uppercase());
            // First look for *_PORT in .env; if not found, extract from lifecycle.toml [services.*] URL
            let new_port = entries
                .iter()
                .find(|e| e.key.to_ascii_uppercase() == port_key)
                .and_then(|e| e.value.trim().parse::<u16>().ok())
                .or_else(|| {
                    lifecycle_ports
                        .iter()
                        .find(|(name, _)| *name == proc_name.to_ascii_uppercase())
                        .map(|(_, port)| *port)
                });
            // Whitelist: only commands known to accept --port (langgraph/vite/uvicorn) are injected or replaced.
            // Commands that do not accept --port (npm run dev, docker compose, uv sync, sh -lc, etc.):
            // clean up existing --port (erroneously injected by old logic) and do not inject new --port.
            if !accepts_port_flag(&command_tokens(line)) {
                if line.contains("--port") {
                    if let Some(cleaned) = remove_command_port(line) {
                        updated = updated.replace(line, &cleaned);
                    }
                }
                continue;
            }

            if let Some(new_port) = new_port {
                // Find the port number following "--port" in the command.
                if let Some(flag_pos) = line.find("--port") {
                    let after_flag = &line[flag_pos..];
                    // Format 1: array "--port", "2024" — inside quotes after comma
                    let after_comma = after_flag.find(',').unwrap_or(after_flag.len());
                    if let Some(replaced) = (|| {
                        let after_comma_s = &after_flag[after_comma..];
                        let q = after_comma_s.find('"')?;
                        let rest = &after_comma_s[q + 1..];
                        let q2 = rest.find('"')?;
                        let current_port_str = &rest[..q2];
                        let current_port = current_port_str.parse::<u16>().ok()?;
                        if current_port != new_port {
                            let old = format!("--port\", \"{current_port}\"");
                            let new_val = format!("--port\", \"{new_port}\"");
                            return Some(line.replace(&old, &new_val));
                        }
                        None
                    })() {
                        updated = updated.replace(line, &replaced);
                        continue;
                    }
                    // Format 2: string "--port 2024" — digits after space
                    let after_flag_trimmed =
                        after_flag.strip_prefix("--port").unwrap_or(after_flag);
                    let after_space = after_flag_trimmed.trim_start();
                    if let Some(replaced) = (|| {
                        let num_end = after_space
                            .find(|c: char| !c.is_ascii_digit())
                            .unwrap_or(after_space.len());
                        if num_end == 0 {
                            return None;
                        }
                        let current_port_str = &after_space[..num_end];
                        let current_port = current_port_str.parse::<u16>().ok()?;
                        if current_port != new_port {
                            let old = format!("--port {current_port}");
                            let new_val = format!("--port {new_port}");
                            return Some(line.replace(&old, &new_val));
                        }
                        None
                    })() {
                        updated = updated.replace(line, &replaced);
                    }
                } else {
                    // No --port in command; insert
                    if let Some(replaced) = insert_command_port(line, new_port) {
                        updated = updated.replace(line, &replaced);
                    }
                }
            }
        }
    }

    updated
}

fn synchronize_instance_port_configs(
    root: &Path,
    entries: &[EnvVariable],
) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    let lifecycle_path = root.join(".agentseek/lifecycle.toml");
    if lifecycle_path.is_file() {
        let content = fs::read_to_string(&lifecycle_path)
            .map_err(|error| format!("Failed to read {}: {error}", lifecycle_path.display()))?;
        let updated = synchronize_lifecycle_content(&content, entries);
        let updated = sync_process_command_ports(&updated, entries);
        if updated != content {
            fs::write(&lifecycle_path, updated)
                .map_err(|error| format!("Failed to write {}: {error}", lifecycle_path.display()))?;
            written.push(lifecycle_path);
        }
    }

    let frontend_example_path = root.join("frontend/.env.example");
    if frontend_example_path.is_file() {
        let frontend_env_path = root.join("frontend/.env");
        let example = parse_env(
            &fs::read_to_string(&frontend_example_path).map_err(|error| {
                format!("Failed to read {}: {error}", frontend_example_path.display())
            })?,
        );
        let existing_content = fs::read_to_string(&frontend_env_path).ok();
        let existing = existing_content
            .as_deref()
            .map(parse_env)
            .unwrap_or_default();
        let existing_by_key = existing
            .iter()
            .map(|entry| (entry.key.to_ascii_uppercase(), entry))
            .collect::<HashMap<_, _>>();
        let example_keys = example
            .iter()
            .map(|entry| entry.key.to_ascii_uppercase())
            .collect::<HashSet<_>>();
        let mut frontend = example
            .into_iter()
            .map(|mut entry| {
                if let Some(saved) = existing_by_key.get(&entry.key.to_ascii_uppercase()) {
                    entry.value = saved.value.clone();
                    if !saved.comment.trim().is_empty() {
                        entry.comment = saved.comment.clone();
                    }
                }
                entry
            })
            .collect::<Vec<_>>();
        frontend.extend(
            existing
                .into_iter()
                .filter(|entry| !example_keys.contains(&entry.key.to_ascii_uppercase())),
        );
        synchronize_env_entries(&mut frontend, entries);
        let rendered = render_env(&frontend);
        if existing_content.as_deref() != Some(rendered.as_str()) {
            fs::write(&frontend_env_path, rendered)
                .map_err(|error| format!("Failed to write {}: {error}", frontend_env_path.display()))?;
            written.push(frontend_env_path);
        }
    }

    let compose_path = root.join("docker-compose.yml");
    if compose_path.is_file() {
        let compose_content = fs::read_to_string(&compose_path)
            .map_err(|error| format!("Failed to read {}: {error}", compose_path.display()))?;
        let compose_updated = sync_docker_compose_port_mappings(&compose_content, entries);
        if compose_updated != compose_content {
            fs::write(&compose_path, &compose_updated)
                .map_err(|error| format!("Failed to write {}: {error}", compose_path.display()))?;
            written.push(compose_path);
        }
    }
    Ok(written)
}

fn synchronize_instance_configs_from_env(
    instance: &InstanceRecord,
) -> Result<Vec<PathBuf>, String> {
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    if !env_path.is_file() {
        return Ok(Vec::new());
    }
    let entries = parse_env(
        &fs::read_to_string(&env_path)
            .map_err(|error| format!("Failed to read {}: {error}", env_path.display()))?,
    );
    let root = Path::new(&instance.work_dir);
    let mut written = synchronize_instance_project_name(root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(root, &entries)? {
        if !written.contains(&path) {
            written.push(path);
        }
    }
    Ok(written)
}

fn port_change_details(changes: &[PortChange]) -> String {
    changes
        .iter()
        .map(|change| format!("{}: {} -> {}", change.key, change.old_port, change.new_port))
        .collect::<Vec<_>>()
        .join("\n")
}

fn recheck_instance_ports(
    state: &DesktopState,
    instance: &InstanceRecord,
) -> Result<Vec<PortChange>, String> {
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| "Instance .env has not been generated yet".to_string())?;
    let mut entries = parse_env(&fs::read_to_string(&env_path).map_err(|error| error.to_string())?);
    let _reserved = collect_assigned_ports(state, Some(&instance.id));
    let changes = resolve_port_conflicts(&mut entries)?;
    sync_env_urls_from_lifecycle(&instance.work_dir, &mut entries);
    if !changes.is_empty() || entries.iter().any(|e| e.modified) {
        fs::write(&env_path, render_env(&entries)).map_err(|error| error.to_string())?;
    }
    let root = PathBuf::from(&instance.work_dir);
    let mut synchronized = synchronize_instance_project_name(&root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(&root, &entries)? {
        if !synchronized.contains(&path) {
            synchronized.push(path);
        }
    }
    if changes.is_empty() {
        if !synchronized.is_empty() {
            state.log(
                Some(&instance.id),
                &instance.name,
                "config",
                "info",
                format!(
                    "Runtime configs updated based on instance .env\n{}",
                    synchronized
                        .iter()
                        .map(|path| format!("  {}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                None,
            );
        }
        return Ok(changes);
    }
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for entry in entries.iter().filter(|entry| entry.modified) {
            if let Some(saved) = data.vault.iter_mut().find(|saved| saved.key == entry.key) {
                saved.value = entry.value.clone();
                saved.comment = entry.comment.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
            } else {
                let mut saved = entry.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
                data.vault.push(saved);
            }
        }
    }
    state.persist_current_vault()?;
    state.log(
        Some(&instance.id),
        &instance.name,
        "config",
        "warning",
        format!(
            "Pre-deployment port recheck found local port conflicts; ports reassigned and synced to instance runtime configs and env vault\nPort changes:\n{}\nSynced files:\n  {}\n{}",
            port_change_details(&changes),
            env_path.display(),
            synchronized
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        None,
    );
    Ok(changes)
}

fn merge_env_entries(source: &[EnvVariable], vault: &[EnvVariable]) -> Vec<EnvVariable> {
    let vault_by_key: HashMap<_, _> = vault
        .iter()
        .map(|entry| (entry.key.clone(), entry))
        .collect();
    source
        .iter()
        .cloned()
        .map(|mut entry| {
            if let Some(saved) = vault_by_key.get(&entry.key) {
                if entry.comment.is_empty() {
                    entry.comment = saved.comment.clone();
                }
                if !saved.value.trim().is_empty() {
                    entry.value = saved.value.clone();
                    entry.source = "vault".to_string();
                }
            }
            entry
        })
        .collect()
}

fn merged_env(state: &DesktopState, source: &[EnvVariable]) -> Vec<EnvVariable> {
    let vault = state
        .data
        .lock()
        .map(|data| data.vault.clone())
        .unwrap_or_default();
    merge_env_entries(source, &vault)
}

fn instance_by_id(state: &DesktopState, instance_id: &str) -> Result<InstanceRecord, String> {
    state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .instances
        .iter()
        .find(|instance| instance.id == instance_id)
        .cloned()
        .ok_or_else(|| "Instance not found".to_string())
}

fn update_instance(state: &DesktopState, record: InstanceRecord) -> Result<(), String> {
    state.persist_instance(&record)?;
    let mut data = state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?;
    let existing = data
        .instances
        .iter_mut()
        .find(|instance| instance.id == record.id)
        .ok_or_else(|| "Instance not found".to_string())?;
    *existing = record.clone();
    Ok(())
}

#[tauri::command]
async fn cli_status(check_latest: Option<bool>) -> Result<CliStatus, String> {
    tauri::async_runtime::spawn_blocking(move || current_cli_status(check_latest.unwrap_or(true)))
        .await
        .map_err(|error| error.to_string())?
}

fn run_dependency_command(program: &str, args: &[&str], printable: &str) -> Result<String, String> {
    let output = configured_command(program)
        .args(args)
        .output()
        .map_err(|error| format!("Failed to execute {printable}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string();
    if output.status.success() {
        Ok(combined)
    } else if combined.is_empty() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by system".to_string());
        Err(format!("Command execution failed: {printable} (exit status: {status})"))
    } else {
        Err(combined)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn required_runtime_dependencies(status: &CliStatus) -> Vec<String> {
    let mut dependencies = Vec::new();
    if !status.uv_compatible {
        dependencies.push("uv".to_string());
    }
    if !status.node_compatible || !status.npm_compatible {
        dependencies.push("node/npm".to_string());
    }
    if !status.cli_compatible || status.cli_update_available {
        dependencies.push("agentseek".to_string());
    }
    dependencies
}

fn windows_uv_installer(requirements: &RuntimeRequirements) -> String {
    requirements
        .sources
        .uv_installer_windows
        .clone()
        .unwrap_or_else(|| {
            requirements
                .sources
                .uv_installer
                .strip_suffix(".sh")
                .map(|value| format!("{value}.ps1"))
                .unwrap_or_else(|| requirements.sources.uv_installer.clone())
        })
}

fn posix_runtime_install_script(
    requirements: &RuntimeRequirements,
    status: &CliStatus,
    task_dir: &Path,
    runtime_root: &Path,
) -> String {
    let status_file = task_dir.join("status.json");
    let log_file = task_dir.join("install.log");
    let nvm_dir = runtime_root.join("nvm");
    let nvm_installer = requirements
        .sources
        .nvm_installer_template
        .replace("{version}", &requirements.versions.nvm.managed);
    let node_major = numeric_version(&requirements.versions.node.managed)
        .first()
        .copied()
        .unwrap_or_default();
    let mut lines = vec![
        "#!/usr/bin/env bash".to_string(),
        "set -eo pipefail".to_string(),
        format!("STATUS_FILE={}", shell_quote(&status_file.to_string_lossy())),
        format!("LOG_FILE={}", shell_quote(&log_file.to_string_lossy())),
        format!("TASK_DIR={}", shell_quote(&task_dir.to_string_lossy())),
        "mkdir -p \"$(dirname \"$STATUS_FILE\")\"".to_string(),
        ": > \"$LOG_FILE\"".to_string(),
        "STAGE=starting".to_string(),
        "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"starting\"}' > \"$STATUS_FILE\"".to_string(),
        "exec > >(tee -a \"$LOG_FILE\") 2>&1".to_string(),
        "on_exit() {".to_string(),
        "  code=$?".to_string(),
        "  if [ \"$code\" -ne 0 ]; then".to_string(),
        "    printf '{\"status\":\"failed\",\"stage\":\"%s\",\"code\":%s}\\n' \"$STAGE\" \"$code\" > \"$STATUS_FILE\"".to_string(),
        "    printf '\\nInstallation failed. Press Enter to close this terminal.\\n'".to_string(),
        "    read -r _ || true".to_string(),
        "  fi".to_string(),
        "}".to_string(),
        "trap on_exit EXIT".to_string(),
        "export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH\"".to_string(),
        "run_shell_installer() {".to_string(),
        "  url=$1".to_string(),
        "  label=$2".to_string(),
        "  installer_file=\"$TASK_DIR/${label}-installer.sh\"".to_string(),
        "  attempt=1".to_string(),
        "  while [ \"$attempt\" -le 3 ]; do".to_string(),
        "    echo \"Installing $label (attempt $attempt/3)\"".to_string(),
        "    rm -f \"$installer_file\" \"$installer_file.tmp\"".to_string(),
        "    if command -v curl >/dev/null 2>&1; then".to_string(),
        "      curl -fL --silent --show-error --connect-timeout 10 --max-time 60 --retry 1 --retry-all-errors --retry-delay 2 --output \"$installer_file.tmp\" \"$url\" || true".to_string(),
        "    elif command -v wget >/dev/null 2>&1; then".to_string(),
        "      wget -qO \"$installer_file.tmp\" --timeout=30 --tries=3 --waitretry=2 \"$url\" || true".to_string(),
        "    else".to_string(),
        "      echo \"Neither curl nor wget is available; cannot install $label.\" >&2; return 1".to_string(),
        "    fi".to_string(),
        "    if [ -s \"$installer_file.tmp\" ] && [ \"$(wc -c < \"$installer_file.tmp\")\" -ge 1024 ] && bash -n \"$installer_file.tmp\"; then".to_string(),
        "      mv \"$installer_file.tmp\" \"$installer_file\"".to_string(),
        "      if bash \"$installer_file\"; then return 0; fi".to_string(),
        "    else".to_string(),
        "      echo \"Downloaded $label installer is incomplete or invalid; it will not be executed.\" >&2".to_string(),
        "    fi".to_string(),
        "    rm -f \"$installer_file\" \"$installer_file.tmp\"".to_string(),
        "    if [ \"$attempt\" -lt 3 ]; then echo \"$label download failed. Retrying in 3 seconds.\"; sleep 3; fi".to_string(),
        "    attempt=$((attempt + 1))".to_string(),
        "  done".to_string(),
        "  return 1".to_string(),
        "}".to_string(),
        "echo 'AgentSeek Desktop runtime installation'".to_string(),
        "echo '================================='".to_string(),
    ];

    if !status.uv_compatible {
        lines.extend([
            "STAGE=uv".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"uv\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[1/3] Install or upgrade uv'".to_string(),
        ]);
        lines.push(format!(
            "run_shell_installer {} uv",
            shell_quote(&requirements.sources.uv_installer)
        ));
        lines.push("UV_BIN=\"$HOME/.local/bin/uv\"".to_string());
    } else {
        lines.push(format!("UV_BIN={}", shell_quote(&status.uv_path)));
        lines.push("echo '[1/3] uv already meets the requirement; skipping'".to_string());
    }

    if !status.node_compatible || !status.npm_compatible {
        lines.extend([
            "STAGE=node".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"node\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[2/3] Install AgentSeek Desktop private Node.js / npm'".to_string(),
            "unset npm_config_prefix".to_string(),
            format!("export NVM_DIR={}", shell_quote(&nvm_dir.to_string_lossy())),
            "export PROFILE=/dev/null".to_string(),
            "mkdir -p \"$NVM_DIR\"".to_string(),
            {
                let nvm_gitee = format!(
                    "https://gitee.com/mirrors/nvm/raw/v{}/install.sh",
                    &requirements.versions.nvm.managed
                );
                let nvm_proxy = format!("https://ghproxy.net/{}", nvm_installer);
                format!(
                    "if [ ! -s \"$NVM_DIR/nvm.sh\" ]; then run_shell_installer {primary} nvm || run_shell_installer {gitee} nvm || run_shell_installer {proxy} nvm; fi",
                    primary = shell_quote(&nvm_installer),
                    gitee = shell_quote(&nvm_gitee),
                    proxy = shell_quote(&nvm_proxy),
                )
            },
            ". \"$NVM_DIR/nvm.sh\"".to_string(),
            "if curl -fsI --connect-timeout 2 --max-time 3 \"https://cdn.npmmirror.com/binaries/node/v24.18.0/SHASUMS256.txt\" > /dev/null 2>&1; then export NVM_NODEJS_ORG_MIRROR=https://cdn.npmmirror.com/binaries/node; fi".to_string(),
            format!("nvm install {node_major}"),
            "node --version".to_string(),
            "npm --version".to_string(),
        ]);
    } else {
        lines
            .push("echo '[2/3] Node.js / npm already meet the requirements; skipping'".to_string());
    }

    if !status.cli_compatible || status.cli_update_available {
        lines.extend([
            "STAGE=agentseek".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"agentseek\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[3/3] Install or upgrade AgentSeek CLI'".to_string(),
            "if [ ! -x \"${UV_BIN:-}\" ]; then UV_BIN=\"$(command -v uv)\"; fi".to_string(),
            "\"$UV_BIN\" tool install --upgrade agentseek".to_string(),
            "\"$UV_BIN\" tool update-shell".to_string(),
            "AGENTSEEK_BIN=\"$HOME/.local/bin/agentseek\"".to_string(),
            "if [ ! -x \"$AGENTSEEK_BIN\" ]; then AGENTSEEK_BIN=\"$(command -v agentseek)\"; fi"
                .to_string(),
            "\"$AGENTSEEK_BIN\" version".to_string(),
        ]);
    } else {
        lines
            .push("echo '[3/3] AgentSeek CLI already meets the requirement; skipping'".to_string());
    }

    lines.extend([
        "STAGE=complete".to_string(),
        "printf '%s\\n' '{\"status\":\"success\",\"stage\":\"complete\",\"code\":0}' > \"$STATUS_FILE\"".to_string(),
        "trap - EXIT".to_string(),
        "kill 0".to_string(),
        String::new(),
    ]);
    lines.join("\n")
}

fn windows_runtime_install_script(
    requirements: &RuntimeRequirements,
    status: &CliStatus,
    task_dir: &Path,
    runtime_root: &Path,
) -> String {
    let status_file = task_dir.join("status.json");
    let log_file = task_dir.join("install.log");
    let architecture = if cfg!(target_arch = "aarch64") {
        "win-arm64"
    } else {
        "win-x64"
    };
    let node_version = &requirements.versions.node.managed;
    let archive_name = format!("node-v{node_version}-{architecture}.zip");
    let node_root = runtime_root.join(format!("node-v{node_version}-{architecture}"));
    let node_url = format!(
        "{}/v{node_version}/{archive_name}",
        requirements.sources.node_distribution.trim_end_matches('/')
    );
    let checksum_url = format!(
        "{}/v{node_version}/SHASUMS256.txt",
        requirements.sources.node_distribution.trim_end_matches('/')
    );
    let uv_installer = windows_uv_installer(requirements);
    let mut lines = vec![
        "$ErrorActionPreference = 'Stop'".to_string(),
        format!(
            "$StatusFile = {}",
            powershell_quote(&status_file.to_string_lossy())
        ),
        format!(
            "$LogFile = {}",
            powershell_quote(&log_file.to_string_lossy())
        ),
        format!(
            "$TaskDir = {}",
            powershell_quote(&task_dir.to_string_lossy())
        ),
        "New-Item -ItemType Directory -Force -Path (Split-Path $StatusFile) | Out-Null".to_string(),
        "$Stage = 'starting'".to_string(),
        "'{\"status\":\"running\",\"stage\":\"starting\"}' | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "Start-Transcript -Path $LogFile -Force".to_string(),
        "function Invoke-DownloadWithRetry {".to_string(),
        "  param([string]$Uri, [string]$OutFile, [string]$Label)".to_string(),
        "  for ($Attempt = 1; $Attempt -le 6; $Attempt++) {".to_string(),
        "    try {".to_string(),
        "      Write-Host \"Downloading $Label (attempt $Attempt/6)\"".to_string(),
        "      Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile".to_string(),
        "      return".to_string(),
        "    } catch {".to_string(),
        "      if ($Attempt -eq 6) { throw }".to_string(),
        "      Write-Host 'Download failed. If security software prompts, allow the download. Retrying in 10 seconds.'".to_string(),
        "      Start-Sleep -Seconds 10".to_string(),
        "    }".to_string(),
        "  }".to_string(),
        "}".to_string(),
        "try {".to_string(),
        "  Write-Host 'AgentSeek Desktop runtime installation'".to_string(),
    ];
    if !status.uv_compatible {
        lines.push("  $Stage = 'uv'; ('{\"status\":\"running\",\"stage\":\"uv\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string());
        if status.uv_available && !status.uv_path.is_empty() {
            lines.push("  Write-Host 'An outdated uv was found. The required version will be installed in the current user tool directory without changing the system installation.'".to_string());
        } else {
            lines.push("  Write-Host 'uv was not found; starting installation.'".to_string());
        }
        lines.push("  $UvInstaller = Join-Path $TaskDir 'uv-install.ps1'".to_string());
        lines.push(format!(
            "  Invoke-DownloadWithRetry -Uri {} -OutFile $UvInstaller -Label 'official uv installer'",
            powershell_quote(&uv_installer)
        ));
        lines.push("  & $UvInstaller".to_string());
        lines.push("  $UvBin = Join-Path $HOME '.local\\bin\\uv.exe'".to_string());
    } else {
        lines.push(format!("  $UvBin = {}", powershell_quote(&status.uv_path)));
    }
    if !status.node_compatible || !status.npm_compatible {
        lines.extend([
            "  $Stage = 'node'; ('{\"status\":\"running\",\"stage\":\"node\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
            format!("  $ArchiveName = {}", powershell_quote(&archive_name)),
            "  $Archive = Join-Path $env:TEMP $ArchiveName".to_string(),
            "  $Checksums = Join-Path $env:TEMP 'agentseek-node-SHASUMS256.txt'".to_string(),
            "  $ExtractDir = Join-Path $env:TEMP ('agentseek-node-' + [guid]::NewGuid())".to_string(),
            format!("  Invoke-DownloadWithRetry -Uri {} -OutFile $Archive -Label 'Node.js ZIP'", powershell_quote(&node_url)),
            format!("  Invoke-DownloadWithRetry -Uri {} -OutFile $Checksums -Label 'Node.js SHA-256 manifest'", powershell_quote(&checksum_url)),
            "  $Line = Get-Content $Checksums | Where-Object { $_ -match ('\\s+' + [regex]::Escape($ArchiveName) + '$') } | Select-Object -First 1".to_string(),
            "  if (-not $Line) { throw 'Node.js SHA-256 entry was not found' }".to_string(),
            "  $Expected = ($Line.Trim() -split '\\s+')[0].ToLowerInvariant()".to_string(),
            "  $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLowerInvariant()".to_string(),
            "  if ($Expected -ne $Actual) { throw 'Node.js SHA-256 verification failed' }".to_string(),
            "  Expand-Archive -Path $Archive -DestinationPath $ExtractDir -Force".to_string(),
            format!("  $NodeRoot = {}", powershell_quote(&node_root.to_string_lossy())),
            "  if (Test-Path $NodeRoot) { Remove-Item -Recurse -Force $NodeRoot }".to_string(),
            "  New-Item -ItemType Directory -Force -Path (Split-Path $NodeRoot) | Out-Null".to_string(),
            "  Move-Item (Join-Path $ExtractDir ($ArchiveName -replace '\\.zip$','')) $NodeRoot".to_string(),
            format!("  & (Join-Path $NodeRoot 'npm.cmd') install -g npm@{}", requirements.versions.npm.managed),
            "  if ($LASTEXITCODE -ne 0) { throw \"npm installation failed with exit code $LASTEXITCODE\" }".to_string(),
            "  Remove-Item -Force $Archive,$Checksums".to_string(),
            "  Remove-Item -Recurse -Force $ExtractDir".to_string(),
        ]);
    }
    if !status.cli_compatible || status.cli_update_available {
        lines.extend([
            "  $Stage = 'agentseek'; ('{\"status\":\"running\",\"stage\":\"agentseek\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
            "  if (-not (Test-Path $UvBin)) { $UvBin = (Get-Command uv).Source }".to_string(),
            "  & $UvBin tool install --upgrade agentseek".to_string(),
            "  if ($LASTEXITCODE -ne 0) { throw \"AgentSeek CLI installation failed with exit code $LASTEXITCODE\" }".to_string(),
            "  & $UvBin tool update-shell".to_string(),
            "  if ($LASTEXITCODE -ne 0) { throw \"uv tool update-shell failed with exit code $LASTEXITCODE\" }".to_string(),
        ]);
    }
    lines.extend([
        "  $Stage = 'complete'; '{\"status\":\"success\",\"stage\":\"complete\",\"code\":0}' | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "} catch {".to_string(),
        "  $Message = $_.Exception.Message.Replace('\\','\\\\').Replace('\"','\\\"')".to_string(),
        "  ('{\"status\":\"failed\",\"stage\":\"' + $Stage + '\",\"code\":1,\"message\":\"' + $Message + '\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "  Write-Error $_".to_string(),
        "} finally {".to_string(),
        "  Stop-Transcript".to_string(),
        "}".to_string(),
        String::new(),
    ]);
    lines.join("\r\n")
}

fn prepare_runtime_install_plan(
    state: &DesktopState,
    force_agentseek_upgrade: bool,
) -> Result<RuntimeInstallPlan, String> {
    let requirements = load_runtime_requirements()?;
    let status = current_cli_status(true)?;
    if force_agentseek_upgrade
        && (!status.cli_update_available || status.cli_latest_version.is_empty())
    {
        return Err("AgentSeek CLI is already up to date".to_string());
    }
    let upgrade_target = if status.cli_update_available {
        Some(status.cli_latest_version.clone())
    } else {
        None
    };
    let dependencies = required_runtime_dependencies(&status);
    if dependencies.is_empty() {
        return Err("Current runtime environment already meets requirements".to_string());
    }
    let runtime_root = managed_runtime_root()
        .ok_or_else(|| "AgentSeek Desktop private runtime directory not yet initialized".to_string())?;
    let task_id = unique_stamp().to_string();
    let task_dir = state.data_dir.join("runtime-install").join(&task_id);
    fs::create_dir_all(&task_dir).map_err(|error| format!("Failed to create install task directory: {error}"))?;
    let (script_name, script) = if cfg!(windows) {
        (
            "install.ps1",
            windows_runtime_install_script(&requirements, &status, &task_dir, &runtime_root),
        )
    } else {
        (
            "install.command",
            posix_runtime_install_script(&requirements, &status, &task_dir, &runtime_root),
        )
    };
    let script_path = task_dir.join(script_name);
    #[cfg(unix)]
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o700)
            .open(&script_path)
            .map_err(|error| format!("Failed to create install script: {error}"))?;
        file.write_all(script.as_bytes())
            .map_err(|error| format!("Failed to write install script: {error}"))?;
    }
    #[cfg(windows)]
    fs::write(&script_path, &script).map_err(|error| format!("Failed to write install script: {error}"))?;
    fs::write(task_dir.join("status.json"), "{\"status\":\"pending\"}\n")
        .map_err(|error| format!("Failed to initialize install task state: {error}"))?;
    if let Some(target) = upgrade_target {
        fs::write(task_dir.join("agentseek-upgrade-target"), target)
            .map_err(|error| format!("Failed to record AgentSeek CLI upgrade target: {error}"))?;
    }
    Ok(RuntimeInstallPlan {
        task_id,
        script,
        script_path: script_path.to_string_lossy().to_string(),
        install_dir: runtime_root.to_string_lossy().to_string(),
        dependencies,
    })
}

fn launch_runtime_install_terminal(script_path: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let output = Command::new("/usr/bin/open")
            .args(["-a", "Terminal"])
            .arg(script_path)
            .output()
            .map_err(|error| format!("Failed to open system terminal: {error}"))?;
        return if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        };
    }
    if cfg!(windows) {
        configured_command("cmd")
            .args(["/C", "start", "", "powershell.exe", "-NoProfile", "-File"])
            .arg(script_path)
            .spawn()
            .map_err(|error| format!("Failed to open PowerShell: {error}"))?;
        return Ok(());
    }
    let script = script_path.to_string_lossy().to_string();
    let candidates: [(&str, Vec<String>); 3] = [
        (
            "x-terminal-emulator",
            vec!["-e".to_string(), "bash".to_string(), script.clone()],
        ),
        (
            "gnome-terminal",
            vec!["--".to_string(), "bash".to_string(), script.clone()],
        ),
        (
            "konsole",
            vec!["-e".to_string(), "bash".to_string(), script],
        ),
    ];
    for (program, args) in candidates {
        if configured_command(program).args(args).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("No available system terminal found; please install x-terminal-emulator, GNOME Terminal, or Konsole".to_string())
}

fn install_log_tail(path: &Path) -> String {
    let Ok(content) = fs::read(path) else {
        return String::new();
    };
    let start = content.len().saturating_sub(16 * 1024);
    String::from_utf8_lossy(&content[start..])
        .trim()
        .to_string()
}

fn runtime_install_task_dir(state: &DesktopState, task_id: &str) -> Result<PathBuf, String> {
    if task_id.is_empty() || !task_id.chars().all(|character| character.is_ascii_digit()) {
        return Err("Invalid install task ID".to_string());
    }
    Ok(state.data_dir.join("runtime-install").join(task_id))
}

#[tauri::command]
async fn runtime_install_progress(
    task_id: String,
    state: State<'_, DesktopState>,
) -> Result<RuntimeInstallProgress, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let task_dir = runtime_install_task_dir(&state, &task_id)?;
        let status = fs::read_to_string(task_dir.join("status.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(content.trim()).ok())
            .unwrap_or_else(|| serde_json::json!({"status": "pending", "stage": "pending"}));
        Ok(RuntimeInstallProgress {
            status: status
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            stage: status
                .get("stage")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            log: install_log_tail(&task_dir.join("install.log")),
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn runtime_install_plan(
    force_agentseek_upgrade: Option<bool>,
    state: State<'_, DesktopState>,
) -> Result<RuntimeInstallPlan, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        prepare_runtime_install_plan(&state, force_agentseek_upgrade.unwrap_or(false))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn execute_runtime_install(
    task_id: String,
    state: State<'_, DesktopState>,
) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let task_dir = runtime_install_task_dir(&state, &task_id)?;
        let script_path = task_dir.join(if cfg!(windows) {
            "install.ps1"
        } else {
            "install.command"
        });
        if !script_path.is_file() {
            return Err("Install script does not exist, please regenerate install plan".to_string());
        }
        state.log(
            None,
            "AgentSeek Desktop",
            "install",
            "info",
            format!(
                "Opened system terminal to execute runtime environment install script\n{}",
                script_path.display()
            ),
            Some(script_path.to_string_lossy().to_string()),
        );
        launch_runtime_install_terminal(&script_path)?;
        let status_path = task_dir.join("status.json");
        for _ in 0..3_600 {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(content) = fs::read_to_string(&status_path) else {
                continue;
            };
            let Ok(status): Result<serde_json::Value, _> = serde_json::from_str(content.trim())
            else {
                continue;
            };
            match status.get("status").and_then(serde_json::Value::as_str) {
                Some("success") => {
                    let checked = current_cli_status(false)?;
                    if !checked.prerequisites_ready {
                        return Err(
                            "Install script completed, but some dependencies still do not meet version requirements; please re-check".to_string()
                        );
                    }
                    if let Ok(target) =
                        fs::read_to_string(task_dir.join("agentseek-upgrade-target"))
                    {
                        if !meets_requirement(&checked.cli_version, target.trim()) {
                            return Err(format!(
                                "AgentSeek CLI upgrade did not reach target version {}; currently detected {}",
                                target.trim(),
                                checked.cli_version
                            ));
                        }
                    }
                    state.log(
                        None,
                        "AgentSeek Desktop",
                        "install",
                        "success",
                        "Terminal install script completed; runtime environment check passed",
                        Some(script_path.to_string_lossy().to_string()),
                    );
                    return Ok(format!(
                        "Runtime environment installation completed\nLog: {}",
                        task_dir.join("install.log").display()
                    ));
                }
                Some("failed") => {
                    let tail = install_log_tail(&task_dir.join("install.log"));
                    state.log(
                        None,
                        "AgentSeek Desktop",
                        "install",
                        "error",
                        format!("Terminal install script execution failed\n{tail}"),
                        Some(script_path.to_string_lossy().to_string()),
                    );
                    return Err(if tail.is_empty() {
                        "Terminal install script execution failed; please check terminal output".to_string()
                    } else {
                        tail
                    });
                }
                _ => {}
            }
        }
        Err("Timed out waiting for terminal install result; please check terminal output and re-check".to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn list_templates(state: State<'_, DesktopState>) -> Result<Vec<TemplateInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = run_cli(&["create", "--list-templates"], None)?;
        if result.code != 0 {
            return Err(result.output);
        }
        let templates = parse_templates(&result.output);
        let cli_path = agentseek_program();
        state.log(
            None,
            "AgentSeek Desktop",
            "lifecycle",
            "info",
            format!(
                "agentseek CLI: {}\nagentseek version: {}\nuv tool dir: {}\n--list-templates returned {} templates\n{}",
                cli_path,
                agentseek_command_version(&cli_path).unwrap_or_default(),
                uv_tool_bin_dir(),
                templates.len(),
                result.output
            ),
            None,
        );
        Ok(templates)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn list_instances(state: State<'_, DesktopState>) -> Result<Vec<InstanceRecord>, String> {
    let mut instances = state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .instances
        .clone();
    for instance in &mut instances {
        enrich_service_endpoints(instance);
    }
    instances.sort_by_key(|instance| std::cmp::Reverse(instance.created_at));
    Ok(instances)
}

#[tauri::command]
fn list_vault(state: State<'_, DesktopState>) -> Result<Vec<EnvVariable>, String> {
    Ok(state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .vault
        .clone())
}

#[tauri::command]
fn save_vault(state: State<'_, DesktopState>, entries: Vec<EnvVariable>) -> Result<(), String> {
    state.replace_vault_entries(entries)
}

fn parse_describe_ports(output: &str) -> HashMap<String, u16> {
    let mut ports = HashMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if key.to_ascii_lowercase().ends_with("_port") {
                if let Ok(port) = value.trim().parse::<u16>() {
                    if port > 0 {
                        ports.insert(key.to_string(), port);
                    }
                }
            }
        }
    }
    ports
}

fn resolve_describe_ports(
    describe_output: &str,
    reserved: &HashSet<u16>,
) -> Result<(HashMap<String, u16>, Vec<PortChange>), String> {
    let defaults = parse_describe_ports(describe_output);
    let mut resolved = HashMap::new();
    let mut changes = Vec::new();
    let mut taken: HashSet<u16> = reserved.iter().copied().collect();
    for (key, default_port) in &defaults {
        let env_key = key.to_ascii_uppercase();
        let port = if port_is_available(*default_port) && taken.insert(*default_port) {
            *default_port
        } else {
            let mut replacement = available_ephemeral_port()?;
            while taken.contains(&replacement) {
                replacement = available_ephemeral_port()?;
            }
            taken.insert(replacement);
            changes.push(PortChange {
                key: env_key.clone(),
                old_port: *default_port,
                new_port: replacement,
            });
            replacement
        };
        resolved.insert(env_key, port);
    }
    Ok((resolved, changes))
}

#[tauri::command]
async fn prepare_instance(
    state: State<'_, DesktopState>,
    input: PrepareInstanceInput,
) -> Result<PrepareInstanceResult, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let first_run = *state
            .storage_setup_required
            .lock()
            .map_err(|_| "Storage setup state lock is poisoned".to_string())?;
        if !first_run {
            state.ensure_storage_ready()?;
        }
        if input.name.trim().is_empty() {
            return Err("Instance name cannot be empty".to_string());
        }
        {
            let data = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?;
            if data
                .instances
                .iter()
                .any(|instance| instance.name == input.name.trim())
            {
                return Err("Instance name already exists".to_string());
            }
        }

        let parent = PathBuf::from(input.target_dir.trim());
        if input.target_dir.trim().is_empty() {
            return Err("Instance working directory cannot be empty".to_string());
        }
        fs::create_dir_all(&parent).map_err(|error| format!("Failed to create instance working directory: {error}"))?;
        if !parent.is_dir() {
            return Err("Instance working path is not a directory".to_string());
        }
        let target = instance_target_path(&parent, &input.name)?;
        validate_target(&target)?;
        let staging = parent.join(format!(".agentseek-desktop-{}", unique_stamp()));
        fs::create_dir_all(&staging).map_err(|error| error.to_string())?;

        let instance_id = format!("instance-{}", unique_stamp());
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "info",
            format!("Starting instance creation\nInstance working directory: {}", target.display()),
            None,
        );
        // Describe template to extract port defaults before creation.
        let describe_result = run_cli(&["create", "--describe", &input.template_id], None)
            .map_err(|error| format!("Failed to read template description: {error}"))?;
        if describe_result.code != 0 {
            return Err(format!("Failed to read template description: {}", describe_result.output));
        }
        let reserved = collect_assigned_ports(&state, None);
        let (mut resolved_ports, mut port_changes) =
            resolve_describe_ports(&describe_result.output, &reserved)?;

        let create_started = Instant::now();
        let result = match run_cli(
            &["create", &input.template_id, "--no-input"],
            Some(&staging),
        ) {
            Ok(result) => result,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            if result.code == 0 { "success" } else { "error" },
            &result.output,
            Some(result.command.clone()),
        );
        if result.code != 0 {
            let _ = fs::remove_dir_all(&staging);
            return Err(result.output);
        }
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "success",
            format!(
                "AgentSeek create completed in {} seconds",
                create_started.elapsed().as_secs()
            ),
            None,
        );

        let generated = match fs::read_dir(&staging)
            .map_err(|error| error.to_string())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| path.is_dir())
            }) {
            Ok(Some(generated)) => generated,
            Ok(None) => {
                let _ = fs::remove_dir_all(&staging);
                return Err("AgentSeek CLI did not return generated project directory".to_string());
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if target.exists() {
            fs::remove_dir(&target).map_err(|error| error.to_string())?;
        }
        fs::rename(&generated, &target).map_err(|error| format!("Failed to move instance directory: {error}"))?;
        let _ = fs::remove_dir_all(&staging);
        // Propagate the instance name into generated files that still hold the template default.
        if let Some(project_name) = &describe_result
            .output
            .lines()
            .find_map(|line| line.trim().strip_prefix("project_name:"))
            .map(|value| value.trim().to_string())
        {
            if project_name != input.name.trim() {
                let _ =
                    replace_project_name_in_directory(&target, &project_name, input.name.trim());
            }
        }
        synchronize_instance_project_name(&target, input.name.trim())?;

        let env_example =
            find_env_example(&target).ok_or_else(|| ".env.example not found in instance".to_string())?;
        let env_content = fs::read_to_string(&env_example).map_err(|error| error.to_string())?;
        let mut parsed_env = parse_env(&env_content);

        // Resolve service ports from lifecycle.toml — some services (e.g. backend:2024)
        // may not appear in cookiecutter variables and thus were missed by resolve_describe_ports.
        // Also handle services that share a port with a cookiecutter variable (e.g. service "app"
        // whose URL uses {{ cookiecutter.frontend_port }}) — these should NOT create a duplicate
        // APP_PORT entry but follow the resolved port of the shared variable.
        let lifecycle_path = target.join(".agentseek/lifecycle.toml");
        if let Ok(lifecycle_content) = fs::read_to_string(&lifecycle_path) {
            if let Ok(manifest) = toml::from_str::<LifecycleManifest>(&lifecycle_content) {
                let mut taken: HashSet<u16> = reserved.iter().copied().collect();
                for (_, port) in &resolved_ports {
                    taken.insert(*port);
                }
                let mut updated = lifecycle_content.clone();
                for (name, service) in &manifest.services {
                    let default_port = extract_url_port(&service.url).unwrap_or(0);
                    if default_port == 0 {
                        continue;
                    }
                    let env_key = format!("{}_PORT", name.to_ascii_uppercase());

                    // 1. Already resolved under this key (from describe output)
                    if let Some(&p) = resolved_ports.get(&env_key) {
                        if p != default_port {
                            let new_url = replace_url_port(&service.url, p);
                            updated = updated.replace(&service.url, &new_url);
                        }
                        continue;
                    }

                    // 2. Port shared with another variable that was changed — apply same change
                    if let Some(change) = port_changes.iter().find(|c| c.old_port == default_port) {
                        if change.new_port != default_port {
                            let new_url = replace_url_port(&service.url, change.new_port);
                            updated = updated.replace(&service.url, &new_url);
                        }
                        continue; // Don't create duplicate entry
                    }

                    // 3. Port shared with another variable (unchanged) — skip
                    if taken.contains(&default_port) {
                        continue; // Don't create duplicate entry
                    }

                    // 4. Service port not in describe output and not shared — resolve now
                    let port = if port_is_available(default_port) {
                        taken.insert(default_port);
                        default_port
                    } else {
                        let mut replacement = available_ephemeral_port()?;
                        while taken.contains(&replacement) {
                            replacement = available_ephemeral_port()?;
                        }
                        taken.insert(replacement);
                        port_changes.push(PortChange {
                            key: env_key.clone(),
                            old_port: default_port,
                            new_port: replacement,
                        });
                        replacement
                    };
                    resolved_ports.insert(env_key.clone(), port);
                    if port != default_port {
                        let new_url = replace_url_port(&service.url, port);
                        updated = updated.replace(&service.url, &new_url);
                    }
                }
                if updated != lifecycle_content {
                    fs::write(&lifecycle_path, &updated).map_err(|error| {
                        format!("Failed to write {}: {error}", lifecycle_path.display())
                    })?;
                }
            }
        }

        // Add resolved port env variables from describe output.
        // New port variables (not in .env.example) are marked modified=true to sync to vault;
        // Existing port variables are marked modified only when value changes.
        let existing_keys: HashSet<String> = parsed_env
            .iter()
            .map(|e| e.key.to_ascii_uppercase())
            .collect();
        for (key, port) in &resolved_ports {
            let new_value = port.to_string();
            if !existing_keys.contains(key.as_str()) {
                // Check if this port value is already covered by another entry
                // (e.g. GATEWAY_PORT=8088 and BUB_AG_UI_PORT=8088 are the same)
                let covered = parsed_env.iter().any(|e| {
                    e.key.to_ascii_uppercase() != *key && e.value.trim() == new_value
                });
                if covered {
                    continue; // Do not create duplicate entry
                }
                // Check if port changed from default; if so, sync existing entries
                // (e.g. 8088 occupied -> assigned 58781; need to update BUB_AG_UI_PORT and URL)
                if let Some(change) = port_changes.iter().find(|c| c.key == *key) {
                    let old_value = change.old_port.to_string();
                    for entry in parsed_env.iter_mut() {
                        if entry.value.trim() == old_value {
                            // Port value match — update directly (e.g. BUB_AG_UI_PORT)
                            entry.value = new_value.clone();
                            entry.modified = true;
                        } else if extract_url_port(&entry.value) == Some(change.old_port) {
                            // URL contains old port — sync update (e.g. BUB_AG_UI_AGENT_URL)
                            let updated_url = replace_url_port(&entry.value, *port);
                            if updated_url != entry.value {
                                entry.value = updated_url;
                                entry.modified = true;
                            }
                        }
                    }
                    continue; // Do not create GATEWAY_PORT; already updated BUB_AG_UI_PORT
                }
                parsed_env.push(EnvVariable {
                    key: key.clone(),
                    value: new_value,
                    comment: String::new(),
                    source: "describe".to_string(),
                    modified: true,
                });
            } else if let Some(entry) = parsed_env
                .iter_mut()
                .find(|e| e.key.to_ascii_uppercase() == *key)
            {
                if entry.value != new_value {
                    entry.value = new_value;
                    entry.modified = true;
                }
            }
        }

        // Ensure LangSmith tracing is disabled by default to prevent 403 Forbidden
        // warnings from langgraph_api.metadata when no LANGCHAIN_API_KEY is configured.
        if !parsed_env
            .iter()
            .any(|e| e.key.to_ascii_uppercase() == "LANGSMITH_TRACING")
        {
            parsed_env.push(EnvVariable {
                key: "LANGSMITH_TRACING".to_string(),
                value: "false".to_string(),
                comment: "Disable LangSmith tracing to avoid metadata submission warnings"
                    .to_string(),
                source: "instance".to_string(),
                modified: true,
            });
        }
        let env = merged_env(&state, &parsed_env);
        let now = timestamp();
        let mut instance = InstanceRecord {
            id: instance_id,
            name: input.name.trim().to_string(),
            template_id: input.template_id,
            status: "configuring".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: target.to_string_lossy().to_string(),
            env_example_path: Some(env_example.to_string_lossy().to_string()),
            env_path: None,
            note: input.note,
            created_at: now,
            updated_at: now,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: Some(input.name.trim().to_string()),
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        enrich_service_endpoints(&mut instance);
        state.persist_instance(&instance)?;
        state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?
            .instances
            .push(instance.clone());
        if let Some(message) = docker_compose_check(&target) {
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "error",
                &message,
                Some("docker --version && docker compose version --short && docker info".to_string()),
            );
            instance.status = "failed".to_string();
            instance.updated_at = timestamp();
            let _ = update_instance(&state, instance.clone());
            return Err(format!("{} instance startup process exited, please check lifecycle logs", instance.name));
        }
        Ok(PrepareInstanceResult {
            instance,
            env,
            docker_warning: None,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn load_instance_env(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<Vec<EnvVariable>, String> {
    let instance = instance_by_id(&state, &instance_id)?;
    let path = instance
        .env_path
        .as_deref()
        .or(instance.env_example_path.as_deref())
        .ok_or_else(|| "Instance has no readable environment variable file".to_string())?;
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let parsed_env = parse_env(&content);
    Ok(merged_env(&state, &parsed_env))
}

fn resolve_lifecycle_ports(
    instance: &InstanceRecord,
    reserved_ports: &HashSet<u16>,
    env_entries: &[EnvVariable],
) -> Result<(String, Vec<PortChange>, Vec<(String, u16)>), String> {
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let content = fs::read_to_string(&lifecycle_path)
        .map_err(|error| format!("Failed to read {}: {error}", lifecycle_path.display()))?;
    let manifest: LifecycleManifest = toml::from_str(&content)
        .map_err(|error| format!("Failed to parse {}: {error}", lifecycle_path.display()))?;

    let mut port_map: Vec<(String, u16)> = Vec::new();
    let mut changes = Vec::new();
    let mut taken: HashSet<u16> = reserved_ports.iter().copied().collect();

    for (name, service) in &manifest.services {
        let default_port = extract_url_port(&service.url).unwrap_or(0);
        if default_port == 0 {
            continue;
        }
        let key = format!("{}_PORT", name.to_ascii_uppercase());
        // Prefer user-configured ports in .env; fall back to lifecycle.toml default ports if not configured.
        let user_port = env_entries
            .iter()
            .find(|e| e.key.to_ascii_uppercase() == key)
            .and_then(|e| e.value.trim().parse::<u16>().ok())
            .filter(|&p| p > 0);

        // If {NAME}_PORT is not in entries, check if the port is already covered by another variable
        // (e.g. service "app" port 5173 is the same as FRONTEND_PORT=5173).
        // Skip to avoid creating duplicate entry (APP_PORT).
        if user_port.is_none() {
            let covered = env_entries.iter().any(|e| {
                e.key.to_ascii_uppercase() != key
                    && e.value.trim().parse::<u16>().ok() == Some(default_port)
            });
            if covered {
                continue;
            }
        }

        let preferred = user_port.unwrap_or(default_port);
        let resolved = if port_is_available(preferred) && taken.insert(preferred) {
            preferred
        } else {
            let mut replacement = available_ephemeral_port()?;
            while taken.contains(&replacement) {
                replacement = available_ephemeral_port()?;
            }
            taken.insert(replacement);
            changes.push(PortChange {
                key: key.clone(),
                old_port: preferred,
                new_port: replacement,
            });
            replacement
        };
        port_map.push((key, resolved));
    }

    // Update lifecycle.toml content with resolved ports.
    let mut updated_content = content;
    for (name, service) in &manifest.services {
        let default_port = extract_url_port(&service.url).unwrap_or(0);
        if default_port == 0 {
            continue;
        }
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == &format!("{}_PORT", name.to_ascii_uppercase()))
            .map(|(_, p)| *p)
            .unwrap_or(default_port);
        if resolved != default_port {
            let new_url = replace_url_port(&service.url, resolved);
            updated_content = updated_content.replace(&service.url, &new_url);
        }
    }

    Ok((updated_content, changes, port_map))
}

#[tauri::command]
fn save_instance_env(
    state: State<'_, DesktopState>,
    input: SaveEnvInput,
) -> Result<SaveEnvResult, String> {
    state.ensure_storage_ready()?;
    let mut instance = instance_by_id(&state, &input.instance_id)?;
    let deployment_completed = instance_has_completed_deployment(state.inner(), &instance)?;
    let env_path = PathBuf::from(&instance.work_dir).join(".env");
    if env_path.is_file() && !input.overwrite {
        return Err(format!("ENV_FILE_EXISTS:{}", env_path.display()));
    }
    let mut entries = input.entries;
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let port_changes = if deployment_completed || !lifecycle_path.is_file() {
        if !deployment_completed {
            resolve_port_conflicts(&mut entries)?
        } else {
            Vec::new()
        }
    } else {
        let reserved = collect_assigned_ports(state.inner(), Some(&instance.id));
        let (updated_lifecycle, changes, port_map) = resolve_lifecycle_ports(&instance, &reserved, &entries)?;
        // Write lifecycle.toml first, then update .env entries to match.
        fs::write(&lifecycle_path, &updated_lifecycle)
            .map_err(|error| format!("Failed to write {}: {error}", lifecycle_path.display()))?;
        for (key, port) in &port_map {
            let new_value = port.to_string();
            if let Some(entry) = entries
                .iter_mut()
                .find(|e| e.key.to_ascii_uppercase() == *key)
            {
                if entry.value != new_value {
                    entry.value = new_value;
                    entry.modified = true;
                }
            } else {
                // .env missing this port variable; create new entry and write to .env + vault
                entries.push(EnvVariable {
                    key: key.clone(),
                    value: new_value,
                    comment: format!("{} service port (auto-resolved)", key.trim_end_matches("_PORT").to_ascii_lowercase()),
                    source: "instance".to_string(),
                    modified: true,
                });
            }
        }
        // Sync *_URL variables with the resolved lifecycle ports so that
        // URLs like LANGGRAPH_URL stay in sync even when no *_PORT variable
        // exists in the .env file.
        for (key, port) in &port_map {
            let prefix = key.trim_end_matches("_PORT");
            for entry in entries.iter_mut().filter(|e| {
                let k = e.key.to_ascii_uppercase();
                k.contains("URL") && k.contains(prefix)
            }) {
                let updated = replace_url_port(&entry.value, *port);
                if updated != entry.value {
                    entry.value = updated;
                    entry.modified = true;
                }
            }
        }
        changes
    };
    // Ensure LangSmith tracing is disabled by default to prevent 403 Forbidden
    // warnings from langgraph_api.metadata when no LANGCHAIN_API_KEY is configured.
    if !entries
        .iter()
        .any(|e| e.key.to_ascii_uppercase() == "LANGSMITH_TRACING")
    {
        entries.push(EnvVariable {
            key: "LANGSMITH_TRACING".to_string(),
            value: "false".to_string(),
            comment: "Disable LangSmith tracing to avoid metadata submission warnings"
                .to_string(),
            source: "instance".to_string(),
            modified: true,
        });
    }
    fs::write(&env_path, render_env(&entries)).map_err(|error| error.to_string())?;
    let root = PathBuf::from(&instance.work_dir);
    let mut synchronized = synchronize_instance_project_name(&root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(&root, &entries)? {
        if !synchronized.contains(&path) {
            synchronized.push(path);
        }
    }

    let mut synced_count = 0;
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for entry in entries.iter().filter(|entry| entry.modified) {
            synced_count += 1;
            if let Some(saved) = data.vault.iter_mut().find(|saved| saved.key == entry.key) {
                saved.value = entry.value.clone();
                saved.comment = entry.comment.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
            } else {
                let mut saved = entry.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
                data.vault.push(saved);
            }
        }
    }
    instance.env_path = Some(env_path.to_string_lossy().to_string());
    if deployment_completed {
        instance.status = "needs-restart".to_string();
        instance.needs_doctor = true;
    } else {
        instance.status = "ready-to-install".to_string();
        instance.needs_doctor = false;
    }
    instance.updated_at = timestamp();
    enrich_service_endpoints(&mut instance);
    state.persist_current_vault()?;
    update_instance(&state, instance.clone())?;
    if !port_changes.is_empty() {
        let details = port_change_details(&port_changes);
        state.log(
            Some(&instance.id),
            &instance.name,
            "config",
            "warning",
            format!(
                "Local port conflicts detected; free ports auto-assigned and synced to instance runtime configs and env vault\nPort changes:\n{details}\nSynced files:\n  {}\n{}",
                env_path.display(),
                synchronized
                    .iter()
                    .map(|path| format!("  {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            None,
        );
    }
    let docker_warning = docker_compose_check(&root);
    if let Some(message) = &docker_warning {
        state.log(
            Some(&instance.id),
            &instance.name,
            "config",
            "error",
            message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
    }
    state.log(
        Some(&instance.id),
        &instance.name,
        "config",
        "success",
        format!(
            "Generated {} ({} keys, synced {} to vault)",
            env_path.display(),
            entries.len(),
            synced_count
        ),
        None,
    );
    let saved_entries: Vec<EnvVariable> = entries
        .into_iter()
        .map(|mut entry| {
            entry.modified = false;
            entry
        })
        .collect();
    Ok(SaveEnvResult {
        path: env_path.to_string_lossy().to_string(),
        key_count: saved_entries.len(),
        synced_count,
        port_changes,
        entries: saved_entries,
        docker_warning,
    })
}

fn process_env_value(raw: &str) -> String {
    let value = raw.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if matches!(
            (bytes[0], bytes[value.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"')
        ) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn apply_instance_environment(
    command: &mut Command,
    instance: &InstanceRecord,
) -> Result<Vec<EnvVariable>, String> {
    let path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let entries = parse_env(
        &fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read instance env file {}: {error}", path.display()))?,
    );
    for entry in &entries {
        if entry.key.contains(['=', '\0']) || entry.value.contains('\0') {
            return Err(format!("Instance .env contains invalid variable: {}", entry.key));
        }
        command.env(&entry.key, process_env_value(&entry.value));
    }
    Ok(entries)
}

fn runtime_environment_summary(entries: &[EnvVariable]) -> String {
    entries
        .iter()
        .filter(|entry| {
            is_local_service_port_key(&entry.key)
                || (entry.key.to_ascii_uppercase().contains("URL")
                    && ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"]
                        .iter()
                        .any(|host| entry.value.contains(host)))
        })
        .map(|entry| format!("{}={}", entry.key, entry.value))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_and_log(
    state: &DesktopState,
    instance: &InstanceRecord,
    args: &[&str],
    category: &str,
) -> Result<CommandResult, String> {
    let started = Instant::now();
    let (program, prefix) = cli_parts();
    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    state.log(
        Some(&instance.id),
        &instance.name,
        category,
        "info",
        "Starting command execution",
        Some(printable.clone()),
    );

    let mut command = configured_command(&program);
    apply_instance_environment(&mut command, instance)?;
    command
        .env("NPM_CONFIG_PREFER_OFFLINE", "true")
        .env("NPM_CONFIG_AUDIT", "false")
        .env("NPM_CONFIG_FUND", "false")
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .args(&prefix)
        .args(args)
        .current_dir(&instance.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        let message = format!("Failed to execute {printable}: {error}");
        state.log(
            Some(&instance.id),
            &instance.name,
            category,
            "error",
            &message,
            Some(printable.clone()),
        );
        message
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_state = state.clone();
    let stdout_id = instance.id.clone();
    let stdout_name = instance.name.clone();
    let stdout_category = category.to_string();
    let stdout_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                stdout_state.log(
                    Some(&stdout_id),
                    &stdout_name,
                    &stdout_category,
                    "info",
                    &line,
                    None,
                );
                output.push(line);
            }
        }
        output
    });
    let stderr_state = state.clone();
    let stderr_id = instance.id.clone();
    let stderr_name = instance.name.clone();
    let stderr_category = category.to_string();
    let stderr_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                stderr_state.log(
                    Some(&stderr_id),
                    &stderr_name,
                    &stderr_category,
                    "info",
                    &line,
                    None,
                );
                output.push(line);
            }
        }
        output
    });

    let status = child
        .wait()
        .map_err(|error| format!("Failed to wait for command: {printable}: {error}"))?;
    let stdout_lines = stdout_handle.join().unwrap_or_default();
    let stderr_lines = stderr_handle.join().unwrap_or_default();
    let output = stdout_lines
        .into_iter()
        .chain(stderr_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let code = status.code().unwrap_or(1);
    state.log(
        Some(&instance.id),
        &instance.name,
        category,
        if code == 0 { "success" } else { "error" },
        if code == 0 {
            format!(
                "Command completed: {printable} ({} seconds)",
                started.elapsed().as_secs()
            )
        } else {
            format!(
                "Command failed: {printable} ({} seconds)",
                started.elapsed().as_secs()
            )
        },
        None,
    );
    if code != 0 {
        return Err(if output.is_empty() {
            format!("Command execution failed: {printable}")
        } else {
            output
        });
    }
    Ok(CommandResult {
        code,
        output,
        command: printable,
    })
}

fn run_instance_cli(instance: &InstanceRecord, args: &[&str]) -> Result<CommandResult, String> {
    let (program, prefix) = cli_parts();
    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = configured_command(&program);
    apply_instance_environment(&mut command, instance)?;
    let output = command
        .args(&prefix)
        .args(args)
        .current_dir(&instance.work_dir)
        .output()
        .map_err(|error| format!("Failed to execute {printable}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(CommandResult {
        code: output.status.code().unwrap_or(1),
        output: combined.trim().to_string(),
        command: printable,
    })
}

fn wait_for_instance_ready(state: &DesktopState, instance: &InstanceRecord) -> Result<(), String> {
    const READY_TIMEOUT: Duration = Duration::from_secs(300);
    const RETRY_INTERVAL: Duration = Duration::from_secs(2);
    let started = Instant::now();
    let mut latest = String::new();
    state.log(
        Some(&instance.id),
        &instance.name,
        "install",
        "info",
        "Instance process started; waiting for lifecycle health checks to pass",
        Some("agentseek doctor --live".to_string()),
    );
    while started.elapsed() < READY_TIMEOUT {
        if instance.pid.is_some_and(|pid| !process_exists(pid)) {
            let (log_path, _) = runtime_log_spool_paths(&state.data_dir, &instance.id);
            let tail = read_runtime_log_tail(&log_path, 80);
            if !tail.is_empty() {
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    format!("Process output (last 80 lines):\n{}", tail),
                    None,
                );
            }
            return Err(format!(
                "{} instance startup process exited, please check lifecycle logs",
                instance.name
            ));
        }
        let result = run_instance_cli(instance, &["doctor", "--live"])?;
        latest = result.output;
        if result.code == 0 {
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                format!(
                    "All lifecycle services ready ({} seconds)",
                    started.elapsed().as_secs()
                ),
                Some(result.command),
            );
            return Ok(());
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
    let latest = latest
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "Timed out waiting for instance services to be ready (300s). Please check lifecycle logs. Last doctor check:\n{latest}"
    ))
}

fn apply_info_urls(instance: &mut InstanceRecord, output: &str) {
    for line in output.lines() {
        let lower = line.to_lowercase();
        let Some(url_start) = lower.find("http") else {
            continue;
        };
        let url = line[url_start..].trim().to_string();
        if lower.contains("studio") || lower.contains("langsmith") {
            instance.studio_url = Some(url);
        } else if lower.contains("frontend") || lower.contains(" ui") {
            instance.ui_url = Some(url);
        } else if lower.contains("agent")
            || lower.contains("gateway")
            || instance.agent_url.is_none()
        {
            instance.agent_url = Some(url);
        }
    }
}

fn runtime_stream_level(line: &str) -> &'static str {
    let lower = line.to_lowercase();
    if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
    {
        "error"
    } else if lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("error")
        || lower.contains("failed")
        || lower.contains("fatal")
        || lower.contains("panic")
    {
        "error"
    } else if lower.contains("warning") || lower.contains("warn") {
        "warning"
    } else {
        "info"
    }
}

fn runtime_log_spool_paths(data_dir: &Path, instance_id: &str) -> (PathBuf, PathBuf) {
    let safe_id = instance_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let directory = data_dir.join(RUNTIME_LOG_SPOOL_DIRECTORY);
    (
        directory.join(format!("{safe_id}.log")),
        directory.join(format!("{safe_id}.cursor")),
    )
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeLogCursor {
    offset: u64,
    suppress_traceback: bool,
}

fn read_runtime_log_cursor(path: &Path) -> RuntimeLogCursor {
    let Ok(value) = fs::read_to_string(path) else {
        return RuntimeLogCursor::default();
    };
    serde_json::from_str(&value).unwrap_or_else(|_| RuntimeLogCursor {
        offset: value.trim().parse::<u64>().unwrap_or_default(),
        suppress_traceback: false,
    })
}

fn write_runtime_log_cursor(path: &Path, cursor: &RuntimeLogCursor) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    let payload = serde_json::to_vec(cursor).map_err(|error| error.to_string())?;
    file.write_all(&payload).map_err(|error| error.to_string())
}

fn prepare_runtime_log_spool(
    state: &DesktopState,
    instance_id: &str,
) -> Result<(fs::File, fs::File), String> {
    let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, instance_id);
    let parent = log_path
        .parent()
        .ok_or_else(|| "Failed to determine runtime log spool directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("Failed to create runtime log spool directory: {error}"))?;

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let stdout = options
        .open(&log_path)
        .map_err(|error| format!("Failed to create runtime log spool file: {error}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("Failed to open runtime log error stream: {error}"))?;
    write_runtime_log_cursor(&cursor_path, &RuntimeLogCursor::default())
        .map_err(|error| format!("Failed to initialize runtime log cursor: {error}"))?;
    Ok((stdout, stderr))
}

fn strip_ansi_codes(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            for control in characters.by_ref() {
                if control.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn runtime_log_field<'a>(value: &'a str, field: &str) -> Option<&'a str> {
    let start = value.find(field)?.saturating_add(field.len());
    value[start..]
        .split_whitespace()
        .next()
        .map(|item| item.trim_matches([',', '}', '\'', '"']))
        .filter(|item| !item.is_empty())
}

fn runtime_exception_summary(value: &str) -> Option<String> {
    let clean = strip_ansi_codes(value);
    if clean.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let trimmed = clean.trim();
    let exception_type = trimmed.split_once(':').map_or(trimmed, |(kind, _)| kind);
    let short_type = exception_type.rsplit('.').next().unwrap_or(exception_type);
    if !short_type.ends_with("Error")
        && !short_type.ends_with("Exception")
        && short_type != "KeyboardInterrupt"
    {
        return None;
    }
    if short_type == "TimeoutError" {
        return Some("Failure reason: TimeoutError (request timeout)".to_string());
    }
    let detail = trimmed
        .split_once(':')
        .map(|(_, detail)| detail.trim())
        .filter(|detail| !detail.is_empty());
    Some(match detail {
        Some(detail) => format!("Failure reason: {short_type}: {detail}"),
        None => format!("Failure reason: {short_type}"),
    })
}

fn compact_runtime_log_record(value: &str, suppress_traceback: &mut bool) -> Option<String> {
    let clean = strip_ansi_codes(value);
    let lower = clean.to_lowercase();
    if lower.contains("tool.call.error") {
        *suppress_traceback = true;
        let name = runtime_log_field(&clean, "name=").unwrap_or("unknown");
        let elapsed = runtime_log_field(&clean, "elapsed_time=");
        return Some(match elapsed {
            Some(elapsed) => format!("Tool call failed: {name} ({elapsed})"),
            None => format!("Tool call failed: {name}"),
        });
    }
    if !*suppress_traceback {
        return Some(value.to_string());
    }
    if lower.contains("traceback (most recent call last)") || clean.trim().is_empty() {
        return None;
    }
    if let Some(summary) = runtime_exception_summary(&clean) {
        *suppress_traceback = false;
        return Some(summary);
    }
    if lower.contains("tool.call.start")
        || lower.contains("loop.step")
        || lower.contains("session.run.")
        || lower.trim_start().starts_with("info:")
    {
        *suppress_traceback = false;
        return Some(value.to_string());
    }
    None
}

fn read_runtime_log_records(
    path: &Path,
    cursor: u64,
    include_partial: bool,
) -> Result<(u64, Vec<(String, u64)>), String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let start = if cursor <= length { cursor } else { 0 };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut position = start;
    let mut records = Vec::new();

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            break;
        }
        let complete = line.ends_with('\n');
        if !complete && !include_partial {
            break;
        }
        position = position.saturating_add(bytes as u64);
        records.push((line.trim_end_matches(['\r', '\n']).to_string(), position));
    }
    Ok((start, records))
}

fn sync_runtime_log_spools(state: &DesktopState) {
    let Ok(_sync) = state.runtime_log_sync.lock() else {
        return;
    };
    if state.ensure_storage_ready().is_err() {
        return;
    }
    let instances = state
        .data
        .lock()
        .map(|data| data.instances.clone())
        .unwrap_or_default();

    for instance in instances {
        let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, &instance.id);
        if !log_path.is_file() {
            continue;
        }
        let mut cursor = read_runtime_log_cursor(&cursor_path);
        let include_partial = instance.pid.is_none_or(|pid| !process_exists(pid));
        let Ok((start, records)) =
            read_runtime_log_records(&log_path, cursor.offset, include_partial)
        else {
            continue;
        };
        if start != cursor.offset {
            cursor.suppress_traceback = false;
        }
        let mut persisted_cursor = start;
        for (message, next_cursor) in records {
            let previous_suppression = cursor.suppress_traceback;
            if let Some(compacted) =
                compact_runtime_log_record(&message, &mut cursor.suppress_traceback)
            {
                if !state.log(
                    Some(&instance.id),
                    &instance.name,
                    "runtime",
                    runtime_stream_level(&message),
                    compacted,
                    None,
                ) {
                    cursor.suppress_traceback = previous_suppression;
                    break;
                }
            }
            persisted_cursor = next_cursor;
        }
        if persisted_cursor != cursor.offset {
            cursor.offset = persisted_cursor;
            let _ = write_runtime_log_cursor(&cursor_path, &cursor);
        }
    }
}

fn read_runtime_log_tail(log_path: &Path, max_lines: usize) -> String {
    let content = match fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let clean = strip_ansi_codes(&content);
    let lines: Vec<&str> = clean.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn remove_runtime_log_spool(state: &DesktopState, instance_id: &str) {
    let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, instance_id);
    let _ = fs::remove_file(log_path);
    let _ = fs::remove_file(cursor_path);
}

fn ensure_docker_compose_ready(
    state: &DesktopState,
    instance: &InstanceRecord,
    category: &str,
) -> Result<(), String> {
    if let Some(message) = docker_compose_check(Path::new(&instance.work_dir)) {
        state.log(
            Some(&instance.id),
            &instance.name,
            category,
            "error",
            &message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
        return Err(format!("{} instance startup process exited, please check lifecycle logs", instance.name));
    }
    Ok(())
}

fn spawn_instance(state: &DesktopState, instance: &mut InstanceRecord) -> Result<(), String> {
    ensure_docker_compose_ready(state, instance, "install")?;
    if instance.deployment_mode == "docker" {
        let output = configured_command("docker")
            .args(["compose", "up", "-d"])
            .current_dir(&instance.work_dir)
            .output()
            .map_err(|error| format!("Failed to execute Docker Compose: {error}"))?;
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            if output.status.success() {
                "success"
            } else {
                "error"
            },
            message,
            Some("docker compose up -d".to_string()),
        );
        if !output.status.success() {
            return Err("Docker Compose failed to start".to_string());
        }
        instance.pid = None;
        return Ok(());
    }

    let (program, prefix) = cli_parts();
    // Clean up runtime log from previous deployment attempt to avoid stale output
    if let Ok(mut storage) = state.storage.lock() {
        let _ = storage.delete_runtime_logs(&instance.id);
    }
    if let Ok(mut data) = state.data.lock() {
        data.logs.retain(|log| {
            !(log.instance_id.as_deref() == Some(instance.id.as_str())
                && log.category == "runtime")
        });
    }
    let (stdout, stderr) = prepare_runtime_log_spool(state, &instance.id)?;
    let mut command = configured_command(&program);
    let environment = apply_instance_environment(&mut command, instance)?;
    command
        .args(&prefix)
        .args(["dev"])
        .current_dir(&instance.work_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let environment_summary = runtime_environment_summary(&environment);
    if !environment_summary.is_empty() {
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "info",
            format!(
                "Instance .env injected into startup process; addresses below override lifecycle default ports\n{environment_summary}"
            ),
            None,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        remove_runtime_log_spool(state, &instance.id);
        format!(
            "Failed to start instance: cannot execute {} (working directory: {}): {}",
            program, instance.work_dir, error
        )
    })?;
    instance.pid = Some(child.id());
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[tauri::command]
async fn continue_install(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    state.set_deployment_stage(&instance_id, "tasks");
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<InstanceRecord, String> {
            let mut instance = instance_by_id(&state, &instance_id)?;
            if instance.env_path.is_none() {
                return Err("Please generate instance .env first".to_string());
            }
            ensure_docker_compose_ready(&state, &instance, "install")?;
            recheck_instance_ports(&state, &instance)?;
            instance.status = "installing".to_string();
            instance.updated_at = timestamp();
            enrich_service_endpoints(&mut instance);
            update_instance(&state, instance.clone())?;

            state.set_deployment_stage(&instance_id, "tasks");
            let tasks = run_and_log(&state, &instance, &["task", "--list"], "execution")?;
            for task in ["backend", "frontend"] {
                if tasks.output.to_lowercase().contains(task) {
                    run_and_log(&state, &instance, &["task", task], "execution")?;
                }
            }
            let info = run_and_log(&state, &instance, &["info"], "execution")?;
            apply_info_urls(&mut instance, &info.output);
            enrich_service_endpoints(&mut instance);
            state.set_deployment_stage(&instance_id, "doctor");
            run_and_log(&state, &instance, &["doctor"], "execution")?;
            ensure_docker_compose_ready(&state, &instance, "install")?;
            state.set_deployment_stage(&instance_id, "dry-run");
            run_and_log(&state, &instance, &["dev", "--dry-run"], "execution")?;
            state.set_deployment_stage(&instance_id, "starting");
            spawn_instance(&state, &mut instance)?;
            instance.status = "starting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            if let Err(error) = wait_for_instance_ready(&state, &instance) {
                let process_already_exited = instance
                    .pid
                    .is_some_and(|pid| !process_exists(pid));
                if !process_already_exited {
                    let _ = stop_instance_process(&state, &instance, "install");
                }
                return Err(error);
            }
            instance.status = "running".to_string();
            instance.needs_doctor = false;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                "Instance deployment completed",
                None,
            );
            state.set_deployment_stage(&instance_id, "complete");
            Ok(instance)
        })();
        if let Err(error) = &result {
            state.set_deployment_stage(&instance_id, "failed");
            remove_runtime_log_spool(&state, &instance_id);
            // Instance failed to run; clean up runtime log (error info already shown in lifecycle log)
            if let Ok(mut storage) = state.storage.lock() {
                let _ = storage.delete_runtime_logs(&instance_id);
            }
            if let Ok(mut data) = state.data.lock() {
                data.logs.retain(|log| {
                    !(log.instance_id.as_deref() == Some(instance_id.as_str())
                        && log.category == "runtime")
                });
            }
            if let Ok(mut instance) = instance_by_id(&state, &instance_id) {
                instance.status = "failed".to_string();
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    error.clone(),
                    None,
                );
            }
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn deployment_progress(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<String, String> {
    Ok(state
        .deployment_stages
        .lock()
        .map_err(|_| "Deployment state lock is poisoned".to_string())?
        .get(&instance_id)
        .cloned()
        .unwrap_or_else(|| "pending".to_string()))
}

fn child_process_ids(parent: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output();
    let direct = output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut descendants = Vec::new();
    for child in direct {
        descendants.push(child);
        descendants.extend(child_process_ids(child));
    }
    descendants
}

fn endpoint_port(url: &str) -> Option<u16> {
    let authority = url.split_once("://")?.1.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn listener_process_ids(instance: &InstanceRecord) -> Vec<u32> {
    let mut pids = Vec::new();
    for port in instance
        .service_endpoints
        .iter()
        .filter_map(|endpoint| endpoint_port(&endpoint.url))
        .collect::<HashSet<_>>()
    {
        let selector = format!("-iTCP:{port}");
        if let Ok(output) = Command::new("lsof")
            .args(["-nP", "-t", &selector, "-sTCP:LISTEN"])
            .output()
        {
            pids.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| line.trim().parse::<u32>().ok()),
            );
        }
    }
    pids
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn signal_process(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Debug)]
struct StoppedProcess {
    pid: u32,
    executable: String,
}

fn process_executable(pid: u32) -> String {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown process".to_string())
}

fn terminate_processes(
    roots: impl IntoIterator<Item = u32>,
) -> Result<Vec<StoppedProcess>, String> {
    let mut pids = HashSet::new();
    for root in roots {
        pids.insert(root);
        pids.extend(child_process_ids(root));
    }
    let mut ordered = pids.into_iter().collect::<Vec<_>>();
    ordered.sort_unstable();
    ordered.retain(|pid| process_exists(*pid));
    let stopped = ordered
        .iter()
        .map(|pid| StoppedProcess {
            pid: *pid,
            executable: process_executable(*pid),
        })
        .collect::<Vec<_>>();
    for pid in ordered.iter().rev().copied() {
        signal_process(pid, "-TERM");
    }
    std::thread::sleep(Duration::from_millis(800));
    let remaining = ordered
        .iter()
        .copied()
        .filter(|pid| process_exists(*pid))
        .collect::<Vec<_>>();
    for pid in remaining.iter().rev().copied() {
        signal_process(pid, "-KILL");
    }
    if !remaining.is_empty() {
        std::thread::sleep(Duration::from_millis(250));
    }
    let still_running = remaining
        .into_iter()
        .filter(|pid| process_exists(*pid))
        .collect::<Vec<_>>();
    if still_running.is_empty() {
        Ok(stopped)
    } else {
        Err(format!("Failed to stop the following processes: {still_running:?}"))
    }
}

fn stop_instance_process(
    state: &DesktopState,
    instance: &InstanceRecord,
    log_category: &str,
) -> Result<Vec<StoppedProcess>, String> {
    if instance.deployment_mode == "docker" {
        let output = Command::new("docker")
            .args(["compose", "down"])
            .current_dir(&instance.work_dir)
            .output()
            .map_err(|error| format!("Failed to execute Docker Compose: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).to_string());
        }
        Ok(Vec::new())
    } else {
        let mut roots = listener_process_ids(instance);
        if let Some(pid) = instance.pid {
            roots.push(pid);
        }
        let stopped = terminate_processes(roots)?;
        let process_details = if stopped.is_empty() {
            "  No running associated processes found".to_string()
        } else {
            stopped
                .iter()
                .map(|process| format!("  PID {}  {}", process.pid, process.executable))
                .collect::<Vec<_>>()
                .join("\n")
        };
        state.log(
            Some(&instance.id),
            &instance.name,
            log_category,
            "info",
            format!(
                "Instance associated processes stopped\nWorking directory: {}\nProcess count: {}\nDetails:\n{}",
                instance.work_dir,
                stopped.len(),
                process_details
            ),
            None,
        );
        Ok(stopped)
    }
}

#[tauri::command]
async fn stop_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut instance = instance_by_id(&state, &instance_id)?;
        enrich_service_endpoints(&mut instance);
        let previous_status = instance.status.clone();
        instance.status = "stopping".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        let _stopped = match stop_instance_process(&state, &instance, "install") {
            Ok(stopped) => stopped,
            Err(error) => {
                instance.status = previous_status;
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    format!("Failed to stop instance: {error}"),
                    None,
                );
                return Err(error);
            }
        };
        instance.pid = None;
        instance.status = "stopped".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "success",
            "Instance stopped",
            None,
        );
        Ok(instance)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn restart_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<InstanceRecord, String> {
            let mut instance = instance_by_id(&state, &instance_id)?;
            ensure_docker_compose_ready(&state, &instance, "install")?;
            let synchronized = synchronize_instance_configs_from_env(&instance)?;
            if !synchronized.is_empty() {
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "config",
                    "info",
                    format!(
                        "Runtime configs updated based on instance .env before restart\n{}",
                        synchronized
                            .iter()
                            .map(|path| format!("  {}", path.display()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    None,
                );
            }
            enrich_service_endpoints(&mut instance);
            instance.status = "restarting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            run_and_log(&state, &instance, &["doctor"], "execution")?;
            ensure_docker_compose_ready(&state, &instance, "install")?;
            let _stopped = stop_instance_process(&state, &instance, "install")?;
            instance.pid = None;
            spawn_instance(&state, &mut instance)?;
            instance.status = "starting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            if let Err(error) = wait_for_instance_ready(&state, &instance) {
                let process_already_exited = instance
                    .pid
                    .is_some_and(|pid| !process_exists(pid));
                if !process_already_exited {
                    let _ = stop_instance_process(&state, &instance, "install");
                }
                return Err(error);
            }
            instance.status = "running".to_string();
            instance.needs_doctor = false;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                "Doctor passed; instance restarted",
                None,
            );
            Ok(instance)
        })();
        if let Err(error) = &result {
            remove_runtime_log_spool(&state, &instance_id);
            // Instance failed to run; clean up runtime log (error info already shown in lifecycle log)
            if let Ok(mut storage) = state.storage.lock() {
                let _ = storage.delete_runtime_logs(&instance_id);
            }
            if let Ok(mut data) = state.data.lock() {
                data.logs.retain(|log| {
                    !(log.instance_id.as_deref() == Some(instance_id.as_str())
                        && log.category == "runtime")
                });
            }
            if let Ok(mut instance) = instance_by_id(&state, &instance_id) {
                instance.status = if instance.needs_doctor {
                    "needs-restart".to_string()
                } else {
                    "failed".to_string()
                };
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    error.clone(),
                    None,
                );
            }
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn mark_env_edited(state: State<'_, DesktopState>, instance_id: String) -> Result<(), String> {
    let mut instance = instance_by_id(&state, &instance_id)?;
    let deployment_completed = instance_has_completed_deployment(state.inner(), &instance)?;
    if !deployment_completed {
        instance.needs_doctor = false;
        instance.status = if instance.env_path.is_some() {
            "ready-to-install".to_string()
        } else {
            "configuring".to_string()
        };
        instance.updated_at = timestamp();
        return update_instance(&state, instance);
    }
    instance.needs_doctor = true;
    instance.status = "needs-restart".to_string();
    instance.updated_at = timestamp();
    update_instance(&state, instance)
}

fn remove_instance_work_dir(work_dir: &str) -> Result<(), String> {
    let path = PathBuf::from(work_dir);
    if !path.exists() {
        return Ok(());
    }
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| format!("Failed to check instance working directory: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("Refusing to delete symlink instance working directory".to_string());
    }
    if !metadata.is_dir() {
        return Err("Instance working directory is not a directory".to_string());
    }
    let canonical =
        fs::canonicalize(&path).map_err(|error| format!("Failed to resolve instance working directory: {error}"))?;
    if canonical.parent().is_none() {
        return Err("Refusing to delete filesystem root".to_string());
    }
    if let Some(home) = env::var_os("HOME") {
        if fs::canonicalize(home).is_ok_and(|home| home == canonical) {
            return Err("Refusing to delete user home directory".to_string());
        }
    }
    if env::current_dir()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|current| current == canonical)
    {
        return Err("Refusing to delete AgentSeek Desktop current working directory".to_string());
    }
    fs::remove_dir_all(&canonical).map_err(|error| format!("Failed to delete instance working directory: {error}"))
}

#[tauri::command]
async fn delete_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut instance = instance_by_id(&state, &instance_id)?;
        enrich_service_endpoints(&mut instance);
        instance.status = "deleting".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        let result = (|| -> Result<(), String> {
            let stopped = stop_instance_process(&state, &instance, "install")
                .map_err(|error| format!("Failed to stop instance associated processes: {error}"))?;
            instance.pid = None;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            remove_instance_work_dir(&instance.work_dir)?;
            remove_runtime_log_spool(&state, &instance.id);
            state.remove_persisted_instance(&instance_id)?;
            {
                let mut data = state
                    .data
                    .lock()
                    .map_err(|_| "State lock is poisoned".to_string())?;
                data.instances.retain(|item| item.id != instance_id);
            }
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                format!(
                    "Instance deletion completed\nInstance name: {}\nInstance ID: {}\nWorking directory: {}\nProcesses stopped: {}\nInstance record: deleted",
                    instance.name,
                    instance.id,
                    instance.work_dir,
                    stopped.len()
                ),
                None,
            );
            Ok(())
        })();
        if let Err(error) = &result {
            if let Ok(mut failed_instance) = instance_by_id(&state, &instance_id) {
                failed_instance.status = "delete-failed".to_string();
                failed_instance.updated_at = timestamp();
                let _ = update_instance(&state, failed_instance);
            }
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "error",
                format!("Failed to delete instance: {error}"),
                None,
            );
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn list_logs(state: State<'_, DesktopState>, query: LogQuery) -> Result<LogPage, String> {
    sync_runtime_log_spools(state.inner());
    state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .query_logs(&query)
}

#[tauri::command]
fn log_settings(state: State<'_, DesktopState>) -> Result<LogSettings, String> {
    let retention_days = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .runtime_log_retention_days;
    Ok(LogSettings {
        runtime_retention_days: retention_days,
    })
}

#[tauri::command]
fn save_log_settings(
    state: State<'_, DesktopState>,
    settings: LogSettings,
) -> Result<LogSettings, String> {
    state.ensure_storage_ready()?;
    if !(1..=3_650).contains(&settings.runtime_retention_days) {
        return Err("Runtime log retention days must be between 1 and 3650".to_string());
    }
    let mut config = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .clone();
    config.runtime_log_retention_days = settings.runtime_retention_days;
    write_storage_config(&state.config_path, &config)?;
    *state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())? = config;
    let removed = state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .cleanup_logs(settings.runtime_retention_days, timestamp())?;
    state.log(
        None,
        "Log Center",
        "config",
        "success",
        format!(
            "Runtime log retention set to {} days; cleaned up {} log entries",
            settings.runtime_retention_days, removed
        ),
        None,
    );
    Ok(settings)
}

#[tauri::command]
fn import_env(state: State<'_, DesktopState>, path: String) -> Result<usize, String> {
    state.ensure_storage_ready()?;
    let file = PathBuf::from(path.trim());
    if !file.is_file()
        || !file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_env_file_name)
    {
        return Err("Please select a valid .env file path".to_string());
    }
    let entries = parse_env(&fs::read_to_string(&file).map_err(|error| error.to_string())?);
    let count = entries.len();
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for mut entry in entries {
            entry.source = "import".to_string();
            entry.modified = false;
            if let Some(saved) = data.vault.iter_mut().find(|saved| saved.key == entry.key) {
                *saved = entry;
            } else {
                data.vault.push(entry);
            }
        }
    }
    state.persist_current_vault()?;
    state.log(
        None,
        "Config Center",
        "config",
        "success",
        format!("Imported {count} variables from {}", file.display()),
        None,
    );
    Ok(count)
}

fn is_env_file_name(name: &str) -> bool {
    name.starts_with(".env")
}

#[tauri::command]
fn list_env_files(path: String) -> Result<Vec<String>, String> {
    let directory = PathBuf::from(path.trim());
    if !directory.is_dir() {
        return Err("Please select an existing project directory".to_string());
    }
    let directory = fs::canonicalize(&directory).map_err(|error| error.to_string())?;
    let mut files = fs::read_dir(&directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_env_file_name)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        let left_name = left
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let right_name = right
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        (left_name != ".env.example", left_name).cmp(&(right_name != ".env.example", right_name))
    });
    Ok(files
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect())
}

#[tauri::command]
fn export_env(
    state: State<'_, DesktopState>,
    input: ExportEnvInput,
) -> Result<ExportEnvResult, String> {
    let source = PathBuf::from(input.source_path.trim());
    if !source.is_file()
        || !source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_env_file_name)
    {
        return Err("Please select a valid source .env file".to_string());
    }
    let output = PathBuf::from(input.output_path.trim());
    if !output.is_absolute()
        || output
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name.trim().is_empty())
    {
        return Err("Target file must be an absolute path with filename".to_string());
    }
    if !output.parent().is_some_and(Path::is_dir) {
        return Err("Target file directory does not exist".to_string());
    }
    let source_entries =
        parse_env(&fs::read_to_string(&source).map_err(|error| error.to_string())?);
    let entries = merged_env(&state, &source_entries);
    if output.is_file() && !input.overwrite {
        return Err(format!("ENV_FILE_EXISTS:{}", output.display()));
    }
    fs::write(&output, render_env(&entries)).map_err(|error| error.to_string())?;
    let filled_count = entries
        .iter()
        .filter(|entry| entry.source == "vault" && !entry.value.trim().is_empty())
        .count();
    let missing_count = entries
        .iter()
        .filter(|entry| entry.value.trim().is_empty())
        .count();
    state.log(
        None,
        "Config Center",
        "config",
        "success",
        format!(
            "Exported {}\nTotal variables: {}\nBackfilled from vault: {}\nStill missing: {}\nSource: {}",
            output.display(),
            entries.len(),
            filled_count,
            missing_count,
            source.display()
        ),
        None,
    );
    Ok(ExportEnvResult {
        path: output.to_string_lossy().to_string(),
        key_count: entries.len(),
        filled_count,
        missing_count,
    })
}

fn ensure_seekdb_runtime(data_dir: &Path) -> Result<PathBuf, String> {
    let runtime = data_dir.join("runtime/seekdb-python");
    let python = if cfg!(windows) {
        runtime.join("Scripts/python.exe")
    } else {
        runtime.join("bin/python")
    };
    if !python.is_file() {
        let uv = uv_program().ok_or_else(|| "Please install uv before configuring SeekDB".to_string())?;
        run_dependency_command(
            &uv,
            &["venv", &runtime.to_string_lossy(), "--python", "3.12"],
            "Creating AgentSeek Desktop SeekDB private Python environment",
        )?;
    }
    let marker = runtime.join(".pyseekdb-1.4.0");
    if !marker.is_file() {
        let uv = uv_program().ok_or_else(|| "Please install uv before configuring SeekDB".to_string())?;
        run_dependency_command(
            &uv,
            &[
                "pip",
                "install",
                "--python",
                &python.to_string_lossy(),
                "pyseekdb==1.4.0",
            ],
            "Installing AgentSeek Desktop private pyseekdb 1.4.0",
        )?;
        fs::write(&marker, "pyseekdb 1.4.0").map_err(|error| error.to_string())?;
    }
    Ok(python)
}

#[tauri::command]
fn storage_status(state: State<'_, DesktopState>) -> Result<StorageStatus, String> {
    storage_status_value(state.inner())
}

fn storage_status_value(state: &DesktopState) -> Result<StorageStatus, String> {
    let config = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .clone();
    let error = state
        .storage_error
        .lock()
        .ok()
        .and_then(|error| error.clone());
    Ok(StorageStatus {
        mode: config.mode,
        effective_mode: state
            .effective_storage_mode
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?
            .clone(),
        path: config.path,
        default_sqlite_path: state.data_dir.to_string_lossy().to_string(),
        default_seekdb_path: state.data_dir.join("seekdb").to_string_lossy().to_string(),
        host: config.host,
        port: config.port,
        tenant: config.tenant,
        database: config.database,
        default_database: default_storage_database(),
        user: config.user,
        password_configured: !config.password.is_empty(),
        runtime_log_retention_days: config.runtime_log_retention_days,
        setup_required: *state
            .storage_setup_required
            .lock()
            .map_err(|_| "Storage setup state lock is poisoned".to_string())?,
        writable: *state
            .storage_ready
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?,
        error,
    })
}

// Remote storage can acknowledge a write before its next read observes it.
// Retry the read-back check briefly so a successful migration is not reported as failed.
fn verify_storage_snapshot(
    engine: &mut StorageEngine,
    expected: &AppStore,
    expected_log_count: usize,
) -> Result<(), String> {
    let mut last_error = String::from("Target storage has not returned migration data");
    for attempt in 1..=5 {
        match (engine.load(), engine.log_count()) {
            (Ok(actual), Ok(actual_log_count)) => {
                let actual = actual.unwrap_or_default();
                if actual.instances.len() == expected.instances.len()
                    && actual.vault.len() == expected.vault.len()
                    && actual_log_count == expected_log_count
                {
                    return Ok(());
                }
                last_error = format!(
                    "Instances {} -> {}, Vault {} -> {}, Logs {} -> {}",
                    expected.instances.len(),
                    actual.instances.len(),
                    expected.vault.len(),
                    actual.vault.len(),
                    expected_log_count,
                    actual_log_count,
                );
            }
            (Err(error), _) | (_, Err(error)) => {
                last_error = error;
            }
        }
        if attempt < 5 {
            let delay = 250_u64 * 2_u64.pow((attempt - 1) as u32);
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    Err(format!("Target storage validation failed: {last_error}"))
}

#[tauri::command]
async fn configure_storage(
    state: State<'_, DesktopState>,
    mut config: StorageConfig,
) -> Result<StorageStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state.ensure_storage_configurable()?;
        if config.password.is_empty() {
            if let Ok(current) = state.storage_config.lock() {
                if current.mode == config.mode
                    && current.host == config.host
                    && current.user == config.user
                {
                    config.password = current.password.clone();
                }
            }
        }
        let allowed = [
            "sqlite_embedded",
            "seekdb_embedded",
            "seekdb_server",
            "oceanbase_server",
        ];
        if !allowed.contains(&config.mode.as_str()) {
            return Err("Unsupported desktop storage type".to_string());
        }
        config.setup_completed = true;
        normalize_storage_database(&mut config);
        if !(1..=3_650).contains(&config.runtime_log_retention_days) {
            config.runtime_log_retention_days = default_runtime_log_retention_days();
        }
        if matches!(config.mode.as_str(), "sqlite_embedded" | "seekdb_embedded") {
            if config.path.trim().is_empty() {
                config.path = if config.mode == "sqlite_embedded" {
                    state.data_dir.to_string_lossy().to_string()
                } else {
                    state.data_dir.join("seekdb").to_string_lossy().to_string()
                };
            }
            if !Path::new(&config.path).is_absolute() {
                return Err("Embedded storage data directory must use an absolute path".to_string());
            }
            fs::create_dir_all(&config.path).map_err(|error| error.to_string())?;
        }
        if matches!(config.mode.as_str(), "seekdb_server" | "oceanbase_server")
            && config.host.trim().is_empty()
        {
            return Err("Server mode requires a host address".to_string());
        }
        let current_config = state
            .storage_config
            .lock()
            .map_err(|_| "Storage config lock is poisoned".to_string())?
            .clone();
        let effective_mode = state
            .effective_storage_mode
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?
            .clone();
        let same_target = effective_mode == config.mode
            && match config.mode.as_str() {
                "sqlite_embedded" => current_config.path == config.path,
                "seekdb_embedded" => {
                    current_config.path == config.path && current_config.database == config.database
                }
                _ => {
                    current_config.host == config.host
                        && current_config.port == config.port
                        && current_config.tenant == config.tenant
                        && current_config.database == config.database
                        && current_config.user == config.user
                }
            };
        let previous_ready = {
            let mut ready = state
                .storage_ready
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())?;
            let previous = *ready;
            *ready = false;
            previous
        };
        let result = (|| -> Result<StorageStatus, String> {
            let snapshot = sanitized_store(
                &state
                    .data
                    .lock()
                    .map_err(|_| "State lock is poisoned".to_string())?
                    .clone(),
            );
            write_storage_backup(&state.data_dir, &snapshot)?;
            let mut engine = if config.mode == "sqlite_embedded" {
                StorageEngine::Sqlite(sqlite_database_path(&state.data_dir, &config))
            } else {
                ensure_seekdb_runtime(&state.data_dir)?;
                let pending = state.data_dir.join("storage.pending.json");
                fs::write(
                    &pending,
                    serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
                let bridge = SeekDbBridge::open(&pending, &state.data_dir);
                let _ = fs::remove_file(&pending);
                StorageEngine::SeekDb(bridge?)
            };
            let final_snapshot = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?
                .clone();
            let source_log_count = if previous_ready {
                state
                    .storage
                    .lock()
                    .map_err(|_| "Storage lock is poisoned".to_string())?
                    .log_count()?
            } else {
                0
            };
            engine
                .save_core(&final_snapshot)
                .map_err(|error| format!("Failed to write target storage instances and vault: {error}"))?;
            if !same_target && previous_ready {
                engine
                    .clear_logs()
                    .map_err(|error| format!("Failed to clean target storage old logs: {error}"))?;
                let mut before_sequence = None;
                // Stream logs in bounded pages so a storage switch does not load the full history.
                loop {
                    let page = state
                        .storage
                        .lock()
                        .map_err(|_| "Storage lock is poisoned".to_string())?
                        .query_logs(&LogQuery {
                            before_sequence,
                            after_sequence: None,
                            limit: LOG_CLEANUP_BATCH_SIZE,
                        })?;
                    if page.entries.is_empty() {
                        break;
                    }
                    before_sequence = page.entries.iter().map(|log| log.sequence).min();
                    engine
                        .append_logs(&page.entries)
                        .map_err(|error| format!("Failed to migrate log pagination: {error}"))?;
                    if !page.has_more {
                        break;
                    }
                }
            }
            // A fresh installation legitimately reads back as an empty store.
            verify_storage_snapshot(&mut engine, &final_snapshot, source_log_count)?;
            let mut data_guard = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?;
            let pending_logs = std::mem::take(&mut data_guard.logs);
            if let Err(error) = engine.append_logs(&pending_logs) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            let expected_log_count = source_log_count + pending_logs.len();
            if let Err(error) =
                verify_storage_snapshot(&mut engine, &final_snapshot, expected_log_count)
            {
                data_guard.logs = pending_logs;
                return Err(format!("Target storage validation failed during switch: {error}"));
            }
            if let Err(error) = write_local_credentials(
                &state.credentials_path,
                &LocalCredentials {
                    storage_password: config.password.clone(),
                },
            ) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            if let Err(error) = write_storage_config(&state.config_path, &config) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            // Publish the new engine only after data, credentials, and configuration are durable.
            *state
                .storage
                .lock()
                .map_err(|_| "Storage lock is poisoned".to_string())? = engine;
            *state
                .storage_config
                .lock()
                .map_err(|_| "Storage config lock is poisoned".to_string())? = config.clone();
            *state
                .effective_storage_mode
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())? = config.mode.clone();
            *state
                .storage_error
                .lock()
                .map_err(|_| "Storage error lock is poisoned".to_string())? = None;
            *state
                .storage_ready
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())? = true;
            *state
                .storage_setup_required
                .lock()
                .map_err(|_| "Storage setup state lock is poisoned".to_string())? = false;
            drop(data_guard);
            storage_status_value(&state)
        })();
        if result.is_err() {
            if let Ok(mut ready) = state.storage_ready.lock() {
                *ready = previous_ready;
            }
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

fn docker_compose_file(project_dir: &Path) -> Option<PathBuf> {
    [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .map(|name| project_dir.join(name))
    .find(|path| path.is_file())
}

fn docker_compose_check(project_dir: &Path) -> Option<String> {
    let compose_file = docker_compose_file(project_dir)?;
    let docker_status = check_docker();
    let mut missing = Vec::new();
    if !docker_status.cli_available {
        missing.push("Docker CLI not installed");
    }
    if !docker_status.compose_v2_available {
        missing.push("Docker Compose V2 not installed");
    }
    if !docker_status.daemon_running {
        missing.push("Docker not started");
    }
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "Project contains {}, but{}. Please install and start Docker before continuing.",
            compose_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("docker-compose.yml"),
            missing.join("、")
        ))
    }
}

fn check_docker() -> DockerStatus {
    let cli_available = configured_command("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let compose_v2_available = cli_available
        && configured_command("docker")
            .args(["compose", "version", "--short"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    let daemon_running = cli_available
        && configured_command("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    DockerStatus {
        cli_available,
        compose_v2_available,
        daemon_running,
    }
}

#[tauri::command]
fn system_info(state: State<'_, DesktopState>) -> SystemInfo {
    let (program, prefix) = cli_parts();
    let config = state
        .storage_config
        .lock()
        .ok()
        .map(|config| config.clone())
        .unwrap_or_default();
    let effective_mode = state
        .effective_storage_mode
        .lock()
        .map(|mode| mode.clone())
        .unwrap_or_else(|_| "sqlite_embedded".to_string());
    let (data_path, storage) = match effective_mode.as_str() {
        "seekdb_embedded" => (config.path, "Embedded SeekDB".to_string()),
        "seekdb_server" | "oceanbase_server" => (
            format!("{}:{} / {}", config.host, config.port, config.database),
            "SeekDB / OceanBase Server".to_string(),
        ),
        _ => (
            sqlite_database_path(&state.data_dir, &config)
                .to_string_lossy()
                .to_string(),
            "Embedded SQLite".to_string(),
        ),
    };
    let docker_status = check_docker();
    SystemInfo {
        app_name: "AgentSeek".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_path,
        cli_strategy: std::iter::once(program)
            .chain(prefix)
            .collect::<Vec<_>>()
            .join(" "),
        storage: format!("{storage} (desktop state only; isolated from template instances)"),
        docker_available: docker_status.cli_available,
        docker_compose_available: docker_status.compose_v2_available,
        docker_running: docker_status.daemon_running,
    }
}

#[tauri::command]
fn check_instance_docker_requirements(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<Option<String>, String> {
    let instance = instance_by_id(&state, &instance_id)?;
    if let Some(message) = docker_compose_check(Path::new(&instance.work_dir)) {
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "error",
            &message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
        Ok(Some(format!("{} instance startup process exited, please check lifecycle logs", instance.name)))
    } else {
        Ok(None)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let runtime_dir = data_dir.join("runtime");
            let requirements = load_runtime_requirements().map_err(std::io::Error::other)?;
            let node_bin = managed_node_bin(&runtime_dir, &requirements.versions.node.managed);
            let _ = fs::create_dir_all(&runtime_dir);
            env::set_var("AGENTSEEK_DESKTOP_RUNTIME_DIR", &runtime_dir);
            env::set_var("AGENTSEEK_DESKTOP_NODE_BIN", &node_bin);
            let state = DesktopState::load(data_dir);
            let cleanup_state = state.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(3_600));
                let _ = cleanup_state.cleanup_logs();
            });
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cli_status,
            runtime_install_plan,
            runtime_install_progress,
            execute_runtime_install,
            list_templates,
            list_instances,
            list_vault,
            save_vault,
            prepare_instance,
            load_instance_env,
            save_instance_env,
            continue_install,
            deployment_progress,
            stop_instance,
            restart_instance,
            mark_env_edited,
            delete_instance,
            list_logs,
            log_settings,
            save_log_settings,
            import_env,
            list_env_files,
            export_env,
            storage_status,
            configure_storage,
            system_info,
            check_instance_docker_requirements,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AgentSeek Desktop");
}

#[cfg(test)]
mod tests {
    use rusqlite::{params, Connection};
    use std::{
        collections::HashSet,
        env, fs,
        io::Write as _,
        net::{Ipv4Addr, Ipv6Addr, TcpListener},
        path::Path,
    };

    use super::{
        accepts_port_flag, agentseek_update_available, available_ephemeral_port, command_tokens,
        compact_runtime_log_record,
        dependency_commands, enrich_service_endpoints, instance_target_path,
        is_local_service_port_key, is_secret_env_key, list_env_files,
        merge_env_entries,
        normalize_storage_database, parse_agentseek_package_version, parse_agentseek_version,
        parse_env, parse_templates, parse_uv_tool_version, port_is_available,
        posix_runtime_install_script, prune_logs, read_runtime_log_records,
        remove_command_port, remove_instance_work_dir, render_env,
        repair_lifecycle_log_categories,
        repair_predeployment_restart_statuses, required_runtime_dependencies,
        resolve_lifecycle_ports, resolve_port_conflicts, runtime_log_spool_paths,
        runtime_stream_level, sanitized_store,
        service_display_name, split_env_value, sqlite_database_path,
        synchronize_instance_port_configs, synchronize_lifecycle_content,
        synchronize_lifecycle_project_name_content,
        sync_docker_compose_port_mappings, sync_process_command_ports,
        truncate_log_text, unique_stamp, validate_runtime_requirements, version_at_least,
        windows_runtime_install_script, write_local_credentials, write_storage_config, AppStore,
        CliStatus, DesktopState, EnvVariable, InstanceRecord, LifecycleManifest, LocalCredentials,
        LogEntry, LogQuery, RuntimeRequirements, StorageConfig, StorageEngine,
        DEFAULT_RUNTIME_REQUIREMENTS, MAX_LOG_TEXT_BYTES, SECONDS_PER_DAY,
    };

    #[test]
    fn default_storage_is_embedded_seekdb() {
        let config = StorageConfig::default();
        assert_eq!(config.mode, "seekdb_embedded");
        assert_eq!(config.database, "agentseek_desktop");
        assert!(!config.setup_completed);
    }

    #[test]
    fn legacy_storage_config_requires_first_run_confirmation() {
        let config: StorageConfig =
            serde_json::from_str(r#"{"mode":"sqlite_embedded"}"#).expect("parse legacy config");
        assert!(!config.setup_completed);
    }

    #[test]
    fn fresh_start_does_not_create_a_sqlite_database() {
        let root = env::temp_dir().join(format!("agentseek-desktop-first-run-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create first-run test directory");

        let state = DesktopState::load(root.clone());

        assert!(!root.join("agentseek-desktop.sqlite3").exists());
        assert!(*state
            .storage_setup_required
            .lock()
            .expect("lock setup state"));
        assert!(!*state.storage_ready.lock().expect("lock storage state"));
        assert!(state.ensure_storage_ready().is_err());
        assert!(state.ensure_storage_configurable().is_ok());

        fs::remove_dir_all(root).expect("remove first-run test directory");
    }

    #[test]
    fn embedded_storage_database_name_is_fixed() {
        for mode in ["sqlite_embedded", "seekdb_embedded"] {
            let mut config = StorageConfig {
                mode: mode.to_string(),
                database: "custom_database".to_string(),
                ..StorageConfig::default()
            };
            normalize_storage_database(&mut config);
            assert_eq!(config.database, "agentseek_desktop");
        }

        let mut server = StorageConfig {
            mode: "seekdb_server".to_string(),
            database: "custom_database".to_string(),
            ..StorageConfig::default()
        };
        normalize_storage_database(&mut server);
        assert_eq!(server.database, "custom_database");

        server.database.clear();
        normalize_storage_database(&mut server);
        assert_eq!(server.database, "agentseek_desktop");
    }

    #[test]
    fn sqlite_database_path_uses_the_selected_directory() {
        let app_data = Path::new("/tmp/agentseek-desktop");
        let default_config = StorageConfig::default();
        assert_eq!(
            sqlite_database_path(app_data, &default_config),
            app_data.join("agentseek-desktop.sqlite3")
        );

        let custom_config = StorageConfig {
            path: "/tmp/custom-agentseek-data".to_string(),
            ..StorageConfig::default()
        };
        assert_eq!(
            sqlite_database_path(app_data, &custom_config),
            Path::new("/tmp/custom-agentseek-data/agentseek-desktop.sqlite3")
        );
    }

    #[test]
    fn runtime_tool_tracebacks_are_compacted_to_error_and_reason() {
        let mut suppress_traceback = false;
        assert_eq!(
            compact_runtime_log_record(
                "2026-07-22 17:52:26.376 | ERROR | bub.tools:wrapped:34 - tool.call.error name=web.fetch elapsed_time=153.81ms",
                &mut suppress_traceback,
            )
            .as_deref(),
            Some("Tool call failed: web.fetch (153.81ms)")
        );
        assert!(suppress_traceback);
        assert_eq!(
            compact_runtime_log_record(
                "Traceback (most recent call last):",
                &mut suppress_traceback
            ),
            None
        );
        assert_eq!(
            compact_runtime_log_record(
                "  File \"/tmp/site-packages/aiohttp/client.py\", line 701, in _request",
                &mut suppress_traceback,
            ),
            None
        );
        assert_eq!(
            compact_runtime_log_record(
                "aiohttp.client_exceptions.ClientResponseError: 403, message='Forbidden'",
                &mut suppress_traceback,
            )
            .as_deref(),
            Some("Failure reason: ClientResponseError: 403, message='Forbidden'")
        );
        assert!(!suppress_traceback);

        let mut timeout_traceback = true;
        assert_eq!(
            compact_runtime_log_record("TimeoutError", &mut timeout_traceback).as_deref(),
            Some("Failure reason: TimeoutError (request timeout)")
        );
        assert!(!timeout_traceback);
    }

    #[test]
    fn runtime_log_spool_resumes_only_after_a_complete_line() {
        let root = env::temp_dir().join(format!("agentseek-runtime-spool-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create runtime spool test directory");
        let (log_path, cursor_path) = runtime_log_spool_paths(&root, "../instance/demo");
        assert_eq!(log_path.parent(), Some(root.join("runtime-logs").as_path()));
        assert_eq!(cursor_path.parent(), log_path.parent());

        fs::create_dir_all(log_path.parent().expect("runtime log parent"))
            .expect("create runtime log directory");
        fs::write(&log_path, "first\npartial").expect("write initial runtime output");
        let (start, records) =
            read_runtime_log_records(&log_path, 0, false).expect("read complete runtime line");
        assert_eq!(start, 0);
        assert_eq!(records, vec![("first".to_string(), 6)]);

        let mut output = fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("reopen runtime output");
        output
            .write_all(b" tail\n")
            .expect("complete partial runtime line");
        let (_, resumed) =
            read_runtime_log_records(&log_path, 6, false).expect("resume runtime output");
        assert_eq!(
            resumed,
            vec![(
                "partial tail".to_string(),
                "first\npartial tail\n".len() as u64
            )]
        );

        fs::remove_dir_all(root).expect("remove runtime spool test directory");
    }

    #[test]
    fn storage_config_and_backups_exclude_secrets() {
        let root = env::temp_dir().join(format!("agentseek-desktop-secret-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create secret test directory");
        let config_path = root.join("storage.json");
        let config = StorageConfig {
            password: "database-secret".to_string(),
            ..StorageConfig::default()
        };
        write_storage_config(&config_path, &config).expect("write sanitized config");
        let config_text = fs::read_to_string(&config_path).expect("read sanitized config");
        assert!(!config_text.contains("database-secret"));

        let store = AppStore {
            instances: Vec::new(),
            vault: vec![EnvVariable {
                key: "API_KEY".to_string(),
                value: "vault-secret".to_string(),
                comment: String::new(),
                source: "vault".to_string(),
                modified: true,
            }],
            logs: Vec::new(),
        };
        let persisted = serde_json::to_string(&sanitized_store(&store)).expect("serialize store");
        assert!(!persisted.contains("vault-secret"));
        assert!(persisted.contains("API_KEY"));
        fs::remove_dir_all(root).expect("remove secret test directory");
    }

    #[test]
    fn local_credentials_are_private_and_do_not_use_system_keyrings() {
        let root =
            env::temp_dir().join(format!("agentseek-desktop-credentials-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create credentials test directory");
        let path = root.join("credentials.json");
        write_local_credentials(
            &path,
            &LocalCredentials {
                storage_password: "database-secret".to_string(),
            },
        )
        .expect("write local credentials");
        let contents = fs::read_to_string(&path).expect("read local credentials");
        assert!(contents.contains("database-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)
                .expect("read credentials metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(root).expect("remove credentials test directory");
    }

    #[test]
    fn oversized_log_text_is_truncated_on_a_utf8_boundary() {
        let value = "\u{2192}".repeat(MAX_LOG_TEXT_BYTES);
        let truncated = truncate_log_text(value);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.contains("log content truncated"));
        assert!(truncated.len() < MAX_LOG_TEXT_BYTES + 100);
    }

    fn test_log(id: &str, instance_id: Option<&str>, category: &str, created_at: u64) -> LogEntry {
        LogEntry {
            id: id.to_string(),
            instance_id: instance_id.map(str::to_string),
            instance_name: instance_id.unwrap_or("AgentSeek").to_string(),
            category: category.to_string(),
            level: "info".to_string(),
            message: id.to_string(),
            command: None,
            created_at,
            sequence: created_at,
        }
    }

    #[test]
    fn log_retention_preserves_active_lifecycle_and_expires_runtime_or_deleted_instances() {
        let now = 20 * SECONDS_PER_DAY;
        let active = InstanceRecord {
            id: "active".to_string(),
            name: "Active".to_string(),
            template_id: "bub/default".to_string(),
            status: "running".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: "/tmp/active".to_string(),
            env_example_path: None,
            env_path: None,
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        let old = now - 10 * SECONDS_PER_DAY;
        let recent = now - SECONDS_PER_DAY;
        let mut store = AppStore {
            instances: vec![active],
            vault: Vec::new(),
            logs: vec![
                test_log("active-lifecycle", Some("active"), "install", old),
                test_log("active-runtime", Some("active"), "runtime", old),
                test_log("deleted-lifecycle-old", Some("deleted"), "install", old),
                test_log(
                    "deleted-lifecycle-recent",
                    Some("deleted"),
                    "install",
                    recent,
                ),
                test_log("deleted-runtime-recent", Some("deleted"), "runtime", recent),
                test_log("platform-lifecycle", None, "config", old),
            ],
        };

        let removed = prune_logs(&mut store, 7, now);
        let remaining = store
            .logs
            .iter()
            .map(|log| log.id.as_str())
            .collect::<HashSet<_>>();

        assert!(removed.contains(&"active-runtime".to_string()));
        assert!(removed.contains(&"deleted-lifecycle-old".to_string()));
        assert!(remaining.contains("active-lifecycle"));
        assert!(remaining.contains("deleted-lifecycle-recent"));
        assert!(remaining.contains("deleted-runtime-recent"));
        assert!(remaining.contains("platform-lifecycle"));
    }

    #[test]
    fn sqlite_log_append_is_incremental_and_deletes_expired_rows() {
        let root = env::temp_dir().join(format!("agentseek-desktop-log-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create log test directory");
        let database = root.join("desktop.sqlite3");
        let mut engine = StorageEngine::Sqlite(database.clone());
        let initial = AppStore {
            instances: Vec::new(),
            vault: vec![EnvVariable {
                key: "PERSISTED".to_string(),
                value: "yes".to_string(),
                comment: String::new(),
                source: "vault".to_string(),
                modified: false,
            }],
            logs: Vec::new(),
        };
        engine.save_core(&initial).expect("save initial core state");
        engine
            .append_log(&test_log("old", None, "runtime", 1), &[])
            .expect("append old log");
        let appended = test_log("new", None, "runtime", 2);
        engine
            .append_log(&appended, &["old".to_string()])
            .expect("append one log");

        let loaded = engine.load().expect("load state").expect("stored state");
        assert_eq!(loaded.vault.len(), 1);
        assert_eq!(loaded.vault[0].key, "PERSISTED");
        assert_eq!(loaded.vault[0].value, "yes");
        assert!(loaded.logs.is_empty());
        let page = engine
            .query_logs(&LogQuery {
                before_sequence: None,
                after_sequence: None,
                limit: 10,
            })
            .expect("query persisted logs");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].id, "new");
        fs::remove_dir_all(root).expect("remove log test directory");
    }

    #[test]
    fn sqlite_logs_are_paged_without_loading_them_into_app_store() {
        let root = env::temp_dir().join(format!("agentseek-desktop-pages-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create paging test directory");
        let mut engine = StorageEngine::Sqlite(root.join("desktop.sqlite3"));
        engine
            .save_core(&AppStore::default())
            .expect("initialize core storage");
        let logs = (1..=6)
            .map(|sequence| test_log(&format!("log-{sequence}"), None, "runtime", sequence))
            .collect::<Vec<_>>();
        engine.append_logs(&logs).expect("append paging logs");

        let latest = engine
            .query_logs(&LogQuery {
                before_sequence: None,
                after_sequence: None,
                limit: 2,
            })
            .expect("query latest page");
        assert_eq!(
            latest
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![6, 5]
        );
        assert!(latest.has_more);

        let earlier = engine
            .query_logs(&LogQuery {
                before_sequence: Some(5),
                after_sequence: None,
                limit: 2,
            })
            .expect("query earlier page");
        assert_eq!(
            earlier
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![4, 3]
        );

        let newer = engine
            .query_logs(&LogQuery {
                before_sequence: None,
                after_sequence: Some(4),
                limit: 10,
            })
            .expect("query newer page");
        assert_eq!(
            newer
                .entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(engine.max_log_sequence().expect("max sequence"), 6);
        assert!(engine
            .load()
            .expect("load core state")
            .expect("core state exists")
            .logs
            .is_empty());
        fs::remove_dir_all(root).expect("remove paging test directory");
    }

    #[test]
    fn sqlite_core_updates_preserve_logs_and_cleanup_runs_in_storage() {
        let root = env::temp_dir().join(format!("agentseek-desktop-cleanup-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create cleanup test directory");
        let mut engine = StorageEngine::Sqlite(root.join("desktop.sqlite3"));
        let mut core = AppStore {
            instances: Vec::new(),
            vault: vec![EnvVariable {
                key: "FIRST".to_string(),
                value: String::new(),
                comment: String::new(),
                source: "vault".to_string(),
                modified: false,
            }],
            logs: Vec::new(),
        };
        engine.save_core(&core).expect("save initial core");
        let now = 20 * SECONDS_PER_DAY;
        engine
            .append_logs(&[
                test_log("old-runtime", None, "runtime", now - 10 * SECONDS_PER_DAY),
                test_log(
                    "old-deleted-lifecycle",
                    Some("deleted"),
                    "install",
                    now - 10 * SECONDS_PER_DAY,
                ),
                test_log(
                    "recent-deleted-lifecycle",
                    Some("deleted"),
                    "install",
                    now - SECONDS_PER_DAY,
                ),
            ])
            .expect("append cleanup logs");
        core.vault[0].key = "UPDATED".to_string();
        engine.save_core(&core).expect("update core without logs");
        assert_eq!(engine.log_count().expect("count preserved logs"), 3);

        assert_eq!(engine.cleanup_logs(7, now).expect("cleanup logs"), 2);
        let remaining = engine
            .query_logs(&LogQuery {
                before_sequence: None,
                after_sequence: None,
                limit: 10,
            })
            .expect("query remaining logs");
        assert_eq!(remaining.entries.len(), 1);
        assert_eq!(remaining.entries[0].id, "recent-deleted-lifecycle");
        assert!(engine
            .load()
            .expect("load core after cleanup")
            .expect("core exists")
            .logs
            .is_empty());
        fs::remove_dir_all(root).expect("remove cleanup test directory");
    }

    #[test]
    fn sqlite_log_migration_streams_pages_and_preserves_sequences() {
        let root = env::temp_dir().join(format!("agentseek-desktop-switch-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create switch test directory");
        let mut source = StorageEngine::Sqlite(root.join("source.sqlite3"));
        let mut target = StorageEngine::Sqlite(root.join("target.sqlite3"));
        source
            .save_core(&AppStore::default())
            .expect("initialize source");
        target
            .save_core(&AppStore::default())
            .expect("initialize target");
        let logs = (1..=2_505)
            .map(|sequence| test_log(&format!("log-{sequence}"), None, "runtime", sequence))
            .collect::<Vec<_>>();
        source.append_logs(&logs).expect("append source logs");

        target.clear_logs().expect("clear target logs");
        let mut before_sequence = None;
        loop {
            let page = source
                .query_logs(&LogQuery {
                    before_sequence,
                    after_sequence: None,
                    limit: 1_000,
                })
                .expect("read migration page");
            if page.entries.is_empty() {
                break;
            }
            before_sequence = page.entries.iter().map(|entry| entry.sequence).min();
            target
                .append_logs(&page.entries)
                .expect("append migration page");
            if !page.has_more {
                break;
            }
        }

        assert_eq!(target.log_count().expect("target log count"), 2_505);
        assert_eq!(
            target.max_log_sequence().expect("target max sequence"),
            2_505
        );
        assert!(target
            .load()
            .expect("load target core")
            .expect("target core exists")
            .logs
            .is_empty());
        fs::remove_dir_all(root).expect("remove switch test directory");
    }

    #[test]
    fn sqlite_storage_migrates_legacy_state_into_domain_tables() {
        let root = env::temp_dir().join(format!("agentseek-desktop-storage-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create storage test directory");
        let database = root.join("desktop.sqlite3");
        let legacy = AppStore {
            instances: Vec::new(),
            vault: vec![EnvVariable {
                key: "OPENAI_API_KEY".to_string(),
                value: "secret".to_string(),
                comment: "API key".to_string(),
                source: "vault".to_string(),
                modified: false,
            }],
            logs: vec![LogEntry {
                id: "log-1".to_string(),
                instance_id: None,
                instance_name: "AgentSeek".to_string(),
                category: "install".to_string(),
                level: "success".to_string(),
                message: "ready".to_string(),
                command: None,
                created_at: 1,
                sequence: 1,
            }],
        };
        {
            let connection = Connection::open(&database).expect("open legacy database");
            connection
                .execute_batch(
                    "CREATE TABLE app_state (
                        id INTEGER PRIMARY KEY CHECK (id = 1),
                        payload TEXT NOT NULL
                    );",
                )
                .expect("create legacy table");
            connection
                .execute(
                    "INSERT INTO app_state (id, payload) VALUES (1, ?1)",
                    params![serde_json::to_string(&legacy).expect("serialize legacy state")],
                )
                .expect("write legacy state");
        }

        let mut engine = StorageEngine::Sqlite(database.clone());
        let loaded = engine
            .load()
            .expect("migrate legacy storage")
            .expect("load migrated storage");

        assert_eq!(loaded.vault.len(), 1);
        assert_eq!(loaded.vault[0].comment, "API key");
        assert_eq!(loaded.vault[0].value, "secret");
        assert!(loaded.logs.is_empty());
        assert_eq!(engine.log_count().expect("count migrated logs"), 1);
        assert_eq!(
            engine
                .query_logs(&LogQuery {
                    before_sequence: None,
                    after_sequence: None,
                    limit: 10,
                })
                .expect("query migrated logs")
                .entries[0]
                .id,
            "log-1"
        );
        let connection = Connection::open(&database).expect("reopen migrated database");
        for table in ["instances", "env_vault", "logs"] {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("check domain table");
            assert!(exists, "missing table {table}");
        }
        let legacy_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'app_state')",
                [],
                |row| row.get(0),
            )
            .expect("check legacy table");
        assert!(!legacy_exists);
        let schema_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read schema version");
        assert_eq!(schema_version, 2);
        drop(connection);
        fs::remove_dir_all(root).expect("remove storage test directory");
    }

    #[test]
    fn bundled_runtime_requirements_are_valid() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        validate_runtime_requirements(&requirements).expect("validate requirements");
    }

    #[test]
    fn agentseek_updates_do_not_change_minimum_version_compatibility() {
        assert!(version_at_least("AGENTSEEK v0.0.4", &[0, 0, 4]));
        assert!(agentseek_update_available(
            "AGENTSEEK v0.0.4",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.5",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.6",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available("AGENTSEEK v0.0.4", None, true));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.4",
            Some("0.0.5"),
            false
        ));
    }

    #[test]
    fn available_agentseek_update_is_included_in_the_install_plan() {
        let status = CliStatus {
            uv_compatible: true,
            node_compatible: true,
            npm_compatible: true,
            cli_compatible: true,
            cli_update_available: true,
            ..CliStatus::default()
        };

        assert_eq!(required_runtime_dependencies(&status), ["agentseek"]);
    }

    #[test]
    fn runtime_install_scripts_use_platform_installers_without_mutating_system_uv() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        let status = CliStatus {
            uv_available: true,
            uv_path: "/usr/local/bin/uv".to_string(),
            node_compatible: true,
            npm_compatible: true,
            cli_compatible: true,
            ..CliStatus::default()
        };
        let task_dir = Path::new("/tmp/agentseek-install-task");
        let runtime_root = Path::new("/tmp/agentseek-runtime");

        let posix = posix_runtime_install_script(&requirements, &status, task_dir, runtime_root);
        assert!(posix.contains("https://astral.sh/uv/install.sh"));
        assert!(posix.contains("$HOME/.local/bin/uv"));
        assert!(posix.contains("--output \"$installer_file.tmp\""));
        assert!(posix.contains("bash -n \"$installer_file.tmp\""));
        assert!(!posix.contains("| sh"));
        assert!(!posix.contains("uv self update"));
        assert!(!posix.contains("export METHOD=script"));
        assert!(!posix.contains(
            "Installation completed. AgentSeek Desktop will recheck automatically. Press Enter"
        ));
        assert!(!posix
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)));
        #[cfg(unix)]
        {
            let script_path =
                env::temp_dir().join(format!("agentseek-install-{}.command", unique_stamp()));
            fs::write(&script_path, &posix).expect("write generated POSIX installer");
            let output = std::process::Command::new("bash")
                .arg("-n")
                .arg(&script_path)
                .output()
                .expect("validate generated POSIX installer");
            fs::remove_file(script_path).expect("remove generated POSIX installer");
            assert!(
                output.status.success(),
                "generated installer is invalid: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let windows =
            windows_runtime_install_script(&requirements, &status, task_dir, runtime_root);
        assert!(windows.contains("https://astral.sh/uv/install.ps1"));
        assert!(windows.contains("Invoke-DownloadWithRetry"));
        assert!(!windows.contains("https://astral.sh/uv/install.sh"));
        assert!(!windows.contains("Read-Host"));
        assert!(!windows.contains("-NoExit"));
        assert!(!windows
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)));
    }

    #[test]
    fn managed_node_install_uses_private_nvm_and_bundled_npm() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        let commands = dependency_commands(
            &requirements,
            "macos",
            Some(Path::new("/tmp/agentseek-runtime")),
            true,
            false,
        );
        let command = commands.get("node").expect("node install command");

        assert!(command.contains("NVM_DIR=\"/tmp/agentseek-runtime/nvm\""));
        assert!(command.contains("nvm install 24"));
        assert!(command.contains("node --version && npm --version"));
        assert!(!command.contains("install -g npm"));
    }

    #[test]
    fn dependency_versions_are_compared_across_command_formats() {
        assert!(version_at_least("uv 0.7.11", &[0, 7, 0]));
        assert!(version_at_least("v20.19.0", &[20, 19, 0]));
        assert!(version_at_least("git version 2.30.0", &[2, 30, 0]));
        assert!(!version_at_least("9.9.0", &[10, 0, 0]));
        assert!(!version_at_least("not installed", &[1, 0, 0]));
    }

    #[test]
    fn only_secret_environment_keys_are_redacted() {
        assert!(is_secret_env_key("OPENAI_API_KEY"));
        assert!(is_secret_env_key("DATABASE_PASSWORD"));
        assert!(!is_secret_env_key("FRONTEND_PORT"));
        assert!(!is_secret_env_key("COPILOTKIT_PORT"));
    }

    #[test]
    fn agentseek_version_is_read_from_uv_tool_list() {
        let output = "agentseek v0.0.4\n- agentseek\n";
        assert_eq!(
            parse_uv_tool_version(output, "agentseek").as_deref(),
            Some("agentseek 0.0.4")
        );
    }

    #[test]
    fn agentseek_version_is_read_after_banner() {
        let output = "    _                    _\n   / \\   __ _  ___ _ __\nAGENTSEEK v0.0.4\n";
        assert_eq!(
            parse_agentseek_version(output).as_deref(),
            Some("AGENTSEEK v0.0.4")
        );
    }

    #[test]
    fn agentseek_latest_version_is_read_from_package_metadata() {
        assert_eq!(
            parse_agentseek_package_version(br#"{"info":{"version":"0.0.5"}}"#)
                .expect("parse package version"),
            "0.0.5"
        );
    }

    #[test]
    fn env_round_trip_preserves_comments_and_values() {
        let input = "# API endpoint\nOPENAI_BASE_URL=https://example.com/v1\n\n# Secret\nOPENAI_API_KEY=test-key\n";
        let entries = parse_env(input);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].comment, "API endpoint");
        assert_eq!(entries[1].value, "test-key");

        let rendered = render_env(&entries);
        assert!(rendered.contains("# API endpoint\nOPENAI_BASE_URL=https://example.com/v1"));
        assert!(rendered.contains("# Secret\nOPENAI_API_KEY=test-key"));
    }

    #[test]
    fn empty_vault_values_do_not_hide_template_defaults() {
        let source = parse_env("MODEL=openai:gpt-4o-mini\nPORT=5173\nAPI_KEY=\n");
        let vault = vec![
            EnvVariable {
                key: "MODEL".to_string(),
                value: String::new(),
                comment: "Model name".to_string(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "PORT".to_string(),
                value: "6000".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
        ];

        let merged = merge_env_entries(&source, &vault);
        assert_eq!(merged[0].value, "openai:gpt-4o-mini");
        assert_eq!(merged[0].source, "template");
        assert_eq!(merged[0].comment, "Model name");
        assert_eq!(merged[1].value, "6000");
        assert_eq!(merged[1].source, "vault");
        assert!(merged[2].value.is_empty());
    }

    #[test]
    fn template_output_is_parsed_into_rows() {
        let output = "\n  langchain (2 templates)\n  ─────\n    langchain/default\n      Default agent.\n    langchain/agentic-rag\n      Agentic RAG.\n";
        let templates = parse_templates(output);

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].id, "langchain/default");
        assert_eq!(templates[1].description, "Agentic RAG.");
    }

    #[test]
    fn inline_env_comments_are_parsed_without_breaking_hash_values() {
        let entries = parse_env("MODEL=openai:gpt-4o # Default model\nTOKEN=abc#123\n");
        assert_eq!(entries[0].value, "openai:gpt-4o");
        assert_eq!(entries[0].comment, "Default model");
        assert_eq!(entries[1].value, "abc#123");
        assert_eq!(
            split_env_value("\"value # text\" # note").1.as_deref(),
            Some("note")
        );
    }

    #[test]
    fn lifecycle_services_expose_all_declared_urls() {
        let manifest: LifecycleManifest = toml::from_str(
            "[services.app]\nurl = \"http://127.0.0.1:5173\"\n[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n",
        )
        .expect("parse lifecycle services");

        assert_eq!(manifest.services.len(), 3);
        assert_eq!(service_display_name("app"), "Frontend");
        assert_eq!(service_display_name("gateway"), "Agent / Gateway");
        assert_eq!(service_display_name("copilotkit"), "CopilotKit Runtime");
    }

    #[test]
    fn deleting_an_instance_removes_its_working_directory() {
        let root = env::temp_dir().join(format!("agentseek-desktop-delete-{}", unique_stamp()));
        fs::create_dir_all(root.join("nested")).expect("create instance directory");
        fs::write(root.join("nested/data.txt"), "instance data").expect("write instance file");

        remove_instance_work_dir(&root.to_string_lossy()).expect("remove instance directory");

        assert!(!root.exists());
    }

    #[test]
    fn instance_working_directory_is_parent_plus_instance_name() {
        let parent = Path::new("/tmp/agentseek-instances");

        assert_eq!(
            instance_target_path(parent, "rag-development").expect("build target path"),
            parent.join("rag-development")
        );
        assert!(instance_target_path(parent, "nested/name").is_err());
        assert!(instance_target_path(parent, "..").is_err());
    }

    #[test]
    fn env_file_scan_prefers_example_and_ignores_nested_files() {
        let root = env::temp_dir().join(format!("agentseek-desktop-env-scan-{}", unique_stamp()));
        fs::create_dir_all(root.join("frontend")).expect("create test directory");
        for name in [
            ".env",
            ".env.example",
            ".env.development",
            ".env1",
            "README.md",
        ] {
            fs::write(root.join(name), "KEY=value\n").expect("write test file");
        }
        fs::write(root.join("frontend/.env"), "FRONTEND=true\n").expect("write nested env");

        let files = list_env_files(root.to_string_lossy().to_string()).expect("scan env files");

        assert_eq!(files.len(), 4);
        assert!(files[0].ends_with("/.env.example"));
        assert!(files.iter().any(|file| file.ends_with("/.env")));
        assert!(files.iter().any(|file| file.ends_with("/.env.development")));
        assert!(files.iter().any(|file| file.ends_with("/.env1")));
        assert!(files.iter().all(|file| !file.contains("frontend")));
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn historical_desktop_operations_move_to_lifecycle_category() {
        let mut store = AppStore {
            instances: Vec::new(),
            vault: Vec::new(),
            logs: [
                "Instance stopped",
                "Instance associated processes stopped\nWorking directory: /tmp/demo",
                "Doctor passed; instance restarted",
                "Instance processes, working directory, and record deleted",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, message)| LogEntry {
                id: format!("lifecycle-log-{index}"),
                instance_id: Some("instance".to_string()),
                instance_name: "Instance".to_string(),
                category: "runtime".to_string(),
                level: "success".to_string(),
                message: message.to_string(),
                command: None,
                created_at: index as u64,
                sequence: index as u64,
            })
            .collect(),
        };

        assert!(repair_lifecycle_log_categories(&mut store));
        assert!(store.logs.iter().all(|log| log.category == "install"));
    }

    #[test]
    fn runtime_stream_level_does_not_treat_normal_stderr_as_an_error() {
        assert_eq!(
            runtime_stream_level("INFO: Application startup complete."),
            "info"
        );
        assert_eq!(runtime_stream_level("WARNING: retrying request"), "warning");
        assert_eq!(
            runtime_stream_level("RuntimeError: connection failed"),
            "error"
        );
        assert_eq!(
            runtime_stream_level("unable to get image 'quay.io/oceanbase/seekdb:latest': Cannot connect to the Docker daemon at unix:///Users/sunchong/.orbstack/run/docker.sock. Is the docker daemon running?"),
            "error"
        );
        assert_eq!(
            runtime_stream_level("Cannot connect to the Docker daemon at unix:///Users/sunchong/.orbstack/run/docker.sock. Is the docker daemon running?"),
            "error"
        );
    }

    #[test]
    fn local_service_ports_are_reassigned_without_touching_database_ports() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind occupied port");
        let occupied_port = occupied.local_addr().expect("read occupied port").port();
        let free_gateway_port = available_ephemeral_port().expect("allocate gateway port");
        let mut entries = vec![
            EnvVariable {
                key: "FRONTEND_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "COPILOTKIT_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "MYSQL_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "COPILOTKIT_RUNTIME_URL".to_string(),
                value: format!("http://127.0.0.1:{occupied_port}/api/copilotkit"),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "BUB_AG_UI_PORT".to_string(),
                value: free_gateway_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "BUB_AG_UI_AGENT_URL".to_string(),
                value: "http://127.0.0.1:8088/agent".to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
        ];

        let changes = resolve_port_conflicts(&mut entries).expect("resolve port conflict");

        assert!(is_local_service_port_key("FRONTEND_PORT"));
        assert!(is_local_service_port_key("COPILOTKIT_PORT"));
        assert!(!is_local_service_port_key("MYSQL_PORT"));
        assert_eq!(changes.len(), 2);
        assert_ne!(entries[0].value, occupied_port.to_string());
        assert!(entries[0].modified);
        assert_ne!(entries[1].value, occupied_port.to_string());
        assert!(entries[1].modified);
        assert_eq!(entries[2].value, occupied_port.to_string());
        assert_eq!(
            entries[3].value,
            format!("http://127.0.0.1:{}/api/copilotkit", entries[1].value)
        );
        assert!(entries[3].modified);
        assert_eq!(
            entries[5].value,
            format!("http://127.0.0.1:{free_gateway_port}/agent")
        );
        assert!(entries[5].modified);
    }

    #[test]
    fn reassigned_ports_are_synchronized_to_instance_runtime_configs() {
        let root = env::temp_dir().join(format!("agentseek-desktop-ports-{}", unique_stamp()));
        fs::create_dir_all(root.join(".agentseek")).expect("create metadata directory");
        fs::create_dir_all(root.join("frontend")).expect("create frontend directory");
        let lifecycle = "version = 1\n\
[env.CTX_SERVER_PORT]\ndefault = \"8089\"\n\
[services.app]\nurl = \"http://127.0.0.1:5173\"\n\
[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n\
[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n\
[services.ctx]\nurl = \"http://127.0.0.1:8089/ctx\"\n\
[checks.frontend]\ntype = \"http\"\ntarget = \"http://127.0.0.1:5173\"\n\
[checks.gateway]\ntype = \"http\"\ntarget = \"http://127.0.0.1:8088/agent/health\"\n\
[checks.copilotkit]\ntype = \"http\"\ntarget = \"http://127.0.0.1:4000/health\"\n\
[checks.ctx]\ntype = \"http\"\ntarget = \"http://127.0.0.1:8089/ctx/health\"\n";
        let frontend_example = "COPILOTKIT_PORT=4000\n\
BUB_AG_UI_AGENT_URL=http://127.0.0.1:8088/agent\n\
VITE_COPILOTKIT_RUNTIME_PROXY=http://127.0.0.1:4000\n\
VITE_BUB_AG_UI_URL=http://127.0.0.1:8088\n\
FRONTEND_PORT=5173\n";
        fs::write(root.join(".agentseek/lifecycle.toml"), lifecycle).expect("write lifecycle");
        fs::write(root.join("frontend/.env.example"), frontend_example)
            .expect("write frontend example");
        let entries = parse_env(
            "BUB_AG_UI_PORT=57975\n\
FRONTEND_PORT=57980\n\
COPILOTKIT_PORT=57985\n\
CTX_SERVER_PORT=57990\n\
BUB_AG_UI_AGENT_URL=http://127.0.0.1:57975/agent\n",
        );

        let written = synchronize_instance_port_configs(&root, &entries)
            .expect("synchronize instance port configs");

        assert_eq!(written.len(), 2);
        let updated_lifecycle =
            fs::read_to_string(root.join(".agentseek/lifecycle.toml")).expect("read lifecycle");
        assert!(updated_lifecycle.contains("http://127.0.0.1:57980"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57975/agent"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57985/api/copilotkit"));
        assert!(updated_lifecycle.contains("default = \"57990\""));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57990/ctx"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57975/agent/health"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57985/health"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57990/ctx/health"));
        assert!(!updated_lifecycle.contains("127.0.0.1:5173"));
        assert!(!updated_lifecycle.contains("127.0.0.1:8088"));
        assert!(!updated_lifecycle.contains("127.0.0.1:4000"));
        assert!(!updated_lifecycle.contains("127.0.0.1:8089"));

        let frontend = fs::read_to_string(root.join("frontend/.env")).expect("read frontend env");
        assert!(frontend.contains("FRONTEND_PORT=57980"));
        assert!(frontend.contains("COPILOTKIT_PORT=57985"));
        assert!(frontend.contains("BUB_AG_UI_AGENT_URL=http://127.0.0.1:57975/agent"));
        assert!(frontend.contains("VITE_COPILOTKIT_RUNTIME_PROXY=http://127.0.0.1:57985"));
        assert!(frontend.contains("VITE_BUB_AG_UI_URL=http://127.0.0.1:57975"));
        assert_eq!(
            fs::read_to_string(root.join("frontend/.env.example"))
                .expect("read unchanged frontend example"),
            frontend_example
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn lifecycle_project_name_is_synchronized_without_changing_section_names() {
        let lifecycle = "version = 1\n\
template = \"deepagents/content-builder\"\n\
name = \"Content Builder DeepAgent\" # generated default\n\
[services.frontend]\n\
name = \"Frontend\"\n\
url = \"http://127.0.0.1:5174\"\n";

        let updated = synchronize_lifecycle_project_name_content(lifecycle, "demo2 \\\"draft\\\"");
        let parsed = updated.parse::<toml::Value>().expect("parse lifecycle");

        assert_eq!(
            parsed.get("name").and_then(toml::Value::as_str),
            Some("demo2 \\\"draft\\\"")
        );
        assert!(updated.contains("# generated default"));
        assert!(updated.contains("name = \"Frontend\""));
        assert!(updated.contains("http://127.0.0.1:5174"));
    }

    #[test]
    fn ipv6_listener_marks_port_as_occupied() {
        let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = listener.local_addr().expect("read IPv6 port").port();

        assert!(!port_is_available(port));
    }

    #[test]
    fn lifecycle_v1_enriches_instance_details() {
        let root = env::temp_dir().join(format!("agentseek-desktop-details-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\nname = \"My Bub Agent\"\n[env.BUB_MODEL]\nrequired = true\n[env.BUB_API_KEY]\nrequired = true\n[services.app]\nurl = \"http://127.0.0.1:5173\"\n[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n",
        )
        .expect("write lifecycle manifest");
        let env_path = root.join(".env");
        fs::write(
            &env_path,
            "BUB_MODEL=openai:gpt-4o-mini\nBUB_API_KEY=secret-value\nBUB_AG_UI_PORT=55550\nFRONTEND_PORT=55551\nCOPILOTKIT_PORT=57278\n",
        )
        .expect("write env");
        let mut instance = InstanceRecord {
            id: "bub-default".to_string(),
            name: "bub_default".to_string(),
            template_id: "bub/default".to_string(),
            status: "running".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: Some(root.join(".env.example").to_string_lossy().to_string()),
            env_path: Some(env_path.to_string_lossy().to_string()),
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };

        enrich_service_endpoints(&mut instance);

        assert_eq!(instance.project_name.as_deref(), Some("My Bub Agent"));
        assert_eq!(instance.lifecycle_version, Some(1));
        assert_eq!(instance.service_endpoints.len(), 3);
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.primary && endpoint.kind == "web"));
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.kind == "protocol"));
        assert_eq!(instance.ui_url.as_deref(), Some("http://127.0.0.1:55551"));
        assert_eq!(
            instance.agent_url.as_deref(),
            Some("http://127.0.0.1:55550/agent")
        );
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.url == "http://127.0.0.1:57278/api/copilotkit"));

        instance.project_name = Some("lag-development".to_string());
        enrich_service_endpoints(&mut instance);
        assert_eq!(instance.project_name.as_deref(), Some("lag-development"));
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn unfinished_instances_are_not_marked_for_restart() {
        let mut store = AppStore {
            instances: vec![InstanceRecord {
                id: "pending-instance".to_string(),
                name: "Pending".to_string(),
                template_id: "deepagents/research".to_string(),
                status: "needs-restart".to_string(),
                deployment_mode: "local".to_string(),
                work_dir: "/tmp/pending-instance".to_string(),
                env_example_path: Some("/tmp/pending-instance/.env.example".to_string()),
                env_path: Some("/tmp/pending-instance/.env".to_string()),
                note: String::new(),
                created_at: 1,
                updated_at: 1,
                needs_doctor: true,
                pid: None,
                agent_url: None,
                ui_url: None,
                studio_url: None,
                project_name: None,
                lifecycle_version: None,
                service_endpoints: Vec::new(),
            }],
            vault: Vec::new(),
            logs: Vec::new(),
        };

        assert!(repair_predeployment_restart_statuses(&mut store));
        assert_eq!(store.instances[0].status, "ready-to-install");
        assert!(!store.instances[0].needs_doctor);
    }

    #[test]
    fn process_command_port_inserted_from_lifecycle_url_when_no_env_port() {
        // cli-remote: .env has no LANGGRAPH_PORT; port extracted from lifecycle.toml [services.langgraph] URL
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:54584\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_URL=http://127.0.0.1:54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain port from URL, got:\n{updated}");
    }

    #[test]
    fn process_command_without_port_gets_inserted() {
        // cli-remote: lifecycle.toml has [processes.langgraph] but command has no --port
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\nLANGGRAPH_URL=http://127.0.0.1:54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain new port, got:\n{updated}");
    }

    #[test]
    fn process_command_without_port_inserted_preserved_by_synchronize_lifecycle() {
        // Simulate full path of synchronize_instance_port_configs:
        // synchronize_lifecycle_content + sync_process_command_ports
        let lifecycle = "version = 1\r\n\
[services.langgraph]\r\nurl = \"http://127.0.0.1:2024\"\r\n\
[processes.langgraph]\r\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\r\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\n");
        let updated = synchronize_lifecycle_content(lifecycle, &entries);
        let updated = sync_process_command_ports(&updated, &entries);
        assert!(updated != lifecycle, "should differ from original");
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain new port, got:\n{updated}");
    }

    #[test]
    fn command_tokens_parses_array_and_string_forms() {
        let arr = command_tokens("command = [\"npm\", \"run\", \"dev\"]");
        assert_eq!(
            arr,
            vec![
                "npm".to_string(),
                "run".to_string(),
                "dev".to_string()
            ]
        );
        let s = command_tokens("command = \"npm run dev\"");
        assert_eq!(
            s,
            vec![
                "npm".to_string(),
                "run".to_string(),
                "dev".to_string()
            ]
        );
        let empty = command_tokens("command = []");
        assert!(empty.is_empty());
    }

    #[test]
    fn remove_command_port_strips_port_from_array_and_string_forms() {
        let arr = "command = [\"npm\", \"install\", \"--port\", \"61986\"]";
        assert_eq!(
            remove_command_port(arr).as_deref(),
            Some("command = [\"npm\", \"install\"]")
        );
        let s = "command = \"npm install --port 61986\"";
        assert_eq!(
            remove_command_port(s).as_deref(),
            Some("command = \"npm install\"")
        );
        // Returns None when no --port
        assert!(remove_command_port("command = [\"npm\", \"install\"]").is_none());
    }

    #[test]
    fn sync_process_command_ports_skips_install_commands() {
        // npm install should not inject --port even if corresponding *_PORT exists
        let lifecycle = "version = 1\n\
[services.app]\nurl = \"http://127.0.0.1:61986\"\n\
[processes.app]\ncommand = [\"npm\", \"install\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "install command must not get --port, got:\n{updated}"
        );
        assert!(
            updated.contains("\"npm\", \"install\"]"),
            "install command should stay clean, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_cleans_injected_port_from_install_commands() {
        // Install commands erroneously injected with --port by old logic should be cleaned up
        let lifecycle = "version = 1\n\
[processes.app]\ncommand = [\"npm\", \"install\", \"--port\", \"61986\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "stale --port must be removed, got:\n{updated}"
        );
        assert!(
            updated.contains("\"npm\", \"install\"]"),
            "install command should be restored, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_skips_npm_run_dev() {
        // npm run dev does not accept --port (npm intercepts --port and reports Unknown cli config),
        // Under whitelist approach, --port is not injected.
        let lifecycle = "version = 1\n\
[services.app]\nurl = \"http://127.0.0.1:61986\"\n\
[processes.app]\ncommand = [\"npm\", \"run\", \"dev\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "npm run dev must not get --port, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_cleans_port_from_npm_run_dev() {
        // npm run dev erroneously injected with --port by old logic should be cleaned up
        let lifecycle = "version = 1\n\
[processes.app]\ncommand = [\"npm\", \"run\", \"dev\", \"--port\", \"61986\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "stale --port on npm run dev must be removed, got:\n{updated}"
        );
        assert!(
            updated.contains("\"npm\", \"run\", \"dev\"]"),
            "npm run dev should be restored, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_skips_docker_compose() {
        // docker compose up does not accept --port (reports unknown flag: --port)
        let lifecycle = "version = 1\n\
[services.seekdb]\nurl = \"http://127.0.0.1:2881\"\n\
[processes.seekdb]\ncommand = [\"docker\", \"compose\", \"up\", \"seekdb\"]\n";
        let entries = parse_env("SEEKDB_PORT=2881\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "docker compose must not get --port, got:\n{updated}"
        );
    }

    #[test]
    fn sync_docker_compose_ports_replaces_host_port() {
        // Original port 2881 should be replaced with ${SEEKDB_PORT:-2881}:2881 variable reference
        let compose = "name: my_rag_agent\nservices:\n  seekdb:\n    image: quay.io/oceanbase/seekdb:latest\n    ports:\n      - \"127.0.0.1:2881:2881\"\n    volumes:\n      - ./.seekdb-data:/var/lib/oceanbase\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(
            updated.contains("${SEEKDB_PORT:-2881}:2881"),
            "should use variable reference, got:\n{updated}"
        );
        assert!(
            !updated.contains("127.0.0.1:2881:2881"),
            "old hardcoded mapping should be gone, got:\n{updated}"
        );
        // Volume mappings unchanged
        assert!(updated.contains("./.seekdb-data:/var/lib/oceanbase"));
    }

    #[test]
    fn sync_docker_compose_ports_no_change_when_no_env_port() {
        // docker-compose.yml should not be modified when .env has no *_PORT
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n";
        let entries = parse_env("BACKEND_PORT=2024\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert_eq!(updated, compose);
    }

    #[test]
    fn sync_docker_compose_ports_handles_plain_mapping() {
        // Port mapping "2881:2881" without IP prefix should also be replaced with variable reference
        let compose = "services:\n  seekdb:\n    ports:\n      - \"2881:2881\"\n";
        let entries = parse_env("SEEKDB_PORT=2900\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(
            updated.contains("${SEEKDB_PORT:-2881}:2881"),
            "should use variable reference, got:\n{updated}"
        );
    }

    #[test]
    fn sync_docker_compose_ports_skips_unrelated_services() {
        // Services without corresponding *_PORT env variable should not be modified
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n  redis:\n    ports:\n      - \"127.0.0.1:6379:6379\"\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(updated.contains("${SEEKDB_PORT:-2881}:2881"));
        assert!(updated.contains("127.0.0.1:6379:6379"));
    }

    #[test]
    fn sync_docker_compose_ports_skips_volume_mappings() {
        // Volume mappings should not be misidentified as port mappings
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n    volumes:\n      - ./.seekdb-data:/var/lib/oceanbase\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(updated.contains("${SEEKDB_PORT:-2881}:2881"));
        assert!(updated.contains("./.seekdb-data:/var/lib/oceanbase"));
    }

    #[test]
    fn sync_docker_compose_ports_idempotent() {
        // Lines already using ${...} syntax should not be modified again (idempotent)
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:${SEEKDB_PORT:-2881}:2881\"\n";
        let entries = parse_env("SEEKDB_PORT=2900\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert_eq!(updated, compose);
    }

    #[test]
    fn sync_process_command_ports_skips_shell_wrapped_commands() {
        // sh -lc wrapped commands: --port would be passed to sh instead of inner command; should not inject
        let lifecycle = "version = 1\n\
[services.backend]\nurl = \"http://127.0.0.1:63928\"\n\
[processes.backend]\ncommand = [\"sh\", \"-lc\", \"uv run langgraph dev --no-browser\"]\n";
        let entries = parse_env("BACKEND_PORT=63928\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "sh -lc wrapped command must not get --port, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_no_leak_into_tasks() {
        // [tasks.*] command should not be injected with process --port (Bug 1)
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:61889\"\n\
[services.frontend]\nurl = \"http://127.0.0.1:61884\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\", \"--port\", \"2024\", \"--no-browser\"]\n\
[processes.frontend]\ncommand = [\"npm\", \"run\", \"dev\"]\n\
[tasks.backend]\ncommand = [\"uv\", \"sync\"]\n\
[tasks.frontend]\ncommand = [\"npm\", \"install\", \"--prefix\", \"frontend\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=61889\nFRONTEND_PORT=61884\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        // langgraph command should have port replaced to 61889
        assert!(
            updated.contains("\"61889\""),
            "langgraph should carry resolved port, got:\n{updated}"
        );
        // tasks should NOT have --port leaked from processes
        assert!(
            !updated.contains("sync\", \"--port\""),
            "uv sync in tasks must not get --port, got:\n{updated}"
        );
        assert!(
            !updated.contains("frontend\", \"--port\""),
            "npm install in tasks must not get --port, got:\n{updated}"
        );
    }

    #[test]
    fn accepts_port_flag_whitelist() {
        // langgraph / vite / uvicorn accept --port
        assert!(accepts_port_flag(&[
            "uv".to_string(),
            "run".to_string(),
            "langgraph".to_string(),
            "dev".to_string()
        ]));
        assert!(accepts_port_flag(&["langgraph".to_string(), "dev".to_string()]));
        assert!(accepts_port_flag(&["vite".to_string()]));
        assert!(accepts_port_flag(&[
            "uvicorn".to_string(),
            "main:app".to_string()
        ]));
        // Others do not accept
        assert!(!accepts_port_flag(&[
            "npm".to_string(),
            "run".to_string(),
            "dev".to_string()
        ]));
        assert!(!accepts_port_flag(&[
            "docker".to_string(),
            "compose".to_string(),
            "up".to_string()
        ]));
        assert!(!accepts_port_flag(&["uv".to_string(), "sync".to_string()]));
        assert!(!accepts_port_flag(&[
            "sh".to_string(),
            "-lc".to_string(),
            "uv run langgraph dev".to_string()
        ]));
        assert!(!accepts_port_flag(&["npm".to_string(), "install".to_string()]));
    }

    #[test]
    fn sync_process_command_ports_still_injects_port_for_non_npm_commands() {
        // Ensure non-npm commands (langgraph dev) still get --port injected normally
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("--port"),
            "langgraph command should get --port, got:\n{updated}"
        );
        assert!(
            updated.contains("\"54584\""),
            "langgraph command should carry the port, got:\n{updated}"
        );
    }

    #[test]
    fn resolve_lifecycle_ports_respects_user_configured_port() {
        let root =
            env::temp_dir().join(format!("agentseek-desktop-port-user-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\n[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n",
        )
        .expect("write lifecycle");
        let instance = InstanceRecord {
            id: "port-test".to_string(),
            name: "port_test".to_string(),
            template_id: "langchain/test".to_string(),
            status: "installing".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: None,
            env_path: None,
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        let user_port = available_ephemeral_port().expect("allocate user port");
        let entries = parse_env(&format!("LANGGRAPH_PORT={user_port}\n"));
        let reserved = std::collections::HashSet::new();
        let (_updated, changes, port_map) =
            resolve_lifecycle_ports(&instance, &reserved, &entries).expect("resolve lifecycle ports");
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == "LANGGRAPH_PORT")
            .map(|(_, p)| *p);
        assert_eq!(
            resolved,
            Some(user_port),
            "user-configured available port must be respected"
        );
        assert!(
            changes.iter().all(|c| c.key != "LANGGRAPH_PORT"),
            "no change expected when user port is available"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn resolve_lifecycle_ports_falls_back_to_default_when_env_absent() {
        let root =
            env::temp_dir().join(format!("agentseek-desktop-port-default-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        let default_port = available_ephemeral_port().expect("allocate default port");
        fs::write(
            metadata.join("lifecycle.toml"),
            format!(
                "version = 1\n[services.langgraph]\nurl = \"http://127.0.0.1:{default_port}\"\n"
            ),
        )
        .expect("write lifecycle");
        let instance = InstanceRecord {
            id: "port-test".to_string(),
            name: "port_test".to_string(),
            template_id: "langchain/test".to_string(),
            status: "installing".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: None,
            env_path: None,
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        let entries = parse_env("");
        let reserved = std::collections::HashSet::new();
        let (_updated, _changes, port_map) =
            resolve_lifecycle_ports(&instance, &reserved, &entries).expect("resolve lifecycle ports");
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == "LANGGRAPH_PORT")
            .map(|(_, p)| *p);
        assert_eq!(
            resolved,
            Some(default_port),
            "should fall back to lifecycle default when env has no port"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn terminate_processes_stops_parent_and_children() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn process tree");
        let pid = child.id();
        let waiter = std::thread::spawn(move || child.wait().expect("wait for process tree"));

        let stopped = super::terminate_processes([pid]).expect("terminate process tree");

        assert!(stopped.iter().any(|process| process.pid == pid));
        assert!(stopped.iter().all(|process| !process.executable.is_empty()));
        waiter.join().expect("join process waiter");
        assert!(!super::process_exists(pid));
    }
}
