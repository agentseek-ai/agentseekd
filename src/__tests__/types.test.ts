import { describe, it, expect } from "vitest";
import type {
  TemplateInfo,
  InstanceRecord,
  EnvVariable,
  LogEntry,
  LogPage,
  LogQuery,
  LogSettings,
  PrepareInstanceInput,
  PrepareInstanceResult,
  SaveEnvResult,
  ExportEnvResult,
  SystemInfo,
  StorageStatus,
  CliStatus,
  RuntimeInstallPlan,
  RuntimeInstallProgress,
  ServiceEndpoint,
} from "../types";

describe("type serialization shapes", () => {
  it("TemplateInfo has expected fields", () => {
    const obj: TemplateInfo = {
      id: "langchain/default",
      name: "LangChain Default",
      description: "desc",
      framework: "langchain",
    };
    expect(obj.id).toBe("langchain/default");
    expect(obj.framework).toBe("langchain");
  });

  it("InstanceRecord has all required fields", () => {
    const obj: InstanceRecord = {
      id: "inst-1",
      name: "test",
      templateId: "langchain/default",
      status: "running",
      deploymentMode: "local",
      workDir: "/tmp/test",
      note: "",
      createdAt: 1,
      updatedAt: 1,
      needsDoctor: false,
    };
    expect(obj.id).toBe("inst-1");
    expect(obj.serviceEndpoints).toBeUndefined();
    expect(obj.pid).toBeUndefined();
  });

  it("InstanceRecord with optional fields", () => {
    const obj: InstanceRecord = {
      id: "inst-1",
      name: "test",
      templateId: "langchain/default",
      status: "running",
      deploymentMode: "local",
      workDir: "/tmp/test",
      note: "",
      createdAt: 1,
      updatedAt: 1,
      needsDoctor: false,
      pid: 12345,
      agentUrl: "http://127.0.0.1:8089",
      uiUrl: "http://127.0.0.1:5173",
      studioUrl: "https://smith.langchain.com/studio",
      projectName: "test",
      lifecycleVersion: 1,
      serviceEndpoints: [{ name: "app", url: "http://127.0.0.1:5173", kind: "web", primary: true }],
    };
    expect(obj.pid).toBe(12345);
    expect(obj.serviceEndpoints).toHaveLength(1);
  });

  it("EnvVariable has all fields", () => {
    const obj: EnvVariable = {
      key: "OPENAI_API_KEY",
      value: "sk-xxx",
      comment: "API key",
      source: "vault",
      modified: true,
    };
    expect(obj.key).toBe("OPENAI_API_KEY");
    expect(obj.modified).toBe(true);
  });

  it("LogEntry has all fields", () => {
    const obj: LogEntry = {
      id: "log-1",
      instanceId: "inst-1",
      instanceName: "test",
      category: "install",
      level: "success",
      message: "done",
      command: "agentseek dev",
      createdAt: 1,
      sequence: 1,
    };
    expect(obj.category).toBe("install");
    expect(obj.level).toBe("success");
  });

  it("LogPage has all fields", () => {
    const obj: LogPage = {
      entries: [],
      hasMore: false,
      groupCount: 0,
    };
    expect(obj.entries).toEqual([]);
    expect(obj.hasMore).toBe(false);
  });

  it("LogQuery has optional fields", () => {
    const obj: LogQuery = { limit: 100 };
    expect(obj.limit).toBe(100);
    expect(obj.beforeSequence).toBeUndefined();
  });

  it("LogSettings has retention days", () => {
    const obj: LogSettings = { runtimeRetentionDays: 7 };
    expect(obj.runtimeRetentionDays).toBe(7);
  });

  it("PrepareInstanceInput has all fields", () => {
    const obj: PrepareInstanceInput = {
      name: "test",
      templateId: "langchain/default",
      targetDir: "/tmp/test",
      deploymentMode: "local",
      note: "",
    };
    expect(obj.templateId).toBe("langchain/default");
  });

  it("PrepareInstanceResult has instance and env", () => {
    const obj: PrepareInstanceResult = {
      instance: {
        id: "inst-1",
        name: "test",
        templateId: "langchain/default",
        status: "ready-to-install",
        deploymentMode: "local",
        workDir: "/tmp/test",
        note: "",
        createdAt: 1,
        updatedAt: 1,
        needsDoctor: false,
      },
      env: [],
    };
    expect(obj.instance.id).toBe("inst-1");
    expect(obj.env).toEqual([]);
  });

  it("SaveEnvResult has all fields", () => {
    const obj: SaveEnvResult = {
      path: "/tmp/.env",
      keyCount: 10,
      syncedCount: 3,
      portChanges: [{ key: "PORT", oldPort: 8080, newPort: 8081 }],
      entries: [],
    };
    expect(obj.keyCount).toBe(10);
    expect(obj.portChanges).toHaveLength(1);
  });

  it("ExportEnvResult has all fields", () => {
    const obj: ExportEnvResult = {
      path: "/tmp/.env",
      keyCount: 5,
      filledCount: 3,
      missingCount: 2,
    };
    expect(obj.missingCount).toBe(2);
  });

  it("SystemInfo has all fields", () => {
    const obj: SystemInfo = {
      appName: "AgentSeek",
      version: "0.0.1-rc.1",
      dataPath: "/tmp",
      cliStrategy: "uv run agentseek",
      storage: "SQLite",
      dockerAvailable: false,
      dockerComposeAvailable: false,
      dockerRunning: false,
    };
    expect(obj.appName).toBe("AgentSeek");
  });

  it("StorageStatus has all fields", () => {
    const obj: StorageStatus = {
      mode: "seekdb_embedded",
      effectiveMode: "seekdb_embedded",
      path: "/tmp",
      defaultSqlitePath: "/tmp/default.db",
      defaultSeekdbPath: "/tmp/default",
      host: "",
      port: 2881,
      tenant: "",
      database: "agentseek",
      defaultDatabase: "agentseek",
      user: "root",
      passwordConfigured: false,
      runtimeLogRetentionDays: 7,
      setupRequired: false,
      writable: true,
      error: null,
    };
    expect(obj.port).toBe(2881);
    expect(obj.writable).toBe(true);
  });

  it("CliStatus has all fields", () => {
    const obj: CliStatus = {
      platform: "macos",
      dependencyCommands: { uv: "uv self update", node: "", npm: "", git: "", agentseek: "" },
      minimumVersions: { uv: "0.7.0", node: "24.18.0", npm: "11.16.0", git: "2.30.0", agentseek: "0.0.4" },
      nodeManaged: true,
      uvAvailable: true,
      uvPath: "/usr/local/bin/uv",
      cliAvailable: true,
      cliCompatible: true,
      cliUpdateAvailable: false,
      cliLatestVersion: "0.1.0",
      cliLatestVersionChecked: true,
      uvVersion: "uv 0.7.0",
      cliVersion: "agentseek 0.1.0",
      nodeAvailable: true,
      nodeCompatible: true,
      nodeVersion: "v24.18.0",
      npmAvailable: true,
      npmCompatible: true,
      npmVersion: "11.16.0",
      gitAvailable: true,
      gitCompatible: true,
      gitVersion: "git version 2.30.0",
      uvCompatible: true,
      prerequisitesReady: true,
      installCommand: "uv tool install agentseek",
    };
    expect(obj.platform).toBe("macos");
    expect(obj.prerequisitesReady).toBe(true);
  });

  it("RuntimeInstallPlan has all fields", () => {
    const obj: RuntimeInstallPlan = {
      taskId: "task-1",
      script: "#!/bin/bash",
      scriptPath: "/tmp/install.sh",
      installDir: "/tmp/install",
      dependencies: ["uv", "node"],
    };
    expect(obj.taskId).toBe("task-1");
  });

  it("RuntimeInstallProgress has all fields", () => {
    const obj: RuntimeInstallProgress = {
      status: "success",
      stage: "complete",
      log: "done",
    };
    expect(obj.status).toBe("success");
  });

  it("ServiceEndpoint has all fields", () => {
    const obj: ServiceEndpoint = {
      name: "app",
      url: "http://127.0.0.1:5173",
      kind: "web",
      primary: true,
    };
    expect(obj.kind).toBe("web");
    expect(obj.primary).toBe(true);
  });
});
