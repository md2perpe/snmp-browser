import { cssEscape, el, startDrag } from "./dom";
import type { Store } from "./state";
import type { MibNode, PaneState, Row, SnmpVersion, TabState } from "./types";

function nodeIcon(node: MibNode): HTMLElement {
  const color = node.type === "table" ? "var(--icon-table)" : node.type === "scalar" ? "var(--icon-scalar)" : "var(--icon-group)";
  if (node.type === "group") {
    return el("div", { class: "tree-icon-group" }, [
      el("div", { class: "lid", style: { background: color } }),
      el("div", { class: "body", style: { background: color } }),
    ]);
  }
  if (node.type === "scalar") {
    return el("div", { class: "tree-icon-scalar", style: { background: color } });
  }
  return el(
    "div",
    { class: "tree-icon-table" },
    [0, 1, 2, 3].map(() => el("div", { style: { background: color } })),
  );
}

function renderTreeRow(store: Store, node: MibNode, depth: number, selectedNodeId: string): HTMLElement {
  const selected = node.id === selectedNodeId;
  const caretVisible = !!node.children;
  const expanded = !!store.state.expanded[node.id];
  const textColor = selected ? "#ffffff" : node.type === "group" ? "var(--text-secondary)" : "#a9aab0";
  const fontWeight = selected ? "600" : node.type === "group" ? "600" : "400";
  const bg = selected ? "var(--accent-selected-bg)" : "transparent";
  const title = node.resolved ? node.oid : `Could not resolve an absolute OID for '${node.label}' - its ancestor chain isn't fully defined in the configured MIB directories.`;
  return el(
    "div",
    {
      class: "tree-row",
      title,
      "data-node-id": node.id,
      style: { paddingLeft: depth * 16 + 2 + "px", opacity: node.resolved ? "1" : "0.45" },
      onclick: () => {
        store.selectNode(store.state.activePaneId, node);
        // selectNode's notify() rebuilds the whole tree synchronously (see
        // renderPreservingFocus), which resets .tree's scrollTop to 0 - scroll
        // the clicked row back into view at the top instead of leaving `iso`
        // pinned there.
        document.querySelector(`.tree-row[data-node-id="${cssEscape(node.id)}"]`)?.scrollIntoView({ block: "start" });
      },
      oncontextmenu:
        node.type === "table"
          ? (e: MouseEvent) => {
              e.preventDefault();
              store.openTreeContextMenu(e.clientX, e.clientY, node.id);
            }
          : undefined,
    },
    [
      el(
        "div",
        { class: "tree-row-bg", style: { background: bg } },
        [
          el(
            "div",
            {
              class: "tree-caret",
              style: { visibility: caretVisible ? "visible" : "hidden", transform: `rotate(${caretVisible && expanded ? 90 : 0}deg)` },
              onclick: caretVisible
                ? (e: MouseEvent) => {
                    e.stopPropagation();
                    store.toggleExpand(node.id);
                  }
                : undefined,
            },
            ["▶"],
          ),
          nodeIcon(node),
          el("div", { class: "tree-label", style: { color: textColor, fontWeight } }, [node.label]),
        ],
      ),
    ],
  );
}

