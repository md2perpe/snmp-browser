import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { DEFAULT_COL_WIDTH, mockHostProfiles } from "./mockData";
import type { AppState, HostProfile, MibNode, PaneState, ParseResult, Row, RowMetaEntry, SnmpVersion, TabState } from "./types";

type Patch<T> = Partial<T> | ((t: T) => Partial<T>);

const AUTO_REFRESH_INTERVAL_MS = 10_000;
/** Diffable field ("Index") every fetched row carries, used as its stable identity across fetches. */
const ROW_KEY_FIELD = "Index";

export class Store {
  state: AppState;
  hostProfiles: HostProfile[] = mockHostProfiles;
  tree: MibNode[] = [];

  private listeners: Array<() => void> = [];
  private autoRefreshTimers = new Map<string, ReturnType<typeof setInterval>>();

  constructor() {
    const tab1 = this.makeTab("t1", "h1");
    const tab2 = this.makeTab("t2", "h2", { autoRefresh: false });
    this.state = {
      filterText: "",
      expanded: {},
      mibDirs: [],
      parseErrors: [],
      parseErrorsOpen: false,
      leftWidth: 330,
      leftCollapsed: false,
      panes: [
        { id: "p1", width: 620, activeTabId: tab1.id, tabs: [tab1] },
        { id: "p2", width: null, activeTabId: tab2.id, tabs: [tab2] },
      ],
      activePaneId: "p1",
    };
  }

  async init() {
    const [mibDirs, hostProfiles] = await Promise.all([invoke<string[]>("list_mib_dirs"), invoke<HostProfile[]>("list_host_profiles")]);
    this.state.mibDirs = mibDirs;
    this.hostProfiles = hostProfiles;
    this.notify();
    await this.loadMibTree();
  }

  onChange(fn: () => void) {
    this.listeners.push(fn);
  }

  private notify() {
    for (const fn of this.listeners) fn();
  }

  // ---------- tab / pane construction ----------

