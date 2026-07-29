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
