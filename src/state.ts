import { invoke, isTauri, pickDirectory } from "./api";
import { DEFAULT_COL_WIDTH, mockHostProfiles } from "./mockData";
import type {
  AppState,
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
} from "./types";

type Patch<T> = Partial<T> | ((t: T) => Partial<T>);

const AUTO_REFRESH_INTERVAL_MS = 10_000;
/** Diffable field ("Index") every fetched row carries, used as its stable identity across fetches. */
const ROW_KEY_FIELD = "Index";
const DEFAULT_SNMP_PORT = "161";
const DEFAULT_SNMP_COMMUNITY = "public";

export class Store {
  state: AppState;
  hostProfiles: HostProfile[] = mockHostProfiles;
  tree: MibNode[] = [];
  tablesTree: MibNode[] = [];

  private listeners: Array<() => void> = [];
  private autoRefreshTimers = new Map<string, ReturnType<typeof setInterval>>();
  /** Epoch ms of each auto-refreshing tab's next fetch, for the countdown ring. */
  private autoRefreshNextAt = new Map<string, number>();
  /** Ticks re-renders while any tab is auto-refreshing, so the countdown ring animates. */
  private uiTickTimer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    this.state = {
      expanded: {},
      selectedTreeNodeId: "",
      tablesOnlyMode: false,
      humanReadableColumns: false,
      useDisplayHints: false,
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
    };
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

  private notify() {
    for (const fn of this.listeners) fn();
  }

  // ---------- tab / pane construction ----------

  makeTab(id: string, hostId: string, opts: Partial<TabState> = {}): TabState {
    // Falls back to blank connection fields when no host profile matches (or
    // none are configured at all) - the fields are freely editable either way.
    const h = this.hostProfiles.find((p) => p.id === hostId) ?? this.hostProfiles[0];
    return {
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
      sortCol: ROW_KEY_FIELD,
      sortDir: 1,
      colWidths: {},
      autoRefresh: false,
      lastFetch: "",
      diffMode: false,
      workingRows: [],
      rowMeta: {},
      removedGhosts: [],
      fetchError: null,
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
  getPaneActiveTab(pane: PaneState): TabState | undefined {
    return pane.tabs.find((t) => t.id === pane.activeTabId) ?? pane.tabs[0];
  }
  getActiveTab(): TabState | undefined {
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
    if (!tab) return;
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

  closeTabInPane(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    this.stopAutoRefresh(tabId);
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
    const newTab: TabState | null = sourceTab ? { ...sourceTab, id: "tab" + Date.now() } : null;
    pane.width = 620;
    const newPane: PaneState = { id: "pane" + Date.now(), width: null, tabs: newTab ? [newTab] : [], activeTabId: newTab?.id ?? null };
    this.state.panes.push(newPane);
    this.state.activePaneId = newPane.id;
    if (newTab?.autoRefresh) this.startAutoRefresh(newPane.id, newTab.id);
    this.notify();
  }

  closePane(paneId: string) {
    if (this.state.panes.length <= 1) return;
    const pane = this.getPane(paneId);
    pane?.tabs.forEach((t) => this.stopAutoRefresh(t.id));
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

  toggleExpand(id: string) {
    this.state.expanded[id] = !this.state.expanded[id];
    this.notify();
  }

  setTablesOnlyMode(value: boolean) {
    this.state.tablesOnlyMode = value;
    this.notify();
  }

  toggleHumanReadableColumns() {
    this.state.humanReadableColumns = !this.state.humanReadableColumns;
    this.notify();
  }

  toggleUseDisplayHints() {
    this.state.useDisplayHints = !this.state.useDisplayHints;
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

  async loadMibTree() {
    const result = await invoke<ParseResult>("get_mib_tree");
    this.tree = result.tree;
    this.tablesTree = result.tablesTree;
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

  /** Whether a tab's connection fields are filled in enough to attempt a fetch. */
  hasCompleteConnection(tab: TabState): boolean {
    if (!tab.hostAddr.trim() || !tab.hostPort.trim() || !tab.version) return false;
    return tab.version === "v3" ? !!tab.v3User.trim() : !!tab.community.trim();
  }

  // ---------- diff mode / fetch ----------

  toggleDiffMode(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ diffMode: !t.diffMode }));
  }

  setAutoRefresh(paneId: string, value: boolean) {
    this.state.refreshMenu = null;
    const pane = this.getPane(paneId);
    const tab = pane && this.getPaneActiveTab(pane);
    if (tab && tab.autoRefresh !== value) {
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
    if (!this.uiTickTimer) this.uiTickTimer = setInterval(() => this.notify(), 200);
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
    if (!pane || !tab) {
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
    if (!tab) return;
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
      const result = await invoke<{ columns: string[]; rows: Row[]; displayHints: Record<string, string> }>("fetch", {
        nodeId: tab.selectedNode,
        connection: {
          hostAddr: tab.hostAddr,
          hostPort: tab.hostPort,
          version: tab.version,
          community: tab.community,
          v3User: tab.v3User,
          v3Auth: tab.v3Auth,
          v3Priv: tab.v3Priv,
        },
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
}