function renderSidebar(store: Store): HTMLElement {
  const selectedNodeId = store.getActiveTab()?.selectedNode ?? "";
  const visibleNodes = store.getVisibleNodes();
  const errors = store.state.parseErrors;

  const dirRows: HTMLElement[] = store.state.mibDirs.map((dir) =>
    el("div", { class: "mib-dir-row" }, [
      el("div", { class: "mib-dir-icon" }),
      el("div", { class: "mib-dir-path", title: dir }, [dir]),
      el("button", { class: "mib-dir-remove", title: "Remove directory", onclick: () => store.removeMibDir(dir) }, ["×"]),
    ]),
  );
  if (errors.length) {
    const issueCount = errors.reduce((n, fe) => n + fe.errors.length, 0);
    dirRows.push(
      el(
        "button",
        { class: "mib-dir-warning", onclick: () => store.toggleParseErrors() },
        [`⚠ ${issueCount} issue${issueCount === 1 ? "" : "s"} while parsing`],
      ),
    );
  }

  const draft = store.state.mibDirDraft;
  const draftRow =
    draft === null
      ? null
      : el("div", { class: "mib-dir-draft-row" }, [
          el("input", {
            class: "mib-dir-draft-input",
            placeholder: "/absolute/path/to/mibs",
            value: draft,
            "data-focus-key": "mibDirDraft",
            oninput: (e: Event) => store.updateMibDirDraft((e.target as HTMLInputElement).value),
            onkeydown: (e: KeyboardEvent) => {
              if (e.key === "Enter") void store.submitMibDirDraft();
              else if (e.key === "Escape") store.cancelMibDirDraft();
            },
          }),
          el("button", { class: "mib-dir-draft-confirm", title: "Add", onclick: () => void store.submitMibDirDraft() }, ["✓"]),
          el("button", { class: "mib-dir-draft-cancel", title: "Cancel", onclick: () => store.cancelMibDirDraft() }, ["×"]),
        ]);

  return el("div", { class: "sidebar", style: { width: store.state.leftWidth + "px" } }, [
    el("div", { class: "sidebar-header" }, [
      el("button", { class: "icon-btn", title: "Hide sidebar", onclick: () => store.toggleLeft() }, ["▤"]),
    ]),
    el("div", { class: "mib-dirs" }, [
      el("div", { class: "mib-dirs-head" }, [
        el("div", { class: "mib-dirs-title" }, ["MIB Directories"]),
        el(
          "button",
          {
            class: "mib-dir-add",
            title: "Add MIB directory",
            onclick: () => {
              // addMibDir() runs synchronously up to its first `await` (only taken
              // on the native-picker path), so the draft input already exists
              // in the DOM by the time this call returns in browser mode.
              void store.addMibDir();
              document.querySelector<HTMLInputElement>(".mib-dir-draft-input")?.focus();
            },
          },
          ["+"],
        ),
      ]),
      el("div", { class: "mib-dir-list" }, dirRows),
      draftRow,
    ]),
    el("div", { class: "tree-mode-box" }, [
      el(
        "div",
        { class: "tree-mode-toggle" },
        [
          { label: "Tree", value: false },
          { label: "Tables", value: true },
        ].map(({ label, value }) =>
          el(
            "button",
            {
              class: "tree-mode-btn" + (store.state.tablesOnlyMode === value ? " active" : ""),
              onclick: () => store.setTablesOnlyMode(value),
            },
            [label],
          ),
        ),
      ),
    ]),
    el(
      "div",
      { class: "tree", "data-preserve-scroll": "tree" },
      visibleNodes.map(({ node, depth }) => renderTreeRow(store, node, depth, selectedNodeId)),
    ),
  ]);
}

function renderParseErrorsModal(store: Store): HTMLElement {
  const groups = store.state.parseErrors.map((fe) =>
    el("div", { class: "parse-error-group" }, [
      el("div", { class: "parse-error-file" }, [fe.file]),
      el(
        "ul",
        { class: "parse-error-list" },
        fe.errors.map((e) => el("li", {}, [e])),
      ),
    ]),
  );

  return el("div", { class: "modal-overlay", onclick: () => store.toggleParseErrors() }, [
    el("div", { class: "modal-panel", onclick: (e: Event) => e.stopPropagation() }, [
      el("div", { class: "modal-header" }, [
        el("div", { class: "modal-title" }, ["Parse issues"]),
        el("button", { class: "icon-btn", title: "Close", onclick: () => store.toggleParseErrors() }, ["✕"]),
      ]),
      el("div", { class: "modal-body" }, groups),
    ]),
  ]);
}

function renderTreeContextMenu(store: Store): HTMLElement | null {
  const menu = store.state.treeContextMenu;
  if (!menu) return null;
  const node = store.findNode(store.activeTree(), menu.nodeId);

  return el(
    "div",
    { class: "context-menu-overlay", onclick: () => store.closeTreeContextMenu(), oncontextmenu: (e: Event) => e.preventDefault() },
    [
      el(
        "div",
        { class: "context-menu", style: { left: menu.x + "px", top: menu.y + "px" }, onclick: (e: Event) => e.stopPropagation() },
        [
          el(
            "button",
            { class: "context-menu-item", onclick: () => store.openNodeInNewTab(menu.nodeId) },
            [`Open "${node?.label ?? menu.nodeId}" in new tab`],
          ),
        ],
      ),
    ],
  );
}

function renderCollapsedRail(store: Store): HTMLElement {
  return el("div", { class: "sidebar-rail" }, [
    el("button", { class: "icon-btn", title: "Show sidebar", onclick: () => store.toggleLeft() }, ["▤"]),
  ]);
}

function renderSplitter(onMouseDown: (e: MouseEvent) => void): HTMLElement {
  return el("div", { class: "splitter", onmousedown: onMouseDown }, [el("div", { class: "splitter-line" })]);
}

