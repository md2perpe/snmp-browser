import { invoke, isTauri, pickDirectory } from "./api";
import { DEFAULT_COL_WIDTH, mockHostProfiles } from "./mockData";
import { checkForUpdate } from "./update";
import type {
  AnyTabState,
  AppState,
  BenchmarkTabState,
  ConnectionParams,
  DirFiles,
  HostProfile,
  MibNode,
  MibProfile,
  MibProfilesResponse,
  PaneState,
  ParseResult,
  Row,
  RowMetaEntry,
  SnmpVersion,
  TabState,
  Theme,
  TrapEvent,
  TrapListenerStatus,
  TrapTabState,
  WalkTiming,
} from "./types";

type Patch<T> = Partial<T> | ((t: T) => Partial<T>);

const AUTO_REFRESH_INTERVAL_MS = 10_000;
/** Diffable field ("Index") every fetched row carries, used as its stable identity across fetches. */
const ROW_KEY_FIELD = "Index";
const DEFAULT_SNMP_PORT = "161";
const DEFAULT_SNMP_COMMUNITY = "public";
const DEFAULT_TRAP_PORT = "162";
const TRAP_POLL_INTERVAL_MS = 1000;
/** How often to re-check for a new release - the app is commonly left open for a long time, so a startup-only check would miss releases that come out mid-session. */
const UPDATE_CHECK_INTERVAL_MS = 60 * 60 * 1000;
/** Client-side mirror of the server's per-listener ring buffer cap (see `trap.rs::MAX_EVENTS`), so a long-idle tab's array doesn't grow unbounded. */
const MAX_CLIENT_TRAP_EVENTS = 2000;
const THEME_STORAGE_KEY = "snmpBrowserTheme";
const DEFAULT_BENCHMARK_ITERATIONS = 10;
const MAX_BENCHMARK_ITERATIONS = 1000;

function loadTheme(): Theme {
  try {
    const saved = localStorage.getItem(THEME_STORAGE_KEY);
    if (saved === "dark" || saved === "classic") return saved;
  } catch {
    // localStorage unavailable (e.g. a restrictive webview) - fall back to the default.
  }
  return "dark";
}

/** `invoke()` rejects with a plain string under Tauri but with an Error over the HTTP fallback - unwrap both to the bare message. */
function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export interface BenchmarkStats {
  count: number;
  min: number;
  max: number;
  mean: number;
  median: number;
  p95: number;
  /** Sample standard deviation (n-1); 0 for a single run, where it's undefined. */
  stdDev: number;
  /** Time spent walking, summed over every run - excludes the gaps between them. */
  total: number;
}

/**
 * Linear-interpolated percentile of an ascending-sorted array, the usual
 * definition - `q = 0.5` gives the median, averaging the two middle values
 * for an even-length sample.
 */
function percentile(sorted: number[], q: number): number {
  if (sorted.length === 1) return sorted[0];
  const pos = (sorted.length - 1) * q;
  const lo = Math.floor(pos);
  const hi = Math.ceil(pos);
  return sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo);
}

/** Descriptive statistics over a benchmark's walk durations (ms); null until at least one walk has finished. */
export function computeStats(durations: number[]): BenchmarkStats | null {
  if (durations.length === 0) return null;
  const sorted = [...durations].sort((a, b) => a - b);
  const n = sorted.length;
  const total = sorted.reduce((sum, v) => sum + v, 0);
  const mean = total / n;
  const variance = n > 1 ? sorted.reduce((sum, v) => sum + (v - mean) ** 2, 0) / (n - 1) : 0;
  return {
    count: n,
    min: sorted[0],
    max: sorted[n - 1],
    mean,
    median: percentile(sorted, 0.5),
    p95: percentile(sorted, 0.95),
    stdDev: Math.sqrt(variance),
    total,
  };
}

export class Store {
  state: AppState;
  hostProfiles: HostProfile[] = mockHostProfiles;
  tree: MibNode[] = [];
  tablesTree: MibNode[] = [];
  /** Files found under each configured MIB directory, for showing them as subnodes in the sidebar. */
  dirFiles: DirFiles[] = [];
  /** This machine's non-loopback IPv4 addresses, for the trap listener's "point your device here" hint. Empty until a trap tab has been opened at least once. */
  localIps: string[] = [];

  /** Iteration count the next benchmark tab opens with - the last one the user picked. */
  private benchmarkIterations = DEFAULT_BENCHMARK_ITERATIONS;

