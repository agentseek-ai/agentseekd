use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
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

mod models;
pub(crate) use models::*;

include!("storage.rs");
include!("state.rs");
include!("logging.rs");
include!("cli.rs");
include!("env.rs");
include!("ports.rs");
include!("lifecycle.rs");
include!("instance.rs");
include!("runtime_install.rs");
include!("commands.rs");

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let runtime_dir = data_dir.join("runtime");
            let requirements =
                load_runtime_requirements(DEFAULT_RUNTIME_REQUIREMENTS).map_err(std::io::Error::other)?;
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
        dependency_commands, enrich_service_endpoints, find_file_recursive,
        instance_target_path,
        is_local_service_port_key, is_secret_env_key, list_env_files,
        meets_requirement, merge_env_entries,
        normalize_storage_database, numeric_version, parse_agentseek_package_version,
        parse_agentseek_version, parse_env, parse_templates, parse_uv_tool_version,
        patch_agent_async_if_needed, patch_convert_models_if_needed,
        patch_dockerfile_apt_mirror_if_needed, patch_dockerfile_mirrors_if_needed,
        patch_langgraph_cors_if_needed,
        port_is_available, posix_runtime_install_script, prune_logs, read_local_credentials,
        read_runtime_log_records, remove_command_port, remove_instance_work_dir, render_env,
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
    fn sync_process_command_ports_injects_port_into_npm_run_dev() {
        // npm run dev — inject "--", "--port", "<port>" so npm passes
        // --port through to the underlying vite/uvicorn process.
        let lifecycle = "version = 1\n\
[services.app]\nurl = \"http://127.0.0.1:61986\"\n\
[processes.app]\ncommand = [\"npm\", \"run\", \"dev\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("\"--\", \"--port\", \"61986\""),
            "npm run dev should get -- --port 61986, got:\n{updated}"
        );
    }

    #[test]
    fn sync_process_command_ports_migrates_bare_port_to_npm_separator() {
        // npm run dev with old-style --port (no -- separator) should be
        // migrated to the correct "--", "--port", "<port>" format.
        let lifecycle = "version = 1\n\
[processes.frontend]\ncommand = [\"npm\", \"run\", \"dev\", \"--port\", \"61986\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("\"--\", \"--port\", \"61986\""),
            "should have -- separator, got:\n{updated}"
        );
        assert!(
            !updated.contains("\"dev\", \"--port\""),
            "old-style bare --port should be gone, got:\n{updated}"
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
    fn sync_process_command_ports_injects_into_shell_wrapped_commands() {
        // sh -lc wrapped commands: --port must be injected INTO the inner
        // command string, not as a separate array element passed to sh.
        let lifecycle = "version = 1\n\
[services.backend]\nurl = \"http://127.0.0.1:63928\"\n\
[processes.backend]\ncommand = [\"sh\", \"-lc\", \"uv run langgraph dev --no-browser\"]\n";
        let entries = parse_env("BACKEND_PORT=63928\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        // --port should appear INSIDE the shell command string
        assert!(
            updated.contains("langgraph dev --no-browser --port 63928"),
            "--port must be injected into the inner shell command string, got:\n{updated}"
        );
        // --port should NOT be a separate array element
        assert!(
            !updated.contains("\", \"--port\", \"63928\""),
            "--port must not be a separate TOML array element for sh -lc commands, got:\n{updated}"
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
        // npm run <script> accepts --port via -- separator
        assert!(accepts_port_flag(&[
            "npm".to_string(),
            "run".to_string(),
            "dev".to_string()
        ]));
        // Others do not accept
        assert!(!accepts_port_flag(&[
            "docker".to_string(),
            "compose".to_string(),
            "up".to_string()
        ]));
        assert!(!accepts_port_flag(&["uv".to_string(), "sync".to_string()]));
        assert!(!accepts_port_flag(&["npm".to_string(), "install".to_string()]));
        // sh -lc wrapped commands containing langgraph/uvicorn SHOULD accept --port
        assert!(accepts_port_flag(&[
            "sh".to_string(),
            "-lc".to_string(),
            "uv run langgraph dev".to_string()
        ]));
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

    // -----------------------------------------------------------------
    // Boundary tests: env parsing
    // -----------------------------------------------------------------

    #[test]
    fn parse_env_empty_input_returns_empty_vec() {
        let entries = parse_env("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_env_whitespace_only_lines_returns_empty_vec() {
        let entries = parse_env("   \n\t\n\n  \t  \n");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_env_value_with_hash_is_preserved() {
        let entries = parse_env("KEY=value#with#hash");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "KEY");
        // Hash inside a value should be preserved (not treated as comment)
        assert_eq!(entries[0].value, "value#with#hash");
    }

    #[test]
    fn parse_env_duplicate_keys_keep_last() {
        let entries = parse_env("KEY=val1\nKEY=val2\nKEY=val3");
        // parse_env does not deduplicate; all entries are returned
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].value, "val3");
    }

    #[test]
    fn parse_env_very_long_value_is_preserved() {
        let long_value = "x".repeat(10_000);
        let input = format!("KEY={long_value}");
        let entries = parse_env(&input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, long_value);
    }

    #[test]
    fn render_env_round_trips_basic_entries() {
        let original = "KEY1=value1\nKEY2=value2\n";
        let entries = parse_env(original);
        let rendered = render_env(&entries);
        assert!(rendered.contains("KEY1=value1"));
        assert!(rendered.contains("KEY2=value2"));
    }

    // -----------------------------------------------------------------
    // Boundary tests: ports
    // -----------------------------------------------------------------

    #[test]
    fn port_is_available_for_port_zero() {
        // Port 0 is the wildcard; binding to it always succeeds.
        assert!(port_is_available(0));
    }

    #[test]
    fn port_is_available_for_high_port() {
        // Port 65535 should be available unless something is listening.
        // This is a best-effort check; if it fails, something is using the port.
        let _ = port_is_available(65535);
    }

    #[test]
    fn port_is_unavailable_when_already_bound() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("bind listener");
        let port = listener.local_addr().expect("read port").port();
        assert!(!port_is_available(port));
    }

    // -----------------------------------------------------------------
    // Boundary tests: lifecycle
    // -----------------------------------------------------------------

    #[test]
    fn synchronize_lifecycle_content_empty_input() {
        let entries = parse_env("");
        let result = synchronize_lifecycle_content("", &entries);
        assert_eq!(result, "");
    }

    #[test]
    fn synchronize_lifecycle_content_missing_services_section() {
        let content = "version = 1\nname = \"test\"\n";
        let entries = parse_env("");
        let result = synchronize_lifecycle_content(content, &entries);
        // Content without services should be returned unchanged (or with minimal changes)
        assert!(result.contains("version = 1"));
        assert!(result.contains("name = \"test\""));
    }

    #[test]
    fn lifecycle_manifest_empty_toml() {
        let manifest: LifecycleManifest = toml::from_str("").expect("parse empty toml");
        assert_eq!(manifest.version, 0);
        assert!(manifest.services.is_empty());
    }

    #[test]
    fn lifecycle_manifest_missing_services_section() {
        let manifest: LifecycleManifest =
            toml::from_str("version = 1\nname = \"test\"\n").expect("parse toml");
        assert_eq!(manifest.version, 1);
        assert!(manifest.services.is_empty());
    }

    // -----------------------------------------------------------------
    // Boundary tests: cli version comparison
    // -----------------------------------------------------------------

    #[test]
    fn numeric_version_empty_string_returns_empty_vec() {
        assert!(numeric_version("").is_empty());
    }

    #[test]
    fn numeric_version_non_numeric_returns_empty_vec() {
        assert!(numeric_version("abc").is_empty());
        assert!(numeric_version("no version here").is_empty());
    }

    #[test]
    fn numeric_version_pre_release_suffix_stops_at_suffix() {
        let v = numeric_version("1.2.3-alpha");
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn version_at_least_empty_version_returns_false() {
        assert!(!version_at_least("", &[1, 0]));
    }

    #[test]
    fn meets_requirement_empty_version_returns_false() {
        assert!(!meets_requirement("", "1.0.0"));
    }

    #[test]
    fn meets_requirement_non_numeric_returns_false() {
        assert!(!meets_requirement("abc", "1.0.0"));
    }

    #[test]
    fn meets_requirement_pre_release_version() {
        // "1.2.3-alpha" should parse to [1, 2, 3] and meet "1.2.0"
        assert!(meets_requirement("1.2.3-alpha", "1.2.0"));
        // But should NOT meet "1.3.0"
        assert!(!meets_requirement("1.2.3-alpha", "1.3.0"));
    }

    // -----------------------------------------------------------------
    // Boundary tests: storage
    // -----------------------------------------------------------------

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

    // ------------------------------------------------------------------
    // Tests for instance patch functions (instance.rs)
    // ------------------------------------------------------------------

    /// Helper: create a temp directory with a unique name.
    fn patch_test_dir(label: &str) -> std::path::PathBuf {
        let dir = env::temp_dir().join(format!("agentseek-patch-{label}-{}", unique_stamp()));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    // -- find_file_recursive -------------------------------------------

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

    // -- patch_dockerfile_apt_mirror_if_needed -------------------------

    #[test]
    fn apt_mirror_patch_adds_timeout_and_fallback() {
        let dir = patch_test_dir("apt-normal");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "RUN apt-get update && apt-get install -y git\n").expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let patched = fs::read_to_string(&dockerfile).expect("read");
        assert!(patched.contains("timeout 60"));
        assert!(patched.contains("mirrors.aliyun.com"));
        assert!(!patched.contains("RUN apt-get update &&")); // original replaced
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apt_mirror_patch_is_idempotent() {
        let dir = patch_test_dir("apt-idem");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "RUN apt-get update && apt-get install -y git\n").expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let after_first = fs::read_to_string(&dockerfile).expect("read");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let after_second = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apt_mirror_patch_skips_when_no_apt_get_update() {
        let dir = patch_test_dir("apt-none");
        let dockerfile = dir.join("Dockerfile");
        let original = "FROM python:3.12\nCMD [\"bash\"]\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apt_mirror_patch_skips_when_no_dockerfile() {
        let dir = patch_test_dir("apt-nofile");
        // Should not panic.
        patch_dockerfile_apt_mirror_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }

    // -- patch_dockerfile_mirrors_if_needed ----------------------------

    /// Build a minimal Dockerfile matching the template structure.
    fn sample_dockerfile_with_uv() -> String {
        [
            "FROM python:3.12-slim AS base",
            "WORKDIR /app",
            "RUN apt-get update && apt-get install -y git",
            "COPY pyproject.toml .",
            "RUN set -e; \\",
            "    if [ -n \"${UV_DEFAULT_INDEX:-}\" ]; then export UV_DEFAULT_INDEX; fi; \\",
            "    if [ -n \"${UV_INDEX_URL:-}\" ]; then export UV_INDEX_URL; fi; \\",
            "    if [ -n \"${UV_INSECURE_HOST:-}\" ]; then export UV_INSECURE_HOST; fi; \\",
            "    UV_LINK_MODE=copy uv sync --no-dev --no-cache",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn mirrors_patch_adds_github_and_pypi_fallback() {
        let dir = patch_test_dir("mirrors-normal");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, sample_dockerfile_with_uv()).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        let patched = fs::read_to_string(&dockerfile).expect("read");
        assert!(patched.contains("ghfast.top"));
        assert!(patched.contains("mirrors.aliyun.com/pypi/simple/"));
        assert!(patched.contains("pyproject.toml"));
        // Original fi; lines should be replaced with elif chain.
        assert!(patched.contains("elif [ -n \"${UV_INDEX_URL"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mirrors_patch_is_idempotent() {
        let dir = patch_test_dir("mirrors-idem");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, sample_dockerfile_with_uv()).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        let after_first = fs::read_to_string(&dockerfile).expect("read");
        patch_dockerfile_mirrors_if_needed(&dir);
        let after_second = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mirrors_patch_skips_when_no_uv_sync() {
        let dir = patch_test_dir("mirrors-nouv");
        let dockerfile = dir.join("Dockerfile");
        let original = "FROM python:3.12\nRUN pip install -r requirements.txt\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mirrors_patch_skips_when_no_default_index_marker() {
        let dir = patch_test_dir("mirrors-nomarker");
        let dockerfile = dir.join("Dockerfile");
        // Has uv sync but no UV_DEFAULT_INDEX block.
        let original = "FROM python:3.12\nRUN uv sync --no-dev\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    // -- patch_langgraph_cors_if_needed --------------------------------

    #[test]
    fn cors_patch_replaces_port_specific_origins() {
        let dir = patch_test_dir("cors-normal");
        let path = dir.join("langgraph.json");
        let original = serde_json::json!({
            "graphs": { "agent": "./src/agent.py:graph" },
            "http": {
                "port": 8089,
                "cors": {
                    "allow_origins": ["http://127.0.0.1:5175"],
                    "allow_origin_regex": "^https?://.*:5175$"
                }
            }
        });
        fs::write(&path, original.to_string()).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        let patched: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        let cors = patched["http"]["cors"].as_object().expect("cors object");
        assert_eq!(cors["allow_origin_regex"].as_str(), Some("^https?://.*$"));
        assert!(cors.get("allow_origins").is_none());
        assert_eq!(cors["allow_methods"], serde_json::json!(["*"]));
        assert_eq!(cors["allow_headers"], serde_json::json!(["*"]));
        // Non-CORS fields should be preserved.
        assert_eq!(patched["http"]["port"], 8089);
        assert_eq!(patched["graphs"]["agent"], "./src/agent.py:graph");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cors_patch_adds_trailing_newline() {
        let dir = patch_test_dir("cors-newline");
        let path = dir.join("langgraph.json");
        fs::write(&path, r#"{"http":{"cors":{"allow_origins":["http://localhost:5175"]}}}"#)
            .expect("write");
        patch_langgraph_cors_if_needed(&dir);
        let content = fs::read_to_string(&path).expect("read");
        assert!(content.ends_with('\n'), "patched file must end with a newline");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cors_patch_skips_when_no_cors_section() {
        let dir = patch_test_dir("cors-nocors");
        let path = dir.join("langgraph.json");
        let original = r#"{"graphs":{"agent":"./src/agent.py:graph"},"http":{"port":8089}}"#;
        fs::write(&path, original).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cors_patch_skips_when_no_langgraph_json() {
        let dir = patch_test_dir("cors-nofile");
        // Should not panic.
        patch_langgraph_cors_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cors_patch_skips_invalid_json() {
        let dir = patch_test_dir("cors-invalid");
        let path = dir.join("langgraph.json");
        let original = "{not valid json}";
        fs::write(&path, original).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    // -- patch_convert_models_if_needed --------------------------------

    #[test]
    fn convert_models_patch_replaces_optimum_command() {
        let dir = patch_test_dir("convert-normal");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        fs::write(&path, "cmd = [sys.executable, \"-m\", \"optimum.exporters.openvino\", \"--model\", model_name]\n")
            .expect("write");
        patch_convert_models_if_needed(&dir);
        let patched = fs::read_to_string(&path).expect("read");
        assert!(patched.contains("\"optimum-cli\", \"export\", \"openvino\""));
        assert!(!patched.contains("\"-m\", \"optimum.exporters.openvino\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn convert_models_patch_skips_when_already_patched() {
        let dir = patch_test_dir("convert-idem");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        let original = "cmd = [\"optimum-cli\", \"export\", \"openvino\", \"--model\", model_name]\n";
        fs::write(&path, original).expect("write");
        patch_convert_models_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn convert_models_patch_skips_when_no_marker() {
        let dir = patch_test_dir("convert-nomarker");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        let original = "print('hello')\n";
        fs::write(&path, original).expect("write");
        patch_convert_models_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    // -- patch_agent_async_if_needed -----------------------------------

    #[test]
    fn agent_async_patch_adds_shim() {
        let dir = patch_test_dir("agent-normal");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        fs::write(&path, "from langchain_oceanbase.vectorstores import OceanbaseVectorStore\nprint('hi')\n")
            .expect("write");
        patch_agent_async_if_needed(&dir);
        let patched = fs::read_to_string(&path).expect("read");
        assert!(patched.contains("_patched_agenerate"));
        assert!(patched.contains("_patched_astream"));
        assert!(patched.contains("asyncio.to_thread"));
        // Original marker should still be present.
        assert!(patched.contains("from langchain_oceanbase.vectorstores import OceanbaseVectorStore"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agent_async_patch_is_idempotent() {
        let dir = patch_test_dir("agent-idem");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        fs::write(&path, "from langchain_oceanbase.vectorstores import OceanbaseVectorStore\nprint('hi')\n")
            .expect("write");
        patch_agent_async_if_needed(&dir);
        let after_first = fs::read_to_string(&path).expect("read");
        patch_agent_async_if_needed(&dir);
        let after_second = fs::read_to_string(&path).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agent_async_patch_skips_when_no_marker() {
        let dir = patch_test_dir("agent-nomarker");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        let original = "from langchain_core.prompts import ChatPromptTemplate\nprint('hi')\n";
        fs::write(&path, original).expect("write");
        patch_agent_async_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agent_async_patch_skips_when_no_agent_py() {
        let dir = patch_test_dir("agent-nofile");
        // Should not panic.
        patch_agent_async_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------------
    // Template-based integration tests
    //
    // These tests read the actual cookiecutter template files from
    // ~/.cookiecutters/agentseek/templates/, render them with example values,
    // and verify that every patch function behaves correctly on every template.
    // ------------------------------------------------------------------

    /// Root directory of the cookiecutter templates.
    fn templates_root() -> std::path::PathBuf {
        std::path::PathBuf::from(
            env::var("HOME").expect("HOME env var"),
        )
        .join(".cookiecutters/agentseek/templates")
    }

    /// Render a cookiecutter template file by replacing Jinja2 variables with
    /// example values and resolving `{% if %}/{% else %}/{% endif %}` blocks
    /// (keeping the else branch, which is the normal deployment path).
    fn render_template(raw: &str) -> String {
        // Resolve {% if %} / {% else %} / {% endif %} blocks by keeping the
        // else branch (source path is empty for normal deployments).
        let mut result = String::with_capacity(raw.len());
        let mut skip_lines = false; // inside an if-branch (before else)
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("{% if ") {
                skip_lines = true;
                continue;
            }
            if trimmed.starts_with("{% else %}") {
                skip_lines = false;
                continue;
            }
            if trimmed.starts_with("{% endif %}") {
                skip_lines = false;
                continue;
            }
            if trimmed.starts_with("{%") {
                // Skip other Jinja2 tags (e.g. {% for %}).
                continue;
            }
            if skip_lines {
                continue; // skip the if-branch
            }
            // We're outside any if/else block or in the else branch.
            // Replace Jinja2 variables with example values.
            let rendered = trimmed
                .replace("{{ cookiecutter.project_slug }}", "my_agent")
                .replace("{{ cookiecutter.frontend_port }}", "5175")
                .replace("{{ cookiecutter.gateway_port }}", "8089")
                .replace("{{ cookiecutter.llm_model_id }}", "Qwen/Qwen2.5-7B-Instruct")
                .replace("{{ cookiecutter.embedding_model_id }}", "BAAI/bge-small-zh-v1.5")
                .replace("{{ cookiecutter.llm_model_path }}", "./models/llm")
                .replace("{{ cookiecutter.embedding_model_path }}", "./models/embedding");
            // Preserve original indentation.
            let indent = &line[..line.len() - line.trim_start().len()];
            result.push_str(indent);
            result.push_str(&rendered);
            result.push('\n');
        }
        result
    }

    /// Copy all relevant template files for a given template id (e.g.
    /// "langchain/default") into a test directory, rendering Jinja2 variables.
    /// Binary files are copied verbatim; text files are rendered.
    fn setup_template_instance(template_id: &str, test_dir: &std::path::Path) -> bool {
        let template_path = templates_root()
            .join(template_id)
            .join("{{cookiecutter.project_slug}}");
        if !template_path.is_dir() {
            eprintln!("skipping: template not cached at {}", template_path.display());
            eprintln!("run `agentseek create {} --no-input` to cache templates", template_id);
            return false;
        }
        // Recursively copy and render template files.
        fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) {
            for entry in fs::read_dir(src).expect("read template dir") {
                let entry = entry.expect("dir entry");
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let src_path = entry.path();
                if src_path.is_dir() {
                    // Replace {{cookiecutter.project_slug}} with my_agent.
                    let dir_name = if name_str.contains("{{") {
                        "my_agent".to_string()
                    } else {
                        name_str.to_string()
                    };
                    let dst_path = dst.join(&dir_name);
                    fs::create_dir_all(&dst_path).expect("mkdir");
                    copy_recursive(&src_path, &dst_path);
                } else {
                    let dst_path = dst.join(&*name);
                    // Try to read as UTF-8 text; if that fails, copy binary.
                    match fs::read_to_string(&src_path) {
                        Ok(raw) => {
                            let rendered = render_template(&raw);
                            fs::write(&dst_path, rendered).expect("write rendered file");
                        }
                        Err(_) => {
                            // Binary file (e.g. PNG images) — copy verbatim.
                            fs::copy(&src_path, &dst_path).expect("copy binary file");
                        }
                    }
                }
            }
        }
        copy_recursive(&template_path, test_dir);
        true
    }

    /// Run all five patch functions on a directory and return the patched
    /// Dockerfile and langgraph.json content (if they exist).
    fn apply_all_patches(dir: &std::path::Path) {
        patch_convert_models_if_needed(dir);
        patch_agent_async_if_needed(dir);
        patch_dockerfile_apt_mirror_if_needed(dir);
        patch_dockerfile_mirrors_if_needed(dir);
        patch_langgraph_cors_if_needed(dir);
    }

    // -- per-template tests --------------------------------------------

    #[test]
    fn template_bub_default() {
        let dir = patch_test_dir("tpl-bub-default");
        if !setup_template_instance("bub/default", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // bub/default Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("ghfast.top"));
        }
        // No langgraph.json or convert_models.py in this template.
        assert!(find_file_recursive(&dir, "langgraph.json", 5).is_none());
        assert!(find_file_recursive(&dir, "convert_models.py", 5).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_deepagents_default() {
        let dir = patch_test_dir("tpl-da-default");
        if !setup_template_instance("deepagents/default", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // deepagents/default Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("ghfast.top"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_deepagents_research() {
        let dir = patch_test_dir("tpl-da-research");
        if !setup_template_instance("deepagents/research", &dir) { return; }
        apply_all_patches(&dir);
        // Has langgraph.json but no cors section — patch should be a no-op.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_deepagents_sandbox() {
        let dir = patch_test_dir("tpl-da-sandbox");
        if !setup_template_instance("deepagents/sandbox", &dir) { return; }
        apply_all_patches(&dir);
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_deepagents_content_builder() {
        let dir = patch_test_dir("tpl-da-cb");
        if !setup_template_instance("deepagents/content-builder", &dir) { return; }
        apply_all_patches(&dir);
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_default() {
        let dir = patch_test_dir("tpl-lc-default");
        if !setup_template_instance("langchain/default", &dir) { return; }
        apply_all_patches(&dir);
        // Dockerfile: should have apt mirror + GitHub/PyPI mirror fallbacks.
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5).expect("Dockerfile");
        let content = fs::read_to_string(&dockerfile).expect("read");
        assert!(content.contains("timeout 60"), "apt mirror patch not applied");
        assert!(content.contains("mirrors.aliyun.com"), "apt mirror fallback missing");
        assert!(content.contains("ghfast.top"), "GitHub mirror fallback missing");
        assert!(content.contains("mirrors.aliyun.com/pypi/simple/"), "PyPI mirror fallback missing");
        // Idempotency: applying again should not change anything.
        let before = content.clone();
        apply_all_patches(&dir);
        let after = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(before, after, "patches should be idempotent");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_cli_remote() {
        let dir = patch_test_dir("tpl-lc-cli");
        if !setup_template_instance("langchain/cli-remote", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // cli-remote Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("ghfast.top"));
        }
        // Has langgraph.json but no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_agentic_rag() {
        let dir = patch_test_dir("tpl-lc-rag");
        if !setup_template_instance("langchain/agentic-rag", &dir) { return; }
        apply_all_patches(&dir);
        // agent.py should have the async shim (has OceanbaseVectorStore marker).
        let agent_py = find_file_recursive(&dir, "agent.py", 5).expect("agent.py");
        let content = fs::read_to_string(&agent_py).expect("read");
        assert!(content.contains("_patched_agenerate"), "async shim not added");
        // langgraph.json has no cors section — should remain valid JSON.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_agentic_rag_hybrid() {
        let dir = patch_test_dir("tpl-lc-rag-hybrid");
        if !setup_template_instance("langchain/agentic-rag-hybrid", &dir) { return; }
        apply_all_patches(&dir);
        // langgraph.json: should have CORS patched to allow any origin.
        let langgraph = find_file_recursive(&dir, "langgraph.json", 5).expect("langgraph.json");
        let content = fs::read_to_string(&langgraph).expect("read");
        let json: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        let cors = &json["http"]["cors"];
        assert_eq!(cors["allow_origin_regex"].as_str(), Some("^https?://.*$"));
        assert!(cors.get("allow_origins").is_none());
        assert_eq!(cors["allow_methods"], serde_json::json!(["*"]));
        assert_eq!(cors["allow_headers"], serde_json::json!(["*"]));
        // agent.py has no OceanbaseVectorStore marker — patch should be no-op.
        if let Some(path) = find_file_recursive(&dir, "agent.py", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(!content.contains("_patched_agenerate"));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_agentic_rag_openvino() {
        let dir = patch_test_dir("tpl-lc-rag-openvino");
        if !setup_template_instance("langchain/agentic-rag-openvino", &dir) { return; }
        apply_all_patches(&dir);
        // convert_models.py: template already uses optimum-cli — patch should skip.
        let convert_py = find_file_recursive(&dir, "convert_models.py", 5)
            .expect("convert_models.py");
        let content = fs::read_to_string(&convert_py).expect("read");
        assert!(content.contains("optimum-cli"), "should still have optimum-cli");
        // agent.py should have the async shim.
        let agent_py = find_file_recursive(&dir, "agent.py", 5).expect("agent.py");
        let content = fs::read_to_string(&agent_py).expect("read");
        assert!(content.contains("_patched_agenerate"), "async shim not added");
        // langgraph.json has no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn template_langchain_markdown_messages() {
        let dir = patch_test_dir("tpl-lc-md");
        if !setup_template_instance("langchain/markdown-messages", &dir) { return; }
        apply_all_patches(&dir);
        // langgraph.json has no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        // agent.py has no OceanbaseVectorStore marker.
        if let Some(path) = find_file_recursive(&dir, "agent.py", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(!content.contains("_patched_agenerate"));
        }
        fs::remove_dir_all(&dir).ok();
    }
}