function renderTabBar(store: Store, pane: PaneState): HTMLElement {
  const tabs = pane.tabs.map((tab) => {
    const active = tab.id === pane.activeTabId;
    const host = store.hostProfiles.find((h) => h.id === tab.hostId);
    const label = (host ? host.label : tab.hostId) + " · " + tab.selectedNode;
    return el(
      "div",
      {
        class: "tab",
        style: {
          borderBottomColor: active ? "var(--accent)" : "transparent",
          background: active ? "var(--bg-sidebar)" : "transparent",
          color: active ? "var(--text-primary)" : "var(--text-muted)",
          fontWeight: active ? "600" : "400",
        },
        onclick: () => store.selectTab(pane.id, tab.id),
      },
      [
        el("div", { class: "tab-dot" }),
        el("div", { class: "tab-label" }, [label]),
        el(
          "button",
          {
            class: "tab-close",
            onclick: (e: Event) => {
              e.stopPropagation();
              store.closeTabInPane(pane.id, tab.id);
            },
          },
          ["×"],
        ),
      ],
    );
  });

  const canSplit = store.state.panes.length < 2;
  const canClosePane = store.state.panes.length > 1;

  const bar: (HTMLElement | null)[] = [
    ...tabs,
    el("button", { class: "tab-add", onclick: () => store.addTabToPane(pane.id) }, ["+"]),
    el("div", { class: "tab-bar-spacer" }),
    canSplit ? el("button", { class: "pane-action", title: "Split right", onclick: () => store.splitPane(pane.id) }, ["⊟"]) : null,
    canClosePane ? el("button", { class: "pane-action", title: "Close group", onclick: () => store.closePane(pane.id) }, ["✕"]) : null,
  ];

  return el("div", { class: "tab-bar" }, bar);
}

function renderToolbar(store: Store, pane: PaneState, tab: TabState): HTMLElement {
  const isV3 = tab.version === "v3";

  const versionRow = el(
    "div",
    { class: "version-toggle" },
    (["v1", "v2c", "v3"] as SnmpVersion[]).map((v) =>
      el(
        "button",
        {
          class: "version-btn" + (tab.version === v ? " active" : ""),
          onclick: () => store.setVersion(pane.id, v),
        },
        [v],
      ),
    ),
  );

  const fields: HTMLElement[] = [
    el("div", { class: "field" }, [
      el("label", { class: "field-label" }, ["Host"]),
      el(
        "select",
        {
          class: "field-select",
          value: tab.hostId,
          "data-focus-key": `pane:${pane.id}:host`,
          onchange: (e: Event) => store.selectHost(pane.id, (e.target as HTMLSelectElement).value),
        },
        [
          ...store.hostProfiles.map((h) => el("option", { value: h.id }, [h.label])),
          el("option", { value: "__new" }, ["+ New host…"]),
        ],
      ),
    ]),
    el("div", { class: "field" }, [
      el("label", { class: "field-label" }, ["Address"]),
      el("input", {
        class: "field-input field-addr field-mono",
        value: tab.hostAddr,
        "data-focus-key": `pane:${pane.id}:addr`,
        oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { hostAddr: (e.target as HTMLInputElement).value }),
      }),
    ]),
    el("div", { class: "field" }, [
      el("label", { class: "field-label" }, ["Port"]),
      el("input", {
        class: "field-input field-port field-mono",
        value: tab.hostPort,
        "data-focus-key": `pane:${pane.id}:port`,
        oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { hostPort: (e.target as HTMLInputElement).value }),
      }),
    ]),
    el("div", { class: "field" }, [el("label", { class: "field-label" }, ["Version"]), versionRow]),
  ];

  if (isV3) {
    fields.push(
      el("div", { class: "v3-fields" }, [
        el("div", { class: "field" }, [
          el("label", { class: "field-label" }, ["Security User"]),
          el("input", {
            class: "field-input field-v3-user",
            value: tab.v3User,
            "data-focus-key": `pane:${pane.id}:v3user`,
            oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { v3User: (e.target as HTMLInputElement).value }),
          }),
        ]),
        el("div", { class: "field" }, [
          el("label", { class: "field-label" }, ["Auth"]),
          el("input", {
            type: "password",
            class: "field-input field-v3-secret",
            value: tab.v3Auth,
            "data-focus-key": `pane:${pane.id}:v3auth`,
            oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { v3Auth: (e.target as HTMLInputElement).value }),
          }),
        ]),
        el("div", { class: "field" }, [
          el("label", { class: "field-label" }, ["Priv"]),
          el("input", {
            type: "password",
            class: "field-input field-v3-secret",
            value: tab.v3Priv,
            "data-focus-key": `pane:${pane.id}:v3priv`,
            oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { v3Priv: (e.target as HTMLInputElement).value }),
          }),
        ]),
      ]),
    );
  } else {
    fields.push(
      el("div", { class: "field" }, [
        el("label", { class: "field-label" }, ["Community"]),
        el("input", {
          class: "field-input field-community field-mono",
          value: tab.community,
          "data-focus-key": `pane:${pane.id}:community`,
          oninput: (e: Event) => store.updateActiveTabInPane(pane.id, { community: (e.target as HTMLInputElement).value }),
        }),
      ]),
    );
  }

  const selectedNode = store.findNode(store.activeTree(), tab.selectedNode);
  const canFetch = store.canFetch(selectedNode);

  fields.push(el("div", { class: "spacer" }));
  fields.push(
    el(
      "button",
      {
        class: "fetch-btn",
        disabled: !canFetch,
        title: canFetch ? "" : "Select a resolvable scalar or table in the tree first",
        onclick: () => store.manualFetch(pane.id),
      },
      ["Fetch"],
    ),
  );

  return el("div", { class: "toolbar" }, [el("div", { class: "toolbar-row" }, fields)]);
}

