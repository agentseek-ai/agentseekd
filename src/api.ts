import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
import requirements from "./runtime-requirements.json";
import type {
  CliStatus,
  AtofEventPage,
  EnvVariable,
  ExportEnvResult,
  InstanceRecord,
  LogEntry,
  LogPage,
  LogQuery,
  LogSettings,
  RuntimeInstallProgress,
  RuntimeInstallPlan,
  PrepareInstanceInput,
  PrepareInstanceResult,
  SaveEnvResult,
  SystemInfo,
  StorageStatus,
  TemplateInfo,
  TemplateConfig,
  TemplateUpdateCheck,
  TraceDetail,
  TracePage,
} from "./types";

const isTauri = () => "__TAURI_INTERNALS__" in window;
export const isNativeDesktop = isTauri();
const mockKey = "agentseek-desktop-preview-v3";
const mockLogSettingsKey = "agentseek-desktop-log-settings-v1";
const mockVaultSecretsKey = "agentseek-desktop-preview-vault-secrets";

interface MockStore {
  instances: InstanceRecord[];
  vault: EnvVariable[];
  logs: LogEntry[];
}

// Template catalog is fetched from the configured template repository in
// preview mode instead of a hard-coded list, keeping it in sync with the
// real catalog (mirrors the Rust `read_template_index` / `display_name`).
let templateIndexCache: TemplateInfo[] | null = null;

/// Convert a template repository URL to the raw templates/index.json URL.
/// Mirrors `parse_template_repo_url` semantics: `tree/<branch>` and
/// `releases/tag/<tag>` forms are supported; plain repo URLs use main.
function templateIndexUrl(repoUrl: string): string | null {
  const normalized = repoUrl.trim().replace(/\.git$/, "");
  const match = normalized.match(
    /^https:\/\/github\.com\/([^/]+)\/([^/]+)(?:\/(?:tree|releases\/tag)\/([^/]+))?$/,
  );
  if (!match) return null;
  const ref = match[3] || "main";
  return `https://raw.githubusercontent.com/${match[1]}/${match[2]}/${ref}/templates/index.json`;
}

