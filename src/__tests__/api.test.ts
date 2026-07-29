import { describe, it, expect, beforeEach } from "vitest";
import { desktopApi } from "../api";
import type { InstanceRecord, EnvVariable } from "../types";

const mockInstance: InstanceRecord = {
  id: "test-inst-1",
  name: "test_instance",
  templateId: "langchain/default",
  status: "ready-to-install",
  deploymentMode: "local",
  workDir: "/tmp/test_instance",
  note: "test note",
  createdAt: 100,
  updatedAt: 100,
  needsDoctor: false,
};

function resetStorage() {
  localStorage.clear();
  sessionStorage.clear();
}

describe("desktopApi mock layer", () => {
  beforeEach(() => {
    resetStorage();
  });

  // -----------------------------------------------------------------
  // URL validation
  // -----------------------------------------------------------------

  describe("openExternalUrl", () => {
    it("rejects non-HTTP protocols", async () => {
      await expect(desktopApi.openExternalUrl("file:///etc/passwd")).rejects.toThrow(
        "Only HTTP and HTTPS URLs can be opened.",
      );
    });

    it("rejects javascript: protocol", async () => {
      await expect(desktopApi.openExternalUrl("javascript:alert(1)")).rejects.toThrow(
        "Only HTTP and HTTPS URLs can be opened.",
      );
    });

    it("rejects data: protocol", async () => {
      await expect(desktopApi.openExternalUrl("data:text/html,<h1>hi</h1>")).rejects.toThrow(
        "Only HTTP and HTTPS URLs can be opened.",
      );
    });
  });

  // -----------------------------------------------------------------
  // Vault save/load
  // -----------------------------------------------------------------

  describe("vault", () => {
    it("returns default vault on first load", async () => {
      const vault = await desktopApi.listVault();
      expect(vault.length).toBeGreaterThan(0);
      expect(vault.some((e) => e.key === "OPENAI_API_KEY")).toBe(true);
    });

    it("round-trips vault entries", async () => {
      const entries: EnvVariable[] = [
        { key: "MY_KEY", value: "my_value", comment: "test", source: "instance", modified: false },
        { key: "OTHER_KEY", value: "other", comment: "", source: "instance", modified: false },
      ];
      await desktopApi.saveVault(entries);
      const loaded = await desktopApi.listVault();
      expect(loaded).toHaveLength(2);
      expect(loaded[0].key).toBe("MY_KEY");
      // Vault values are stored in sessionStorage, not localStorage
      const secrets = JSON.parse(sessionStorage.getItem("agentseek-desktop-preview-vault-secrets") || "{}");
      expect(secrets["MY_KEY"]).toBe("my_value");
    });

    it("clears modified flag on save", async () => {
      const entries: EnvVariable[] = [
        { key: "KEY1", value: "val1", comment: "", source: "instance", modified: true },
      ];
      await desktopApi.saveVault(entries);
      const loaded = await desktopApi.listVault();
      expect(loaded[0].modified).toBe(false);
    });

    it("handles empty vault", async () => {
      await desktopApi.saveVault([]);
      const loaded = await desktopApi.listVault();
      expect(loaded).toEqual([]);
    });
  });

  // -----------------------------------------------------------------
  // Instance CRUD
  // -----------------------------------------------------------------

  describe("instances", () => {
    it("returns empty list initially", async () => {
      const instances = await desktopApi.listInstances();
      expect(instances).toEqual([]);
    });

    it("returns sorted by createdAt descending", async () => {
      // Manually populate mock store
      const store: { instances: InstanceRecord[]; vault: EnvVariable[]; logs: unknown[] } = {
        instances: [
          { ...mockInstance, id: "old", createdAt: 100 },
          { ...mockInstance, id: "new", createdAt: 200 },
        ],
        vault: [],
        logs: [],
      };
      localStorage.setItem("agentseek-desktop-preview-v2", JSON.stringify({
        ...store,
        vault: [],
      }));
      const instances = await desktopApi.listInstances();
      expect(instances[0].id).toBe("new");
      expect(instances[1].id).toBe("old");
    });
  });

  // -----------------------------------------------------------------
  // Instance env
  // -----------------------------------------------------------------

  describe("loadInstanceEnv", () => {
    it("throws for missing instance", async () => {
      await expect(desktopApi.loadInstanceEnv("nonexistent")).rejects.toThrow("Instance not found");
    });

    it("returns vault entries for existing instance", async () => {
      const store = {
        instances: [mockInstance],
        vault: [
          { key: "KEY1", value: "val1", comment: "", source: "instance", modified: false },
        ],
        logs: [],
      };
      localStorage.setItem("agentseek-desktop-preview-v2", JSON.stringify({
        ...store,
        vault: store.vault.map((e) => ({ ...e, value: "", modified: false })),
      }));
      sessionStorage.setItem("agentseek-desktop-preview-vault-secrets", JSON.stringify({ KEY1: "val1" }));
      const env = await desktopApi.loadInstanceEnv("test-inst-1");
      expect(env).toHaveLength(1);
      expect(env[0].key).toBe("KEY1");
      expect(env[0].source).toBe("vault");
    });
  });

  // -----------------------------------------------------------------
  // Stop / restart / delete
  // -----------------------------------------------------------------

  describe("stopInstance", () => {
    it("throws for missing instance", async () => {
      await expect(desktopApi.stopInstance("nonexistent")).rejects.toThrow("Instance not found");
    });

    it("sets status to stopped", async () => {
      const store = {
        instances: [{ ...mockInstance, status: "running" }],
        vault: [],
        logs: [],
      };
      localStorage.setItem("agentseek-desktop-preview-v2", JSON.stringify({
        ...store,
        vault: [],
      }));
      const stopped = await desktopApi.stopInstance("test-inst-1");
      expect(stopped.status).toBe("stopped");
    });
  });

  describe("deleteInstance", () => {
    it("removes instance from store", async () => {
      const store = {
        instances: [mockInstance],
        vault: [],
        logs: [],
      };
      localStorage.setItem("agentseek-desktop-preview-v2", JSON.stringify({
        ...store,
        vault: [],
      }));
      await desktopApi.deleteInstance("test-inst-1");
      const instances = await desktopApi.listInstances();
      expect(instances).toEqual([]);
    });

    it("succeeds even for nonexistent instance", async () => {
      await expect(desktopApi.deleteInstance("nonexistent")).resolves.toBeUndefined();
    });
  });

  // -----------------------------------------------------------------
  // Logs
  // -----------------------------------------------------------------

  describe("listLogs", () => {
    it("returns empty page initially", async () => {
      const page = await desktopApi.listLogs();
      expect(page.entries).toEqual([]);
      expect(page.hasMore).toBe(false);
      expect(page.groupCount).toBe(0);
    });

    it("respects limit parameter", async () => {
      const logs = Array.from({ length: 10 }, (_, i) => ({
        id: `log-${i}`,
        instanceId: null,
        instanceName: "Test",
        category: "install" as const,
        level: "info",
        message: `msg ${i}`,
        createdAt: i,
        sequence: i,
      }));
      const store = { instances: [], vault: [], logs };
      localStorage.setItem("agentseek-desktop-preview-v2", JSON.stringify({
        ...store,
        vault: [],
      }));
      const page = await desktopApi.listLogs({ limit: 5 });
      expect(page.entries).toHaveLength(5);
      expect(page.hasMore).toBe(true);
    });

    it("clamps limit to minimum 1", async () => {
      const page = await desktopApi.listLogs({ limit: 0 });
      // Should not throw, should return empty
      expect(page.entries).toEqual([]);
    });

    it("clamps limit to maximum 1000", async () => {
      const page = await desktopApi.listLogs({ limit: 99999 });
      expect(page.entries).toEqual([]);
    });
  });

  // -----------------------------------------------------------------
  // Log settings
  // -----------------------------------------------------------------

  describe("logSettings", () => {
    it("returns default 7 days", async () => {
      const settings = await desktopApi.logSettings();
      expect(settings.runtimeRetentionDays).toBe(7);
    });

    it("round-trips saved settings", async () => {
      await desktopApi.saveLogSettings({ runtimeRetentionDays: 30 });
      const settings = await desktopApi.logSettings();
      expect(settings.runtimeRetentionDays).toBe(30);
    });

    it("rejects retention < 1", async () => {
      await expect(desktopApi.saveLogSettings({ runtimeRetentionDays: 0 })).rejects.toThrow(
        "Runtime log retention must be between 1 and 3650 days",
      );
    });

    it("rejects retention > 3650", async () => {
      await expect(desktopApi.saveLogSettings({ runtimeRetentionDays: 3651 })).rejects.toThrow(
        "Runtime log retention must be between 1 and 3650 days",
      );
    });
  });

  // -----------------------------------------------------------------
  // Import / export env
  // -----------------------------------------------------------------

  describe("importEnv", () => {
    it("imports a variable and returns count", async () => {
      const count = await desktopApi.importEnv("/tmp/.env.example");
      expect(count).toBe(1);
      const vault = await desktopApi.listVault();
      expect(vault.some((e) => e.key === "IMPORTED_CONFIG_PATH")).toBe(true);
    });
  });

  describe("exportEnv", () => {
    it("returns export result", async () => {
      const result = await desktopApi.exportEnv("/tmp/.env.example", "/tmp/output.env");
      expect(result.path).toBe("/tmp/output.env");
      expect(result.keyCount).toBe(3);
      expect(result.filledCount).toBe(2);
      expect(result.missingCount).toBe(1);
    });
  });

  describe("listEnvFiles", () => {
    it("returns expected env file names", async () => {
      const files = await desktopApi.listEnvFiles("/tmp/project");
      expect(files).toHaveLength(3);
      expect(files[0]).toContain(".env.example");
    });
  });

  // -----------------------------------------------------------------
  // Storage status
  // -----------------------------------------------------------------

  describe("storageStatus", () => {
    it("returns default status with setupRequired", async () => {
      const status = await desktopApi.storageStatus();
      expect(status.mode).toBe("seekdb_embedded");
      expect(status.effectiveMode).toBe("seekdb_embedded");
      expect(status.setupRequired).toBe(true);
      expect(status.writable).toBe(true);
    });

    it("setupRequired becomes false after configureStorage", async () => {
      await desktopApi.configureStorage({
        mode: "sqlite_embedded",
        effectiveMode: "sqlite_embedded",
        path: "/tmp",
        defaultSqlitePath: "/tmp",
        defaultSeekdbPath: "/tmp",
        host: "",
        port: 2881,
        tenant: "",
        database: "test",
        defaultDatabase: "test",
        user: "root",
        passwordConfigured: false,
        runtimeLogRetentionDays: 7,
        setupRequired: true,
        writable: true,
        error: null,
      });
      const status = await desktopApi.storageStatus();
      expect(status.setupRequired).toBe(false);
    });
  });

  // -----------------------------------------------------------------
  // System info
  // -----------------------------------------------------------------

  describe("systemInfo", () => {
    it("returns system info", async () => {
      const info = await desktopApi.systemInfo();
      expect(info.appName).toBe("AgentSeek");
      expect(typeof info.dockerAvailable).toBe("boolean");
    });
  });

  // -----------------------------------------------------------------
  // Templates
  // -----------------------------------------------------------------

  describe("listTemplates", () => {
    it("returns templates list", async () => {
      const templates = await desktopApi.listTemplates();
      expect(templates.length).toBeGreaterThan(0);
      expect(templates[0].id).toBeDefined();
    });
  });
});
