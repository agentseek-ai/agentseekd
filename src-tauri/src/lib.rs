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

/// Loopback URL prefixes treated as local service URLs (host, wildcard, IPv6).
const LOOPBACK_URL_PREFIXES: [&str; 8] = [
    "http://127.0.0.1",
    "https://127.0.0.1",
    "http://localhost",
    "https://localhost",
    "http://0.0.0.0",
    "https://0.0.0.0",
    "http://[::1]",
    "https://[::1]",
];

/// Common 127.0.0.1 / localhost prefixes used for endpoint alignment.
const LOCALHOST_URL_PREFIXES: [&str; 4] = [
    LOOPBACK_URL_PREFIXES[0],
    LOOPBACK_URL_PREFIXES[1],
    LOOPBACK_URL_PREFIXES[2],
    LOOPBACK_URL_PREFIXES[3],
];

mod models;
pub(crate) use models::*;

include!("storage.rs");
include!("state.rs");
include!("logging.rs");
include!("cli.rs");
include!("templates.rs");
include!("env.rs");
include!("ports.rs");
include!("lifecycle.rs");
include!("instance.rs");
include!("runtime_install.rs");
include!("traces.rs");

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
            // SAFETY: `env::set_var` is not thread-safe in general, but this
            // runs early inside the Tauri `setup` closure before any of the
            // app's own background threads or async tasks are spawned, and
            // framework threads do not access the process environment at
            // this stage.
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
            system_info,
            runtime_install_plan,
            runtime_install_progress,
            execute_runtime_install,
            list_templates,
            check_template_update,
            update_templates,
            get_template_settings,
            save_template_settings,
            prepare_instance,
            list_instances,
            list_vault,
            save_vault,
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
            check_instance_docker_requirements,
            list_atof_traces,
            get_atof_trace_detail,
            query_phoenix_traces,
            query_phoenix_trace_detail_cmd,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AgentSeek Desktop");
}