/// Display name mirroring the Rust `display_name` helper: last path segment,
/// split on "-" / "_", each part capitalized, joined with spaces.
function templateDisplayName(templateId: string): string {
  const last = templateId.split("/").pop() || templateId;
  return last
    .split(/[-_]/)
    .filter((part) => part.length > 0)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

/// Fetch and parse the template catalog from the template repository.
/// Returns an empty list on any failure (offline, unreachable, invalid JSON).
async function fetchTemplateIndex(force: boolean): Promise<TemplateInfo[]> {
  if (!force && templateIndexCache) return templateIndexCache;
  try {
    const repoUrl =
      localStorage.getItem("agentseek-template-repo") ||
      "https://github.com/agentseek-ai/agentseek-templates.git";
    const indexUrl = templateIndexUrl(repoUrl);
    if (!indexUrl) return [];
    const response = await fetch(indexUrl);
    if (!response.ok) return [];
    const map = (await response.json()) as Record<string, string>;
    const result = Object.entries(map).map(([id, description]) => ({
      id,
      name: templateDisplayName(id),
      description,
      framework: id.split("/")[0] || "",
    }));
    templateIndexCache = result;
    return result;
  } catch {
    return [];
  }
}

const defaultVault: EnvVariable[] = [
  { key: "OPENAI_API_KEY", value: "", comment: "OpenAI compatible API key", source: "instance", modified: false },
  { key: "OPENAI_BASE_URL", value: "https://api.openai.com/v1", comment: "OpenAI compatible API endpoint", source: "instance", modified: false },
  { key: "MODEL_NAME", value: "openai:gpt-4o-mini", comment: "Default model identifier", source: "instance", modified: false },
];

function loadMock(): MockStore {
  const saved = localStorage.getItem(mockKey);
  if (saved) {
    const store = JSON.parse(saved) as MockStore;
    const secrets = JSON.parse(sessionStorage.getItem(mockVaultSecretsKey) || "{}") as Record<string, string>;
    for (const entry of store.vault) {
      if (entry.value) secrets[entry.key] = entry.value;
      entry.value = secrets[entry.key] || "";
    }
    sessionStorage.setItem(mockVaultSecretsKey, JSON.stringify(secrets));
    return store;
  }
  return { instances: [], vault: defaultVault, logs: [] };
}

function saveMock(store: MockStore) {
  const secrets = Object.fromEntries(store.vault.map((entry) => [entry.key, entry.value]));
  sessionStorage.setItem(mockVaultSecretsKey, JSON.stringify(secrets));
  localStorage.setItem(mockKey, JSON.stringify({
    ...store,
    vault: store.vault.map((entry) => ({ ...entry, value: "", modified: false })),
  }));
}

function loadMockLogSettings(): LogSettings {
  const saved = Number(localStorage.getItem(mockLogSettingsKey));
  return { runtimeRetentionDays: Number.isInteger(saved) && saved >= 1 && saved <= 3650 ? saved : 7 };
}

function pruneMockLogs(store: MockStore, runtimeRetentionDays: number) {
  const now = Math.floor(Date.now() / 1000);
  const runtimeCutoff = now - runtimeRetentionDays * 86_400;
  const deletedCutoff = now - 7 * 86_400;
  const activeInstances = new Set(store.instances.map((instance) => instance.id));
  store.logs = store.logs.filter((log) =>
    log.category === "runtime"
      ? log.createdAt >= runtimeCutoff
      : !log.instanceId || activeInstances.has(log.instanceId) || log.createdAt >= deletedCutoff,
  );
  store.logs.sort((left, right) => right.createdAt - left.createdAt || (right.sequence || 0) - (left.sequence || 0));
  if (store.logs.length > 100_000) store.logs.length = 99_000;
}

function mockLog(store: MockStore, instance: InstanceRecord | null, category: LogEntry["category"], level: string, message: string, command?: string) {
  store.logs.unshift({
    id: `log-${Date.now()}-${store.logs.length}`,
    instanceId: instance?.id,
    instanceName: instance?.name ?? "AgentSeek Desktop",
    category,
    level,
    message,
    command,
    createdAt: Math.floor(Date.now() / 1000),
    sequence: store.logs.length,
  });
  pruneMockLogs(store, loadMockLogSettings().runtimeRetentionDays);
}

const wait = (milliseconds: number) => new Promise((resolve) => window.setTimeout(resolve, milliseconds));

export const desktopApi = {
  async openExternalUrl(url: string): Promise<void> {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new Error("Only HTTP and HTTPS URLs can be opened.");
    }
    if (isTauri()) {
      await openUrl(parsed.toString());
      return;
    }
    const opened = window.open(parsed.toString(), "_blank", "noopener,noreferrer");
    if (!opened) throw new Error("The browser blocked the new window.");
  },

  async openLocalPath(path: string): Promise<void> {
    if (isTauri()) {
      await openPath(path);
      return;
    }
    throw new Error("Opening local files is only available in the desktop app.");
  },

  async cliStatus(checkLatest = true): Promise<CliStatus> {
    if (isTauri()) return invoke("cli_status", { checkLatest });
    return { platform: "macos", dependencyCommands: { uv: "uv self update", node: "managed Node runtime", npm: "managed npm runtime", git: "brew upgrade git", agentseek: "uv tool install --upgrade agentseek" }, minimumVersions: { uv: requirements.versions.uv.minimum, node: requirements.versions.node.minimum, npm: requirements.versions.npm.minimum, git: requirements.versions.git.minimum, agentseek: requirements.versions.agentseek.minimum }, nodeManaged: true, uvAvailable: true, uvPath: "/Users/name/.local/bin/uv", cliAvailable: true, cliCompatible: true, cliUpdateAvailable: false, cliLatestVersion: requirements.versions.agentseek.minimum, cliLatestVersionChecked: true, uvVersion: `uv ${requirements.versions.uv.minimum}`, cliVersion: `agentseek ${requirements.versions.agentseek.minimum}`, nodeAvailable: true, nodeCompatible: true, nodeVersion: `v${requirements.versions.node.managed}`, npmAvailable: true, npmCompatible: true, npmVersion: requirements.versions.npm.managed, gitAvailable: true, gitCompatible: true, gitVersion: `git version ${requirements.versions.git.minimum}`, uvCompatible: true, prerequisitesReady: true, installCommand: "uv tool install agentseek" };
  },

  async runtimeInstallPlan(forceAgentseekUpgrade = false): Promise<RuntimeInstallPlan> {
    if (isTauri()) return invoke("runtime_install_plan", { forceAgentseekUpgrade });
    return {
      taskId: `preview-${Date.now()}`,
      script: "#!/usr/bin/env bash\nset -eo pipefail\n# Detect and install required AgentSeek Desktop runtime dependencies.",
      scriptPath: "/tmp/agentseek-runtime-install/install.command",
      installDir: "/Users/name/Library/Application Support/com.oceanbase.agentseek.desktop/runtime",
      dependencies: forceAgentseekUpgrade ? ["agentseek"] : ["node/npm", "agentseek"],
    };
  },

  async executeRuntimeInstall(taskId: string): Promise<string> {
    if (isTauri()) return invoke("execute_runtime_install", { taskId });
    await wait(800);
    return "runtime ready";
  },

  async runtimeInstallProgress(taskId: string): Promise<RuntimeInstallProgress> {
    if (isTauri()) return invoke("runtime_install_progress", { taskId });
    return { status: "success", stage: "complete", log: "runtime ready" };
  },

  async listTemplates(force = false): Promise<TemplateInfo[]> {
    if (isTauri()) return invoke("list_templates", { force });
    return fetchTemplateIndex(force);
  },

  async checkTemplateUpdate(): Promise<TemplateUpdateCheck> {
    if (isTauri()) return invoke("check_template_update");
    await wait(200);
    return { currentVersion: "vX.Y.Z", latestVersion: "vX.Y.Z", hasUpdate: false };
  },

  async updateTemplates(): Promise<TemplateInfo[]> {
    if (isTauri()) return invoke("update_templates");
    return fetchTemplateIndex(true);
  },

  async getTemplateSettings(): Promise<TemplateConfig> {
    if (isTauri()) return invoke("get_template_settings");
    return {
      repoUrl: localStorage.getItem("agentseek-template-repo") || "https://github.com/agentseek-ai/agentseek-templates.git",
      checkout: localStorage.getItem("agentseek-template-checkout") || "",
      catalogUrl: localStorage.getItem("agentseek-template-catalog") || "",
    };
  },

  async saveTemplateSettings(cfg: TemplateConfig): Promise<void> {
    if (isTauri()) return invoke("save_template_settings", { cfg });
    localStorage.setItem("agentseek-template-repo", cfg.repoUrl);
    localStorage.setItem("agentseek-template-checkout", cfg.checkout);
    localStorage.setItem("agentseek-template-catalog", cfg.catalogUrl);
  },

  async listInstances(): Promise<InstanceRecord[]> {
    if (isTauri()) return invoke("list_instances");
    return loadMock().instances.sort((a, b) => b.createdAt - a.createdAt);
  },

  async listVault(): Promise<EnvVariable[]> {
    if (isTauri()) return invoke("list_vault");
    return loadMock().vault;
  },

  async saveVault(entries: EnvVariable[]): Promise<void> {
    if (isTauri()) return invoke("save_vault", { entries });
    const store = loadMock();
    store.vault = entries.map((entry) => ({ ...entry, modified: false }));
    saveMock(store);
  },

  async chooseDirectory(): Promise<string | null> {
    if (isTauri()) {
      const selected = await open({ directory: true, multiple: false, createDirectories: true });
      return typeof selected === "string" ? selected : null;
    }
    return "/Users/demo/AgentSeek/instances";
  },

  async chooseEnvFile(): Promise<string | null> {
    if (isTauri()) {
      const selected = await open({ directory: false, multiple: false });
      return typeof selected === "string" ? selected : null;
    }
    return "/Users/demo/project/.env.example";
  },

  async prepareInstance(input: PrepareInstanceInput): Promise<PrepareInstanceResult> {
    if (isTauri()) return invoke("prepare_instance", { input });
    void input;
    throw new Error("The browser preview cannot run the local CLI or write files. Use the Tauri desktop app.");
  },

  async loadInstanceEnv(instanceId: string): Promise<EnvVariable[]> {
    if (isTauri()) return invoke("load_instance_env", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (!instance) throw new Error("Instance not found");
    return store.vault.map((entry) => ({ ...entry, source: "vault", modified: false }));
  },

  async saveInstanceEnv(instanceId: string, entries: EnvVariable[], overwrite = false): Promise<SaveEnvResult> {
    if (isTauri()) return invoke("save_instance_env", { input: { instanceId, entries, overwrite } });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (!instance) throw new Error("Instance not found");
    if (instance.envPath && !overwrite) throw new Error(`ENV_FILE_EXISTS:${instance.envPath}`);
    const changed = entries.filter((entry) => entry.modified);
    for (const entry of changed) {
      const existing = store.vault.find((item) => item.key === entry.key);
      if (existing) Object.assign(existing, { ...entry, source: "instance", modified: false });
      else store.vault.push({ ...entry, source: "instance", modified: false });
    }
    const deployed = instance.pid != null || ["running", "stopped", "needs-doctor", "needs-restart"].includes(instance.status);
    instance.envPath = `${instance.workDir}/.env`;
    instance.status = deployed ? "needs-restart" : "ready-to-install";
    instance.needsDoctor = deployed;
    instance.updatedAt = Math.floor(Date.now() / 1000);
    mockLog(
      store,
      instance,
      "lifecycle",
      "success",
      `Generated ${instance.envPath} (${entries.length} keys, synced ${changed.length} to the vault)`,
    );
    saveMock(store);
    return {
      path: instance.envPath,
      keyCount: entries.length,
      syncedCount: changed.length,
      portChanges: [],
      entries: entries.map((entry) => ({ ...entry, modified: false })),
      dockerWarning: undefined,
    };
  },

  async continueInstall(instanceId: string): Promise<InstanceRecord> {
    if (isTauri()) return invoke("continue_install", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (!instance) throw new Error("Instance not found");
    instance.status = "installing";
    saveMock(store);
    await wait(900);
    mockLog(store, instance, "lifecycle", "success", "backend / frontend tasks completed", "uvx agentseek task backend && uvx agentseek task frontend");
    mockLog(store, instance, "lifecycle", "success", "Doctor passed: 0 failed", "uvx agentseek doctor");
    mockLog(store, instance, "lifecycle", "success", "Instance started successfully", "uvx agentseek dev");
    instance.status = "running";
    instance.updatedAt = Math.floor(Date.now() / 1000);
    instance.agentUrl = "http://127.0.0.1:8089";
    instance.uiUrl = "http://127.0.0.1:5174";
    instance.studioUrl = "https://smith.langchain.com/studio";
    saveMock(store);
    return instance;
  },

  async checkInstanceDockerRequirements(instanceId: string): Promise<string | null> {
    if (isTauri()) return invoke("check_instance_docker_requirements", { instanceId });
    return null;
  },

  async deploymentProgress(instanceId: string): Promise<string> {
    if (isTauri()) return invoke("deployment_progress", { instanceId });
    return "complete";
  },

  async stopInstance(instanceId: string): Promise<InstanceRecord> {
    if (isTauri()) return invoke("stop_instance", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (!instance) throw new Error("Instance not found");
    instance.status = "stopped";
    instance.updatedAt = Math.floor(Date.now() / 1000);
    mockLog(store, instance, "lifecycle", "success", "Instance stopped");
    saveMock(store);
    return instance;
  },

  async restartInstance(instanceId: string): Promise<InstanceRecord> {
    if (isTauri()) return invoke("restart_instance", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (!instance) throw new Error("Instance not found");
    await wait(500);
    instance.status = "running";
    instance.needsDoctor = false;
    instance.updatedAt = Math.floor(Date.now() / 1000);
    mockLog(store, instance, "lifecycle", "success", "Doctor passed; instance restarted", "uvx agentseek doctor && uvx agentseek dev");
    saveMock(store);
    return instance;
  },

  async markEnvEdited(instanceId: string): Promise<void> {
    if (isTauri()) return invoke("mark_env_edited", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId);
    if (instance) {
      instance.needsDoctor = true;
      instance.status = "needs-doctor";
      mockLog(store, instance, "lifecycle", "warning", "Instance configuration changed; restart is required");
      saveMock(store);
    }
  },

  async deleteInstance(instanceId: string): Promise<void> {
    if (isTauri()) return invoke("delete_instance", { instanceId });
    const store = loadMock();
    const instance = store.instances.find((item) => item.id === instanceId) ?? null;
    store.instances = store.instances.filter((item) => item.id !== instanceId);
    mockLog(store, instance, "lifecycle", "success", "Instance processes, working directory, and record deleted");
    saveMock(store);
  },

  async listLogs(query: LogQuery = { limit: 500 }): Promise<LogPage> {
    if (isTauri()) return invoke("list_logs", { query: { limit: 500, ...query } });
    const limit = Math.min(1000, Math.max(1, query.limit || 500));
    const entries = loadMock().logs
      .filter((log) => (query.beforeSequence == null || (log.sequence || 0) < query.beforeSequence) && (query.afterSequence == null || (log.sequence || 0) > query.afterSequence))
      .sort((left, right) => query.afterSequence == null ? (right.sequence || 0) - (left.sequence || 0) : (left.sequence || 0) - (right.sequence || 0));
    const groupCount = new Set(loadMock().logs.map((log) => log.instanceId || `name:${log.instanceName}`)).size;
    return { entries: entries.slice(0, limit), hasMore: entries.length > limit, groupCount };
  },

  async listAllLogs(query: LogQuery = {}): Promise<LogPage> {
    if (isTauri()) {
      return invoke("list_logs", { query: { ...query, loadAll: true, limit: 100_000 } });
    }
    const entries: LogEntry[] = [];
    let beforeSequence = query.beforeSequence;
    let hasMore = true;
    let groupCount = 0;
    while (hasMore && entries.length < 100_000) {
      const page = await this.listLogs({
        ...query,
        afterSequence: undefined,
        beforeSequence,
        limit: 1_000,
      });
      groupCount = page.groupCount;
      entries.push(...page.entries);
      hasMore = page.hasMore && page.entries.length > 0;
      const nextBeforeSequence = page.entries.reduce<number | undefined>(
        (minimum, entry) => {
          const sequence = Number(entry.sequence ?? 0);
          return minimum == null ? sequence : Math.min(minimum, sequence);
        },
        undefined,
      );
      if (nextBeforeSequence == null || nextBeforeSequence === beforeSequence) break;
      beforeSequence = nextBeforeSequence;
    }
    return { entries, hasMore, groupCount };
  },

  async logSettings(): Promise<LogSettings> {
    if (isTauri()) return invoke("log_settings");
    return loadMockLogSettings();
  },

  async saveLogSettings(settings: LogSettings): Promise<LogSettings> {
    if (isTauri()) return invoke("save_log_settings", { settings });
    if (!Number.isInteger(settings.runtimeRetentionDays) || settings.runtimeRetentionDays < 1 || settings.runtimeRetentionDays > 3650) {
      throw new Error("Runtime log retention must be between 1 and 3650 days");
    }
    localStorage.setItem(mockLogSettingsKey, String(settings.runtimeRetentionDays));
    const store = loadMock();
    pruneMockLogs(store, settings.runtimeRetentionDays);
    saveMock(store);
    return settings;
  },

  async importEnv(path: string): Promise<number> {
    if (isTauri()) return invoke("import_env", { path });
    const store = loadMock();
    const imported: EnvVariable = { key: "IMPORTED_CONFIG_PATH", value: path, comment: "Imported configuration source", source: "import", modified: false };
    const existing = store.vault.find((item) => item.key === imported.key);
    if (existing) Object.assign(existing, imported);
    else store.vault.push(imported);
    mockLog(store, null, "lifecycle", "success", `Imported 1 variable from ${path}`);
    saveMock(store);
    return 1;
  },

  async listEnvFiles(path: string): Promise<string[]> {
    if (isTauri()) return invoke("list_env_files", { path });
    return [".env.example", ".env.local", ".env"].map((name) => `${path.replace(/[\\/]+$/, "")}/${name}`);
  },

  async exportEnv(sourcePath: string, outputPath: string, overwrite = false): Promise<ExportEnvResult> {
    if (isTauri()) return invoke("export_env", { input: { sourcePath, outputPath, overwrite } });
    void sourcePath;
    return { path: outputPath, keyCount: 3, filledCount: 2, missingCount: 1 };
  },

  async systemInfo(): Promise<SystemInfo> {
    if (isTauri()) return invoke("system_info");
    return { appName: "AgentSeek Desktop", version: __APP_VERSION__, dataPath: "Browser localStorage preview", cliStrategy: "uv run agentseek", storage: "Embedded SQLite (desktop state only; isolated from template instances)", dockerAvailable: false, dockerComposeAvailable: false, dockerRunning: false };
  },

  async storageStatus(): Promise<StorageStatus> {
    if (isTauri()) return invoke("storage_status");
    const setupRequired = localStorage.getItem("agentseek-storage-configured") !== "1";
    return { mode: "seekdb_embedded", effectiveMode: "seekdb_embedded", path: "Browser SeekDB preview", defaultSqlitePath: "Browser localStorage preview", defaultSeekdbPath: "Browser SeekDB preview", host: "", port: 2881, tenant: "", database: "agentseek_desktop", defaultDatabase: "agentseek_desktop", user: "root", passwordConfigured: false, runtimeLogRetentionDays: loadMockLogSettings().runtimeRetentionDays, setupRequired, writable: true, error: null };
  },

  async configureStorage(config: StorageStatus & { password?: string }): Promise<StorageStatus> {
    if (isTauri()) return invoke("configure_storage", { config });
    localStorage.setItem("agentseek-storage-configured", "1");
    return { ...config, effectiveMode: config.mode, setupRequired: false, writable: true, error: null };
  },

  async listAtofTraces(workDir: string, page = 1, pageSize = 20): Promise<TracePage> {
    if (isTauri()) return invoke("list_atof_traces", { workDir, page, pageSize });
    await wait(120);
    return { entries: [], total: 0, page, pageSize };
  },

  async getAtofTraceDetail(workDir: string, traceId: string): Promise<TraceDetail | null> {
    if (isTauri()) return invoke("get_atof_trace_detail", { workDir, traceId });
    await wait(120);
    return null;
  },

  async readAtofEvents(workDir: string, limit = 40, offset = 0): Promise<AtofEventPage> {
    if (isTauri()) return invoke("read_atof_events", { workDir, limit, offset });
    await wait(120);
    return { entries: [], total: 0, offset, limit, hasMore: false };
  },

  async queryPhoenixTraces(
    phoenixUrl: string,
    serviceName?: string,
    page = 1,
    pageSize = 20,
  ): Promise<TracePage> {
    if (isTauri()) return invoke("query_phoenix_traces", { phoenixUrl, serviceName, page, pageSize });
    await wait(120);
    return { entries: [], total: 0, page, pageSize };
  },

  async queryPhoenixTraceDetail(phoenixUrl: string, traceId: string): Promise<TraceDetail | null> {
    if (isTauri()) return invoke("query_phoenix_trace_detail_cmd", { phoenixUrl, traceId });
    await wait(120);
    return null;
  },
};
