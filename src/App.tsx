import { startTransition, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { filterTemplates, templateFrameworks as collectTemplateFrameworks } from "./template-utils";
import JsonView from "@uiw/react-json-view";
import {
  Bot,
  Boxes,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  CircleDot,
  CircleStop,
  Copy,
  Database,
  ExternalLink,
  FileDown,
  FileKey,
  FileText,
  FileUp,
  FolderOpen,
  Info,
  KeyRound,
  Languages,
  LayoutTemplate,
  LoaderCircle,
  Maximize2,
  Moon,
  Orbit,
  Pencil,
  Plus,
  RefreshCw,
  RotateCw,
  Search,
  Server,
  SlidersHorizontal,
  ShieldCheck,
  SquareTerminal,
  Sun,
  Trash2,
  Workflow,
  Wrench,
  X,
} from "lucide-react";
import { desktopApi, isNativeDesktop } from "./api";
import { translations, type Language, type TranslationKey } from "./i18n";
import type {
  CliStatus,
  DockerEnvironmentStatus,
  AtofJsonLine,
  DeploymentStage,
  EnvVariable,
  InstanceRecord,
  LogEntry,
  LogCategory,
  Page,
  PrepareInstanceInput,
  RuntimeInstallProgress,
  RuntimeInstallPlan,
  SaveEnvResult,
  SystemInfo,
  StorageStatus,
  TemplateInfo,
  TemplateUpdateCheck,
  TraceSummary,
  TraceDetail,
  SpanNode,
} from "./types";

type Theme = "light" | "dark";
type InstallStep = "form" | "env" | "deploying" | "failed";
type DependencyKey = "uv" | "node" | "npm" | "git" | "agentseek";
type DetailTab = "entry" | "trace-center" | "atof";

interface SetupDependency {
  id: DependencyKey;
  name: string;
  version: string;
  minimum: string;
  requirementLabel?: string;
  ready: boolean;
  scope?: string;
}

interface InstallState {
  template: TemplateInfo;
  step: InstallStep;
  form: PrepareInstanceInput;
  instance: InstanceRecord | null;
  entries: EnvVariable[];
  generated: SaveEnvResult | null;
  warning: string;
  error: string;
  deploymentStage: DeploymentStage;
  dockerStatus: DockerEnvironmentStatus | null;
  dockerChecking: boolean;
}

interface EnvOverwriteState {
  context: "install" | "config" | "export";
  path: string;
}

type DockerConfirmationAction = "deploy" | "restart" | "delete";

interface DockerConfirmationState {
  instance: InstanceRecord;
  action: DockerConfirmationAction;
  status: DockerEnvironmentStatus | null;
  checking: boolean;
  starting: boolean;
  error: string;
}

interface CommentTooltipState {
  text: string;
  onChange: (value: string) => void;
  left: number;
  top?: number;
  bottom?: number;
  width: number;
  maxHeight: number;
  placement: "above" | "below";
}

const secretPattern = /(key|token|secret|password|credential)/i;
function formatTime(value: number, language: Language) {
  return new Intl.DateTimeFormat(language === "zh" ? "zh-CN" : "en-US", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value * 1000));
}

function paginationSummary(language: Language, page: number, total: number): string {
  return language === "zh"
    ? `第 ${page} 页 / 共 ${total} 条`
    : `Page ${page} / ${total} total`;
}