function renderTableToolbar(store: Store, pane: PaneState, tab: TabState): HTMLElement {
  const node = store.findNode(store.activeTree(), tab.selectedNode);
  const label = node ? node.label : "(nothing selected)";
  const oid = node ? node.oid || "(unresolved)" : "";

  const children: HTMLElement[] = [
    el("div", { class: "selected-node-label" }, [label]),
    el("div", { class: "selected-node-oid" }, [oid]),
  ];
  if (tab.diffMode) {
    children.push(
      el("div", { class: "diff-legend" }, [
        el("span", { class: "swatch added" }, ["Added"]),
        el("span", { class: "swatch removed" }, ["Removed"]),
        el("span", { class: "swatch changed" }, ["Changed"]),
      ]),
    );
  }
  children.push(el("div", { class: "spacer" }));
  children.push(
    el("label", { class: "toggle-label", onclick: () => store.toggleDiffMode(pane.id) }, [
      el("div", { class: "toggle-track" + (tab.diffMode ? " on" : "") }, [el("div", { class: "toggle-knob" })]),
      "Diff mode",
    ]),
  );
  children.push(
    el("label", { class: "toggle-label", onclick: () => store.toggleAutoRefresh(pane.id) }, [
      el("div", { class: "toggle-track" + (tab.autoRefresh ? " on" : "") }, [el("div", { class: "toggle-knob" })]),
      "Auto-refresh (10s)",
    ]),
  );
  children.push(el("button", { class: "export-btn" }, ["Export…"]));

  return el("div", { class: "table-toolbar" }, children);
}

/** A couple of literal values read like a status enum regardless of which MIB table they came from. */
function statusColor(value: string): string | null {
  const v = value.toLowerCase();
  if (v === "up" || v.startsWith("up(")) return "var(--green)";
  if (v === "down" || v.startsWith("down(")) return "var(--red)";
  return null;
}

function renderCell(colKey: string, row: Row, changed: boolean): HTMLTableCellElement {
  const bg = changed ? "var(--yellow-bg)" : "transparent";
  const value = row[colKey] ?? "";
  const color = statusColor(value);
  if (color) {
    return el("td", { style: { background: bg } }, [
      el("span", { class: "status-chip", style: { color } }, [el("span", { class: "status-dot", style: { background: color } }), value]),
    ]);
  }
  return el("td", { style: { background: bg } }, [value]);
}