  makeTab(id: string, hostId: string, opts: Partial<TabState> = {}): TabState {
    const h = this.hostProfiles.find((p) => p.id === hostId) ?? this.hostProfiles[0];
    return {
      id,
      hostId: h.id,
      hostAddr: h.addr,
      hostPort: h.port,
      version: "v2c",
      community: h.community,
      v3User: h.v3User,
      v3Auth: "",
      v3Priv: "",
      selectedNode: "",
      columns: [],
      sortCol: ROW_KEY_FIELD,
      sortDir: 1,
      colWidths: {},
      autoRefresh: true,
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
  getPaneActiveTab(pane: PaneState): TabState {
    return pane.tabs.find((t) => t.id === pane.activeTabId) ?? pane.tabs[0];
  }
  getActiveTab(): TabState {
    return this.getPaneActiveTab(this.getActivePane());
  }

  private applyPatch<T extends object>(obj: T, patch: Patch<T>) {
    const p = typeof patch === "function" ? patch(obj) : patch;
    Object.assign(obj, p);
  }

  updateActiveTabInPane(paneId: string, patch: Patch<TabState>) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    this.applyPatch(this.getPaneActiveTab(pane), patch);
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

  addTabToPane(paneId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.makeTab("tab" + Date.now(), "h1");
    pane.tabs.push(tab);
    pane.activeTabId = tab.id;
    if (tab.autoRefresh) this.startAutoRefresh(paneId, tab.id);
    this.notify();
  }

  closeTabInPane(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    this.stopAutoRefresh(tabId);
    if (pane.tabs.length <= 1) {
      this.closePane(paneId);
      return;
    }
    pane.tabs = pane.tabs.filter((t) => t.id !== tabId);
    if (pane.activeTabId === tabId) pane.activeTabId = pane.tabs[pane.tabs.length - 1].id;
    this.notify();
  }

  splitPane(paneId: string) {
    if (this.state.panes.length >= 2) return;
    const pane = this.getPane(paneId);
    if (!pane) return;
    const sourceTab = this.getPaneActiveTab(pane);
    const newTab: TabState = { ...sourceTab, id: "tab" + Date.now() };
    pane.width = 620;
    const newPane: PaneState = { id: "pane" + Date.now(), width: null, tabs: [newTab], activeTabId: newTab.id };
    this.state.panes.push(newPane);
    this.state.activePaneId = newPane.id;
    if (newTab.autoRefresh) this.startAutoRefresh(newPane.id, newTab.id);
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

  setFilterText(text: string) {
    this.state.filterText = text;
    this.notify();
  }

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

  selectNode(paneId: string, node: MibNode) {
    this.updateActiveTabInPane(paneId, { selectedNode: node.id });
    if (node.children) this.toggleExpand(node.id);
  }

  async loadMibTree() {
    const result = await invoke<ParseResult>("get_mib_tree");
    this.tree = result.tree;
    this.state.parseErrors = result.errors;
    if (result.errors.length === 0) this.state.parseErrorsOpen = false;
    this.notify();
  }

  async addMibDir() {
    const selected = await open({ directory: true, multiple: false });
    if (!selected || Array.isArray(selected)) return;
    this.state.mibDirs = await invoke<string[]>("add_mib_dir", { path: selected });
    this.notify();
    await this.loadMibTree();
  }

  async removeMibDir(path: string) {
    this.state.mibDirs = await invoke<string[]>("remove_mib_dir", { path });
    this.notify();
    await this.loadMibTree();
  }

  getVisibleNodes(): { node: MibNode; depth: number }[] {
    const filter = this.state.filterText.trim().toLowerCase();
    if (!filter) return this.flatten(this.tree, 0, [], this.state.expanded);
    // A filter searches the whole tree regardless of collapse state - a
    // search box that only finds nodes you'd already expanded manually
    // isn't useful, since everything starts collapsed.
    return this.flattenAll(this.tree, 0, []).filter(({ node }) => node.label.toLowerCase().includes(filter));
  }

  private flatten(nodes: MibNode[], depth: number, out: { node: MibNode; depth: number }[], expanded: Record<string, boolean>) {
    for (const n of nodes) {
      out.push({ node: n, depth });
      if (n.children && expanded[n.id]) this.flatten(n.children, depth + 1, out, expanded);
    }
    return out;
  }

  private flattenAll(nodes: MibNode[], depth: number, out: { node: MibNode; depth: number }[]) {
    for (const n of nodes) {
      out.push({ node: n, depth });
      if (n.children) this.flattenAll(n.children, depth + 1, out);
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

  // ---------- host / connection fields ----------

  selectHost(paneId: string, hostId: string) {
    const h = this.hostProfiles.find((p) => p.id === hostId);
    if (h) {
      this.updateActiveTabInPane(paneId, { hostId, hostAddr: h.addr, hostPort: h.port, community: h.community, v3User: h.v3User });
    } else {
      this.updateActiveTabInPane(paneId, { hostId });
    }
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

  // ---------- diff mode / fetch ----------

  toggleDiffMode(paneId: string) {
    this.updateActiveTabInPane(paneId, (t) => ({ diffMode: !t.diffMode }));
  }

  toggleAutoRefresh(paneId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    const tab = this.getPaneActiveTab(pane);
    tab.autoRefresh = !tab.autoRefresh;
    if (tab.autoRefresh) this.startAutoRefresh(paneId, tab.id);
    else this.stopAutoRefresh(tab.id);
    this.notify();
  }

  private startAutoRefresh(paneId: string, tabId: string) {
    this.stopAutoRefresh(tabId);
    const timer = setInterval(() => void this.fetchForTab(paneId, tabId), AUTO_REFRESH_INTERVAL_MS);
    this.autoRefreshTimers.set(tabId, timer);
  }

  private stopAutoRefresh(tabId: string) {
    const timer = this.autoRefreshTimers.get(tabId);
    if (timer != null) {
      clearInterval(timer);
      this.autoRefreshTimers.delete(tabId);
    }
  }

  private async fetchForTab(paneId: string, tabId: string) {
    const pane = this.getPane(paneId);
    const tab = pane?.tabs.find((t) => t.id === tabId);
    if (!pane || !tab) {
      this.stopAutoRefresh(tabId);
      return;
    }
    await this.runFetch(tab);
    this.notify();
  }

  /** Manual "Fetch Table" click for a pane's active tab. */
  async manualFetch(paneId: string) {
    const pane = this.getPane(paneId);
    if (!pane) return;
    await this.runFetch(this.getPaneActiveTab(pane));
    this.notify();
  }

  private async runFetch(tab: TabState) {
    const node = this.findNode(this.tree, tab.selectedNode);
    if (!this.canFetch(node)) {
      tab.fetchError = node ? `'${node.label}' can't be fetched` : "Select an OID first";
      return;
    }
    try {
      const result = await invoke<{ columns: string[]; rows: Row[] }>("fetch", {
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