function PaginationControls({
  language,
  page,
  total,
  pageSize,
  onPageChange,
  disabled = false,
  className = "",
}: {
  language: Language;
  page: number;
  total: number;
  pageSize: number;
  onPageChange: (page: number) => void;
  disabled?: boolean;
  className?: string;
}) {
  if (total === 0) return null;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  const currentPage = Math.min(page, totalPages);
  return (
    <div className={`log-pagination ${className}`.trim()}>
      <span className="log-pagination-info">{paginationSummary(language, currentPage, total)}</span>
      <button className="button secondary compact" disabled={disabled || currentPage <= 1} onClick={() => onPageChange(currentPage - 1)} type="button"><ChevronLeft /></button>
      <button className="button secondary compact" disabled={disabled || currentPage >= totalPages} onClick={() => onPageChange(currentPage + 1)} type="button"><ChevronRight /></button>
    </div>
  );
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function storageModeLabel(mode: StorageStatus["mode"] | StorageStatus["effectiveMode"]) {
  if (mode === "sqlite_embedded") return "sqlite";
  if (mode === "seekdb_embedded") return "seekdb";
  return "SeekDB / OceanBase Server";
}

function storagePathForMode(config: StorageStatus, mode: StorageStatus["mode"]) {
  if (mode === "sqlite_embedded") return config.defaultSqlitePath;
  if (mode === "seekdb_embedded") return config.defaultSeekdbPath;
  return config.path;
}

function storageDatabaseForMode(config: StorageStatus, mode: StorageStatus["mode"]) {
  if (mode === "sqlite_embedded" || mode === "seekdb_embedded" || mode !== config.mode) {
    return config.defaultDatabase;
  }
  return config.database;
}

function envExistsPath(error: unknown) {
  const message = errorMessage(error);
  const marker = "ENV_FILE_EXISTS:";
  return message.startsWith(marker) ? message.slice(marker.length) : null;
}

function hasNemoRelayCapability(instance: InstanceRecord | null | undefined) {
  if (!instance) return false;
  const fingerprint = [
    instance.templateId,
    instance.name,
    instance.projectName,
    instance.workDir,
  ]
    .filter((value): value is string => Boolean(value))
    .join(" ")
    .toLowerCase();
  return /(^|[\/_\-\s])relay([\/_\-\s]|$)/.test(fingerprint) || /nemo[\/_\-\s]?relay/.test(fingerprint);
}

function atofSourceRelativePath() {
  return ".nemo-relay/atof/events.jsonl";
}

function portChangeSummary(result: SaveEnvResult) {
  return result.portChanges.map((change) => `${change.key} ${change.oldPort} → ${change.newPort}`).join(", ");
}

function SystemInfoValue({ value, expanded }: { value: string; expanded?: boolean }) {
  return <div className={`info-value ${expanded ? "has-popover" : ""}`} tabIndex={expanded ? 0 : undefined}>
    <code>{value}</code>
    {expanded && <span className="info-value-popover" role="tooltip">{value}</span>}
  </div>;
}

function waitForPaint() {
  return new Promise<void>((resolve) => {
    window.requestAnimationFrame(() => window.requestAnimationFrame(() => resolve()));
  });
}

function wait(milliseconds: number) {
  return new Promise<void>((resolve) => window.setTimeout(resolve, milliseconds));
}

function cleanLogText(value: string) {
  return value
    .replace(/[\u001b\u009b][[\]()#;?]*(?:(?:(?:[a-zA-Z\d]*(?:;[-a-zA-Z\d\/#&.:=?%@~_]+)*)?\u0007)|(?:(?:\d{1,4}(?:[;:]\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]))/g, "")
    .replace(/\r/g, "");
}

function compactInstallError(error: unknown) {
  const lines = cleanLogText(errorMessage(error).replace(/\r/g, "\n"))
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && !/^\d+(?:\.\d+)?%#+$/.test(line));
  return lines.slice(-8).join("\n") || errorMessage(error);
}

function logSequence(entry: LogEntry) {
  if (typeof entry.sequence === "number") return entry.sequence;
  const match = entry.id.match(/-(\d+)$/);
  return match ? Number(match[1]) : 0;
}

function mergeLogEntries(current: LogEntry[], incoming: LogEntry[]) {
  const entries = new Map(current.map((entry) => [entry.id, entry]));
  for (const entry of incoming) entries.set(entry.id, entry);
  return Array.from(entries.values()).sort((left, right) => logSequence(right) - logSequence(left));
}

function LogTerminal({ instanceName, entries, language, liveLabel }: { instanceName: string; entries: LogEntry[]; language: Language; liveLabel: string }) {
  const outputRef = useRef<HTMLDivElement>(null);
  const followLatestRef = useRef(true);

  useLayoutEffect(() => {
    if (!followLatestRef.current) return;
    const output = outputRef.current;
    if (!output) return;
    output.scrollTop = output.scrollHeight;
    const frame = window.requestAnimationFrame(() => {
      if (followLatestRef.current) output.scrollTop = output.scrollHeight;
    });
    return () => window.cancelAnimationFrame(frame);
  }, [entries]);

  return <div className="terminal-window">
    <div className="terminal-toolbar"><span className="terminal-dots"><i /><i /><i /></span><strong>{instanceName}</strong><span className="terminal-live"><i />{liveLabel}</span></div>
    <div
      className="terminal-output"
      ref={outputRef}
      role="log"
      aria-live="polite"
      onWheel={(event) => {
        if (event.deltaY < 0) followLatestRef.current = false;
      }}
      onScroll={(event) => {
        const output = event.currentTarget;
        followLatestRef.current = output.scrollHeight - output.clientHeight - output.scrollTop <= 24;
      }}
    >
      {[...entries].reverse().map((entry) => <div className={`terminal-line ${entry.level}`} key={entry.id}>
        <time>{formatTime(entry.createdAt, language)}</time>
        <span className="terminal-level">[{entry.level.toUpperCase()}]</span>
        <div>{entry.command && <code>$ {cleanLogText(entry.command)}</code>}<span>{cleanLogText(entry.message)}</span></div>
      </div>)}
    </div>
  </div>;
}

function statusTone(status: string) {
  if (status === "running") return "success";
  if (["installing", "starting", "checking", "restarting", "stopping", "deleting"].includes(status)) return "progress";
  if (["configuring", "needs-doctor", "needs-restart"].includes(status)) return "warning";
  if (["failed", "delete-failed"].includes(status)) return "error";
  if (status === "stopped") return "neutral";
  return "ready";
}

/// Number of log groups displayed per page in the log center.
const LOGS_PER_PAGE = 10;
/// Number of instances displayed per page in the instance list.
const INSTANCES_PER_PAGE = 10;
/// Number of trace summaries displayed per page.
const TRACES_PER_PAGE = 20;
/// Number of trace summaries displayed per page in the instance drawer.
const TRACE_CENTER_PER_PAGE = 10;
const ATOF_EVENTS_PAGE_SIZE = 40;

export default function App() {
  const [page, setPage] = useState<Page>("instances");
  const [language, setLanguage] = useState<Language>(() => (localStorage.getItem("agentseek-language") as Language) || "zh");
  const [theme, setTheme] = useState<Theme>(() => (localStorage.getItem("agentseek-theme") as Theme) || "light");
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const [instances, setInstances] = useState<InstanceRecord[]>([]);
  const [vault, setVault] = useState<EnvVariable[]>([]);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshingTemplates, setRefreshingTemplates] = useState(false);
  const [templateUpdateInfo, setTemplateUpdateInfo] = useState<TemplateUpdateCheck | null>(null);
  const [templateUpdating, setTemplateUpdating] = useState(false);
  const [templateVersionDisplay, setTemplateVersionDisplay] = useState<TemplateUpdateCheck | null>(null);
  const [templateUrlEditOpen, setTemplateUrlEditOpen] = useState(false);
  const [templateRepoUrlInput, setTemplateRepoUrlInput] = useState("");
  const [templateCatalogInput, setTemplateCatalogInput] = useState("");
  const [templateUrlSaving, setTemplateUrlSaving] = useState(false);
  const [search, setSearch] = useState("");
  const [templateTab, setTemplateTab] = useState("all");
  const [toast, setToast] = useState("");
  const [installState, setInstallState] = useState<InstallState | null>(null);
  const [installModalVisible, setInstallModalVisible] = useState(true);
  const [installDrag, setInstallDrag] = useState({ x: 0, y: 0 });
  const [installDragging, setInstallDragging] = useState(false);
  const installDragRef = useRef({ startX: 0, startY: 0, offsetX: 0, offsetY: 0 });
  const [detailInstance, setDetailInstance] = useState<InstanceRecord | null>(null);
  const [traceLoading, setTraceLoading] = useState(false);
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null);
  const [cliStatus, setCliStatus] = useState<CliStatus | null>(null);
  const [cliInstalling, setCliInstalling] = useState(false);
  const [cliChecking, setCliChecking] = useState(false);
  const [installQueue, setInstallQueue] = useState<DependencyKey[]>([]);
  const [activeDependency, setActiveDependency] = useState<DependencyKey | null>(null);
  const [cliInstallOutput, setCliInstallOutput] = useState("");
  const [failedDependency, setFailedDependency] = useState<DependencyKey | null>(null);
  const [installConfirmOpen, setInstallConfirmOpen] = useState(false);
  const [runtimeInstallPlan, setRuntimeInstallPlan] = useState<RuntimeInstallPlan | null>(null);
  const [installPlanLoading, setInstallPlanLoading] = useState(false);
  const [cliUpgradeRequested, setCliUpgradeRequested] = useState(false);
  const [envOverwrite, setEnvOverwrite] = useState<EnvOverwriteState | null>(null);
  const [openActionMenuId, setOpenActionMenuId] = useState<string | null>(null);
  const [instanceAction, setInstanceAction] = useState<{ id: string; action: "stop" | "restart" | "delete" } | null>(null);
  const [instanceActionError, setInstanceActionError] = useState("");
  const [dockerConfirmation, setDockerConfirmation] = useState<DockerConfirmationState | null>(null);
  const dockerConfirmationResolver = useRef<((approved: boolean) => void) | null>(null);
  const dockerConfirmationRequestId = useRef(0);
  const [showSystemInfo, setShowSystemInfo] = useState(false);
  const [storageConfig, setStorageConfig] = useState<(StorageStatus & { password?: string }) | null>(null);
  const [storageSetupRequired, setStorageSetupRequired] = useState(false);
  const [storageSetupFlow, setStorageSetupFlow] = useState(false);
  const [storageBusy, setStorageBusy] = useState(false);
  const [storageConfigDirty, setStorageConfigDirty] = useState(false);
  const [selectedConfigId, setSelectedConfigId] = useState("");
  const [configEntries, setConfigEntries] = useState<EnvVariable[]>([]);
  const [configBusy, setConfigBusy] = useState(false);
  const [configGenerated, setConfigGenerated] = useState<SaveEnvResult | null>(null);
  const [configTab, setConfigTab] = useState<"vault" | "instance">("vault");
  const [logCategory, setLogCategory] = useState<"all" | LogCategory>("all");
  const [logInstance, setLogInstance] = useState("all");
  const [traceInstanceId, setTraceInstanceId] = useState("");
  const [traceSummaries, setTraceSummaries] = useState<TraceSummary[]>([]);
  const [tracePage, setTracePage] = useState(1);
  const [traceTotal, setTraceTotal] = useState(0);
  const [traceDetailView, setTraceDetailView] = useState<TraceDetail | null>(null);
  const [selectedSpanId, setSelectedSpanId] = useState<string | null>(null);
  const [tracePanelOpen, setTracePanelOpen] = useState(false);
  const [detailTab, setDetailTab] = useState<DetailTab>("entry");
  const [traceCenterSummaries, setTraceCenterSummaries] = useState<TraceSummary[]>([]);
  const [traceCenterPage, setTraceCenterPage] = useState(1);
  const [traceCenterTotal, setTraceCenterTotal] = useState(0);
  const [traceCenterLoading, setTraceCenterLoading] = useState(false);
  const [traceCenterRefreshKey, setTraceCenterRefreshKey] = useState(0);
  const [tracePanelDetail, setTracePanelDetail] = useState<TraceDetail | null>(null);
  const [tracePanelSelectedSpanId, setTracePanelSelectedSpanId] = useState<string | null>(null);
  const [atofEvents, setAtofEvents] = useState<AtofJsonLine[]>([]);
  const [selectedAtofEventIndex, setSelectedAtofEventIndex] = useState(0);
  const [atofEventTotal, setAtofEventTotal] = useState(0);
  const [atofEventsHasMore, setAtofEventsHasMore] = useState(false);
  const [atofEventsLoading, setAtofEventsLoading] = useState(false);
  const [atofEventsLoadingMore, setAtofEventsLoadingMore] = useState(false);
  const [atofEventsError, setAtofEventsError] = useState("");
  const [atofEventsRefreshKey, setAtofEventsRefreshKey] = useState(0);
  const [runtimeRetentionDays, setRuntimeRetentionDays] = useState(7);
  const [logSettingsBusy, setLogSettingsBusy] = useState(false);
  const [expandedLogGroups, setExpandedLogGroups] = useState<Set<string>>(new Set());
  const [logPage, setLogPage] = useState(1);
  const [instancePage, setInstancePage] = useState(1);
  const [importOpen, setImportOpen] = useState(false);
  const [importPath, setImportPath] = useState("");
  const [exportOpen, setExportOpen] = useState(false);
  const [exportPath, setExportPath] = useState("");
  const [exportFiles, setExportFiles] = useState<string[]>([]);
  const [exportSource, setExportSource] = useState("");
  const [exportOutput, setExportOutput] = useState("");
  const [exportScanning, setExportScanning] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  const [commentTooltip, setCommentTooltip] = useState<CommentTooltipState | null>(null);
  const commentTooltipTimer = useRef<number | null>(null);
  const latestLogSequenceRef = useRef(0);
  const logHistoryLoadedRef = useRef(false);
  const logHistoryLoadingRef = useRef(false);
  const initializedRef = useRef(false);

  const copy = translations[language];
  const tr = useCallback((key: TranslationKey) => copy[key], [copy]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("agentseek-theme", theme);
  }, [theme]);

  useEffect(() => {
    document.documentElement.lang = language === "zh" ? "zh-CN" : "en";
    localStorage.setItem("agentseek-language", language);
  }, [language]);

  useEffect(() => {
    const closeActionMenu = (event: PointerEvent) => {
      if (!(event.target instanceof Element) || !event.target.closest(".action-menu")) {
        setOpenActionMenuId(null);
      }
    };
    const closeActionMenuWithKeyboard = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpenActionMenuId(null);
      }
    };
    document.addEventListener("pointerdown", closeActionMenu);
    document.addEventListener("keydown", closeActionMenuWithKeyboard);
    return () => {
      document.removeEventListener("pointerdown", closeActionMenu);
      document.removeEventListener("keydown", closeActionMenuWithKeyboard);
    };
  }, []);

  const notify = useCallback((message: string) => {
    setToast(message);
    window.setTimeout(() => setToast(""), 3200);
  }, []);

  const firstRunStorageReady = !storageSetupRequired
    && !!storageConfig
    && !storageConfigDirty
    && storageConfig.writable
    && storageConfig.effectiveMode === storageConfig.mode;

  const refreshData = useCallback(async () => {
    const [nextInstances, nextVault, nextLogPage] = await Promise.all([
      desktopApi.listInstances(),
      desktopApi.listVault(),
      desktopApi.listLogs({ limit: 500 }),
    ]);
    setInstances(nextInstances);
    setVault(nextVault);
    // Refreshing instance/template data must not replace the complete history
    // already hydrated by the log center with only the latest 500 entries.
    setLogs((current) => mergeLogEntries(current, nextLogPage.entries));
    latestLogSequenceRef.current = Math.max(0, ...nextLogPage.entries.map((entry) => logSequence(entry)));
    setSelectedConfigId((current) => nextInstances.some((instance) => instance.id === current) ? current : nextInstances[0]?.id || "");
  }, []);

  const refreshLatestLogs = useCallback(async () => {
    const page = await desktopApi.listLogs({ limit: 1_000 });
    setLogs((current) => mergeLogEntries(current, page.entries));
    latestLogSequenceRef.current = Math.max(latestLogSequenceRef.current, ...page.entries.map((entry) => logSequence(entry)));
  }, []);

  const refreshTemplates = useCallback(async (checkCliVersion = false) => {
    setRefreshingTemplates(true);
    try {
      if (checkCliVersion) {
        const [nextTemplates, nextCliStatus] = await Promise.all([
          desktopApi.listTemplates(),
          desktopApi.cliStatus(true),
        ]);
        setTemplates(nextTemplates);
        setCliStatus(nextCliStatus);
      } else {
        setTemplates(await desktopApi.listTemplates());
      }
    } catch (error) {
      notify(errorMessage(error));
    } finally {
      setRefreshingTemplates(false);
    }
  }, [notify]);

  const checkAndPromptTemplateUpdate = useCallback(async () => {
    setRefreshingTemplates(true);
    try {
      const [updateInfo, nextTemplates] = await Promise.all([
        desktopApi.checkTemplateUpdate(),
        desktopApi.listTemplates(true),
      ]);
      setTemplates(nextTemplates);
      setTemplateVersionDisplay(updateInfo);
      if (updateInfo.hasUpdate) {
        setTemplateUpdateInfo(updateInfo);
      }
    } catch (error) {
      notify(errorMessage(error));
    } finally {
      setRefreshingTemplates(false);
    }
  }, [notify]);

  const confirmTemplateUpdate = useCallback(async () => {
    setTemplateUpdating(true);
    setTemplateUpdateInfo(null);
    try {
      const nextTemplates = await desktopApi.updateTemplates();
      setTemplates(nextTemplates);
      const updateInfo = await desktopApi.checkTemplateUpdate();
      setTemplateVersionDisplay(updateInfo);
      notify(tr("templateUpdateSuccess"));
    } catch (error) {
      notify(tr("templateUpdateFailed") + ": " + errorMessage(error));
    } finally {
      setTemplateUpdating(false);
    }
  }, [notify, tr]);

  const openTemplateUrlEdit = useCallback(async () => {
    const cfg = await desktopApi.getTemplateSettings();
    setTemplateRepoUrlInput(cfg.repoUrl);
    setTemplateCatalogInput(cfg.catalogUrl);
    setTemplateUrlEditOpen(true);
  }, []);

  const saveTemplateUrl = useCallback(async () => {
    setTemplateUrlSaving(true);
    try {
      await desktopApi.saveTemplateSettings({
        repoUrl: templateRepoUrlInput,
        checkout: "",
        catalogUrl: templateCatalogInput,
      });
      setTemplateUrlEditOpen(false);
      notify(tr("templateUrlSaved"));
      await refreshTemplates();
    } catch (error) {
      notify(tr("templateUrlInvalid") + ": " + errorMessage(error));
    } finally {
      setTemplateUrlSaving(false);
    }
  }, [templateRepoUrlInput, templateCatalogInput, notify, tr, refreshTemplates]);

  useEffect(() => {
    if (initializedRef.current) return;
    initializedRef.current = true;

    Promise.all([refreshData(), desktopApi.systemInfo(), desktopApi.cliStatus(false), desktopApi.logSettings(), desktopApi.storageStatus()])
      .then(([, nextSystemInfo, nextCliStatus, nextLogSettings, nextStorageStatus]) => {
        setSystemInfo(nextSystemInfo);
        setCliStatus(nextCliStatus);
        setRuntimeRetentionDays(nextLogSettings.runtimeRetentionDays);
        if (nextStorageStatus.setupRequired || !nextStorageStatus.writable || nextStorageStatus.effectiveMode !== nextStorageStatus.mode) {
          // Keep the first-run storage choice blocking until the backend persists it successfully.
          setStorageConfig(nextStorageStatus);
          setStorageSetupRequired(nextStorageStatus.setupRequired);
          setStorageSetupFlow(true);
          setStorageConfigDirty(true);
        }
        if (nextCliStatus.cliAvailable) void refreshTemplates();
        if (nextCliStatus.cliAvailable) {
          // Version discovery uses the network, so it runs after the local first-run check.
          void desktopApi.cliStatus(true).then(setCliStatus).catch(() => undefined);
        }
      })
      .catch((error) => notify(errorMessage(error)))
      .finally(() => setLoading(false));
  }, [notify, refreshData, refreshTemplates]);

  useEffect(() => {
    if (!storageSetupFlow || !firstRunStorageReady || !cliStatus?.prerequisitesReady) return;
    setStorageConfig(null);
    setStorageConfigDirty(false);
    setStorageSetupFlow(false);
    void refreshTemplates();
  }, [cliStatus?.prerequisitesReady, firstRunStorageReady, refreshTemplates, storageSetupFlow]);

  useEffect(() => {
    if (!installDragging) return;
    const onMove = (event: MouseEvent) => {
      setInstallDrag({
        x: installDragRef.current.offsetX + event.clientX - installDragRef.current.startX,
        y: installDragRef.current.offsetY + event.clientY - installDragRef.current.startY,
      });
    };
    const onUp = () => setInstallDragging(false);
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [installDragging]);

  useEffect(() => {
    if (page === "logs") setExpandedLogGroups(new Set());
  }, [page]);

  useEffect(() => {
    if (page !== "logs") return;
    setExpandedLogGroups(logInstance === "all" ? new Set() : new Set([logInstance]));
  }, [logInstance, page]);

  useEffect(() => {
    setLogPage(1);
  }, [logCategory, logInstance]);

  // Whether any instance is currently in a transitioning state. Used as a
  // stable boolean dependency so the polling interval is not torn down and
  // recreated on every poll cycle (which caused re-render storms + errors).
  const hasTransitioning = useMemo(
    () =>
      instances.some((inst) =>
        ["installing", "starting", "checking", "restarting", "stopping", "deleting"].includes(inst.status),
      ),
    [instances],
  );

  // Poll instance status while any instance is in a transitioning state so the
  // UI reflects backend status changes (e.g. restart finishing or failing).
  useEffect(() => {
    if (!hasTransitioning) return;
    let active = true;
    const timer = window.setInterval(async () => {
      try {
        const next = await desktopApi.listInstances();
        if (!active) return;
        setInstances(next);
      } catch {
        // Ignore transient polling errors.
      }
    }, 3_000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, [hasTransitioning]);

  useEffect(() => {
    if (page !== "logs") return;
    let active = true;
    let polling = false;
    const pollLogs = async () => {
      if (polling) return;
      polling = true;
      try {
        const [nextLogPage, nextInstances] = await Promise.all([
          desktopApi.listLogs({ afterSequence: latestLogSequenceRef.current, limit: 500 }),
          desktopApi.listInstances(),
        ]);
        if (!active) return;
        if (nextLogPage.entries.length) {
          latestLogSequenceRef.current = Math.max(latestLogSequenceRef.current, ...nextLogPage.entries.map((entry) => logSequence(entry)));
          setLogs((current) => {
            const known = new Set(current.map((entry) => entry.id));
            const added = nextLogPage.entries.filter((entry) => !known.has(entry.id)).reverse();
            return added.length ? [...added, ...current] : current;
          });
        }
        setInstances(nextInstances);
      } catch {
        // Preserve the current terminal output when a background poll is interrupted.
      } finally {
        polling = false;
      }
    };
    let timer: number | null = null;
    const hydrateHistory = async () => {
      if (logHistoryLoadedRef.current || logHistoryLoadingRef.current) return;
      logHistoryLoadingRef.current = true;
      try {
        const history = await desktopApi.listAllLogs();
        if (!active) return;
        startTransition(() => {
          setLogs(history.entries);
        });
        latestLogSequenceRef.current = Math.max(0, ...history.entries.map((entry) => logSequence(entry)));
        logHistoryLoadedRef.current = true;
      } catch {
        // Keep the recent log view if historical hydration is interrupted.
      } finally {
        logHistoryLoadingRef.current = false;
      }
    };
    const hydrateAndPoll = () => {
      // Let the log page paint its recent entries before parsing/grouping the
      // full history. This prevents a large store from blocking navigation.
      const frame = window.requestAnimationFrame(() => void hydrateHistory());
      void pollLogs();
      timer = window.setInterval(() => void pollLogs(), 5_000);
      return () => window.cancelAnimationFrame(frame);
    };
    const cancelInitialHydration = hydrateAndPoll();
    return () => {
      active = false;
      cancelInitialHydration();
      if (timer !== null) window.clearInterval(timer);
    };
  }, [page]);

  // ── Helper: extract Phoenix URL from an instance ──
  const phoenixUrlFor = useCallback((inst: InstanceRecord | null | undefined) => {
    if (!inst?.serviceEndpoints) return null;
    const ep = inst.serviceEndpoints.find(
      (s) => s.name.toLowerCase() === "phoenix" || (s.kind === "web" && !s.primary && s.url?.includes("6006")),
    );
    return ep?.url?.replace(/\/+$/, "") ?? null;
  }, []);

  // ── Helpers: ATOF provides the stable timeline; Phoenix is the fallback. ──
  const fetchTracePage = async (instance: InstanceRecord, page: number, limit: number) => {
    let result = await desktopApi.listAtofTraces(instance.workDir, page, limit);
    if (result.entries.length > 0 || result.total > 0) return result;
    const pUrl = phoenixUrlFor(instance);
    if (pUrl) {
      try {
        return await desktopApi.queryPhoenixTraces(pUrl, instance.name, page, limit);
      } catch {
        // Keep Trace center usable while Phoenix is still starting.
      }
    }
    return result;
  };

  const fetchTraceDetail = async (instance: InstanceRecord, traceId: string) => {
    const atofDetail = await desktopApi.getAtofTraceDetail(instance.workDir, traceId);
    if (atofDetail) return atofDetail;
    const pUrl = phoenixUrlFor(instance);
    if (pUrl) {
      try {
        const detail = await desktopApi.queryPhoenixTraceDetail(pUrl, traceId);
        if (detail) return detail;
      } catch {
        // Fall through to the local ATOF detail.
      }
    }
    return null;
  };

  // ── Trace center data ──
  useEffect(() => {
    if (page !== "traces" || traceDetailView) return;
    // Auto-select first running instance when entering trace center.
    if (!traceInstanceId) {
      const firstRunning = instances.find((i) => i.status === "running");
      if (firstRunning) { setTraceInstanceId(firstRunning.id); return; }
      return;
    }
    const instance = instances.find((i) => i.id === traceInstanceId);
    if (!instance?.workDir) return;
    let active = true;
    const poll = async () => {
      try {
        const result = await fetchTracePage(instance, tracePage, TRACES_PER_PAGE);
        if (active) { setTraceSummaries(result.entries); setTraceTotal(result.total); }
      } catch { /* keep stale data */ }
    };
    setTraceLoading(true);
    void poll().finally(() => { if (active) setTraceLoading(false); });
    const timer = window.setInterval(() => void poll(), 5_000);
    return () => { active = false; window.clearInterval(timer); };
  }, [page, traceInstanceId, instances, traceDetailView, tracePage]);

  const phoenixBaseUrl = useMemo(() => phoenixUrlFor(detailInstance), [detailInstance, phoenixUrlFor]);
  const detailHasPhoenixTraceCenter = Boolean(phoenixBaseUrl);
  const detailHasAtofTab = hasNemoRelayCapability(detailInstance);
  const detailAtofSourceRelativePath = useMemo(
    () => detailInstance?.workDir ? atofSourceRelativePath() : "",
    [detailInstance?.workDir],
  );

  useEffect(() => {
    if (!detailInstance || !detailHasPhoenixTraceCenter || detailTab !== "trace-center") {
      setTraceCenterSummaries([]);
      setTraceCenterTotal(0);
      setTraceCenterLoading(false);
      return;
    }
    const isRunning = detailInstance.status === "running";
    if (!detailInstance.workDir || !isRunning) {
      setTraceCenterSummaries([]);
      setTraceCenterTotal(0);
      setTraceCenterLoading(false);
      return;
    }
    let active = true;
    setTraceCenterLoading(true);
    (async () => {
      try {
        const result = await fetchTracePage(detailInstance, traceCenterPage, TRACE_CENTER_PER_PAGE);
        if (active) { setTraceCenterSummaries(result.entries); setTraceCenterTotal(result.total); setTraceCenterLoading(false); }
      } catch (err) { console.error(err); notify(errorMessage(err)); if (active) { setTraceCenterSummaries([]); setTraceCenterTotal(0); setTraceCenterLoading(false); } }
    })();
    return () => { active = false; };
  }, [detailHasPhoenixTraceCenter, detailInstance?.id, detailInstance?.status, detailInstance?.workDir, detailTab, notify, traceCenterPage, traceCenterRefreshKey]);

  useEffect(() => {
    if (!detailInstance || !detailHasAtofTab || detailTab !== "atof" || !detailInstance.workDir) {
      setAtofEvents([]);
      setSelectedAtofEventIndex(0);
      setAtofEventTotal(0);
      setAtofEventsHasMore(false);
      setAtofEventsLoading(false);
      setAtofEventsLoadingMore(false);
      setAtofEventsError("");
      return;
    }
    let active = true;
    setAtofEventsLoading(true);
    setAtofEventsError("");
    desktopApi.readAtofEvents(detailInstance.workDir, ATOF_EVENTS_PAGE_SIZE, 0)
      .then((result) => {
        if (!active) return;
        setAtofEvents(result.entries);
        setAtofEventTotal(result.total);
        setAtofEventsHasMore(result.hasMore);
        setSelectedAtofEventIndex(0);
      })
      .catch((error) => {
        if (!active) return;
        setAtofEvents([]);
        setAtofEventsError(errorMessage(error));
      })
      .finally(() => { if (active) setAtofEventsLoading(false); });
    return () => { active = false; };
  }, [atofEventsRefreshKey, detailHasAtofTab, detailInstance?.id, detailInstance?.workDir, detailTab]);

  const loadMoreAtofEvents = async () => {
    if (!detailInstance?.workDir || !atofEventsHasMore || atofEventsLoading || atofEventsLoadingMore) return;
    setAtofEventsLoadingMore(true);
    try {
      const result = await desktopApi.readAtofEvents(detailInstance.workDir, ATOF_EVENTS_PAGE_SIZE, atofEvents.length);
      setAtofEvents((current) => [...current, ...result.entries]);
      setAtofEventTotal(result.total);
      setAtofEventsHasMore(result.hasMore);
    } catch (error) {
      setAtofEventsError(errorMessage(error));
    } finally {
      setAtofEventsLoadingMore(false);
    }
  };

  useEffect(() => {
    setDetailTab("entry");
    setTraceCenterPage(1);
  }, [detailInstance?.id]);

  useEffect(() => {
    if (!detailInstance) return;
    if (detailTab === "trace-center" && !detailHasPhoenixTraceCenter) {
      setDetailTab(detailHasAtofTab ? "atof" : "entry");
    }
    if (detailTab === "atof" && !detailHasAtofTab) {
      setDetailTab(detailHasPhoenixTraceCenter ? "trace-center" : "entry");
    }
  }, [detailHasAtofTab, detailHasPhoenixTraceCenter, detailInstance, detailTab]);

  useEffect(() => {
    setTracePage(1);
  }, [traceInstanceId]);

  const saveRuntimeLogRetention = async () => {
    if (!Number.isInteger(runtimeRetentionDays) || runtimeRetentionDays < 1 || runtimeRetentionDays > 3650) {
      notify(tr("retentionRange"));
      return;
    }
    setLogSettingsBusy(true);
    try {
      const saved = await desktopApi.saveLogSettings({ runtimeRetentionDays });
      setRuntimeRetentionDays(saved.runtimeRetentionDays);
      const page = await desktopApi.listAllLogs();
      setLogs(page.entries);
      latestLogSequenceRef.current = Math.max(0, ...page.entries.map((entry) => logSequence(entry)));
      notify(tr("retentionSaved"));
    } catch (error) {
      notify(errorMessage(error));
    } finally {
      setLogSettingsBusy(false);
    }
  };

  const recheckCli = async () => {
    setCliChecking(true);
    setCliInstallOutput("");
    setFailedDependency(null);
    try {
      await waitForPaint();
      const nextStatus = await desktopApi.cliStatus();
      setCliStatus(nextStatus);
      if (nextStatus.cliAvailable) await refreshTemplates();
    } catch (error) {
      setCliInstallOutput(errorMessage(error));
    } finally {
      setCliChecking(false);
    }
  };

  const openRuntimeInstallConfirm = async (forceAgentseekUpgrade = false) => {
    setCliInstallOutput("");
    setInstallPlanLoading(true);
    setCliUpgradeRequested(forceAgentseekUpgrade);
    try {
      const plan = await desktopApi.runtimeInstallPlan(forceAgentseekUpgrade);
      setRuntimeInstallPlan(plan);
      setInstallConfirmOpen(true);
    } catch (error) {
      setCliInstallOutput(errorMessage(error));
      setCliUpgradeRequested(false);
    } finally {
      setInstallPlanLoading(false);
    }
  };

  const installRequiredDependencies = async () => {
    if (!cliStatus || !runtimeInstallPlan) return;
    setInstallConfirmOpen(false);
    setCliInstalling(true);
    setCliInstallOutput("");
    try {
      const queue = runtimeInstallPlan.dependencies
        .map((dependency) => dependency === "node/npm" ? "node" : dependency)
        .filter((dependency): dependency is DependencyKey => ["uv", "node", "agentseek"].includes(dependency));
      setInstallQueue(queue);
      setActiveDependency(queue[0] || null);
      const taskId = runtimeInstallPlan.taskId;
      let lastStage = "";
      let lastStatusRefreshStage = "";
      let stopWatching = false;
      const watchInstall = async () => {
        while (!stopWatching) {
          let progress: RuntimeInstallProgress;
          try {
            progress = await desktopApi.runtimeInstallProgress(taskId);
          } catch {
            await wait(800);
            continue;
          }
          if (progress.stage !== lastStage) {
            lastStage = progress.stage;
            const activeStage = ["uv", "node", "agentseek"].includes(progress.stage)
              ? progress.stage as DependencyKey
              : progress.stage === "starting" ? queue[0] || null : null;
            setActiveDependency(activeStage);
            if (["node", "agentseek", "complete"].includes(progress.stage) && progress.stage !== lastStatusRefreshStage) {
              lastStatusRefreshStage = progress.stage;
              void desktopApi.cliStatus().then(setCliStatus).catch(() => undefined);
            }
          }
          if (progress.status === "success" || progress.status === "failed") { if (progress.status === "failed") { setFailedDependency(["uv","node","agentseek"].includes(progress.stage) ? progress.stage as DependencyKey : queue[0] || null); } return; }
          await wait(800);
        }
      };
      const watchPromise = watchInstall();
      let output: string;
      try {
        output = await desktopApi.executeRuntimeInstall(taskId);
      } finally {
        stopWatching = true;
        await watchPromise;
      }
      const nextStatus = await desktopApi.cliStatus();
      setCliStatus(nextStatus);
      if (nextStatus.cliCompatible) await refreshTemplates();
      setCliInstallOutput(output);
    } catch (error) {
      setCliInstallOutput(compactInstallError(error));
    } finally {
      setActiveDependency(null);
      setInstallQueue([]);
      setCliInstalling(false);
      setRuntimeInstallPlan(null);
      setCliUpgradeRequested(false);
    }
  };

  useEffect(() => {
    if (page !== "config" || configTab !== "instance" || !selectedConfigId) return;
    let active = true;
    setConfigEntries([]);
    setConfigBusy(true);
    setConfigGenerated(null);
    desktopApi
      .loadInstanceEnv(selectedConfigId)
      .then((entries) => {
        if (active) setConfigEntries(entries);
      })
      .catch((error) => {
        if (active) notify(errorMessage(error));
      })
      .finally(() => {
        if (active) setConfigBusy(false);
      });
    return () => {
      active = false;
    };
  }, [configTab, notify, page, selectedConfigId]);

  useEffect(() => {
    const instanceId = installState?.step === "deploying" ? installState.instance?.id : null;
    if (!instanceId) return;
    let active = true;
    const poll = async () => {
      try {
        const deploymentStage = await desktopApi.deploymentProgress(instanceId);
        if (!active) return;
        setInstallState((current) => current?.instance?.id === instanceId ? { ...current, deploymentStage } : current);
      } catch {
        // Deployment itself reports failures; a transient progress poll must not replace that error.
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), 500);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, [installState?.instance?.id, installState?.step]);

  const pageCopy = {
    instances: [tr("instances"), tr("instancesDesc")],
    templates: [tr("templates"), tr("templatesDesc")],
    config: [tr("config"), tr("configDesc")],
    logs: [tr("logs"), tr("logsDesc")],
    traces: [tr("traces"), tr("tracesDesc")],
  }[page];

  const filteredInstances = useMemo(() => {
    const query = search.trim().toLowerCase();
    return instances.filter((instance) =>
      !query || [instance.name, instance.templateId, instance.workDir, instance.note].some((value) => value.toLowerCase().includes(query)),
    );
  }, [instances, search]);

  const totalInstancePages = Math.max(1, Math.ceil(filteredInstances.length / INSTANCES_PER_PAGE));
  const currentInstancePage = Math.min(instancePage, totalInstancePages);
  const paginatedInstances = filteredInstances.slice(
    (currentInstancePage - 1) * INSTANCES_PER_PAGE,
    currentInstancePage * INSTANCES_PER_PAGE,
  );

  // Distinct template types (framework = first segment of the template id),
  // rendered as tabs above the template list.
  const templateFrameworks = useMemo(() => collectTemplateFrameworks(templates), [templates]);

  const filteredTemplates = useMemo(() => filterTemplates(templates, templateTab, search), [templates, templateTab, search]);

  const deferredLogs = useDeferredValue(logs);
  const filteredLogs = useMemo(
    () =>
      deferredLogs.filter(
        (entry) =>
          (logCategory === "all" || entry.category === logCategory) &&
          (logInstance === "all" || entry.instanceId === logInstance),
      ),
    [deferredLogs, logCategory, logInstance],
  );

  const buildLogGroups = (entries: LogEntry[]) => {
    const grouped = new Map<string, LogEntry[]>();
    for (const entry of entries) {
      const key = entry.instanceId || `name:${entry.instanceName}`;
      const current = grouped.get(key) || [];
      current.push(entry);
      grouped.set(key, current);
    }
    return Array.from(grouped, ([id, entries]) => {
      const orderedEntries = entries.sort((a, b) => b.createdAt - a.createdAt || logSequence(b) - logSequence(a));
      const latestEntry = orderedEntries[0];
      const instanceId = latestEntry?.instanceId;
      const instanceStatus = instanceId ? instances.find((instance) => instance.id === instanceId)?.status : undefined;
      const latestMessage = latestEntry?.message.toLowerCase() || "";
      const deleted = !instanceStatus && latestEntry?.level === "success" && latestMessage.includes("deleted");
      return {
        id,
        instanceName: latestEntry?.instanceName || "AgentSeek Desktop",
        instanceStatus,
        entries: orderedEntries,
        latestAt: Math.max(...entries.map((entry) => entry.createdAt)),
        startedAt: Math.min(...entries.map((entry) => entry.createdAt)),
        failed: latestEntry?.level === "error",
        deleted,
        categories: Array.from(new Set(entries.map((entry) => entry.category))),
      };
    }).sort((a, b) => b.id.localeCompare(a.id));
  };

  const logGroups = useMemo(() => buildLogGroups(filteredLogs), [filteredLogs, instances]);

  const totalLogPages = Math.max(1, Math.ceil(logGroups.length / LOGS_PER_PAGE));
  const currentLogPage = Math.min(logPage, totalLogPages);
  const paginatedLogGroups = logGroups.slice(
    (currentLogPage - 1) * LOGS_PER_PAGE,
    currentLogPage * LOGS_PER_PAGE,
  );

  // Keep the sidebar badge aligned with the rows shown in the unfiltered log view.
  const lifecycleCount = logGroups.length;

  const statusLabel = (status: string) => {
    if (status === "running") return tr("running");
    if (status === "stopped") return tr("stopped");
    if (status === "configuring") return tr("configuring");
    if (status === "installing") return tr("installing");
    if (status === "starting") return tr("starting");
    if (status === "checking" || status === "restarting") return tr("restarting");
    if (status === "stopping") return tr("stopping");
    if (status === "deleting") return tr("deleting");
    if (status === "delete-failed") return tr("deleteFailed");
    if (status === "ready-to-install") return tr("ready");
    if (status === "needs-doctor" || status === "needs-restart") return tr("doctorRequired");
    if (status === "failed") return tr("deploymentFailed");
    return status;
  };

  const replaceInstance = (updated: InstanceRecord) => {
    setInstances((current) => current.map((instance) => (instance.id === updated.id ? updated : instance)));
    setDetailInstance((current) => (current?.id === updated.id ? updated : current));
  };

  const closeDockerConfirmation = (approved: boolean) => {
    dockerConfirmationRequestId.current += 1;
    dockerConfirmationResolver.current?.(approved);
    dockerConfirmationResolver.current = null;
    setDockerConfirmation(null);
  };

  const requestDockerConfirmation = (instance: InstanceRecord, action: DockerConfirmationAction) => {
    dockerConfirmationResolver.current?.(false);
    const requestId = dockerConfirmationRequestId.current + 1;
    dockerConfirmationRequestId.current = requestId;
    setDockerConfirmation({ instance, action, status: null, checking: true, starting: false, error: "" });
    const result = new Promise<boolean>((resolve) => {
      dockerConfirmationResolver.current = resolve;
    });
    void desktopApi.getInstanceDockerStatus(instance.id).then((status) => {
      if (dockerConfirmationRequestId.current !== requestId) return;
      if (!status) {
        closeDockerConfirmation(true);
        return;
      }
      setDockerConfirmation((current) => current?.instance.id === instance.id ? { ...current, status, checking: false } : current);
    }).catch((error) => {
      if (dockerConfirmationRequestId.current !== requestId) return;
      setDockerConfirmation((current) => current?.instance.id === instance.id ? { ...current, checking: false, error: errorMessage(error) } : current);
    });
    return result;
  };

  const recheckDockerConfirmation = async () => {
    if (!dockerConfirmation) return;
    const instanceId = dockerConfirmation.instance.id;
    const requestId = dockerConfirmationRequestId.current;
    setDockerConfirmation((current) => current ? { ...current, checking: true, error: "" } : current);
    try {
      const status = await desktopApi.getInstanceDockerStatus(instanceId);
      if (dockerConfirmationRequestId.current !== requestId) return;
      if (!status) {
        closeDockerConfirmation(true);
        return;
      }
      setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, status, checking: false, error: "" } : current);
    } catch (error) {
      if (dockerConfirmationRequestId.current !== requestId) return;
      setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, checking: false, error: errorMessage(error) } : current);
    }
  };

  const startDockerFromConfirmation = async () => {
    if (!dockerConfirmation) return;
    const instanceId = dockerConfirmation.instance.id;
    const requestId = dockerConfirmationRequestId.current;
    setDockerConfirmation((current) => current ? { ...current, starting: true, error: "" } : current);
    try {
      await desktopApi.startInstanceDockerRuntime(instanceId);
      if (dockerConfirmationRequestId.current !== requestId) return;
      setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, starting: false, checking: true } : current);
      for (let attempt = 0; attempt < 30; attempt += 1) {
        await new Promise((resolve) => window.setTimeout(resolve, 1000));
        if (dockerConfirmationRequestId.current !== requestId) return;
        const status = await desktopApi.getInstanceDockerStatus(instanceId);
        if (dockerConfirmationRequestId.current !== requestId) return;
        if (!status) {
          closeDockerConfirmation(true);
          return;
        }
        if (status.daemonRunning) {
          setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, status, checking: false, starting: false, error: "" } : current);
          return;
        }
        setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, status, checking: true } : current);
      }
      setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, checking: false, starting: false, error: tr("dockerStartTimeout") } : current);
    } catch (error) {
      if (dockerConfirmationRequestId.current !== requestId) return;
      setDockerConfirmation((current) => current?.instance.id === instanceId ? { ...current, checking: false, starting: false, error: errorMessage(error) } : current);
    }
  };

  const runInstanceAction = async (action: "stop" | "restart" | "delete", instance: InstanceRecord) => {
    setOpenActionMenuId(null);
    setInstanceActionError("");
    try {
      if (action === "restart" && !(await requestDockerConfirmation(instance, "restart"))) {
        return;
      }
      if (action === "stop" || action === "restart") {
        setInstanceAction({ id: instance.id, action });
        replaceInstance({ ...instance, status: action === "stop" ? "stopping" : "restarting", updatedAt: Math.floor(Date.now() / 1000) });
      }
      if (action === "stop") replaceInstance(await desktopApi.stopInstance(instance.id));
      if (action === "restart") replaceInstance(await desktopApi.restartInstance(instance.id));
      if (action === "delete") {
        if (!window.confirm(tr("confirmDelete"))) return;
        if (!(await requestDockerConfirmation(instance, "delete"))) {
          return;
        }
        setInstanceAction({ id: instance.id, action: "delete" });
        replaceInstance({ ...instance, status: "deleting", updatedAt: Math.floor(Date.now() / 1000) });
        await desktopApi.deleteInstance(instance.id);
        setInstances((current) => current.filter((item) => item.id !== instance.id));
        setDetailInstance(null);
      }
      await refreshLatestLogs();
      notify(tr("success"));
    } catch (error) {
      await refreshData();
      setInstanceActionError(errorMessage(error));
      notify(errorMessage(error));
    } finally {
      setInstanceAction(null);
    }
  };

  const openInstall = (template: TemplateInfo) => {
    setInstallModalVisible(true);
    setInstallState({
      template,
      step: "form",
      form: { name: "", templateId: template.id, targetDir: "", note: "" },
      instance: null,
      entries: [],
      generated: null,
      warning: "",
      error: "",
      deploymentStage: "create",
      dockerStatus: null,
      dockerChecking: false,
    });
  };

  const prepareInstall = async () => {
    if (!installState) return;
    if (!installState.form.name.trim()) {
      setInstallState({ ...installState, error: tr("requiredName") });
      return;
    }
    if (!installState.form.targetDir.trim()) {
      setInstallState({ ...installState, error: tr("requiredDir") });
      return;
    }
    setInstallModalVisible(true);
    setInstallState({ ...installState, step: "deploying", deploymentStage: "create", error: "" });
    try {
      const prepared = await desktopApi.prepareInstance(installState.form);
      setInstallState({ ...installState, step: "env", instance: prepared.instance, entries: prepared.env, generated: null, warning: "", deploymentStage: "tasks", error: "", dockerStatus: null, dockerChecking: false });
      await refreshData();
    } catch (error) {
      await refreshData();
      setInstallState({ ...installState, step: "form", error: errorMessage(error) });
    }
  };

  const generateInstallEnv = async (overwrite = false) => {
    if (!installState?.instance) return;
    try {
      const generated = await desktopApi.saveInstanceEnv(installState.instance.id, installState.entries, overwrite);
      setEnvOverwrite(null);
      setInstallState({ ...installState, entries: generated.entries, generated, warning: installState.warning });
      await refreshData();
    } catch (error) {
      const path = envExistsPath(error);
      if (path) setEnvOverwrite({ context: "install", path });
      else setInstallState({ ...installState, error: errorMessage(error) });
    }
  };

  const continueInstall = async () => {
    if (!installState?.instance || !installState.generated) return;
    const targetInstanceId = installState.instance.id;
    setInstallModalVisible(false);
    if (!(await requestDockerConfirmation(installState.instance, "deploy"))) {
      setInstallModalVisible(true);
      return;
    }
    setInstallModalVisible(true);
    const deployingInstance = { ...installState.instance, status: "installing", updatedAt: Math.floor(Date.now() / 1000) };
    setInstallState({ ...installState, instance: deployingInstance, step: "deploying", deploymentStage: "tasks", dockerStatus: null, dockerChecking: false, error: "" });
    replaceInstance(deployingInstance);
    try {
      const updated = await desktopApi.continueInstall(targetInstanceId);
      replaceInstance(updated);
      await refreshData();
      // Only close the dialog if installState still belongs to this instance,
      // to avoid closing another instance's .env config page when a background deployment completes
      setInstallState((current) => current?.instance?.id === targetInstanceId ? null : current);
      notify(tr("success"));
    } catch (error) {
      await refreshData();
      const message = errorMessage(error);
      let failedDockerStatus: DockerEnvironmentStatus | null = null;
      try {
        failedDockerStatus = await desktopApi.getInstanceDockerStatus(targetInstanceId);
      } catch {
        // Preserve the deployment error even if the follow-up status check fails.
      }
      setInstallState((current) => current?.instance?.id === targetInstanceId ? { ...current, step: "failed", deploymentStage: "failed", dockerStatus: failedDockerStatus ?? current.dockerStatus, dockerChecking: false, error: message } : current);
    }
  };

  const continueReadyInstance = async (instance: InstanceRecord) => {
    setDetailInstance(null);
    setInstallModalVisible(true);
    const template = templates.find((item) => item.id === instance.templateId) || {
      id: instance.templateId,
      name: instance.name,
      description: "",
      framework: instance.templateId.split("/")[0] || "agentseek",
    };
    let entries: EnvVariable[] = [];
    try {
      entries = await desktopApi.loadInstanceEnv(instance.id);
    } catch (error) {
      notify(errorMessage(error));
      return;
    }
    setInstallState({
      template,
      step: "env",
      form: { name: instance.name, templateId: instance.templateId, targetDir: instance.workDir, note: instance.note },
      instance,
      entries,
      generated: { path: instance.envPath || `${instance.workDir}/.env`, keyCount: entries.length, syncedCount: 0, portChanges: [], entries },
      warning: "",
      error: "",
      deploymentStage: "tasks",
      dockerStatus: null,
      dockerChecking: false,
    });
    setInstallModalVisible(false);
    if (!(await requestDockerConfirmation(instance, "deploy"))) {
      setInstallModalVisible(true);
      return;
    }
    setInstallModalVisible(true);
    const targetInstanceId = instance.id;
    setInstallState((current) => current ? { ...current, step: "deploying", dockerStatus: null, dockerChecking: false, instance: { ...instance, status: "installing", updatedAt: Math.floor(Date.now() / 1000) } } : current);
    replaceInstance({ ...instance, status: "installing", updatedAt: Math.floor(Date.now() / 1000) });
    try {
      const updated = await desktopApi.continueInstall(targetInstanceId);
      replaceInstance(updated);
      await refreshData();
      // Only close the dialog if installState still belongs to this instance
      setInstallState((current) => current?.instance?.id === targetInstanceId ? null : current);
      notify(tr("success"));
    } catch (error) {
      await refreshData();
      const message = errorMessage(error);
      let failedDockerStatus: DockerEnvironmentStatus | null = null;
      try {
        failedDockerStatus = await desktopApi.getInstanceDockerStatus(targetInstanceId);
      } catch {
        // Preserve the deployment error even if the follow-up status check fails.
      }
      setInstallState((current) => current?.instance?.id === targetInstanceId ? { ...current, step: "failed", deploymentStage: "failed", dockerStatus: failedDockerStatus ?? current.dockerStatus, dockerChecking: false, error: message } : current);
    }
  };

  const editEnvEntry = (entries: EnvVariable[], setter: (entries: EnvVariable[]) => void, index: number, field: "key" | "value" | "comment", value: string) => {
    setter(entries.map((entry, entryIndex) => (entryIndex === index ? { ...entry, [field]: value, modified: true } : entry)));
  };

  const saveConfigEnv = async (overwrite = false) => {
    if (!selectedConfigId) return;
    setConfigBusy(true);
    try {
      const result = await desktopApi.saveInstanceEnv(selectedConfigId, configEntries, overwrite);
      setEnvOverwrite(null);
      setConfigGenerated(result);
      setConfigEntries(result.entries);
      await refreshData();
      notify(result.portChanges.length ? `${tr("success")} · ${portChangeSummary(result)}` : tr("success"));
    } catch (error) {
      const path = envExistsPath(error);
      if (path) setEnvOverwrite({ context: "config", path });
      else notify(errorMessage(error));
    } finally {
      setConfigBusy(false);
    }
  };

  const saveVault = async () => {
    try {
      await desktopApi.saveVault(vault);
      setVault((current) => current.map((entry) => ({ ...entry, modified: false })));
      notify(tr("success"));
    } catch (error) {
      notify(errorMessage(error));
    }
  };

  const importConfiguration = async () => {
    if (!importPath.trim()) return;
    try {
      await desktopApi.importEnv(importPath.trim());
      setVault(await desktopApi.listVault());
      await refreshLatestLogs();
      setImportOpen(false);
      setImportPath("");
      notify(tr("success"));
    } catch (error) {
      notify(errorMessage(error));
    }
  };

  const openStorageSettings = async () => {
    try {
      const status = await desktopApi.storageStatus();
      setStorageConfig(status);
      setStorageSetupRequired(status.setupRequired);
      setStorageSetupFlow(status.setupRequired);
      setStorageConfigDirty(status.setupRequired);
      setShowSystemInfo(false);
    }
    catch (error) { notify(errorMessage(error)); }
  };

  const saveStorageSettings = async () => {
    if (!storageConfig) return;
    setStorageBusy(true);
    try {
      const status = await desktopApi.configureStorage(storageConfig);
      setStorageSetupRequired(status.setupRequired);
      setStorageConfigDirty(false);
      setStorageConfig(storageSetupFlow ? status : null);
      setSystemInfo(await desktopApi.systemInfo());
      notify(tr("success"));
    } catch (error) { notify(errorMessage(error)); }
    finally { setStorageBusy(false); }
  };

  const inspectExportDirectory = async (path: string) => {
    const directory = path.trim().replace(/[\\/]+$/, "");
    if (!directory) return;
    setExportScanning(true);
    try {
      const files = await desktopApi.listEnvFiles(directory);
      const defaultSource = files.find((file) => /(^|[\\/])\.env\.example$/.test(file)) || files[0] || "";
      setExportFiles(files);
      setExportSource(defaultSource);
      const separator = directory.includes("\\") && !directory.includes("/") ? "\\" : "/";
      setExportOutput(`${directory}${separator}.env`);
      if (!files.length) notify(tr("noEnvFiles"));
    } catch (error) {
      setExportFiles([]);
      setExportSource("");
      setExportOutput("");
      notify(errorMessage(error));
    } finally {
      setExportScanning(false);
    }
  };

  const openExportDialog = () => {
    setExportPath("");
    setExportFiles([]);
    setExportSource("");
    setExportOutput("");
    setExportOpen(true);
  };

  const exportConfiguration = async (overwrite = false) => {
    if (!exportSource || !exportOutput.trim()) return;
    setExportBusy(true);
    try {
      const result = await desktopApi.exportEnv(exportSource, exportOutput.trim(), overwrite);
      setEnvOverwrite(null);
      setExportOpen(false);
      setExportPath("");
      setExportFiles([]);
      setExportSource("");
      setExportOutput("");
      await refreshLatestLogs();
      notify(`${tr("exportCompleted")} ${result.path} (${result.filledCount}/${result.keyCount}, ${result.missingCount} ${tr("missing")})`);
    } catch (error) {
      const path = envExistsPath(error);
      if (path) setEnvOverwrite({ context: "export", path });
      else notify(errorMessage(error));
    } finally {
      setExportBusy(false);
    }
  };

  const toggleReveal = (key: string) => {
    setRevealed((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const keepCommentTooltip = () => {
    if (commentTooltipTimer.current !== null) {
      window.clearTimeout(commentTooltipTimer.current);
      commentTooltipTimer.current = null;
    }
  };

  const scheduleCommentTooltipClose = () => {
    keepCommentTooltip();
    commentTooltipTimer.current = window.setTimeout(() => setCommentTooltip(null), 140);
  };

  const showCommentTooltip = (element: HTMLElement, text: string, onChange: (value: string) => void) => {
    keepCommentTooltip();
    if (!text.trim()) {
      setCommentTooltip(null);
      return;
    }
    const rect = element.getBoundingClientRect();
    const width = Math.min(340, Math.max(240, rect.width), window.innerWidth - 32);
    const left = Math.min(Math.max(16, rect.left), window.innerWidth - width - 16);
    const spaceBelow = window.innerHeight - rect.bottom - 16;
    const spaceAbove = rect.top - 16;
    if (spaceBelow >= 180 || spaceBelow >= spaceAbove) {
      setCommentTooltip({ text, onChange, left, top: rect.bottom + 8, width, maxHeight: Math.max(96, spaceBelow - 8), placement: "below" });
    } else {
      setCommentTooltip({ text, onChange, left, bottom: window.innerHeight - rect.top + 8, width, maxHeight: Math.max(96, spaceAbove - 8), placement: "above" });
    }
  };

  const renderEnvTable = (entries: EnvVariable[], setter: (entries: EnvVariable[]) => void, prefix: string, allowDelete = false) => (
    <div className="env-table">
      <div className="env-head">
        <span>{tr("key")}</span><span>{tr("value")}</span><span>{tr("comment")}</span><span />
      </div>
      {entries.map((entry, index) => {
        const revealKey = `${prefix}-${index}-${entry.key}`;
        const secret = secretPattern.test(entry.key);
        return (
          <div className="env-row" key={`${prefix}-${index}`}>
            <input value={entry.key} onChange={(event) => editEnvEntry(entries, setter, index, "key", event.target.value)} spellCheck={false} />
            <div className="secret-input">
              <input
                type={secret && !revealed.has(revealKey) ? "password" : "text"}
                value={entry.value}
                onChange={(event) => editEnvEntry(entries, setter, index, "value", event.target.value)}
                spellCheck={false}
              />
              {secret && <button className="reveal-button" onClick={() => toggleReveal(revealKey)} type="button">{revealed.has(revealKey) ? "—" : "••"}</button>}
            </div>
            <textarea
              className="env-comment-input"
              value={entry.comment}
              placeholder={tr("commentUnavailable")}
              rows={1}
              wrap="off"
              onMouseEnter={(event) => {
                keepCommentTooltip();
                showCommentTooltip(event.currentTarget, entry.comment, (value) => editEnvEntry(entries, setter, index, "comment", value));
              }}
              onMouseLeave={scheduleCommentTooltipClose}
              onFocus={(event) => showCommentTooltip(event.currentTarget, entry.comment, (value) => editEnvEntry(entries, setter, index, "comment", value))}
              onBlur={scheduleCommentTooltipClose}
              onChange={(event) => {
                editEnvEntry(entries, setter, index, "comment", event.target.value);
                setCommentTooltip((current) => current ? { ...current, text: event.target.value } : current);
              }}
            />
            {allowDelete ? (
              <button className="icon-button danger-ghost" type="button" aria-label={tr("deleteVariable")} title={tr("deleteVariable")} onClick={() => setter(entries.filter((_, itemIndex) => itemIndex !== index))}><Trash2 /></button>
            ) : <span className="modified-dot">{entry.modified ? "●" : ""}</span>}
          </div>
        );
      })}
    </div>
  );

  const endpointLabel = (name: string) => {
    const normalized = name.toLowerCase();
    if (normalized.includes("gateway") || normalized === "agent") return tr("agentUrl");
    if (normalized.includes("frontend") || normalized === "app" || normalized === "web") return tr("frontendUrl");
    if (normalized.includes("copilotkit")) return tr("copilotkitUrl");
    if (normalized.includes("studio") || normalized.includes("langsmith")) return tr("studioUrl");
    return name;
  };

  const inferredEndpointKind = (name: string) => {
    const normalized = name.toLowerCase();
    if (normalized.includes("frontend") || normalized === "app" || normalized === "web") return "web";
    if (normalized.includes("gateway") || normalized === "agent") return "protocol";
    if (normalized.includes("copilotkit") || normalized.includes("langgraph")) return "api";
    if (normalized.includes("studio") || normalized.includes("langsmith") || normalized.includes("phoenix")) return "web";
    return "other";
  };
  const detailEndpoints = detailInstance
    ? detailInstance.serviceEndpoints?.length
      ? detailInstance.serviceEndpoints.map((endpoint) => ({
          ...endpoint,
          label: endpointLabel(endpoint.name),
          kind: endpoint.kind || inferredEndpointKind(endpoint.name),
          primary: endpoint.primary ?? (inferredEndpointKind(endpoint.name) === "web" && endpoint.name.toLowerCase().includes("frontend")),
        }))
      : [
          detailInstance.uiUrl ? { name: "Frontend", label: tr("frontendUrl"), url: detailInstance.uiUrl, kind: "web", primary: true } : null,
          detailInstance.agentUrl ? { name: "Agent", label: tr("agentUrl"), url: detailInstance.agentUrl, kind: "protocol", primary: false } : null,
          detailInstance.studioUrl ? { name: "Studio", label: tr("studioUrl"), url: detailInstance.studioUrl, kind: "web", primary: false } : null,
        ].filter((endpoint): endpoint is NonNullable<typeof endpoint> => endpoint !== null)
    : [];
  const primaryEndpoint = detailEndpoints.find((endpoint) => endpoint.primary && endpoint.kind === "web");
  const integrationEndpoints = detailEndpoints.filter((endpoint) => endpoint !== primaryEndpoint);
  const observationEndpoints = detailEndpoints.filter((endpoint) => {
    if (endpoint.kind !== "web") return false;
    const fingerprint = `${endpoint.name} ${endpoint.label} ${endpoint.url}`.toLowerCase();
    return /phoenix|langsmith|langfuse|arize|helicone|observability|trace|6006/.test(fingerprint);
  });
  const detailIsRunning = detailInstance?.status === "running";
  const detailIsReady = detailInstance?.status === "ready-to-install";
  const managedNodeRequired = cliStatus ? !cliStatus.nodeCompatible || !cliStatus.npmCompatible : false;
  const setupDependencies: SetupDependency[] = cliStatus ? [
    { id: "uv", name: "uv", version: cliStatus.uvVersion, minimum: cliStatus.minimumVersions.uv, ready: cliStatus.uvCompatible, scope: cliStatus.uvAvailable ? (cliStatus.uvPath.includes("/.local/bin/") ? tr("userToolDirectory") : tr("systemRuntime")) : undefined },
    { id: "node", name: "Node.js / npm", version: `${cliStatus.nodeVersion || tr("notInstalled")} / npm ${cliStatus.npmVersion || tr("notInstalled")}`, minimum: `${cliStatus.minimumVersions.node} / npm ${cliStatus.minimumVersions.npm}`, ready: cliStatus.nodeCompatible && cliStatus.npmCompatible, scope: !managedNodeRequired && cliStatus.nodeAvailable && cliStatus.npmAvailable ? (cliStatus.nodeManaged ? tr("managedRuntime") : tr("systemRuntime")) : undefined },
    { id: "agentseek", name: "AgentSeek CLI", version: cliStatus.cliVersion, minimum: cliStatus.cliLatestVersionChecked ? cliStatus.cliLatestVersion : cliStatus.minimumVersions.agentseek, requirementLabel: cliStatus.cliLatestVersionChecked ? tr("latestVersion") : tr("minimumVersion"), ready: cliStatus.cliAvailable && cliStatus.cliCompatible && !cliStatus.cliUpdateAvailable },
  ] : [];
  const dependencyIsActive = (dependency: DependencyKey) => activeDependency === dependency;
  const dependencyIsQueued = (dependency: DependencyKey) => installQueue.includes(dependency);
  const deploymentSteps = [
    { label: "agentseek create", icon: SquareTerminal },
    { label: "task backend / frontend", icon: Server },
    { label: tr("doctorGate"), icon: ShieldCheck },
    { label: "dry-run / dev", icon: Server },
  ];
  const deploymentStageIndex: Record<string, number> = { create: 0, pending: 1, tasks: 1, doctor: 2, "dry-run": 3, starting: 3, complete: 4 };
  const dockerProviderLabel = (provider: string) => provider === "orbstack" ? "OrbStack" : provider === "docker-desktop" ? "Docker Desktop" : provider === "rancher-desktop" ? "Rancher Desktop" : provider === "colima" ? "Colima" : "Docker Engine";
  const installScopeItems = cliStatus ? [
    (runtimeInstallPlan ? runtimeInstallPlan.dependencies.includes("uv") : !cliStatus.uvCompatible) ? { name: "uv", description: tr("uvInstallScope") } : null,
    (runtimeInstallPlan ? runtimeInstallPlan.dependencies.includes("node/npm") : !cliStatus.nodeCompatible || !cliStatus.npmCompatible) ? { name: "Node.js / npm", description: tr("nodeInstallScope") } : null,
    (runtimeInstallPlan ? runtimeInstallPlan.dependencies.includes("agentseek") : !cliStatus.cliCompatible) ? { name: "AgentSeek CLI", description: tr("cliInstallScope") } : null,
  ].filter((item): item is { name: string; description: string } => item !== null) : [];
  const closeRuntimeInstallConfirm = () => {
    setInstallConfirmOpen(false);
    setRuntimeInstallPlan(null);
    setCliUpgradeRequested(false);
  };
  const runtimeInstallConfirmDialog = installConfirmOpen && (
    <div className="modal-backdrop">
      <div className="modal dependency-confirm-modal" role="alertdialog" aria-modal="true">
        <div className="modal-head"><div><span className="eyebrow">ENVIRONMENT</span><h2>{tr(cliUpgradeRequested ? "confirmCliUpgradeTitle" : "confirmDependencyTitle")}</h2><p>{tr(cliUpgradeRequested ? "confirmCliUpgradeHint" : "confirmDependencyHint")}</p></div><button className="icon-button" onClick={closeRuntimeInstallConfirm} type="button"><X /></button></div>
        <div className="modal-body dependency-summary"><ShieldCheck /><div><strong>{tr("installSummaryTitle")}</strong><div className="dependency-scope-list">{installScopeItems.map((item) => <div key={item.name}><code>{item.name}</code><span>{item.description}</span></div>)}</div></div></div>
        {runtimeInstallPlan && <div className="runtime-install-preview"><div><span>{tr("installDirectory")}</span><code>{runtimeInstallPlan.installDir}</code></div><div><span>{tr("installScript")}</span><code>{runtimeInstallPlan.scriptPath}</code></div><pre>{runtimeInstallPlan.script}</pre><p>{tr("terminalInstallHint")}</p></div>}
        <div className="modal-foot"><button className="button secondary" onClick={closeRuntimeInstallConfirm} type="button">{tr("cancel")}</button><button className="button primary" onClick={installRequiredDependencies} type="button" disabled={!runtimeInstallPlan}><SquareTerminal />{tr("confirmInstall")}</button></div>
      </div>
    </div>
  );

  if (loading) {
    return <div className="loading-screen"><div className="app-mark compact"><SquareTerminal /></div><LoaderCircle className="spin" /><span>{tr("loading")}</span></div>;
  }

  if (isNativeDesktop && cliStatus && (storageSetupFlow || !cliStatus.prerequisitesReady)) {
    return (
      <div className="cli-setup-shell">
        <header className="cli-setup-header">
          <div className="brand"><div className="app-mark"><Orbit /></div><div><strong>AgentSeek Desktop</strong><span>Template Runtime</span></div></div>
          <div className="top-actions">
            <button className="language-button" type="button" onClick={() => setLanguage(language === "zh" ? "en" : "zh")}><Languages /><span>{language === "zh" ? tr("languageChinese") : tr("languageEnglish")}</span></button>
            <button className="icon-button" type="button" onClick={() => setTheme(theme === "light" ? "dark" : "light")} aria-label={tr("theme")}>{theme === "light" ? <Moon /> : <Sun />}</button>
          </div>
        </header>
        <main className="cli-setup-main">
          <section className="cli-setup-panel">
            <div className="cli-setup-icon"><SquareTerminal /></div>
            <div className="eyebrow">FIRST RUN SETUP</div>
            <h1>{tr("cliSetupTitle")}</h1>
            <p>{tr("cliSetupDesc")}</p>
            {storageConfig && (
              <div className="first-run-storage">
                <div className="first-run-storage-heading"><Database /><strong>{tr("storageSetupTitle")}</strong></div>
                <div className="first-run-storage-content">
                  <label>
                    <span>{tr("storageType")}</span>
                    <div className="select-field"><select value={storageConfig.mode === "oceanbase_server" ? "seekdb_server" : storageConfig.mode} onChange={(event) => {
                      const mode = event.target.value as StorageStatus["mode"];
                      setStorageConfig({ ...storageConfig, mode, path: storagePathForMode(storageConfig, mode), database: storageDatabaseForMode(storageConfig, mode) });
                      setStorageConfigDirty(true);
                    }}>
                      <option value="sqlite_embedded">{tr("embeddedSqlite")}</option>
                      {cliStatus?.platform !== "windows" && <option value="seekdb_embedded">{tr("embeddedSeekdb")}</option>}
                      <option value="seekdb_server">{tr("seekdbServer")}</option>
                    </select><ChevronDown /></div>
                  </label>
                  {storageConfig.mode === "sqlite_embedded" || storageConfig.mode === "seekdb_embedded" ? (
                    <div className="first-run-storage-fields embedded">
                      <label><span>{tr("dataDirectory")}</span><input title={storageConfig.path} value={storageConfig.path} onChange={(event) => { setStorageConfig({ ...storageConfig, path: event.target.value }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("database")}</span><input value={storageConfig.database} disabled /></label>
                    </div>
                  ) : (
                    <div className="first-run-storage-fields server">
                      <label><span>{tr("host")}</span><input value={storageConfig.host} onChange={(event) => { setStorageConfig({ ...storageConfig, host: event.target.value }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("port")}</span><input type="number" value={storageConfig.port} onChange={(event) => { setStorageConfig({ ...storageConfig, port: Number(event.target.value) }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("tenant")}</span><input value={storageConfig.tenant} onChange={(event) => { setStorageConfig({ ...storageConfig, tenant: event.target.value }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("database")}</span><input value={storageConfig.database} onChange={(event) => { setStorageConfig({ ...storageConfig, database: event.target.value }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("user")}</span><input value={storageConfig.user} onChange={(event) => { setStorageConfig({ ...storageConfig, user: event.target.value }); setStorageConfigDirty(true); }} /></label>
                      <label><span>{tr("password")}</span><input type="password" placeholder={storageConfig.passwordConfigured ? tr("passwordKeepHint") : ""} onChange={(event) => { setStorageConfig({ ...storageConfig, password: event.target.value }); setStorageConfigDirty(true); }} /></label>
                    </div>
                  )}
                  <div className="first-run-storage-actions">
                    {storageConfig.mode === "seekdb_embedded" && !cliStatus.uvCompatible && <small>{tr("storageRequiresUv")}</small>}
                    {!storageSetupRequired && !storageConfigDirty && storageConfig.writable && storageConfig.effectiveMode === storageConfig.mode && <span className="storage-connection-success"><Check />{tr("storageConnected")}</span>}
                    <button className="button primary" disabled={storageBusy || !storageConfigDirty || cliInstalling || cliChecking || (storageConfig.mode === "seekdb_embedded" && !cliStatus.uvCompatible)} onClick={() => void saveStorageSettings()} type="button">
                      {storageBusy ? <LoaderCircle className="spin" /> : <Database />}
                      {storageBusy ? tr("storageInitializing") : tr("storageInitializeAction")}
                    </button>
                  </div>
                </div>
              </div>
            )}
            <div className="dependency-panel">
              <div className="dependency-title"><ShieldCheck /><strong>{tr("dependencyCheck")}</strong></div>
              {setupDependencies.map((dependency) => { const active = dependencyIsActive(dependency.id); const queued = dependencyIsQueued(dependency.id); const failed = failedDependency === dependency.id; return <div className="dependency-row" key={dependency.id}><span><code>{dependency.name}</code><small>{dependency.version || tr("notInstalled")} · {dependency.requirementLabel || tr("minimumVersion")} {dependency.minimum}{dependency.scope ? ` · ${dependency.scope}` : ""}</small></span><strong className={active ? "installing" : failed ? "failed" : dependency.ready ? "available" : queued ? "pending" : "missing"}>{active ? <><LoaderCircle className="spin" />{tr("installingDependency")}</> : failed ? <><CircleAlert />{tr("installFailed")}</> : dependency.ready ? <><Check />{tr("checkPassed")}</> : queued ? tr("pendingInstall") : dependency.version ? tr("updateRequired") : tr("notInstalled")}</strong></div>; })}
              <div className="dependency-panel-footer">
                {cliInstallOutput && !cliInstalling && <pre className="cli-install-output">{cliInstallOutput}</pre>}
                <div className="cli-setup-actions">
                  <button className="button secondary" type="button" onClick={recheckCli} disabled={cliInstalling || cliChecking}><RefreshCw className={cliChecking ? "spin" : ""} />{cliChecking ? tr("checking") : tr("recheck")}</button>
                  <button className="button primary" type="button" onClick={() => void openRuntimeInstallConfirm(false)} disabled={cliInstalling || cliChecking || installPlanLoading || cliStatus.prerequisitesReady}>{cliInstalling || installPlanLoading ? <LoaderCircle className="spin" /> : cliStatus.prerequisitesReady ? <Check /> : <SquareTerminal />}{cliInstalling ? tr("processingDependency") : installPlanLoading ? tr("preparingInstallPlan") : cliStatus.prerequisitesReady ? tr("checkPassed") : tr("installAll")}</button>
                </div>
                <small className="cli-setup-hint">{tr("prerequisitesHint")}</small>
              </div>
            </div>
          </section>
        </main>
        {runtimeInstallConfirmDialog}
        {toast && <div className="toast"><Check />{toast}</div>}
      </div>
    );
  }

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="app-mark"><Orbit /></div>
          <div><strong>AgentSeek Desktop</strong><span>Template Runtime</span></div>
        </div>

        <nav className="nav-list" aria-label="Primary">
          {([
            ["instances", Boxes, tr("instances"), instances.length],
            ["templates", LayoutTemplate, tr("templates"), templates.length],
            ["config", KeyRound, tr("config"), ".env"],
            ["logs", FileText, tr("logs"), lifecycleCount],
          ] as const).map(([target, Icon, label, count]) => {
            const locked = !!installState && installState.step === "env" && !installState.generated;
            return (
            <button key={target} className={page === target ? "active" : ""} onClick={() => { if (!locked) setPage(target); }} disabled={locked} type="button" title={locked ? tr("configureEnvFirst") : undefined}>
              <Icon /><span>{label}</span><small>{count}</small>
            </button>
          )})}
        </nav>

      </aside>

      <main className="workspace">
        <header className="topbar">
          <div className="page-heading"><h1>{pageCopy[0]}</h1><p>{pageCopy[1]}</p></div>
          <div className="top-actions">
            <button className="language-button" type="button" onClick={() => setLanguage(language === "zh" ? "en" : "zh")} aria-label={tr("language")} title={tr("language")}><span>{language === "zh" ? tr("languageChinese") : tr("languageEnglish")}</span><ChevronDown /></button>
            <button className="icon-button" type="button" onClick={() => setTheme(theme === "light" ? "dark" : "light")} aria-label={tr("theme")} title={tr("theme")}>{theme === "light" ? <Moon /> : <Sun />}</button>
            <button className="icon-button" type="button" onClick={() => setShowSystemInfo(true)} aria-label={tr("systemInfo")} title={tr("systemInfo")}><Info /></button>
          </div>
        </header>

        <section className="content-area">
          {page === "templates" && (
            <div className="command-bar">
              <label className="search-box"><Search /><input value={search} onChange={(event) => setSearch(event.target.value)} placeholder={page === "templates" ? tr("searchTemplates") : tr("search")} /></label>
              <div className="command-actions">
                {cliStatus?.cliUpdateAvailable && <div className="cli-update-notice"><span><strong>{tr("cliUpdateAvailable")}</strong><small>{cliStatus.cliVersion} → v{cliStatus.cliLatestVersion}</small></span><button className="button" type="button" onClick={() => void openRuntimeInstallConfirm(true)} disabled={cliInstalling || installPlanLoading}>{cliUpgradeRequested && (cliInstalling || installPlanLoading) ? <LoaderCircle className="spin" /> : <SquareTerminal />}{cliInstalling && cliUpgradeRequested ? tr("upgradingCli") : tr("upgradeCli")}</button></div>}
                {page === "templates" && <><button className="button secondary" type="button" onClick={() => void checkAndPromptTemplateUpdate()} disabled={refreshingTemplates || templateUpdating}><RefreshCw className={(refreshingTemplates || templateUpdating) ? "spin" : ""} />{templateUpdating ? tr("templateUpdating") : tr("refresh")}</button><button className="icon-button" type="button" onClick={() => void openTemplateUrlEdit()} title={tr("editTemplateSettings")} aria-label={tr("editTemplateSettings")}><Pencil /></button></>}
              </div>
            </div>
          )}

          {page === "instances" && (
            <div className="surface instance-surface">
              <div className="section-heading instance-section-heading"><div><h2>{tr("instances")}</h2><p>{tr("instancesDesc")}</p></div><button className="button secondary" type="button" onClick={() => setPage("templates")}><Plus />{tr("createInstance")}</button></div>
              {instanceActionError && <div className="instance-action-warning"><CircleAlert /><span>{instanceActionError}</span></div>}
              <div className="table-head instance-grid"><span>{tr("instance")}</span><span>{tr("status")}</span><span>{tr("workDir")}</span><span>{tr("note")}</span><span>{tr("actions")}</span></div>
              {filteredInstances.length ? paginatedInstances.map((instance) => (
                <div className="table-row instance-grid" key={instance.id}>
                  <button className="instance-name" type="button" onClick={() => setDetailInstance(instance)}><span><strong>{instance.name}</strong><small>{instance.templateId}</small></span></button>
                  <span><span className={`status ${statusTone(instance.status)}`}><i />{statusLabel(instance.status)}</span></span>
                  <span className="path-cell" title={instance.workDir}>{instance.workDir}</span>
                  <span className="note-cell">{instance.note || "—"}</span>
                  <div className="action-menu">
                    <button className="action-menu-trigger" aria-label={tr("actions")} aria-haspopup="menu" aria-expanded={openActionMenuId === instance.id} onClick={() => setOpenActionMenuId((current) => current === instance.id ? null : instance.id)} disabled={instanceAction?.id === instance.id} type="button"><SlidersHorizontal /><span>{tr("actions")}</span></button>
                    {openActionMenuId === instance.id && <div className="menu-popover" role="menu">
                      {instance.status === "ready-to-install" ? <button type="button" role="menuitem" onClick={() => { setOpenActionMenuId(null); void continueReadyInstance(instance); }}><SquareTerminal />{tr("continueDeploy")}</button> : instance.status === "configuring" ? <button type="button" role="menuitem" onClick={() => { setSelectedConfigId(instance.id); setConfigTab("instance"); setPage("config"); setOpenActionMenuId(null); }}><FileKey />{tr("editConfig")}</button> : <><button type="button" role="menuitem" onClick={() => runInstanceAction("restart", instance)} disabled={instanceAction?.id === instance.id}><RotateCw />{tr("restart")}</button><button type="button" role="menuitem" onClick={() => runInstanceAction("stop", instance)} disabled={instance.status === "stopped" || instanceAction?.id === instance.id}><CircleStop />{tr("stop")}</button></>}
                      <button className="danger" type="button" role="menuitem" onClick={() => runInstanceAction("delete", instance)} disabled={instanceAction?.id === instance.id}><Trash2 />{tr("delete")}</button>
                    </div>}
                  </div>
                </div>
              )) : <div className="empty-state"><Boxes /><strong>{tr("noInstances")}</strong><span>{tr("noInstancesHint")}</span><button className="button primary" type="button" onClick={() => setPage("templates")}><Plus />{tr("createInstance")}</button></div>}
              <PaginationControls language={language} page={currentInstancePage} total={filteredInstances.length} pageSize={INSTANCES_PER_PAGE} onPageChange={setInstancePage} className="instance-pagination" />
            </div>
          )}

          {page === "templates" && (
            <div className="template-layout">
              <div className="template-list surface">
                <div className="section-heading"><div><h2>{tr("templates")}</h2><p><SquareTerminal />agentseek create --list-templates{templateVersionDisplay?.currentVersion && <span className="template-version-tag">{tr("templateVersion")}: {templateVersionDisplay.currentVersion}{templateVersionDisplay.hasUpdate && <span className="template-update-badge">{tr("templateUpdateAvailable")}</span>}</span>}</p></div><span className="count-label">{filteredTemplates.length}</span></div>
                <div className="template-tabs" role="tablist" aria-label={tr("template")}>
                  <button type="button" role="tab" aria-selected={templateTab === "all"} className={templateTab === "all" ? "active" : ""} onClick={() => setTemplateTab("all")}>{tr("templateAll")}<small>{templates.length}</small></button>
                  {templateFrameworks.map((framework) => (
                    <button key={framework} type="button" role="tab" aria-selected={templateTab === framework} className={templateTab === framework ? "active" : ""} onClick={() => setTemplateTab(framework)}>{framework}<small>{templates.filter((template) => template.framework === framework).length}</small></button>
                  ))}
                </div>
                {filteredTemplates.length ? filteredTemplates.map((template) => (
                  <div className="template-row" key={template.id}>
                    <div className={`framework-mark ${template.framework}`}>{template.framework.slice(0, 2).toUpperCase()}</div>
                    <div className="template-copy"><strong>{template.name}</strong><code>{template.id}</code><p>{template.description}</p></div>
                    <span className="framework-label">{template.framework}</span>
                    <button className="button secondary" type="button" onClick={() => openInstall(template)}>{tr("install")}</button>
                  </div>
                )) : <div className="empty-state"><LayoutTemplate /><strong>{tr("noTemplates")}</strong></div>}
              </div>
              <aside className="process-panel">
                <div className="section-heading"><div><h2>{tr("installSteps")}</h2></div></div>
                {(["processCreate", "processReadEnv", "processEnv", "processTasks", "processDoctor", "processDev"] as TranslationKey[]).map((key, index) => <div className="process-step" key={key}><span>{index + 1}</span><code>{tr(key)}</code></div>)}
              </aside>
            </div>
          )}

          {page === "config" && (
            <div className="config-page">
              <div className="segmented"><button className={configTab === "vault" ? "active" : ""} onClick={() => setConfigTab("vault")} type="button"><Database />{tr("vault")}</button><button className={configTab === "instance" ? "active" : ""} onClick={() => setConfigTab("instance")} type="button"><FileKey />{tr("instanceEnv")}</button></div>

              {configTab === "vault" ? (
                <div className="config-workspace surface with-footer">
                  <div className="config-toolbar"><div><h2>{tr("vault")}</h2><p>{tr("vaultDesc")}</p></div><div><button className="button secondary" onClick={openExportDialog} type="button"><FileDown />{tr("exportEnv")}</button><button className="button secondary" onClick={() => setImportOpen(true)} type="button"><FileUp />{tr("importConfig")}</button><button className="button secondary" onClick={() => setVault((current) => [...current, { key: "", value: "", comment: "", source: "instance", modified: true }])} type="button"><Plus />{tr("addVariable")}</button></div></div>
                  <div className="stats-row"><div><span>{tr("all")}</span><strong>{vault.length}</strong></div><div><span>{tr("fromVault")}</span><strong>{vault.filter((entry) => entry.source !== "template").length}</strong></div><div><span>{tr("missing")}</span><strong>{vault.filter((entry) => !entry.value).length}</strong></div></div>
                  <div className="env-scroll">{renderEnvTable(vault, setVault, "vault", true)}</div>
                  <div className="sticky-actions"><span>{vault.filter((entry) => entry.modified).length} {tr("modified")}</span><button className="button primary" onClick={saveVault} type="button"><Check />{tr("saveVault")}</button></div>
                </div>
              ) : instances.length === 0 ? (
                <div className="config-workspace surface">
                  <div className="empty-state"><Boxes /><strong>{tr("noInstances")}</strong><span>{tr("noInstancesHint")}</span></div>
                </div>
              ) : (
                <div className="config-workspace surface with-footer">
                  <div className="config-toolbar"><div><h2>{tr("instanceEnv")}</h2><p>{instances.find((instance) => instance.id === selectedConfigId)?.envPath || instances.find((instance) => instance.id === selectedConfigId)?.envExamplePath || "—"}</p></div><div><button className="button secondary" onClick={() => setConfigEntries((current) => [...current, { key: "", value: "", comment: "", source: "instance", modified: true }])} type="button"><Plus />{tr("addVariable")}</button><label className="select-field"><span>{tr("selectInstance")}</span><select value={selectedConfigId} onChange={(event) => setSelectedConfigId(event.target.value)}>{instances.map((instance) => <option key={instance.id} value={instance.id}>{instance.name} · {formatTime(instance.createdAt, language)}</option>)}</select><ChevronDown /></label></div></div>
                  <div className="stats-row"><div><span>{tr("all")}</span><strong>{configEntries.length}</strong></div><div><span>{tr("fromVault")}</span><strong>{configEntries.filter((entry) => entry.source === "vault").length}</strong></div><div><span>{tr("templateDefault")}</span><strong>{configEntries.filter((entry) => entry.source === "template").length}</strong></div><div><span>{tr("missing")}</span><strong>{configEntries.filter((entry) => !entry.value).length}</strong></div></div>
                  <div className="env-scroll">{configBusy ? <div className="inline-loading"><LoaderCircle className="spin" />{tr("loading")}</div> : renderEnvTable(configEntries, setConfigEntries, "config", true)}</div>
                  <div className="sticky-actions"><span>{configGenerated ? `${tr("generated")} ${configGenerated.path} (${configGenerated.keyCount} ${tr("keys")})${configGenerated.portChanges.length ? ` · ${portChangeSummary(configGenerated)}` : ""}` : tr("envHint")}</span><button className="button primary" onClick={() => saveConfigEnv()} disabled={!selectedConfigId || configBusy} type="button"><FileKey />{tr("generateEnv")}</button></div>
                </div>
              )}
            </div>
          )}

          {page === "logs" && (
            <div className="logs-page surface">
              <div className="log-toolbar">
                <div className="segmented compact">
                  {(["all", "lifecycle", "runtime"] as const).map((category) => <button className={logCategory === category ? "active" : ""} onClick={() => setLogCategory(category)} type="button" key={category}>{category === "all" ? tr("all") : tr(category === "lifecycle" ? "lifecycleLog" : "runtimeLog")}</button>)}
                </div>
                <div className="log-toolbar-actions">
                  {logCategory === "runtime" && <label className="retention-control"><span>{tr("runtimeRetention")}</span><input type="number" min="1" max="3650" value={runtimeRetentionDays} onChange={(event) => setRuntimeRetentionDays(Number(event.target.value))} /><span>{tr("days")}</span><button className="button secondary" disabled={logSettingsBusy || runtimeRetentionDays < 1 || runtimeRetentionDays > 3650} onClick={() => void saveRuntimeLogRetention()} type="button">{logSettingsBusy ? <LoaderCircle className="spin" /> : <Check />}{tr("save")}</button></label>}
                  <label className="select-field compact-select"><select value={logInstance} onChange={(event) => setLogInstance(event.target.value)}><option value="all">{tr("all")}</option>{instances.map((instance) => <option value={instance.id} key={instance.id}>{instance.name}</option>)}</select><ChevronDown /></label>
                </div>
              </div>
              <div className={`log-head lifecycle-grid ${logCategory !== "all" ? "without-type" : ""}`}><span>{tr("time")}</span><span>{tr("instance")}</span>{logCategory === "all" && <span>{tr("lifecycle")}</span>}<span>{tr("status")}</span><span>{logCategory === "runtime" ? tr("latestLog") : tr("latestEvent")}</span><span /></div>
              <div className="log-list lifecycle-list">
                {paginatedLogGroups.map((group) => {
                  const expanded = expandedLogGroups.has(group.id);
                  const lifecycleLabel = group.deleted
                    ? tr("deleted")
                    : group.instanceStatus
                      ? statusLabel(group.instanceStatus)
                      : group.failed
                        ? tr("lifecycleFailed")
                        : tr("lifecycleSuccess");
                  const lifecycleTone = group.deleted
                    ? "neutral"
                    : group.instanceStatus
                      ? statusTone(group.instanceStatus)
                      : group.failed
                        ? "error"
                        : "success";
                  return <section className={`lifecycle-group ${expanded ? "expanded" : ""}`} key={group.id}>
                    <button className={`lifecycle-summary lifecycle-grid ${logCategory !== "all" ? "without-type" : ""}`} type="button" onClick={() => setExpandedLogGroups((current) => { const next = new Set(current); if (next.has(group.id)) next.delete(group.id); else next.add(group.id); return next; })}>
                      <time>{formatTime(group.startedAt, language)}</time>
                      <span className="lifecycle-instance"><strong>{group.instanceName}</strong><small>{group.entries.length} {group.entries.every((entry) => entry.category === "runtime") ? tr("logEntries") : group.entries.every((entry) => entry.category === "lifecycle") ? tr("lifecycleSteps") : tr("logRecords")}</small></span>
                      {logCategory === "all" && <span className="lifecycle-categories">{group.categories.map((category) => <i key={category}>{tr(category === "runtime" ? "runtimeLog" : "lifecycleLog")}</i>)}</span>}
                      <span className={`log-level ${lifecycleTone}`}>{lifecycleLabel}</span>
                      <span className="lifecycle-latest">{cleanLogText(group.entries[0]?.message || "")}</span>
                      <span className="lifecycle-toggle" aria-label={expanded ? tr("collapseDetails") : tr("expandDetails")}><ChevronDown /></span>
                    </button>
                    {expanded && <LogTerminal instanceName={group.instanceName} entries={group.entries} language={language} liveLabel={tr("live")} />}
                  </section>;
                })}
                {!logGroups.length && <div className="empty-state small"><FileText /><span>{tr("noLogs")}</span></div>}
              </div>
              <PaginationControls language={language} page={currentLogPage} total={logGroups.length} pageSize={LOGS_PER_PAGE} onPageChange={setLogPage} />
            </div>
          )}

          {page === "traces" && (
            <div className="traces-page surface">
              {!traceDetailView ? (
                <>
              <div className="log-toolbar">
                <h2>{tr("traces")}</h2>
              </div>
              {!traceInstanceId ? <div className="empty-state"><Orbit /><strong>{tr("openTraces")}</strong><span>{tr("selectTraceHint")}</span></div> : traceLoading ? <div className="inline-loading"><LoaderCircle className="spin" />{tr("loading")}</div> : traceSummaries.length === 0 ? <div className="empty-state small"><CircleAlert /><span>{tr("noTraces")}</span><span>{tr("noTracesHint")}</span></div> : <><div className="trace-list-page"><div className="trace-table-head"><span>{tr("traceId")}</span><span>{tr("traceStatus")}</span><span>{tr("kind")}</span><span>{tr("traceTabInput")}</span><span>{tr("traceTabOutput")}</span><span>{tr("startTime")}</span><span>{tr("traceLatency")}</span></div>{traceSummaries.map((t) => { const tone = t.status === "ERROR" ? "error" : "success"; const latency = t.latencyMs != null ? t.latencyMs >= 1000 ? `${(t.latencyMs / 1000).toFixed(1)}s` : `${t.latencyMs}ms` : "—"; return (<button className="trace-table-row" key={t.traceId} type="button" onClick={() => { setTraceLoading(true); const inst = instances.find((i) => i.id === traceInstanceId); if (inst) { desktopApi.getAtofTraceDetail(inst.workDir, t.traceId).then((d) => { if (d) { setTraceDetailView(d); setSelectedSpanId(null); return; } const pUrl = phoenixUrlFor(inst); if (pUrl) { return desktopApi.queryPhoenixTraceDetail(pUrl, t.traceId).then((pd) => { setTraceDetailView(pd); setSelectedSpanId(null); }); } }).catch(() => notify(tr("traceDetailLoadFailed"))).finally(() => setTraceLoading(false)); } }}><span className="trace-id-cell" title={t.traceId}>{t.traceId.slice(0, 12)}</span><span className={`status ${tone}`}><i />{t.status}</span><span className="trace-kind-badge">{t.kind}</span><span className="trace-io-cell" title={t.inputSummary ?? ""}>{t.inputSummary ?? "—"}</span><span className="trace-io-cell" title={t.outputSummary ?? ""}>{t.outputSummary ?? "—"}</span><span>{t.startTime ? String(t.startTime).slice(0, 19) : "—"}</span><span>{latency}</span></button>); })}</div><PaginationControls language={language} page={tracePage} total={traceTotal} pageSize={TRACES_PER_PAGE} onPageChange={setTracePage} className="trace-pagination" /></>}
                </>
              ) : (
                <TraceDetailPanel detail={traceDetailView!} onBack={() => { setTraceDetailView(null); setSelectedSpanId(null); }} selectedSpanId={selectedSpanId} onSelectSpan={setSelectedSpanId} tr={tr} language={language} />
              )}
            </div>
          )}
        </section>
      </main>

      {runtimeInstallConfirmDialog}

      {dockerConfirmation && (
        <div className="modal-backdrop docker-confirm-backdrop">
          <div className="modal docker-confirm-modal" role="dialog" aria-modal="true">
            <div className="modal-head"><div><span className="eyebrow">DOCKER CHECK</span><h2>{tr("dockerConfirmTitle")}</h2><p>{dockerConfirmation.instance.name} · {tr(`dockerAction_${dockerConfirmation.action}` as TranslationKey)}</p></div><button className="icon-button" onClick={() => closeDockerConfirmation(false)} type="button"><X /></button></div>
            <div className="modal-body docker-confirm-body">
              {dockerConfirmation.checking && !dockerConfirmation.status ? <div className="docker-confirm-loading"><LoaderCircle className="spin" /><strong>{tr("dockerChecking")}</strong><span>{tr("dockerCheckingHint")}</span></div> : dockerConfirmation.status && <>
                <div className={`docker-confirm-hero ${dockerConfirmation.status.ready ? "ready" : "attention"}`}><span>{dockerConfirmation.status.ready ? <ShieldCheck /> : <CircleAlert />}</span><div><strong>{dockerConfirmation.status.ready ? tr("dockerEnvironmentReady") : tr("dockerEnvironmentUnavailable")}</strong><p>{dockerProviderLabel(dockerConfirmation.status.provider)} · {dockerConfirmation.status.platform}</p></div></div>
                <div className="docker-confirm-checks">
                  {[
                    ["Docker", dockerConfirmation.status.dockerInstalled],
                    [`Docker daemon (${dockerProviderLabel(dockerConfirmation.status.provider)})`, dockerConfirmation.status.daemonRunning],
                    ["Docker Compose v2", dockerConfirmation.status.composeV2Installed],
                  ].map(([label, ready]) => <div className={ready ? "ready" : "failed"} key={String(label)}>{ready ? <Check /> : <CircleAlert />}<span>{label}</span><strong>{ready ? tr("dockerReady") : tr("dockerUnavailable")}</strong></div>)}
                </div>
                {!dockerConfirmation.status.dockerInstalled && <section className="docker-confirm-action"><strong>{tr("dockerNotInstalled")}</strong><p>{tr("dockerInstallHint")}</p>{dockerConfirmation.status.installScript && <div className="docker-command"><code>{dockerConfirmation.status.installScript}</code><button className="icon-button" type="button" onClick={() => { void navigator.clipboard.writeText(dockerConfirmation.status!.installScript!); notify(tr("copied")); }}><Copy /></button></div>}<button className="button primary" type="button" onClick={() => void desktopApi.openExternalUrl(dockerConfirmation.status!.installUrl)}><ExternalLink />{tr("installDocker")}</button></section>}
                {dockerConfirmation.status.dockerInstalled && !dockerConfirmation.status.daemonRunning && <section className="docker-confirm-action"><strong>{tr("dockerNotRunning")}</strong><p>{dockerConfirmation.status.guidance}</p>{dockerConfirmation.status.startSupported && <button className="button primary" disabled={dockerConfirmation.starting || dockerConfirmation.checking} type="button" onClick={() => void startDockerFromConfirmation()}>{dockerConfirmation.starting || dockerConfirmation.checking ? <LoaderCircle className="spin" /> : <SquareTerminal />}{dockerConfirmation.starting || dockerConfirmation.checking ? tr("dockerStarting") : tr("startDocker")}</button>}</section>}
                {dockerConfirmation.status.dockerInstalled && !dockerConfirmation.status.composeV2Installed && <section className="docker-confirm-action"><strong>{tr("composeV2Missing")}</strong><p>{tr("composeV2InstallHint")}</p>{dockerConfirmation.status.composeInstallCommand && <div className="docker-command"><code>{dockerConfirmation.status.composeInstallCommand}</code><button className="icon-button" type="button" onClick={() => { void navigator.clipboard.writeText(dockerConfirmation.status!.composeInstallCommand!); notify(tr("copied")); }}><Copy /></button></div>}</section>}
                {dockerConfirmation.status.composeFile && <code className="docker-confirm-compose-file">{dockerConfirmation.status.composeFile}</code>}
              </>}
              {dockerConfirmation.error && <div className="docker-confirm-error"><CircleAlert /><div><strong>{tr("dockerStartFailed")}</strong><p>{dockerConfirmation.error}</p></div></div>}
            </div>
            <div className="modal-foot"><button className="button secondary" type="button" onClick={() => closeDockerConfirmation(false)}>{tr("cancel")}</button>{dockerConfirmation.status && !dockerConfirmation.status.ready && <button className="button secondary" disabled={dockerConfirmation.checking || dockerConfirmation.starting} type="button" onClick={() => void recheckDockerConfirmation()}><RefreshCw className={dockerConfirmation.checking ? "spin" : ""} />{tr("recheck")}</button>}{dockerConfirmation.status?.ready && <button className="button primary" type="button" onClick={() => closeDockerConfirmation(true)}><Check />{tr("confirmContinue")}</button>}</div>
          </div>
        </div>
      )}

      {installState && installModalVisible && (
        <div className="modal-backdrop">
          <div className={`modal install-modal ${installState.step === "env" ? "wide" : ""}`} role="dialog" aria-modal="true">
            <div className="modal-head"><div><span className="eyebrow">{installState.template.id}</span><h2>{installState.step === "env" ? tr("envTitle") : installState.step === "failed" ? tr("deploymentFailed") : tr("createInstance")}</h2></div><button className="icon-button" onClick={() => { if (installState.step === "deploying") { setInstallModalVisible(false); } else if (installState.step !== "env" || installState.generated) { setInstallState(null); } }} disabled={installState.step === "env" && !installState.generated} aria-label={installState.step === "deploying" ? tr("hideTask") : installState.step === "env" && !installState.generated ? tr("configureEnvFirst") : tr("close")} title={installState.step === "deploying" ? tr("hideTask") : installState.step === "env" && !installState.generated ? tr("configureEnvFirst") : tr("close")} type="button"><X /></button></div>
            {installState.step === "form" && <div className="modal-body form-grid">
              <label><span>{tr("instanceName")}</span><input autoFocus value={installState.form.name} onChange={(event) => setInstallState({ ...installState, form: { ...installState.form, name: event.target.value }, error: "" })} placeholder="rag-development" /></label>
              <label className="full"><span>{tr("targetDir")}</span><div className="path-picker"><input value={installState.form.targetDir} onChange={(event) => setInstallState({ ...installState, form: { ...installState.form, targetDir: event.target.value }, error: "" })} placeholder="/Users/name/AgentSeek/instances" /><button type="button" onClick={async () => { const path = await desktopApi.chooseDirectory(); if (path) setInstallState({ ...installState, form: { ...installState.form, targetDir: path }, error: "" }); }}><FolderOpen />{tr("choose")}</button></div>{installState.form.targetDir.trim() && installState.form.name.trim() && <code className="target-path-preview">{`${installState.form.targetDir.replace(/[\\/]+$/, "")}/${installState.form.name.trim()}`}</code>}</label>
              <label className="full"><span>{tr("note")}</span><textarea value={installState.form.note} onChange={(event) => setInstallState({ ...installState, form: { ...installState.form, note: event.target.value } })} rows={3} /></label>
              {!isNativeDesktop && <div className="native-required full"><CircleAlert /><span><strong>{tr("nativeRequired")}</strong><code>{tr("nativeCommand")}</code></span></div>}
            </div>}
            {installState.step === "env" && <div className="modal-body env-modal-body"><div className="env-context"><ShieldCheck /><div><strong>{tr("envTitle")}</strong><p>{tr("envHint")}</p><code>{installState.instance?.envExamplePath}</code></div></div>{installState.warning && <div className="modal-warning"><CircleAlert />{installState.warning}</div>}<div className="env-toolbar"><button className="button secondary" onClick={() => setInstallState({ ...installState, entries: [...installState.entries, { key: "", value: "", comment: "", source: "instance", modified: true }], generated: null })} type="button"><Plus />{tr("addVariable")}</button></div><div className="env-scroll modal-env-scroll">{renderEnvTable(installState.entries, (entries) => setInstallState({ ...installState, entries, generated: null }), "install", true)}</div>{installState.generated && <div className="generated-banner"><Check /><span>{tr("generated")} {installState.generated.path} ({installState.generated.keyCount} {tr("keys")}, {installState.generated.syncedCount} → {tr("vault")}){installState.generated.portChanges.length ? ` · ${portChangeSummary(installState.generated)}` : ""}</span></div>}</div>}
            {installState.step === "deploying" && <div className="modal-body progress-body"><div className="progress-ring"><LoaderCircle className="spin" /></div><strong>{installState.instance ? tr("deploying") : tr("preparing")}</strong><div className="progress-steps">{deploymentSteps.map(({ label, icon: Icon }, index) => { const currentIndex = deploymentStageIndex[installState.deploymentStage] ?? 0; const done = installState.deploymentStage === "complete" || index < currentIndex; const active = installState.deploymentStage !== "complete" && index === currentIndex; return <span className={done ? "done" : active ? "active" : "pending"} key={label}>{done ? <Check /> : active ? <LoaderCircle className="spin" /> : <Icon />}{label}</span>; })}</div></div>}
            {installState.step === "failed" && <div className="modal-body deployment-failed-body"><div className="deployment-failed-icon"><CircleAlert /></div><strong>{tr("deploymentFailed")}</strong><p>{installState.error}</p></div>}
            {installState.error && !installState.dockerStatus && installState.step !== "failed" && <div className="modal-error"><CircleAlert />{installState.error}</div>}
            {installState.step === "failed" ? <div className="modal-foot"><button className="button secondary" onClick={() => setInstallState((current) => current ? { ...current, step: "env", error: "" } : current)} type="button"><ChevronLeft />{tr("backToConfig")}</button><button className="button primary" onClick={() => void continueInstall()} type="button"><RefreshCw />{tr("dockerRecheckContinue")}</button></div> : installState.step !== "deploying" && <div className="modal-foot">{installState.step !== "env" || installState.generated ? <button className="button secondary" onClick={() => setInstallState(null)} type="button">{tr("cancel")}</button> : <button className="button secondary" disabled type="button">{tr("configureEnvFirst")}</button>}{installState.step === "form" ? <button className="button primary" onClick={prepareInstall} disabled={!isNativeDesktop} type="button"><SquareTerminal />{tr("next")}</button> : <><button className="button secondary" onClick={() => generateInstallEnv()} type="button"><FileKey />{tr("generateEnv")}</button><button className="button primary" onClick={continueInstall} disabled={!installState.generated} type="button"><Check />{tr("saveContinue")}</button></>}</div>}
          </div>
        </div>
      )}

      {installState && !installModalVisible && (
        <div
          className={`install-task-dock${installState.error ? " attention" : ""}`}
          style={{ transform: `translate(${installDrag.x}px, ${installDrag.y}px)` }}
          onMouseDown={(event) => {
            if (event.button !== 0) return;
            installDragRef.current = { startX: event.clientX, startY: event.clientY, offsetX: installDrag.x, offsetY: installDrag.y };
            setInstallDragging(true);
            event.preventDefault();
          }}
          onDoubleClick={() => setInstallModalVisible(true)}
        >
          <span className="install-task-icon">{installState.step === "deploying" ? <LoaderCircle className="spin" /> : installState.error ? <CircleAlert /> : <FileKey />}</span>
          <span className="install-task-copy"><small>{tr("backgroundTask")}</small><strong>{installState.step === "deploying" ? (installState.instance ? tr("deploying") : tr("preparing")) : installState.error ? tr("taskNeedsAttention") : tr("waitingConfig")}</strong><span>{installState.instance?.name || installState.form.name || installState.template.name} · {installState.template.id}</span></span>
          <button className="icon-button" onClick={() => setInstallModalVisible(true)} aria-label={tr("openTask")} type="button"><Maximize2 /></button>
        </div>
      )}

      {detailInstance && (
        <div className="modal-backdrop align-right" onMouseDown={(event) => event.currentTarget === event.target && setDetailInstance(null)}>
          <aside className="detail-drawer">
            <div className="modal-head"><div><span className="eyebrow">{detailInstance.templateId}</span><h2>{detailInstance.name}</h2></div><button className="icon-button" onClick={() => setDetailInstance(null)} type="button"><X /></button></div>
            <div className="detail-body instance-detail-body">
              {detailIsReady && <div className="deploy-ready-notice"><CircleAlert /><div><strong>{tr("deployReadyTitle")}</strong><p>{tr("deployReadyHint")}</p></div></div>}
              <section className="detail-overview"><h3>{tr("detail")}</h3><dl><dt>{tr("projectName")}</dt><dd>{detailInstance.projectName || detailInstance.name}</dd><dt>{tr("template")}</dt><dd><code>{detailInstance.templateId}</code></dd><dt>{tr("workDir")}</dt><dd><code title={detailInstance.workDir}>{detailInstance.workDir}</code></dd><dt>{tr("lifecycleVersion")}</dt><dd>V{detailInstance.lifecycleVersion || 1}</dd><dt>{tr("note")}</dt><dd>{detailInstance.note || "—"}</dd></dl></section>
              {/* ── Tab Switcher ── */}
              <div className="detail-tabs">
                <button className={`detail-tab ${detailTab === "entry" ? "active" : ""}`} onClick={() => setDetailTab("entry")} type="button">{tr("applicationEntry")}</button>
                {detailHasPhoenixTraceCenter && <button className={`detail-tab ${detailTab === "trace-center" ? "active" : ""}`} onClick={() => { setTraceCenterPage(1); setDetailTab("trace-center"); setTraceCenterRefreshKey((key) => key + 1); }} type="button">{tr("traces")}{traceCenterTotal > 0 && <span className="detail-tab-badge">{traceCenterTotal}</span>}</button>}
                {detailHasAtofTab && <button className={`detail-tab ${detailTab === "atof" ? "active" : ""}`} onClick={() => setDetailTab("atof")} type="button">{tr("atofTab")}</button>}
              </div>

              {/* ── Tab: Application entry ── */}
              {detailTab === "entry" && (
                <>
                  <section className="application-section"><h3>{tr("webEntry")}</h3>{primaryEndpoint ? <div className={`application-entry ${!detailIsRunning ? "inactive" : ""}`}><div className="application-entry-icon"><ExternalLink /></div><div className="application-entry-copy"><span>{primaryEndpoint.label}</span><strong>{primaryEndpoint.url}</strong>{!detailIsRunning && <small>{tr("availableAfterDeploy")}</small>}</div><div className="endpoint-actions"><button className="icon-button" title={tr("copyAddress")} aria-label={tr("copyAddress")} onClick={() => { void navigator.clipboard.writeText(primaryEndpoint.url); notify(tr("copied")); }} type="button"><Copy /></button><button className="button primary" disabled={!detailIsRunning} onClick={() => { void desktopApi.openExternalUrl(primaryEndpoint.url).catch((error) => notify(errorMessage(error))); }} type="button"><ExternalLink />{tr("openApplication")}</button></div></div> : <div className="no-application-entry"><CircleAlert /><span>{tr("noApplicationEntry")}</span></div>}</section>
                  {integrationEndpoints.length > 0 && <section><h3>{tr("integrationEndpoints")}</h3>{integrationEndpoints.map((endpoint) => <div className={`endpoint ${!detailIsRunning ? "inactive" : ""}`} key={`${endpoint.label}-${endpoint.url}`}><div><span>{endpoint.label}</span><strong>{endpoint.url}</strong>{!detailIsRunning && <small>{tr("availableAfterDeploy")}</small>}</div>{endpoint.kind === "web" && <button className="icon-button" disabled={!detailIsRunning} title={tr("openAddress")} aria-label={tr("openAddress")} onClick={() => { void desktopApi.openExternalUrl(endpoint.url).catch((error) => notify(errorMessage(error))); }} type="button"><ExternalLink /></button>}</div>)}</section>}
                </>
              )}

              {/* ── Tab: Trace center ── */}
              {detailTab === "trace-center" && (
                <>
                  {traceCenterLoading ? (
                    <div className="inline-loading"><LoaderCircle className="spin" />{tr("loading")}</div>
                  ) : traceCenterSummaries.length === 0 ? (
                    <div className="empty-state small"><CircleAlert /><span>{tr("noTraces")}</span><span>{tr("noTracesHint")}</span></div>
                  ) : (
                    <div className="detail-trace-content">
                      <div className="trace-list-page">
                      <div className="trace-table-head"><span>{tr("traceId")}</span><span>{tr("traceStatus")}</span><span>{tr("kind")}</span><span>{tr("traceTabInput")}</span><span>{tr("traceTabOutput")}</span><span>{tr("startTime")}</span><span>{tr("traceLatency")}</span></div>
                      {traceCenterSummaries.map((t) => {
                        const tone = t.status === "ERROR" ? "error" : "success";
                        const latency = t.latencyMs != null ? t.latencyMs >= 1000 ? `${(t.latencyMs / 1000).toFixed(1)}s` : `${t.latencyMs}ms` : "—";
                        return (
                          <button className="trace-table-row" key={t.traceId} type="button" onClick={() => {
                            setTraceCenterLoading(true);
                            fetchTraceDetail(detailInstance, t.traceId)
                              .then((d) => {
                                if (d) { setTracePanelDetail(d); setTracePanelSelectedSpanId(null); setTracePanelOpen(true); }
                              })
                              .catch(() => notify(tr("traceDetailLoadFailed")))
                              .finally(() => setTraceCenterLoading(false));
                          }}>
                            <span className="trace-id-cell" title={t.traceId}>{t.traceId.slice(0, 12)}</span>
                            <span className={`status ${tone}`}><i />{t.status}</span>
                            <span className="trace-kind-badge">{spanKindLabel(t.kind)}</span>
                            <span className="trace-io-cell" title={t.inputSummary ?? ""}>{t.inputSummary ?? "—"}</span>
                            <span className="trace-io-cell" title={t.outputSummary ?? ""}>{t.outputSummary ?? "—"}</span>
                            <span>{t.startTime ? String(t.startTime).slice(0, 19) : "—"}</span>
                            <span>{latency}</span>
                          </button>
                        );
                      })}
                      </div>
                      <PaginationControls language={language} page={traceCenterPage} total={traceCenterTotal} pageSize={TRACE_CENTER_PER_PAGE} onPageChange={setTraceCenterPage} disabled={traceCenterLoading} className="detail-trace-pagination" />
                    </div>
                  )}
                </>
              )}

              {/* ── Tab: ATOF source ── */}
              {detailTab === "atof" && (
                <section className="atof-source-panel">
                  <div className="atof-source-content">
                    <div className="atof-source-field">
                      <div className="atof-source-field-header">
                        <span>{tr("atofPathLabel")}</span>
                        <button className="icon-button compact" type="button" onClick={() => { void navigator.clipboard.writeText(detailAtofSourceRelativePath); notify(tr("copied")); }} title={tr("copyAddress")} aria-label={tr("copyAddress")}><Copy /></button>
                      </div>
                      <code title={detailAtofSourceRelativePath || tr("unavailable")}>{detailAtofSourceRelativePath || tr("unavailable")}</code>
                    </div>
                  </div>
                  <div className="atof-events-viewer">
                    <div className="atof-viewer-header">
                      <div>
                        <strong>{tr("atofEventsTitle")}</strong>
                        <small>{atofEventTotal > 0 ? `${atofEvents.length}/${atofEventTotal} ${tr("atofEventsLoaded")}` : tr("atofEventsHint")}</small>
                      </div>
                      <button className="icon-button compact" type="button" onClick={() => setAtofEventsRefreshKey((key) => key + 1)} title={tr("refresh")} aria-label={tr("refresh")} disabled={atofEventsLoading}>
                        <RefreshCw className={atofEventsLoading ? "spin" : ""} />
                      </button>
                    </div>
                    {atofEventsLoading ? (
                      <div className="inline-loading atof-viewer-loading"><LoaderCircle className="spin" />{tr("loading")}</div>
                    ) : atofEventsError ? (
                      <div className="atof-viewer-empty"><CircleAlert /><span>{tr("atofEventsLoadFailed")}</span><small>{atofEventsError}</small></div>
                    ) : atofEvents.length === 0 ? (
                      <div className="atof-viewer-empty"><FileText /><span>{tr("atofNoEvents")}</span></div>
                    ) : (
                      <div className="atof-viewer-grid">
                        <div className="atof-event-list" onScroll={(event) => {
                          const list = event.currentTarget;
                          if (list.scrollTop + list.clientHeight >= list.scrollHeight - 36) void loadMoreAtofEvents();
                        }}>
                          {atofEvents.map((entry, index) => {
                            return (
                              <button className={`atof-event-item ${selectedAtofEventIndex === index ? "active" : ""}`} key={entry.lineNumber} type="button" onClick={() => setSelectedAtofEventIndex(index)}>
                                <span className="atof-event-index">L{entry.lineNumber}</span>
                                <code className="atof-event-raw-preview">{entry.raw}</code>
                              </button>
                            );
                          })}
                          {atofEventsLoadingMore && <div className="atof-events-loading-more"><LoaderCircle className="spin" />{tr("atofEventsLoadingMore")}</div>}
                        </div>
                        <div className="atof-event-json">
                          {(() => {
                            const selected = atofEvents[selectedAtofEventIndex];
                            const formatted = selected ? parseAtofJson(selected.raw) : null;
                            return formatted ? <JsonView value={formatted} keyName={`line_${selected?.lineNumber || 1}`} collapsed={1} displayObjectSize enableClipboard shortenTextAfterLength={180} /> : <pre className="atof-event-raw">{selected?.raw || tr("unavailable")}</pre>;
                          })()}
                        </div>
                      </div>
                    )}
                  </div>
                </section>
              )}
            </div>
            <div className="drawer-foot">
              {(detailTab === "trace-center" || detailTab === "atof") && observationEndpoints.length > 0 ? observationEndpoints.map((endpoint) => {
                const isPhoenix = endpoint.name.toLowerCase().includes("phoenix") || endpoint.url.includes(":6006");
                return <button className="phoenix-foot-btn observability-foot-btn" key={`${endpoint.name}-${endpoint.url}`} onClick={() => { void desktopApi.openExternalUrl(endpoint.url).catch((error) => notify(errorMessage(error))); }} type="button"><ExternalLink />{isPhoenix ? tr("phoenixDashboard") : endpoint.label || endpoint.name}</button>;
              }) : <><button className="button secondary" onClick={() => { setSelectedConfigId(detailInstance.id); setConfigTab("instance"); setPage("config"); setDetailInstance(null); }} type="button"><FileKey />{tr("editConfig")}</button>{detailIsReady ? <button className="button primary" onClick={() => void continueReadyInstance(detailInstance)} type="button"><SquareTerminal />{tr("continueDeploy")}</button> : <button className="button primary" onClick={() => { setLogInstance(detailInstance.id); setPage("logs"); setDetailInstance(null); }} type="button"><FileText />{tr("openLogs")}</button>}</>}
            </div>
          </aside>
        </div>
      )}

      {/* ── Trace detail floating panel ── */}
      {tracePanelOpen && detailInstance && tracePanelDetail && (
        <div className="modal-backdrop align-right" onMouseDown={(e) => e.currentTarget === e.target && setTracePanelOpen(false)}>
          <aside className="detail-drawer wide">
            <TraceDetailPanel detail={tracePanelDetail} onBack={() => { setTracePanelDetail(null); setTracePanelSelectedSpanId(null); setTracePanelOpen(false); }} selectedSpanId={tracePanelSelectedSpanId} onSelectSpan={setTracePanelSelectedSpanId} tr={tr} language={language} />
          </aside>
        </div>
      )}

      {showSystemInfo && (
        <div className="modal-backdrop">
          <div className="modal info-modal">
            <div className="modal-head"><div><span className="eyebrow">AgentSeek Desktop</span><h2>{tr("systemInfo")}</h2></div><button className="icon-button" onClick={() => setShowSystemInfo(false)} type="button"><X /></button></div>
            <div className="modal-body info-list">
              {systemInfo && [
                { label: tr("appName"), value: systemInfo.appName },
                { label: tr("version"), value: systemInfo.version },
                { label: tr("dataPath"), value: systemInfo.dataPath, expanded: true },
                { label: tr("cliStrategy"), value: systemInfo.cliStrategy },
                { label: tr("storage"), value: systemInfo.storage, expanded: true },
              ].map(({ label, value, expanded }) => <div key={label}><span>{label}</span><SystemInfoValue value={value} expanded={expanded} /></div>)}
              <div><span>{tr("language")}</span><div className="segmented compact"><button className={language === "zh" ? "active" : ""} onClick={() => setLanguage("zh")} type="button">{tr("languageChinese")}</button><button className={language === "en" ? "active" : ""} onClick={() => setLanguage("en")} type="button">{tr("languageEnglish")}</button></div></div>
              <div><span>{tr("theme")}</span><div className="segmented compact"><button className={theme === "light" ? "active" : ""} onClick={() => setTheme("light")} type="button"><Sun />{tr("light")}</button><button className={theme === "dark" ? "active" : ""} onClick={() => setTheme("dark")} type="button"><Moon />{tr("dark")}</button></div></div>
            </div>
            <div className="modal-foot"><button className="button secondary" onClick={() => void openStorageSettings()} type="button"><Database />{tr("storageSettings")}</button><button className="button primary" onClick={() => setShowSystemInfo(false)} type="button">{tr("close")}</button></div>
          </div>
        </div>
      )}

      {storageConfig && !storageSetupFlow && (
        <div className="modal-backdrop">
          <div className="modal storage-modal" role="dialog" aria-modal="true">
            <div className="modal-head">
              <div>
                <span className="eyebrow">STORAGE</span>
                <h2>{storageSetupRequired ? tr("storageSetupTitle") : tr("desktopStorage")}</h2>
                <p>{storageSetupRequired ? tr("storageSetupHint") : tr("storageIsolationHint")}</p>
              </div>
              <div className="storage-modal-actions">
                <button className="language-button" type="button" onClick={() => setLanguage(language === "zh" ? "en" : "zh")} aria-label={tr("language")} title={tr("language")}><Languages /><span>{language === "zh" ? tr("languageChinese") : tr("languageEnglish")}</span></button>
                {!storageSetupRequired && <button className="icon-button" onClick={() => setStorageConfig(null)} type="button"><X /></button>}
              </div>
            </div>
            <div className="modal-body form-grid">
              {!storageSetupRequired && (storageConfig.error || !storageConfig.writable || storageConfig.effectiveMode !== storageConfig.mode) && (
                <div className="storage-warning full">
                  <CircleAlert />
                  <div>
                    <strong>{storageConfig.writable ? tr("storageFallback") : tr("storageReadOnly")}</strong>
                    <span>{tr("configuredStorage")}: {storageModeLabel(storageConfig.mode)} · {tr("effectiveStorage")}: {storageModeLabel(storageConfig.effectiveMode)}</span>
                    {storageConfig.error && <p>{storageConfig.error}</p>}
                  </div>
                </div>
              )}
              <label className="full">
                <span>{tr("storageType")}</span>
                <div className="select-field"><select value={storageConfig.mode === "oceanbase_server" ? "seekdb_server" : storageConfig.mode} onChange={(event) => {
                  const mode = event.target.value as StorageStatus["mode"];
                  setStorageConfig({ ...storageConfig, mode, path: storagePathForMode(storageConfig, mode), database: storageDatabaseForMode(storageConfig, mode) });
                  setStorageConfigDirty(true);
                }}>
                  <option value="sqlite_embedded">{tr("embeddedSqlite")}</option>
                  {cliStatus?.platform !== "windows" && <option value="seekdb_embedded">{tr("embeddedSeekdb")}</option>}
                  <option value="seekdb_server">{tr("seekdbServer")}</option>
                </select><ChevronDown /></div>
              </label>
              {storageConfig.mode === "sqlite_embedded" || storageConfig.mode === "seekdb_embedded" ? (
                <>
                  <label className="full"><span>{tr("dataDirectory")}</span><input value={storageConfig.path} onChange={(event) => { setStorageConfig({ ...storageConfig, path: event.target.value }); setStorageConfigDirty(true); }} /></label>
                  <label className="full"><span>{tr("database")}</span><input value={storageConfig.database} disabled /></label>
                </>
              ) : (
                <>
                  <label><span>{tr("host")}</span><input value={storageConfig.host} onChange={(event) => { setStorageConfig({ ...storageConfig, host: event.target.value }); setStorageConfigDirty(true); }} /></label>
                  <label><span>{tr("port")}</span><input type="number" value={storageConfig.port} onChange={(event) => { setStorageConfig({ ...storageConfig, port: Number(event.target.value) }); setStorageConfigDirty(true); }} /></label>
                  <label><span>{tr("tenant")}</span><input value={storageConfig.tenant} onChange={(event) => { setStorageConfig({ ...storageConfig, tenant: event.target.value }); setStorageConfigDirty(true); }} /></label>
                  <label><span>{tr("database")}</span><input value={storageConfig.database} onChange={(event) => { setStorageConfig({ ...storageConfig, database: event.target.value }); setStorageConfigDirty(true); }} /></label>
                  <label><span>{tr("user")}</span><input value={storageConfig.user} onChange={(event) => { setStorageConfig({ ...storageConfig, user: event.target.value }); setStorageConfigDirty(true); }} /></label>
                  <label><span>{tr("password")}</span><input type="password" placeholder={storageConfig.passwordConfigured ? tr("passwordKeepHint") : ""} onChange={(event) => { setStorageConfig({ ...storageConfig, password: event.target.value }); setStorageConfigDirty(true); }} /></label>
                </>
              )}
            </div>
            <div className="modal-foot">
              {!storageSetupRequired && <button className="button secondary" onClick={() => setStorageConfig(null)} type="button">{tr("cancel")}</button>}
              <button className="button primary" disabled={storageBusy || !storageConfigDirty} onClick={() => void saveStorageSettings()} type="button">
                {storageBusy ? <LoaderCircle className="spin" /> : <Database />}
                {storageBusy ? tr("storageConnecting") : storageSetupRequired ? tr("storageConfirmAction") : tr("storageSwitchAction")}
              </button>
            </div>
          </div>
        </div>
      )}

      {importOpen && (
        <div className="modal-backdrop"><div className="modal import-modal"><div className="modal-head"><div><h2>{tr("importTitle")}</h2><p>{tr("importHint")}</p></div><button className="icon-button" onClick={() => setImportOpen(false)} type="button"><X /></button></div><div className="modal-body"><label><span>{tr("envAddress")}</span><div className="path-picker"><input autoFocus value={importPath} onChange={(event) => setImportPath(event.target.value)} placeholder="/Users/name/project/.env.example" /><button type="button" onClick={async () => { const path = await desktopApi.chooseEnvFile(); if (path) setImportPath(path); }}><FileUp />{tr("choose")}</button></div></label></div><div className="modal-foot"><button className="button secondary" onClick={() => setImportOpen(false)} type="button">{tr("cancel")}</button><button className="button primary" disabled={!importPath.trim()} onClick={importConfiguration} type="button"><FileUp />{tr("confirmImport")}</button></div></div></div>
      )}

      {exportOpen && (
        <div className="modal-backdrop"><div className="modal import-modal"><div className="modal-head"><div><h2>{tr("exportEnv")}</h2><p>{tr("exportEnvHint")}</p></div><button className="icon-button" onClick={() => setExportOpen(false)} type="button"><X /></button></div><div className="modal-body form-grid"><label className="full"><span>{tr("projectAddress")}</span><div className="path-picker"><input autoFocus value={exportPath} onChange={(event) => { setExportPath(event.target.value); setExportFiles([]); setExportSource(""); setExportOutput(""); }} onBlur={() => void inspectExportDirectory(exportPath)} placeholder="/Users/name/project" /><button type="button" onClick={async () => { const path = await desktopApi.chooseDirectory(); if (path) { setExportPath(path); await inspectExportDirectory(path); } }}><FolderOpen />{tr("choose")}</button></div></label>{exportPath.trim() && <><label className="full"><span>{tr("sourceEnvFile")}</span><select value={exportSource} onChange={(event) => setExportSource(event.target.value)} disabled={exportScanning || !exportFiles.length}>{exportScanning ? <option>{tr("scanningEnvFiles")}</option> : exportFiles.length ? exportFiles.map((file) => <option key={file} value={file}>{file.split(/[\\/]/).pop()}</option>) : <option>{tr("noEnvFiles")}</option>}</select></label><label className="full"><span>{tr("targetEnvFile")}</span><input value={exportOutput} onChange={(event) => setExportOutput(event.target.value)} placeholder="/Users/name/project/.env" /></label></>}</div><div className="modal-foot"><button className="button secondary" onClick={() => setExportOpen(false)} type="button">{tr("cancel")}</button><button className="button primary" disabled={!exportSource || !exportOutput.trim() || exportBusy || exportScanning} onClick={() => void exportConfiguration()} type="button">{exportBusy ? <LoaderCircle className="spin" /> : <FileDown />}{tr("confirmExport")}</button></div></div></div>
      )}

      {envOverwrite && (
        <div className="modal-backdrop">
          <div className="modal overwrite-modal" role="alertdialog" aria-modal="true">
            <div className="modal-head"><div><span className="eyebrow">.env</span><h2>{tr("envExistsTitle")}</h2><p>{tr("envExistsHint")}</p></div><button className="icon-button" onClick={() => setEnvOverwrite(null)} type="button"><X /></button></div>
            <div className="overwrite-path"><FileKey /><code>{envOverwrite.path}</code></div>
            <div className="modal-foot"><button className="button secondary" onClick={() => setEnvOverwrite(null)} type="button">{tr("cancel")}</button><button className="button danger-button" onClick={() => { const context = envOverwrite.context; setEnvOverwrite(null); if (context === "install") void generateInstallEnv(true); else if (context === "config") void saveConfigEnv(true); else void exportConfiguration(true); }} type="button"><FileKey />{tr("overwriteEnv")}</button></div>
          </div>
        </div>
      )}

      {templateUpdateInfo && (
        <div className="modal-backdrop">
          <div className="modal" role="alertdialog" aria-modal="true">
            <div className="modal-head"><div><span className="eyebrow">{tr("templateUpdateTitle")}</span><h2>{tr("templateUpdateTitle")}</h2><p>{tr("templateUpdateMessage")}</p></div><button className="icon-button" onClick={() => setTemplateUpdateInfo(null)} type="button"><X /></button></div>
            <div className="modal-body" style={{ padding: "16px" }}>
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                <div><strong>{tr("currentVersion")}:</strong> {templateUpdateInfo.currentVersion}</div>
                <div><strong>{tr("latestVersion")}:</strong> {templateUpdateInfo.latestVersion}</div>
              </div>
            </div>
            <div className="modal-foot"><button className="button secondary" onClick={() => setTemplateUpdateInfo(null)} type="button">{tr("cancel")}</button><button className="button primary" onClick={() => void confirmTemplateUpdate()} type="button"><RefreshCw />{tr("confirmUpdate")}</button></div>
          </div>
        </div>
      )}

      {commentTooltip && <div
        className={`comment-popover ${commentTooltip.placement}`}
        role="dialog"
        aria-label={tr("comment")}
        onMouseEnter={keepCommentTooltip}
        onMouseLeave={scheduleCommentTooltipClose}
        style={{
          left: commentTooltip.left,
          top: commentTooltip.top,
          bottom: commentTooltip.bottom,
          width: commentTooltip.width,
          maxHeight: commentTooltip.maxHeight,
        }}
      ><textarea
        className="comment-popover-editor"
        value={commentTooltip.text}
        onChange={(event) => {
          commentTooltip.onChange(event.target.value);
          setCommentTooltip((current) => current ? { ...current, text: event.target.value } : current);
        }}
        rows={5}
        spellCheck={false}
        aria-label={tr("comment")}
      /></div>}

      {templateUrlEditOpen && (
        <div className="modal-backdrop">
          <div className="modal" role="dialog" aria-modal="true">
            <div className="modal-head"><div><h2>{tr("editTemplateSettings")}</h2></div><button className="icon-button" onClick={() => setTemplateUrlEditOpen(false)} type="button"><X /></button></div>
            <div className="modal-body" style={{ padding: "16px", display: "flex", flexDirection: "column", gap: "12px" }}>
              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>{tr("templateRepoUrl")}</span>
                <small style={{ opacity: 0.6 }}>{tr("templateRepoUrlHint")}</small>
                <input
                  type="text"
                  value={templateRepoUrlInput}
                  onChange={(event) => setTemplateRepoUrlInput(event.target.value)}
                  placeholder="https://github.com/agentseek-ai/agentseek-templates.git"
                  autoFocus
                />
              </label>
              <label style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                <span>{tr("templateCatalogUrl")}</span>
                <small style={{ opacity: 0.6 }}>{tr("templateCatalogUrlHint")}</small>
                <input
                  type="text"
                  value={templateCatalogInput}
                  onChange={(event) => setTemplateCatalogInput(event.target.value)}
                  placeholder="https://corp.com/catalog.json"
                />
              </label>
            </div>
            <div className="modal-foot"><button className="button secondary" onClick={() => setTemplateUrlEditOpen(false)} type="button">{tr("cancel")}</button><button className="button primary" onClick={() => void saveTemplateUrl()} disabled={templateUrlSaving || !templateRepoUrlInput.trim()} type="button">{templateUrlSaving ? <LoaderCircle className="spin" /> : <Check />}{tr("save")}</button></div>
          </div>
        </div>
      )}

      {toast && <div className={`toast ${installState && !installModalVisible ? "task-visible" : ""}`}><Check />{toast}</div>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Trace Detail Panel (Three-Pane Layout)
// ---------------------------------------------------------------------------

function TraceDetailPanel({
  detail,
  onBack,
  selectedSpanId,
  onSelectSpan,
  tr,
  language,
}: {
  detail: TraceDetail;
  onBack: () => void;
  selectedSpanId: string | null;
  onSelectSpan: (id: string | null) => void;
  tr: (key: TranslationKey) => string;
  language: Language;
}) {
  const [treePanelWidth, setTreePanelWidth] = useState(280);
  const [treePanelResizing, setTreePanelResizing] = useState(false);
  const treePanelResizeRef = useRef<{ startX: number; startWidth: number } | null>(null);

  useEffect(() => {
    if (!treePanelResizing) return;
    const onMouseMove = (event: MouseEvent) => {
      const resize = treePanelResizeRef.current;
      if (!resize) return;
      const nextWidth = resize.startWidth + event.clientX - resize.startX;
      setTreePanelWidth(Math.max(220, Math.min(520, nextWidth)));
    };
    const onMouseUp = () => {
      treePanelResizeRef.current = null;
      setTreePanelResizing(false);
    };
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    window.addEventListener("mousemove", onMouseMove);
    window.addEventListener("mouseup", onMouseUp);
    return () => {
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      window.removeEventListener("mousemove", onMouseMove);
      window.removeEventListener("mouseup", onMouseUp);
    };
  }, [treePanelResizing]);

  const startTreePanelResize = (event: React.MouseEvent<HTMLDivElement>) => {
    event.preventDefault();
    treePanelResizeRef.current = { startX: event.clientX, startWidth: treePanelWidth };
    setTreePanelResizing(true);
  };

  const selectedSpan = selectedSpanId
    ? findSpanById(detail.spans, selectedSpanId)
    : detail.spans[0] ?? null;

  const latency = detail.latencyMs != null
    ? detail.latencyMs >= 1000
      ? `${(detail.latencyMs / 1000).toFixed(1)}s`
      : `${detail.latencyMs}ms`
    : "—";
  const spanCount = countSpanNodes(detail.spans);

  return (
    <div className="trace-detail">
      {/* Top status bar */}
      <div className="trace-detail-topbar">
        <button className="button secondary compact" onClick={onBack} type="button"><ChevronLeft />{tr("back")}</button>
        <div className="trace-detail-meta">
          <code className="trace-detail-id">{detail.traceId.slice(0, 16)}…</code>
          <span className={`status ${detail.status === "ERROR" ? "error" : "success"}`}><i />{detail.status}</span>
          <span className="trace-detail-stat">{tr("traceLatency")} <strong>{latency}</strong></span>
          <span className="trace-detail-stat">{tr("traceSpans")} <strong>{spanCount}</strong></span>
        </div>
      </div>

      {/* Three-pane body */}
      <div className="trace-detail-body">
        {/* Left: Span tree */}
        <aside className="trace-tree-panel" style={{ width: treePanelWidth }}>
          <h3 className="trace-pane-title">{tr("traceSpans")}</h3>
          <div className="trace-tree">
            {detail.spans.map((span) => (
              <SpanTreeNode key={span.spanId} span={span} depth={0} selectedSpanId={selectedSpanId} onSelect={onSelectSpan} />
            ))}
          </div>
        </aside>
        <div
          className={`trace-tree-resizer ${treePanelResizing ? "active" : ""}`}
          onMouseDown={startTreePanelResize}
          role="separator"
          aria-label={language === "zh" ? "调整左侧导航宽度" : "Resize span navigation"}
          aria-orientation="vertical"
        />

        {/* Center: Inspector */}
        <section className="trace-inspector">
          {selectedSpan ? (
            <>
              <div className="trace-inspector-head">
                <h3>{selectedSpan.name}</h3>
                <div className="trace-inspector-meta">
                  <code>{selectedSpan.spanId.slice(0, 12)}</code>
                  <span className={`status ${selectedSpan.status === "ERROR" ? "error" : "success"}`}>{selectedSpan.status}</span>
                  <span>{spanKindLabel(selectedSpan.kind)}</span>
                  {selectedSpan.durationMs != null && <span>{selectedSpan.durationMs >= 1000 ? `${(selectedSpan.durationMs / 1000).toFixed(1)}s` : `${selectedSpan.durationMs}ms`}</span>}
                </div>
              </div>
              <div className="trace-inspector-tabs">
                {(["input", "output", "attributes"] as const).map((tab) => {
                  const data = tab === "input" ? selectedSpan.input : tab === "output" ? selectedSpan.output : selectedSpan.attributes;
                  if (data == null) return null;
                  const tabKey = tab === "input" ? "traceTabInput" : tab === "output" ? "traceTabOutput" : "traceTabAttributes";
                  return (
                    <details className="trace-inspector-section" key={tab} open={tab === "input" || tab === "output"}>
                      <summary>{tr(tabKey)}</summary>
                      <pre className="trace-json">{formatJson(data)}</pre>
                    </details>
                  );
                })}
              </div>
            </>
          ) : (
            <div className="empty-state small"><CircleAlert /><span>{tr("selectSpanHint")}</span></div>
          )}
        </section>
      </div>
    </div>
  );
}

function SpanTreeNode({
  span,
  depth,
  selectedSpanId,
  onSelect,
}: {
  span: SpanNode;
  depth: number;
  selectedSpanId: string | null;
  onSelect: (id: string | null) => void;
}) {
  const isSelected = selectedSpanId === span.spanId;
  const hasChildren = span.children.length > 0;
  const [expanded, setExpanded] = useState(true);
  const kind = spanKindLabel(span.kind);
  const KindIcon = kind === "LLM" ? Bot : kind === "TOOL" ? Wrench : kind === "CHAIN" ? Workflow : kind === "AGENT" ? Boxes : CircleDot;

  return (
    <div className="trace-tree-node">
      <button
        className={`trace-tree-row ${isSelected ? "selected" : ""}`}
        style={{ paddingLeft: 8 + depth * 20 }}
        type="button"
        onClick={() => onSelect(isSelected ? null : span.spanId)}
      >
        {hasChildren && (
          <span className="trace-tree-toggle" onClick={(e) => { e.stopPropagation(); setExpanded(!expanded); }}>
            <ChevronDown style={{ transform: expanded ? "" : "rotate(-90deg)", width: 14, height: 14 }} />
          </span>
        )}
        {!hasChildren && <span className="trace-tree-toggle" />}
        <span className="trace-tree-icon"><KindIcon /></span>
        <span className="trace-tree-name">{span.name}</span>
        <span className="trace-tree-status">
          <span className={`status ${span.status === "ERROR" ? "error" : "success"}`}>{span.status}</span>
        </span>
        {span.durationMs != null && (
          <span className="trace-tree-duration">{span.durationMs >= 1000 ? `${(span.durationMs / 1000).toFixed(1)}s` : `${span.durationMs}ms`}</span>
        )}
      </button>
      {expanded && hasChildren && (
        <div className="trace-tree-children">
          {span.children.map((child) => (
            <SpanTreeNode key={child.spanId} span={child} depth={depth + 1} selectedSpanId={selectedSpanId} onSelect={onSelect} />
          ))}
        </div>
      )}
    </div>
  );
}

function spanKindLabel(kind: string): string {
  const normalized = kind.trim().toUpperCase().replace(/^SPAN_?KIND\./, "");
  return normalized || "UNKNOWN";
}

function findSpanById(spans: SpanNode[], id: string): SpanNode | null {
  for (const span of spans) {
    if (span.spanId === id) return span;
    const found = findSpanById(span.children, id);
    if (found) return found;
  }
  return null;
}

function countSpanNodes(spans: SpanNode[]): number {
  return spans.reduce((total, span) => total + 1 + countSpanNodes(span.children), 0);
}

function formatJson(value: unknown): string {
  if (typeof value === "string") {
    try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return value; }
  }
  try { return JSON.stringify(value, null, 2); } catch { return String(value); }
}

function parseAtofJson(raw: string): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(raw);
    return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
  } catch {
    return null;
  }
}
