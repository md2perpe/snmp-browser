export type NodeType = "group" | "scalar" | "table";

export interface MibNode {
  id: string;
  label: string;
  /** Absolute dotted OID, or "" if it couldn't be resolved. */
  oid: string;
  resolved: boolean;
  type: NodeType;
  children?: MibNode[];
}

export interface FileErrors {
  file: string;
  errors: string[];
}

export interface ParseResult {
  tree: MibNode[];
  /** Alternate view: every table as a root, its columns as children. */
  tablesTree: MibNode[];
  errors: FileErrors[];
}

export interface MibProfile {
  id: string;
  name: string;
  dirs: string[];
}

export interface MibProfilesResponse {
  profiles: MibProfile[];
  activeProfileId: string;
}

export interface HostProfile {
  id: string;
  label: string;
  addr: string;
  port: string;
  community: string;
  v3User: string;
}

export type SnmpVersion = "v1" | "v2c" | "v3";

export type Theme = "dark" | "classic";

/** A fetched row's columns are whatever the selected MIB table defines - not fixed ahead of time. */
export type Row = Record<string, string>;

export type RowStatus = "added" | "removed" | "changed";

export interface RowMetaEntry {
  status: RowStatus;
  fields?: string[];
}

export type ColWidths = Record<string, number>;

export interface TabState {
  kind: "query";
  id: string;
  hostId: string;
  hostAddr: string;
  hostPort: string;
  version: SnmpVersion;
  community: string;
  v3User: string;
  v3Auth: string;
  v3Priv: string;
  selectedNode: string;
  /** Column order from the last successful fetch; empty until the first fetch. */
  columns: string[];
  /** DISPLAY-HINT per column that has one (e.g. "d-1"), from the last successful fetch. */
  displayHints: Record<string, string>;
  sortCol: string;
  sortDir: 1 | -1;
  colWidths: ColWidths;
  autoRefresh: boolean;
  lastFetch: string;
  diffMode: boolean;
  /** When true, table column headers show a humanized form (shared prefix stripped, camelCase split into title-cased words) instead of the raw MIB identifier. */
  humanReadableColumns: boolean;
  /** When true, numeric values in a column with a DISPLAY-HINT (e.g. "d-1") are shown reformatted (123 -> 12.3) instead of raw. */
  useDisplayHints: boolean;
  workingRows: Row[];
  rowMeta: Record<string, RowMetaEntry>;
  removedGhosts: Row[];
  fetchError: string | null;
}

export interface TrapVarbind {
  oid: string;
  /** MIB-resolved name (e.g. "ifDescr.3"), or the same as `oid` when nothing matched. */
  name: string;
  value: string;
}

export interface TrapEvent {
  seq: number;
  timeMs: number;
  /** "ip:port" the packet arrived from. */
  source: string;
  version: string;
  /** Community string (v1/v2c) or security user name (v3). */
  principal: string;
  trapType: string;
  trapOid: string;
  varbinds: TrapVarbind[];
  /** True for an SNMPv2c/v3 Inform, which RFC 3416 expects to be acknowledged; this listener never sends that ack. */
  confirmed: boolean;
  error: string | null;
}

export interface TrapListenerStatus {
  running: boolean;
  boundAddr: string;
}

export interface TrapTabState {
  kind: "trap";
  id: string;
  bindAddr: string;
  port: string;
  version: SnmpVersion;
  /** v1/v2c only: exact community a packet must carry to be accepted; empty accepts any. */
  community: string;
  v3User: string;
  v3Auth: string;
  v3Priv: string;
  running: boolean;
  /** The actual bound "ip:port" once started (e.g. after binding port 0). */
  boundAddr: string;
  /** Set when the last start attempt failed (bad address, port in use, ...). */
  startError: string | null;
  events: TrapEvent[];
  /** Highest event `seq` already merged in, so polling only asks for what's new. */
  lastSeq: number;
  expandedSeq: number | null;
  filterText: string;
}

export type AnyTabState = TabState | TrapTabState;

export interface PaneState {
  id: string;
  /** Fixed pixel width; null means "flexible" (always true for the last pane). */
  width: number | null;
  /** null when the pane has no tabs open. */
  activeTabId: string | null;
  tabs: AnyTabState[];
}

export interface AppState {
  expanded: Record<string, boolean>;
  /** Id of the tree row highlighted by a single click; independent of any tab's `selectedNode` (which a double-click sets). */
  selectedTreeNodeId: string;
  /** When true, the sidebar shows only tables (as roots) with their columns as children, instead of the full group hierarchy. */
  tablesOnlyMode: boolean;
  /** Named sets of MIB directories (e.g. one per software release) - only the active one's directories are parsed. */
  mibProfiles: MibProfile[];
  activeMibProfileId: string;
  /** Inline text-entry draft for adding a MIB directory outside Tauri (no native picker there); null when not editing. */
  mibDirDraft: string | null;
  /** Inline text-entry draft for naming a new MIB profile; null when not editing. */
  mibProfileDraft: string | null;
  /** True while the active MIB profile's name is being edited inline. */
  renamingMibProfile: boolean;
  parseErrors: FileErrors[];
  parseErrorsOpen: boolean;
  leftWidth: number;
  leftCollapsed: boolean;
  panes: PaneState[];
  activePaneId: string;
  /** Right-click context menu on a tree node; null when closed. */
  treeContextMenu: { x: number; y: number; nodeId: string } | null;
  /** Fetch mode dropdown (manual vs. auto-refresh) for a pane's split button; null when closed. */
  refreshMenu: { paneId: string; x: number; y: number } | null;
  theme: Theme;
  /** Theme picker dropdown; null when closed. */
  themeMenu: { x: number; y: number } | null;
}
