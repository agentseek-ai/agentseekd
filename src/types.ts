export type Page = "instances" | "templates" | "config" | "logs";
export type DeploymentMode = "local" | "docker";
export type LogCategory = "install" | "config" | "execution" | "runtime";

export interface TemplateInfo {
  id: string;
  name: string;
  description: string;
  framework: string;
}

export interface InstanceRecord {
  id: string;
  name: string;
  templateId: string;
  status: string;
  deploymentMode: DeploymentMode;
  workDir: string;
  envExamplePath?: string | null;
  envPath?: string | null;
  note: string;
  createdAt: number;
  updatedAt: number;
  needsDoctor: boolean;
  pid?: number | null;
  agentUrl?: string | null;
  uiUrl?: string | null;
  studioUrl?: string | null;
  projectName?: string | null;
  lifecycleVersion?: number | null;
  serviceEndpoints?: ServiceEndpoint[];
}

export interface ServiceEndpoint {
  name: string;
  url: string;
  kind?: "web" | "api" | "protocol" | "database" | "other" | string;
  primary?: boolean;
}

export interface EnvVariable {
  key: string;
  value: string;
  comment: string;
  source: "template" | "vault" | "instance" | "import" | string;
  modified: boolean;
}

export interface LogEntry {
  id: string;
  instanceId?: string | null;
  instanceName: string;
  category: LogCategory;
  level: "info" | "success" | "warning" | "error" | string;
  message: string;
  command?: string | null;
  createdAt: number;
  sequence?: number;
}

export interface LogSettings {
  runtimeRetentionDays: number;
}

export interface LogPage {
  entries: LogEntry[];
  hasMore: boolean;
  groupCount: number;
}

export interface LogQuery {
  beforeSequence?: number;
  afterSequence?: number;
  limit?: number;
}

export interface PrepareInstanceInput {
  name: string;
  templateId: string;
  targetDir: string;
  deploymentMode: DeploymentMode;
  note: string;
}

export interface PrepareInstanceResult {
  instance: InstanceRecord;
  env: EnvVariable[];
  dockerWarning?: string;
}

export interface SaveEnvResult {
  path: string;
  keyCount: number;
  syncedCount: number;
  portChanges: Array<{ key: string; oldPort: number; newPort: number }>;
  entries: EnvVariable[];
  dockerWarning?: string;
}

export interface ExportEnvResult {
  path: string;
  keyCount: number;
  filledCount: number;
  missingCount: number;
}

export interface SystemInfo {
  appName: string;
  version: string;
  dataPath: string;
  cliStrategy: string;
  storage: string;
  dockerAvailable: boolean;
  dockerComposeAvailable: boolean;
  dockerRunning: boolean;
}

export interface StorageStatus {
  mode: "sqlite_embedded" | "seekdb_embedded" | "seekdb_server" | "oceanbase_server";
  effectiveMode: "sqlite_embedded" | "seekdb_embedded" | "seekdb_server" | "oceanbase_server";
  path: string; defaultSqlitePath: string; defaultSeekdbPath: string;
  host: string; port: number; tenant: string; database: string; defaultDatabase: string; user: string;
  passwordConfigured: boolean; runtimeLogRetentionDays: number; setupRequired: boolean; writable: boolean; error?: string | null;
}

export interface CliStatus {
  platform: string;
  dependencyCommands: Record<"uv" | "node" | "npm" | "git" | "agentseek", string>;
  minimumVersions: Record<"uv" | "node" | "npm" | "git" | "agentseek", string>;
  nodeManaged: boolean;
  uvAvailable: boolean;
  uvPath: string;
  cliAvailable: boolean;
  cliCompatible: boolean;
  cliUpdateAvailable: boolean;
  cliLatestVersion: string;
  cliLatestVersionChecked: boolean;
  uvVersion: string;
  cliVersion: string;
  nodeAvailable: boolean;
  nodeCompatible: boolean;
  nodeVersion: string;
  npmAvailable: boolean;
  npmCompatible: boolean;
  npmVersion: string;
  gitAvailable: boolean;
  gitCompatible: boolean;
  gitVersion: string;
  uvCompatible: boolean;
  prerequisitesReady: boolean;
  installCommand: string;
}

export interface RuntimeInstallPlan {
  taskId: string;
  script: string;
  scriptPath: string;
  installDir: string;
  dependencies: string[];
}

export interface RuntimeInstallProgress {
  status: "pending" | "running" | "success" | "failed";
  stage: "pending" | "starting" | "uv" | "node" | "agentseek" | "complete" | string;
  log: string;
}

export type DeploymentStage = "create" | "tasks" | "doctor" | "dry-run" | "starting" | "complete" | "failed" | string;
