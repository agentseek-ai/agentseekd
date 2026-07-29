// Desktop application state: data loading, persistence, log management,
// and pre-deployment status repair helpers.

fn is_deployment_completed_log(log: &LogEntry) -> bool {
    log.category == "install" && log.level == "success" && log.message == "Instance deployment completed"
}

/// Reset a stale "needs-restart" instance that was never actually deployed.
/// Returns true if the instance was modified.
fn repair_instance_restart_status(instance: &mut InstanceRecord, deployed: bool) -> bool {
    if instance.status == "needs-restart" && !deployed {
        instance.status = if instance.env_path.is_some() {
            "ready-to-install".to_string()
        } else {
            "configuring".to_string()
        };
        instance.needs_doctor = false;
        instance.updated_at = timestamp();
        return true;
    }
    false
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
        let is_deployed = instance.pid.is_some() || deployed.contains(&instance.id);
        changed |= repair_instance_restart_status(instance, is_deployed);
    }
    changed
}

fn is_desktop_lifecycle_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower == "instance stopped"
        || lower.starts_with("stopped instance process tree")
        || lower.starts_with("instance associated processes stopped")
        || lower.starts_with("instance deletion completed")
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
                changed |= repair_instance_restart_status(instance, deployed);
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