function renderTable(store: Store, pane: PaneState, tab: TabState): HTMLElement {
  if (tab.columns.length === 0) {
    return el("div", { class: "table-scroll" }, [el("div", { class: "table-empty" }, [tab.fetchError ?? "No data yet - click Fetch."])]);
  }

  const headRow = el(
    "tr",
    {},
    tab.columns.map((col) => {
      const startWidth = store.colWidth(tab, col);
      const sorted = tab.sortCol === col;
      return el("th", { style: { width: startWidth + "px" } }, [
        el("div", { class: "th-btn" + (sorted ? " sorted" : ""), onclick: () => store.setSortCol(pane.id, col) }, [
          col,
          sorted ? el("span", { style: { fontSize: "9px" } }, [tab.sortDir === 1 ? "▲" : "▼"]) : null,
        ]),
        el("div", {
          class: "th-resize-handle",
          onmousedown: (e: MouseEvent) => startDrag(e, (dx) => store.setColWidth(pane.id, col, startWidth + dx)),
        }),
      ]);
    }),
  );

  const rows = store.getSortedRows(tab).map((row) => {
    const meta = store.rowMetaFor(tab, row);
    const status = meta?.status;
    const rowBg = status === "added" ? "var(--added-bg)" : status === "removed" ? "var(--removed-bg)" : "transparent";
    const opacity = status === "removed" ? "0.55" : "1";
    const textDecoration = status === "removed" ? "line-through" : "none";
    const changedFields = meta?.fields ?? [];
    return el(
      "tr",
      { style: { background: rowBg, opacity, textDecoration } },
      tab.columns.map((col) => renderCell(col, row, changedFields.includes(col))),
    );
  });

  return el("div", { class: "table-scroll" }, [
    el("table", { class: "data-table" }, [el("thead", {}, [headRow]), el("tbody", {}, rows)]),
  ]);
}

function renderStatusBar(tab: TabState): HTMLElement {
  const rowCount = tab.workingRows.length;
  const versionLabel = tab.version.replace("v", "");
  const connCell = tab.fetchError
    ? el("div", { class: "status-conn", title: tab.fetchError }, [el("span", { class: "status-conn-dot error" }), tab.fetchError])
    : el("div", { class: "status-conn" }, [el("span", { class: "status-conn-dot" }), `Connected to ${tab.hostAddr}:${tab.hostPort}`]);
  return el("div", { class: "status-bar" }, [
    connCell,
    el("div", {}, [`${rowCount} rows`]),
    el("div", {}, [tab.lastFetch ? `Last fetch: ${tab.lastFetch}` : "Not fetched yet"]),
    el("div", { class: "spacer" }),
    el("div", {}, [`SNMPv${versionLabel}`]),
  ]);
}

function renderEmptyPane(store: Store, pane: PaneState): HTMLElement {
  return el("div", { class: "pane-empty" }, [
    el("div", { class: "pane-empty-text" }, ["No tab open"]),
    el("button", { class: "pane-empty-btn", onclick: () => store.addTabToPane(pane.id) }, ["+ Open a tab"]),
  ]);
}

function renderPane(store: Store, pane: PaneState, isLast: boolean): HTMLElement {
  const tab = store.getPaneActiveTab(pane);
  const flexCss = isLast ? "1 1 0%" : `0 0 ${pane.width}px`;
  const body = tab
    ? [renderToolbar(store, pane, tab), renderTableToolbar(store, pane, tab), renderTable(store, pane, tab), renderStatusBar(tab)]
    : [renderEmptyPane(store, pane)];
  return el(
    "div",
    { class: "pane", style: { flex: flexCss }, onclick: () => store.focusPane(pane.id) },
    [renderTabBar(store, pane), ...body],
  );
}

function renderPaneGroup(store: Store): HTMLElement {
  const panes = store.state.panes;
  const children: HTMLElement[] = [];
  panes.forEach((pane, idx) => {
    const isLast = idx === panes.length - 1;
    children.push(renderPane(store, pane, isLast));
    if (!isLast) {
      const startWidth = pane.width ?? 620;
      children.push(renderSplitter((e) => startDrag(e, (dx) => store.setPaneWidth(pane.id, startWidth + dx))));
    }
  });
  return el("div", { class: "pane-group" }, children);
}

export function renderApp(store: Store): HTMLElement {
  const bodyChildren: HTMLElement[] = [];
  if (!store.state.leftCollapsed) {
    const startWidth = store.state.leftWidth;
    bodyChildren.push(renderSidebar(store));
    bodyChildren.push(renderSplitter((e) => startDrag(e, (dx) => store.setLeftWidth(startWidth + dx))));
  } else {
    bodyChildren.push(renderCollapsedRail(store));
  }
  bodyChildren.push(renderPaneGroup(store));
  const appBody = el("div", { class: "app-body" }, bodyChildren);

  const overlays: HTMLElement[] = [];
  if (store.state.parseErrorsOpen && store.state.parseErrors.length > 0) overlays.push(renderParseErrorsModal(store));
  const contextMenu = renderTreeContextMenu(store);
  if (contextMenu) overlays.push(contextMenu);
  if (overlays.length === 0) return appBody;

  // Wrapped in a `display: contents` div so fixed-position overlays sit
  // alongside `.app-body` without becoming a flex child of `#app` itself.
  return el("div", { style: { display: "contents" } }, [appBody, ...overlays]);
}
