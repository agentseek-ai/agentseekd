// Storage engine: SQLite embedded, SeekDB bridge, config and credential I/O.

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

fn canonical_log_category(category: &str) -> &'static str {
    if category == "runtime" {
        "runtime"
    } else {
        "lifecycle"
    }
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

fn initialize_sqlite_schema(connection: &Connection) -> Result<(), String> {
    let user_version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS instances (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                template_id TEXT NOT NULL,
                status TEXT NOT NULL,
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
            CREATE TABLE IF NOT EXISTS app_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
                VALUES (2, strftime('%s', 'now'));
            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
                VALUES (3, strftime('%s', 'now'));
            INSERT OR IGNORE INTO schema_migrations (version, applied_at)
                VALUES (4, strftime('%s', 'now'));
            PRAGMA user_version = 4;",
        )
        .map_err(|error| error.to_string())?;
    if user_version < 4 {
        // Keep historical rows while moving old operation-specific labels into
        // the two streams used by the current log center.
        connection
            .execute(
                "UPDATE logs
                 SET category = CASE WHEN category = 'runtime' THEN 'runtime' ELSE 'lifecycle' END
                 WHERE category <> 'runtime'",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if user_version < 5 {
        let has_deployment_mode = connection
            .prepare("PRAGMA table_info(instances)")
            .and_then(|mut statement| {
                let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
                columns.collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| error.to_string())?
            .iter()
            .any(|column| column == "deployment_mode");
        if has_deployment_mode {
            connection
                .execute_batch("ALTER TABLE instances DROP COLUMN deployment_mode;")
                .map_err(|error| error.to_string())?;
        }
        connection
            .execute_batch(
                "INSERT OR IGNORE INTO schema_migrations (version, applied_at)
                 VALUES (5, strftime('%s', 'now'));
                 PRAGMA user_version = 5;",
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn replace_sqlite_store(connection: &mut Connection, data: &AppStore) -> Result<(), String> {
    // Full store replacement including logs (legacy migration path).
    replace_sqlite_tables(connection, data, true)
}

fn replace_sqlite_core(connection: &mut Connection, data: &AppStore) -> Result<(), String> {
    // Core data replacement (instances + vault); logs are appended incrementally.
    replace_sqlite_tables(connection, data, false)
}

fn replace_sqlite_tables(
    connection: &mut Connection,
    data: &AppStore,
    include_logs: bool,
) -> Result<(), String> {
    initialize_sqlite_schema(connection)?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    if include_logs {
        transaction
            .execute_batch("DELETE FROM instances; DELETE FROM env_vault; DELETE FROM logs;")
            .map_err(|error| error.to_string())?;
    } else {
        transaction
            .execute_batch("DELETE FROM instances; DELETE FROM env_vault;")
            .map_err(|error| error.to_string())?;
    }
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO instances (
                    id, name, template_id, status, work_dir,
                    env_example_path, env_path, note, created_at, updated_at,
                    needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                    lifecycle_version, service_endpoints
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18
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
    if include_logs {
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
                    canonical_log_category(&log.category),
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
                "SELECT id, name, template_id, status, work_dir,
                        env_example_path, env_path, note, created_at, updated_at,
                        needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                        lifecycle_version, service_endpoints
                 FROM instances ORDER BY created_at, id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let endpoints: String = row.get(17)?;
                Ok(InstanceRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    template_id: row.get(2)?,
                    status: row.get(3)?,
                    work_dir: row.get(4)?,
                    env_example_path: row.get(5)?,
                    env_path: row.get(6)?,
                    note: row.get(7)?,
                    created_at: row.get::<_, i64>(8)? as u64,
                    updated_at: row.get::<_, i64>(9)? as u64,
                    needs_doctor: row.get(10)?,
                    pid: row.get::<_, Option<i64>>(11)?.map(|value| value as u32),
                    agent_url: row.get(12)?,
                    ui_url: row.get(13)?,
                    studio_url: row.get(14)?,
                    project_name: row.get(15)?,
                    lifecycle_version: row.get::<_, Option<i64>>(16)?.map(|value| value as u32),
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
        let limit = if query.load_all {
            100_000
        } else {
            query.limit.clamp(1, 1_000)
        };
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
                              AND category = 'lifecycle'
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
                                canonical_log_category(&log.category),
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
                            canonical_log_category(&log.category),
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
                            id, name, template_id, status, work_dir,
                            env_example_path, env_path, note, created_at, updated_at,
                            needs_doctor, pid, agent_url, ui_url, studio_url, project_name,
                            lifecycle_version, service_endpoints
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                            ?13, ?14, ?15, ?16, ?17, ?18
                         )",
                        params![
                            instance.id,
                            instance.name,
                            instance.template_id,
                            instance.status,
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

    fn get_app_config(&mut self, key: &str) -> Result<Option<String>, String> {
        match self {
            Self::Pending => Ok(None),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .query_row(
                        "SELECT value FROM app_config WHERE key = ?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())
            }
            Self::SeekDb(bridge) => Ok(bridge
                .request(serde_json::json!({"op": "get_config", "key": key}))
                .ok()
                .and_then(|response| response.get("value")?.as_str().map(str::to_string))),
        }
    }

    fn set_app_config(&mut self, key: &str, value: &str) -> Result<(), String> {
        match self {
            Self::Pending => Ok(()),
            Self::Sqlite(path) => {
                let connection = open_sqlite(path)?;
                initialize_sqlite_schema(&connection)?;
                connection
                    .execute(
                        "INSERT OR REPLACE INTO app_config (key, value) VALUES (?1, ?2)",
                        params![key, value],
                    )
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            Self::SeekDb(bridge) => {
                bridge.request(serde_json::json!({"op": "set_config", "key": key, "value": value}))?;
                Ok(())
            }
        }
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

// ---------------------------------------------------------------------------
// Storage configuration commands
// ---------------------------------------------------------------------------

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
    let marker = runtime.join(".pyseekdb-installed");
    if !marker.is_file() {
        let uv = uv_program().ok_or_else(|| "Please install uv before configuring SeekDB".to_string())?;
        // Version resolution: versions.pyseekdb.pinned in the runtime
        // requirements manifest. When unset, install the latest PyPI release.
        let package = load_runtime_requirements(DEFAULT_RUNTIME_REQUIREMENTS)
            .map(|requirements| requirements.versions.pyseekdb.pinned)
            .ok()
            .filter(|version| !version.trim().is_empty())
            .map(|version| format!("pyseekdb=={}", version.trim()))
            .unwrap_or_else(|| "pyseekdb".to_string());
        run_dependency_command(
            &uv,
            &[
                "pip",
                "install",
                "--python",
                &python.to_string_lossy(),
                "--upgrade",
                &package,
            ],
            "Installing AgentSeek Desktop private pyseekdb",
        )?;
        fs::write(&marker, "pyseekdb").map_err(|error| error.to_string())?;
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
        // Embedded SeekDB requires a Python runtime (pyseekdb) that is not
        // supported on Windows. Reject the mode early so the frontend can
        // surface a clear error instead of failing at runtime.
        if cfg!(windows) && config.mode == "seekdb_embedded" {
            return Err("Embedded SeekDB is not supported on Windows. Use SQLite or a remote server.".to_string());
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
                            load_all: false,
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

#[cfg(test)]
mod tests_storage {
    use super::*;

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
                load_all: false,
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
                load_all: false,
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
                load_all: false,
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
                load_all: false,
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
                    "lifecycle",
                    now - 10 * SECONDS_PER_DAY,
                ),
                test_log(
                    "recent-deleted-lifecycle",
                    Some("deleted"),
                    "lifecycle",
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
                load_all: false,
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
                    load_all: false,
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
        assert_eq!(engine.log_count().expect("migrated logs are preserved"), 1);
        assert_eq!(
            engine
                .query_logs(&LogQuery {
                    before_sequence: None,
                    after_sequence: None,
                    limit: 10,
                    load_all: false,
                })
                .expect("query migrated logs")
                .entries[0]
                .id,
            "log-1"
        );
        assert_eq!(
            engine
                .query_logs(&LogQuery {
                    before_sequence: None,
                    after_sequence: None,
                    limit: 10,
                    load_all: false,
                })
                .expect("query migrated logs")
                .entries[0]
                .category,
            "lifecycle"
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
        assert_eq!(schema_version, 5);
        drop(connection);
        fs::remove_dir_all(root).expect("remove storage test directory");
    }
    #[test]
    fn read_local_credentials_missing_file_returns_default() {
        let path = env::temp_dir().join(format!(
            "agentseek-desktop-cred-missing-{}",
            unique_stamp()
        ));
        let credentials = read_local_credentials(&path).expect("read missing credentials");
        assert!(credentials.storage_password.is_empty());
    }
    #[test]
    fn read_local_credentials_corrupt_file_returns_error() {
        let path = env::temp_dir().join(format!(
            "agentseek-desktop-cred-corrupt-{}",
            unique_stamp()
        ));
        fs::write(&path, "{ this is not valid json }").expect("write corrupt file");
        let result = read_local_credentials(&path);
        assert!(result.is_err());
    }
    #[test]
    fn write_and_read_local_credentials_round_trip() {
        let path = env::temp_dir().join(format!(
            "agentseek-desktop-cred-rt-{}",
            unique_stamp()
        ));
        let credentials = LocalCredentials {
            storage_password: "secret123".to_string(),
        };
        write_local_credentials(&path, &credentials).expect("write credentials");
        let read = read_local_credentials(&path).expect("read credentials");
        assert_eq!(read.storage_password, "secret123");
        fs::remove_file(&path).ok();
    }
    #[test]
    fn write_storage_config_excludes_password() {
        let path = env::temp_dir().join(format!(
            "agentseek-desktop-config-{}",
            unique_stamp()
        ));
        let config = StorageConfig {
            mode: "sqlite_embedded".to_string(),
            path: String::new(),
            host: String::new(),
            port: 0,
            tenant: String::new(),
            database: "agentseek.db".to_string(),
            user: String::new(),
            password: "should_not_be_persisted".to_string(),
            runtime_log_retention_days: 7,
            setup_completed: false,
        };
        write_storage_config(&path, &config).expect("write config");
        let content = fs::read_to_string(&path).expect("read config file");
        assert!(content.contains("sqlite_embedded"));
        assert!(!content.contains("should_not_be_persisted"));
        fs::remove_file(&path).ok();
    }
    #[test]
    fn find_file_recursive_finds_top_level() {
        let dir = patch_test_dir("find-top");
        fs::write(dir.join("Dockerfile"), "FROM scratch").expect("write");
        let found = find_file_recursive(&dir, "Dockerfile", 5);
        assert_eq!(found, Some(dir.join("Dockerfile")));
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn find_file_recursive_finds_nested() {
        let dir = patch_test_dir("find-nested");
        let nested = dir.join("a/b/c");
        fs::create_dir_all(&nested).expect("mkdir");
        fs::write(nested.join("langgraph.json"), "{}").expect("write");
        let found = find_file_recursive(&dir, "langgraph.json", 5);
        assert_eq!(found, Some(nested.join("langgraph.json")));
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn find_file_recursive_respects_depth_limit() {
        let dir = patch_test_dir("find-depth");
        let deep = dir.join("a/b/c/d");
        fs::create_dir_all(&deep).expect("mkdir");
        fs::write(deep.join("Dockerfile"), "FROM scratch").expect("write");
        // File is 4 levels deep; max_depth=3 should not find it.
        assert_eq!(find_file_recursive(&dir, "Dockerfile", 3), None);
        // max_depth=5 should find it.
        assert!(find_file_recursive(&dir, "Dockerfile", 5).is_some());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn find_file_recursive_returns_none_when_missing() {
        let dir = patch_test_dir("find-missing");
        assert_eq!(find_file_recursive(&dir, "nonexistent.txt", 5), None);
        fs::remove_dir_all(&dir).ok();
    }
}
