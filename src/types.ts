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

export interface HostProfile {
  id: string;
  label: string;
  addr: string;
  port: string;
  community: string;
  v3User: string;
}

export type SnmpVersion = "v1" | "v2c" | "v3";

/** A fetched row's columns are whatever the selected MIB table defines - not fixed ahead of time. */
export type Row = Record<string, string>;

export type RowStatus = "added" | "removed" | "changed";

export interface RowMetaEntry {
  status: RowStatus;
  fields?: string[];
}

export type ColWidths = Record<string, number>;

export interface TabState {
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
  sortCol: string;
  sortDir: 1 | -1;
  colWidths: ColWidths;
  autoRefresh: boolean;
  lastFetch: string;
  diffMode: boolean;
  workingRows: Row[];
  rowMeta: Record<string, RowMetaEntry>;
  removedGhosts: Row[];
  fetchError: string | null;
}

export interface PaneState {
  id: string;
  /** Fixed pixel width; null means "flexible" (always true for the last pane). */
  width: number | null;
  activeTabId: string;
  tabs: TabState[];
}

export interface AppState {
  filterText: string;
  expanded: Record<string, boolean>;
  /** When true, the sidebar shows only tables (as roots) with their columns as children, instead of the full group hierarchy. */
  tablesOnlyMode: boolean;
  mibDirs: string[];
  /** Inline text-entry draft for adding a MIB directory outside Tauri (no native picker there); null when not editing. */
  mibDirDraft: string | null;
  parseErrors: FileErrors[];
  parseErrorsOpen: boolean;
  leftWidth: number;
  leftCollapsed: boolean;
  panes: PaneState[];
  activePaneId: string;
}