  private listeners: Array<() => void> = [];
  private tickListeners: Array<() => void> = [];
  private autoRefreshTimers = new Map<string, ReturnType<typeof setInterval>>();
  /** Epoch ms of each auto-refreshing tab's next fetch, for the countdown ring. */
  private autoRefreshNextAt = new Map<string, number>();
  private trapPollTimers = new Map<string, ReturnType<typeof setInterval>>();
  /** Ticks re-renders while any tab is auto-refreshing, so the countdown ring animates. */
  private uiTickTimer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    this.state = {
      expanded: {},
      selectedTreeNodeId: "",
      tablesOnlyMode: false,
      mibProfiles: [],
      activeMibProfileId: "",
      mibDirDraft: null,
      mibProfileDraft: null,
      renamingMibProfile: false,
      parseErrors: [],
      parseErrorsOpen: false,
      leftWidth: 330,
      leftCollapsed: false,
      panes: [{ id: "p1", width: null, activeTabId: null, tabs: [] }],
      activePaneId: "p1",
      treeContextMenu: null,
      refreshMenu: null,
      exportMenu: null,
      theme: loadTheme(),
      themeMenu: null,
      updateInfo: null,
    };
    this.applyTheme(this.state.theme);
  }

  async init() {
    const [profiles, hostProfiles] = await Promise.all([
      invoke<MibProfilesResponse>("list_mib_profiles"),
      invoke<HostProfile[]>("list_host_profiles"),
    ]);
    this.applyMibProfilesResponse(profiles);
    this.hostProfiles = hostProfiles;
    this.notify();
    await this.loadMibTree();
    if (isTauri) {
      void this.runUpdateCheck();
      setInterval(() => void this.runUpdateCheck(), UPDATE_CHECK_INTERVAL_MS);
    }
  }

  /** Fire-and-forget update check, run on startup and then every `UPDATE_CHECK_INTERVAL_MS` - see `checkForUpdate()` in update.ts for what "newer" means and how failures are handled. */
  private async runUpdateCheck() {
    const info = await checkForUpdate();
    if (!info) return;
    this.state.updateInfo = info;
    this.notify();
  }

  private applyMibProfilesResponse(resp: MibProfilesResponse) {
    this.state.mibProfiles = resp.profiles;
    this.state.activeMibProfileId = resp.activeProfileId;
  }

  activeMibProfile(): MibProfile | undefined {
    return this.state.mibProfiles.find((p) => p.id === this.state.activeMibProfileId);
  }

  onChange(fn: () => void) {
    this.listeners.push(fn);
  }

  /**
   * Registers a callback for the lightweight countdown-ring tick (every
   * 200ms while any tab is auto-refreshing). Unlike onChange, this must NOT
   * trigger a full re-render: replacing the whole DOM tree that often would
   * intermittently swallow clicks, since a mousedown/mouseup pair that
   * straddles a rebuild loses its target element mid-click.
   */
  onTick(fn: () => void) {
    this.tickListeners.push(fn);
  }

  private notify() {
    for (const fn of this.listeners) fn();
  }

  // ---------- tab / pane construction ----------

  makeTab(id: string, hostId: string, opts: Partial<TabState> = {}): TabState {
    // Falls back to blank connection fields when no host profile matches (or
    // none are configured at all) - the fields are freely editable either way.
    const h = this.hostProfiles.find((p) => p.id === hostId) ?? this.hostProfiles[0];
    return {
      kind: "query",
      id,
      hostId: h?.id ?? "",
      hostAddr: h?.addr ?? "",
      hostPort: h?.port ?? DEFAULT_SNMP_PORT,
      version: "v2c",
      community: h?.community ?? DEFAULT_SNMP_COMMUNITY,
      v3User: h?.v3User ?? "",
      v3Auth: "",
      v3Priv: "",
      selectedNode: "",
      columns: [],
      displayHints: {},
      enumLabels: {},
      sortCol: ROW_KEY_FIELD,
      sortDir: 1,
      colWidths: {},
      autoRefresh: false,
      lastFetch: "",
      diffMode: false,
      humanReadableColumns: false,
      useDisplayHints: false,
      transposed: false,
      workingRows: [],
      rowMeta: {},
      removedGhosts: [],
      fetchError: null,
      ...opts,
    };
  }

  makeTrapTab(id: string, opts: Partial<TrapTabState> = {}): TrapTabState {
    return {
      kind: "trap",
      id,
      bindAddr: "0.0.0.0",
      port: DEFAULT_TRAP_PORT,
      version: "v2c",
      community: "",
      v3User: "",
      v3Auth: "",
      v3Priv: "",
      running: false,
      boundAddr: "",
      startError: null,
      events: [],
      lastSeq: 0,
      expandedSeq: null,
      filterText: "",
      ...opts,
    };
  }

  /** A benchmark tab starts aimed at `node` with its own blank/default connection fields - independent of whatever query tab it was opened from, since it's its own pane tab. */
  makeBenchmarkTab(id: string, node: MibNode, opts: Partial<BenchmarkTabState> = {}): BenchmarkTabState {
    const h = this.hostProfiles[0];
    return {
      kind: "benchmark",
      id,
      nodeLabel: node.label,
      oid: node.oid,
      hostAddr: h?.addr ?? "",
      hostPort: h?.port ?? DEFAULT_SNMP_PORT,
      version: "v2c",
      community: h?.community ?? DEFAULT_SNMP_COMMUNITY,
      v3User: h?.v3User ?? "",
      v3Auth: "",
      v3Priv: "",
      iterations: this.benchmarkIterations,
      running: false,
      cancelling: false,
      runs: [],
      failures: 0,
      error: null,
      ...opts,
    };
  }

  // ---------- lookups ----------

  getPane(id: string): PaneState | undefined {
    return this.state.panes.find((p) => p.id === id);
  }
  getActivePane(): PaneState {
    return this.getPane(this.state.activePaneId) ?? this.state.panes[0];
  }
  getPaneActiveTab(pane: PaneState): AnyTabState | undefined {
    return pane.tabs.find((t) => t.id === pane.activeTabId) ?? pane.tabs[0];
  }
  getActiveTab(): AnyTabState | undefined {
    return this.getPaneActiveTab(this.getActivePane());
  }

  private applyPatch<T extends object>(obj: T, patch: Patch<T>) {
    const p = typeof patch === "function" ? patch(obj) : patch;
    Object.assign(obj, p);
  }

  updateActiveTabInPane(paneId: string, patch: Patch<TabState>) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "query") return;
    this.applyPatch(tab, patch);
    this.notify();
  }

  updateActiveTrapTabInPane(paneId: string, patch: Patch<TrapTabState>) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "trap") return;
    this.applyPatch(tab, patch);
    this.notify();
  }

  updateActiveBenchmarkTabInPane(paneId: string, patch: Patch<BenchmarkTabState>) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "benchmark") return;
    this.applyPatch(tab, patch);
    this.notify();
  }

  // ---------- pane / tab management ----------

  focusPane(paneId: string) {
    this.state.activePaneId = paneId;
    this.notify();
  }

  selectTab(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    pane.activeTabId = tabId;
    this.state.activePaneId = paneId;
    this.notify();
  }

  /** Cycles the active pane's active tab forward (1) or backward (-1), wrapping around. */
  cycleActiveTab(direction: 1 | -1) {
    const pane = this.getActivePane();
    if (pane.tabs.length < 2) return;
    const from = pane.tabs.findIndex((t) => t.id === pane.activeTabId);
    const next = ((from === -1 ? 0 : from) + direction + pane.tabs.length) % pane.tabs.length;
    this.selectTab(pane.id, pane.tabs[next].id);
  }

  /** Opens a new tab in the active pane with the given tree node (e.g. a table) pre-selected. */
  openNodeInNewTab(nodeId: string) {
    this.state.selectedTreeNodeId = nodeId;
    this.pushNewTab(this.state.activePaneId, "h1", { selectedNode: nodeId });
    this.closeTreeContextMenu();
  }

  private pushNewTab(paneId: string, defaultHostId: string, opts: Partial<TabState> = {}) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.makeTab("tab" + Date.now(), defaultHostId, opts);
    pane.tabs.push(tab);
    pane.activeTabId = tab.id;
    if (tab.autoRefresh) this.startAutoRefresh(paneId, tab.id);
    this.notify();
  }

  /** Opens a new, not-yet-listening trap listener tab in the given pane. */
  openTrapListenerTab(paneId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.makeTrapTab("tab" + Date.now());
    pane.tabs.push(tab);
    pane.activeTabId = tab.id;
    this.state.activePaneId = paneId;
    this.notify();
    void this.refreshLocalIps();
  }

  /** Refreshes the cached local IP list shown as a trap-destination hint - cheap enough to just re-fetch on each trap tab open, in case the machine switched networks since launch. */
  private async refreshLocalIps() {
    try {
      this.localIps = await invoke<string[]>("local_ips");
      this.notify();
    } catch {
      // Leave the previous (possibly empty) list in place.
    }
  }

  /** Opens a new benchmark tab in the active pane, aimed at the given tree node. */
  openBenchmarkTab(nodeId: string) {
    const node = this.findNode(this.activeTree(), nodeId);
    if (!this.canBenchmark(node)) return;
    this.state.selectedTreeNodeId = nodeId;
    const pane = this.getPane(this.state.activePaneId);
    if (!pane) return;
    const tab = this.makeBenchmarkTab("tab" + Date.now(), node!);
    pane.tabs.push(tab);
    pane.activeTabId = tab.id;
    this.notify();
    this.closeTreeContextMenu();
  }

  closeTabInPane(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    this.stopAutoRefresh(tabId);
    const tab = pane.tabs.find((t) => t.id === tabId);
    if (tab?.kind === "trap") {
      this.stopTrapPolling(tabId);
      if (tab.running) void invoke("stop_trap_listener", { id: tabId });
    }
    // The walk in flight can't be aborted, but this stops the run from starting another.
    if (tab?.kind === "benchmark" && tab.running) tab.cancelling = true;
    pane.tabs = pane.tabs.filter((t) => t.id !== tabId);
    if (pane.activeTabId === tabId) {
      pane.activeTabId = pane.tabs.length ? pane.tabs[pane.tabs.length - 1].id : null;
    }
    this.notify();
  }

  splitPane(paneId: string) {
    if (this.state.panes.length >= 2) return;
    const pane = this.getPane(paneId);
    if (!pane) return;
    const sourceTab = this.getPaneActiveTab(pane);
    let newTab: AnyTabState | null = sourceTab ? { ...sourceTab, id: "tab" + Date.now() } : null;
    // A duplicated trap tab doesn't inherit a live backend listener (there isn't one
    // registered under its new id yet), so it starts out as a fresh, stopped copy.
    if (newTab?.kind === "trap") newTab = { ...newTab, running: false, boundAddr: "", startError: null, events: [], lastSeq: 0, expandedSeq: null };
    // Likewise, a duplicated benchmark tab doesn't inherit the run loop backing it.
    if (newTab?.kind === "benchmark") newTab = { ...newTab, running: false, cancelling: false };
    pane.width = 620;
    const newPane: PaneState = { id: "pane" + Date.now(), width: null, tabs: newTab ? [newTab] : [], activeTabId: newTab?.id ?? null };
    this.state.panes.push(newPane);
    this.state.activePaneId = newPane.id;
    if (newTab?.kind === "query" && newTab.autoRefresh) this.startAutoRefresh(newPane.id, newTab.id);
    this.notify();
  }

  closePane(paneId: string) {
    if (this.state.panes.length <= 1) return;
    const pane = this.getPane(paneId);
    pane?.tabs.forEach((t) => {
      this.stopAutoRefresh(t.id);
      if (t.kind === "trap") {
        this.stopTrapPolling(t.id);
        if (t.running) void invoke("stop_trap_listener", { id: t.id });
      }
      if (t.kind === "benchmark" && t.running) t.cancelling = true;
    });
    this.state.panes = this.state.panes.filter((p) => p.id !== paneId);
    if (this.state.panes.length === 1) this.state.panes[0].width = null;
    if (this.state.activePaneId === paneId) {
      this.state.activePaneId = this.state.panes[this.state.panes.length - 1].id;
    }
    this.notify();
  }

  setPaneWidth(paneId: string, width: number) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    pane.width = Math.max(340, width);
    this.notify();
  }

  // ---------- sidebar / MIB directories ----------

  toggleLeft() {
    this.state.leftCollapsed = !this.state.leftCollapsed;
    this.notify();
  }

  toggleParseErrors() {
    this.state.parseErrorsOpen = !this.state.parseErrorsOpen;
    this.notify();
  }

  setLeftWidth(width: number) {
    this.state.leftWidth = Math.min(520, Math.max(220, width));
    this.notify();
  }

  private applyTheme(theme: Theme) {
    document.documentElement.setAttribute("data-theme", theme);
  }

  setTheme(theme: Theme) {
    this.state.theme = theme;
    this.state.themeMenu = null;
    this.applyTheme(theme);
    try {
      localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      // localStorage unavailable - the choice just won't survive a restart.
    }
    this.notify();
  }

  toggleThemeMenu(x: number, y: number) {
    this.state.themeMenu = this.state.themeMenu ? null : { x, y };
    this.notify();
  }

  closeThemeMenu() {
    this.state.themeMenu = null;
    this.notify();
  }

  toggleExpand(id: string) {
    this.state.expanded[id] = !this.state.expanded[id];
    this.notify();
  }

  setTablesOnlyMode(value: boolean) {
    this.state.tablesOnlyMode = value;
    this.notify();
  }

  selectTreeNode(node: MibNode) {
    this.state.selectedTreeNodeId = node.id;
    this.notify();
  }

  openTreeContextMenu(x: number, y: number, nodeId: string) {
    this.state.treeContextMenu = { x, y, nodeId };
    this.notify();
  }

  closeTreeContextMenu() {
    this.state.treeContextMenu = null;
    this.notify();
  }

  toggleRefreshMenu(paneId: string, x: number, y: number) {
    this.state.refreshMenu = this.state.refreshMenu?.paneId === paneId ? null : { paneId, x, y };
    this.notify();
  }

  closeRefreshMenu() {
    this.state.refreshMenu = null;
    this.notify();
  }

  toggleExportMenu(paneId: string, x: number, y: number) {
    this.state.exportMenu = this.state.exportMenu?.paneId === paneId ? null : { paneId, x, y };
    this.notify();
  }

  closeExportMenu() {
    this.state.exportMenu = null;
    this.notify();
  }

  async loadMibTree() {
    const result = await invoke<ParseResult>("get_mib_tree");
    this.tree = result.tree;
    this.tablesTree = result.tablesTree;
    this.dirFiles = result.dirFiles;
    this.state.parseErrors = result.errors;
    if (result.errors.length === 0) this.state.parseErrorsOpen = false;
    this.notify();
  }

  async addMibDir() {
    if (!isTauri) {
      this.state.mibDirDraft = "";
      this.notify();
      return;
    }
    const selected = await pickDirectory();
    if (!selected) return;
    await this.commitMibDir(selected);
  }

  updateMibDirDraft(text: string) {
    this.state.mibDirDraft = text;
    this.notify();
  }

  cancelMibDirDraft() {
    this.state.mibDirDraft = null;
    this.notify();
  }

  async submitMibDirDraft() {
    const path = this.state.mibDirDraft?.trim();
    this.state.mibDirDraft = null;
    if (!path) {
      this.notify();
      return;
    }
    await this.commitMibDir(path);
  }

  private async commitMibDir(path: string) {
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("add_mib_dir", { path }));
    this.notify();
    await this.loadMibTree();
  }

  async removeMibDir(path: string) {
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("remove_mib_dir", { path }));
    this.notify();
    await this.loadMibTree();
  }

  // ---------- MIB profiles ----------

  async switchMibProfile(id: string) {
    if (id === this.state.activeMibProfileId) return;
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("set_active_mib_profile", { id }));
    // The previous profile's tree state doesn't apply to the new one.
    this.state.expanded = {};
    this.state.selectedTreeNodeId = "";
    this.notify();
    await this.loadMibTree();
  }

  startMibProfileDraft() {
    this.state.mibProfileDraft = "";
    this.notify();
  }

  updateMibProfileDraft(text: string) {
    this.state.mibProfileDraft = text;
    this.notify();
  }

  cancelMibProfileDraft() {
    this.state.mibProfileDraft = null;
    this.notify();
  }

  async submitMibProfileDraft() {
    const name = this.state.mibProfileDraft?.trim();
    this.state.mibProfileDraft = null;
    if (!name) {
      this.notify();
      return;
    }
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("add_mib_profile", { name }));
    this.state.expanded = {};
    this.state.selectedTreeNodeId = "";
    this.notify();
    await this.loadMibTree();
  }

  async removeMibProfile(id: string) {
    if (this.state.mibProfiles.length <= 1) return;
    const wasActive = id === this.state.activeMibProfileId;
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("remove_mib_profile", { id }));
    if (wasActive) {
      this.state.expanded = {};
      this.state.selectedTreeNodeId = "";
      this.notify();
      await this.loadMibTree();
    } else {
      this.notify();
    }
  }

  startRenamingMibProfile() {
    this.state.renamingMibProfile = true;
    this.notify();
  }

  cancelRenamingMibProfile() {
    this.state.renamingMibProfile = false;
    this.notify();
  }

  async renameMibProfile(id: string, name: string) {
    this.state.renamingMibProfile = false;
    const trimmed = name.trim();
    if (!trimmed) {
      this.notify();
      return;
    }
    this.applyMibProfilesResponse(await invoke<MibProfilesResponse>("rename_mib_profile", { id, name: trimmed }));
    this.notify();
  }

  /** The tree currently shown in the sidebar: the full group hierarchy, or the flat tables-only view. */
  activeTree(): MibNode[] {
    return this.state.tablesOnlyMode ? this.tablesTree : this.tree;
  }

  getVisibleNodes(): { node: MibNode; depth: number }[] {
    return this.flatten(this.activeTree(), 0, [], this.state.expanded);
  }

  private flatten(nodes: MibNode[], depth: number, out: { node: MibNode; depth: number }[], expanded: Record<string, boolean>) {
    for (const n of nodes) {
      out.push({ node: n, depth });
      if (n.children && expanded[n.id]) this.flatten(n.children, depth + 1, out, expanded);
    }
    return out;
  }

  findNode(nodes: MibNode[], id: string): MibNode | null {
    for (const n of nodes) {
      if (n.id === id) return n;
      if (n.children) {
        const f = this.findNode(n.children, id);
        if (f) return f;
      }
    }
    return null;
  }

  setVersion(paneId: string, version: SnmpVersion) {
    this.updateActiveTabInPane(paneId, { version });
  }

  setTrapVersion(paneId: string, version: SnmpVersion) {
    this.updateActiveTrapTabInPane(paneId, { version });
  }

  setBenchmarkVersion(paneId: string, version: SnmpVersion) {
    this.updateActiveBenchmarkTabInPane(paneId, { version });
  }

  // ---------- table ----------

  setSortCol(paneId: string, col: string) {
    this.updateActiveTabInPane(paneId, (t) => ({
      sortCol: col,
      sortDir: t.sortCol === col ? (t.sortDir === 1 ? -1 : 1) : 1,
    }));
  }

  colWidth(tab: TabState, col: string): number {
    return tab.colWidths[col] ?? DEFAULT_COL_WIDTH;
  }

  setColWidth(paneId: string, col: string, width: number) {
    this.updateActiveTabInPane(paneId, (t) => ({ colWidths: { ...t.colWidths, [col]: Math.max(50, width) } }));
  }

  getSortedRows(tab: TabState): Row[] {
    const rows = [...tab.workingRows, ...tab.removedGhosts];
    rows.sort((a, b) => {
      const av = a[tab.sortCol] ?? "";
      const bv = b[tab.sortCol] ?? "";
      const an = Number(av);
      const bn = Number(bv);
      const bothNumeric = av !== "" && bv !== "" && !Number.isNaN(an) && !Number.isNaN(bn);
      const cmp = bothNumeric ? an - bn : av.localeCompare(bv);
      return cmp * tab.sortDir;
    });
    return rows;
  }

  rowMetaFor(tab: TabState, row: Row): RowMetaEntry | undefined {
    return tab.rowMeta[row[ROW_KEY_FIELD] ?? ""];
  }

  canFetch(node: MibNode | null): boolean {
    return !!node && node.type !== "group" && node.resolved;
  }

  /** A tab's connection fields in the shape the backend commands expect - shared by query and benchmark tabs, which carry the same fields independently. */
  connectionOf(tab: ConnectionParams): ConnectionParams {
    return {
      hostAddr: tab.hostAddr,
      hostPort: tab.hostPort,
      version: tab.version,
      community: tab.community,
      v3User: tab.v3User,
      v3Auth: tab.v3Auth,
      v3Priv: tab.v3Priv,
    };
  }

  /** Whether a tab's connection fields are filled in enough to attempt a fetch or a walk. */
  hasCompleteConnection(tab: ConnectionParams): boolean {
    if (!tab.hostAddr.trim() || !tab.hostPort.trim() || !tab.version) return false;
    return tab.version === "v3" ? !!tab.v3User.trim() : !!tab.community.trim();
  }

  // ---------- diff mode / fetch ----------

  toggleDiffMode(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ diffMode: !t.diffMode }));
  }

  toggleHumanReadableColumns(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ humanReadableColumns: !t.humanReadableColumns }));
  }

  toggleUseDisplayHints(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ useDisplayHints: !t.useDisplayHints }));
  }

  toggleTransposed(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ transposed: !t.transposed }));
  }

  setAutoRefresh(paneId: string, value: boolean) {
    this.state.refreshMenu = null;
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (tab && tab.kind === "query" && tab.autoRefresh !== value) {
      tab.autoRefresh = value;
      if (value) this.startAutoRefresh(paneId, tab.id);
      else this.stopAutoRefresh(tab.id);
    }
    this.notify();
  }

  private startAutoRefresh(paneId: string, tabId: string) {
    this.stopAutoRefresh(tabId);
    this.autoRefreshNextAt.set(tabId, Date.now() + AUTO_REFRESH_INTERVAL_MS);
    const timer = setInterval(() => void this.fetchForTab(paneId, tabId), AUTO_REFRESH_INTERVAL_MS);
    this.autoRefreshTimers.set(tabId, timer);
    if (!this.uiTickTimer) {
      this.uiTickTimer = setInterval(() => {
        for (const fn of this.tickListeners) fn();
      }, 200);
    }
  }

  private stopAutoRefresh(tabId: string) {
    const timer = this.autoRefreshTimers.get(tabId);
    if (timer != null) {
      clearInterval(timer);
      this.autoRefreshTimers.delete(tabId);
    }
    this.autoRefreshNextAt.delete(tabId);
    if (this.autoRefreshTimers.size === 0 && this.uiTickTimer) {
      clearInterval(this.uiTickTimer);
      this.uiTickTimer = null;
    }
  }

  /** Fraction of the auto-refresh interval remaining before the next fetch (1 = just fetched, 0 = about to fetch); null when not auto-refreshing. */
  autoRefreshFraction(tabId: string): number | null {
    const nextAt = this.autoRefreshNextAt.get(tabId);
    if (nextAt == null) return null;
    return Math.max(0, Math.min(1, (nextAt - Date.now()) / AUTO_REFRESH_INTERVAL_MS));
  }

  private async fetchForTab(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    const tab = pane?.tabs.find((t) => t.id === tabId);
    if (!pane || !tab || tab.kind !== "query") {
      this.stopAutoRefresh(tabId);
      return;
    }
    this.autoRefreshNextAt.set(tabId, Date.now() + AUTO_REFRESH_INTERVAL_MS);
    await this.runFetch(tab);
    this.notify();
  }

  /** Manual "Fetch Table" click for a pane's active tab. */
  async manualFetch(paneId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "query") return;
    await this.runFetch(tab);
    this.notify();
  }

  private async runFetch(tab: TabState) {
    const node = this.findNode(this.activeTree(), tab.selectedNode);
    if (!this.canFetch(node)) {
      tab.fetchError = node ? `'${node.label}' can't be fetched` : "Select an OID first";
      return;
    }
    if (!this.hasCompleteConnection(tab)) {
      tab.fetchError = "Fill in the host address, port, and " + (tab.version === "v3" ? "security user" : "community") + " first";
      return;
    }
    try {
      const result = await invoke<{
        columns: string[];
        rows: Row[];
        displayHints: Record<string, string>;
        enumLabels: Record<string, Record<string, string>>;
      }>("fetch", {
        nodeId: tab.selectedNode,
        connection: this.connectionOf(tab),
      });
      if (tab.diffMode) {
        const { meta, removed } = this.computeRowDiff(tab.workingRows, result.rows);
        tab.workingRows = result.rows;
        tab.rowMeta = meta;
        tab.removedGhosts = removed;
      } else {
        tab.workingRows = result.rows;
        tab.rowMeta = {};
        tab.removedGhosts = [];
      }
      tab.columns = result.columns;
      tab.displayHints = result.displayHints;
      tab.enumLabels = result.enumLabels;
      if (!tab.columns.includes(tab.sortCol)) tab.sortCol = tab.columns[0] ?? ROW_KEY_FIELD;
      tab.fetchError = null;
    } catch (e) {
      tab.fetchError = String(e);
    }
    tab.lastFetch = new Date().toLocaleTimeString();
  }

  private rowKey(row: Row): string {
    return row[ROW_KEY_FIELD] ?? "";
  }

  private computeRowDiff(oldRows: Row[], newRows: Row[]) {
    const oldMap = new Map(oldRows.map((r) => [this.rowKey(r), r]));
    const newMap = new Map(newRows.map((r) => [this.rowKey(r), r]));
    const meta: Record<string, RowMetaEntry> = {};
    for (const r of newRows) {
      const key = this.rowKey(r);
      const old = oldMap.get(key);
      if (!old) {
        meta[key] = { status: "added" };
      } else {
        const fields = Object.keys(r).filter((f) => old[f] !== r[f]);
        if (fields.length) meta[key] = { status: "changed", fields };
      }
    }
    const removed = oldRows.filter((r) => !newMap.has(this.rowKey(r))).map((r) => ({ ...r }));
    removed.forEach((r) => {
      meta[this.rowKey(r)] = { status: "removed" };
    });
    return { meta, removed };
  }

  // ---------- walk benchmark ----------

  /** Any resolved node can be walked, group nodes included - unlike Fetch, which needs a table or a scalar. */
  canBenchmark(node: MibNode | null): boolean {
    return !!node && node.resolved && !!node.oid;
  }

  setBenchmarkIterations(paneId: string, count: number) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "benchmark" || tab.running) return;
    tab.iterations = Math.min(MAX_BENCHMARK_ITERATIONS, Math.max(1, Math.floor(count) || 1));
    this.benchmarkIterations = tab.iterations;
    this.notify();
  }

  /** Asks a run to stop; the walk already in flight can't be aborted, so it finishes first. */
  cancelBenchmark(paneId: string) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "benchmark" || !tab.running) return;
    tab.cancelling = true;
    this.notify();
  }

  /** Finds a benchmark tab by id across every pane - the run loop only knows the tab id, not which pane it's in (and it may have been closed). */
  private findBenchmarkTab(tabId: string): BenchmarkTabState | undefined {
    for (const pane of this.state.panes) {
      const t = pane.tabs.find((t) => t.id === tabId);
      if (t && t.kind === "benchmark") return t;
    }
    return undefined;
  }

  /**
   * Walks the benchmark tab's OID `iterations` times, one walk at a time so the
   * runs don't contend with each other, re-rendering after each so results fill
   * in as they arrive. A walk that fails (SNMP is over UDP, so a dropped packet
   * shows up as a timeout) is counted as a failure and the run carries on - one
   * unlucky packet shouldn't throw away a long benchmark. A failure on the very
   * first walk is different: nothing is reachable, so there's nothing to
   * measure and the run stops there.
   */
  async runBenchmark(paneId: string) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "benchmark" || tab.running) return;
    const tabId = tab.id;
    tab.runs = [];
    tab.failures = 0;
    tab.error = null;
    tab.cancelling = false;
    tab.running = true;
    this.notify();

    for (let i = 0; i < tab.iterations; i++) {
      if (tab.cancelling || !this.findBenchmarkTab(tabId)) break;
      try {
        const timing = await invoke<WalkTiming>("walk_timed", { oid: tab.oid, connection: this.connectionOf(tab) });
        tab.runs.push(timing);
      } catch (e) {
        tab.failures++;
        tab.error = errorMessage(e);
        if (i === 0) break;
      }
      this.notify();
    }

    tab.running = false;
    tab.cancelling = false;
    this.notify();
  }

  // ---------- trap listener ----------

  /** Starts (or re-starts after a failed attempt) the active trap tab's backend listener. */
  async startTrapListener(paneId: string) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "trap") return;
    try {
      const status = await invoke<TrapListenerStatus>("start_trap_listener", {
        id: tab.id,
        config: {
          bindAddr: tab.bindAddr,
          port: tab.port,
          version: tab.version,
          community: tab.community,
          v3User: tab.v3User,
          v3Auth: tab.v3Auth,
          v3Priv: tab.v3Priv,
        },
      });
      tab.running = status.running;
      tab.boundAddr = status.boundAddr;
      tab.startError = null;
      this.startTrapPolling(tab.id);
    } catch (e) {
      tab.startError = String(e);
    }
    this.notify();
  }

  async stopTrapListenerTab(paneId: string) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "trap") return;
    this.stopTrapPolling(tab.id);
    await invoke("stop_trap_listener", { id: tab.id });
    tab.running = false;
    tab.boundAddr = "";
    this.notify();
  }

  /** Clears a trap tab's received events, both locally and in the backend's ring buffer. */
  async clearTraps(paneId: string) {
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (!tab || tab.kind !== "trap") return;
    tab.events = [];
    tab.expandedSeq = null;
    this.notify();
    await invoke("clear_traps", { id: tab.id });
  }

  toggleTrapExpanded(paneId: string, seq: number) {
    this.updateActiveTrapTabInPane(paneId, (t) => ({ expandedSeq: t.expandedSeq === seq ? null : seq }));
  }

  setTrapFilter(paneId: string, text: string) {
    this.updateActiveTrapTabInPane(paneId, { filterText: text });
  }

  private startTrapPolling(tabId: string) {
    this.stopTrapPolling(tabId);
    const timer = setInterval(() => void this.pollTrapTab(tabId), TRAP_POLL_INTERVAL_MS);
    this.trapPollTimers.set(tabId, timer);
  }

  private stopTrapPolling(tabId: string) {
    const timer = this.trapPollTimers.get(tabId);
    if (timer != null) {
      clearInterval(timer);
      this.trapPollTimers.delete(tabId);
    }
  }

  /** Finds a trap tab by id across every pane - the poll timer only knows the tab id, not which pane it's in. */
  private findTrapTab(tabId: string): TrapTabState | undefined {
    for (const pane of this.state.panes) {
      const t = pane.tabs.find((t) => t.id === tabId);
      if (t && t.kind === "trap") return t;
    }
    return undefined;
  }

  private async pollTrapTab(tabId: string) {
    const tab = this.findTrapTab(tabId);
    if (!tab) {
      this.stopTrapPolling(tabId);
      return;
    }
    if (!tab.running) return;
    try {
      const events = await invoke<TrapEvent[]>("poll_traps", { id: tabId, afterSeq: tab.lastSeq });
      if (events.length === 0) return;
      tab.events = [...tab.events, ...events];
      if (tab.events.length > MAX_CLIENT_TRAP_EVENTS) tab.events = tab.events.slice(tab.events.length - MAX_CLIENT_TRAP_EVENTS);
      tab.lastSeq = events[events.length - 1].seq;
      this.notify();
    } catch {
      // Transient poll failure (e.g. a mid-request app restart) - retried on the next tick.
    }
  }
}
