# SNMP MIB Client — Usage Guide

A macOS app for browsing MIB files and polling live values from SNMP-managed
devices (switches, routers, and similar network equipment).

## Installing

1. Open the `.dmg` file and drag **SNMP MIB Client** into **Applications**.
2. On first launch, macOS will likely refuse to open it with an "Apple could
   not verify this app is free of malware" warning — the app isn't signed
   with a paid Apple Developer certificate. To open it anyway:
   - Right-click (or Control-click) the app in Applications → **Open** →
     confirm **Open Anyway** in the dialog that appears.
   - Or: **System Settings → Privacy & Security**, scroll down to the
     blocked-app notice, and click **Open Anyway**.

   You only need to do this once — after the first approval, it opens
   normally like any other app.

## The window, at a glance

- **Left sidebar** — the MIB directories you've configured, and the OID tree
  parsed from them.
- **Right side** — one or two panes, each with its own tabs. Each tab is an
  independent connection to a device, viewing one table or scalar value.

## Getting started

### 1. Add a MIB directory

Click the **+** above the MIB directory list and choose a folder containing
your `.mib` files. Every directory you add is parsed immediately and its
contents appear in the tree below; add as many as you need; remove one with
the **×** next to it. If any files fail to parse, a warning banner appears —
click it to see which files and what went wrong.

### 2. Browse the tree

- **Tree** mode (default) shows the full group hierarchy, exactly as the
  MIBs define it — click the caret to expand/collapse a branch.
- **Tables** mode flattens the view to just the SNMP tables, each listed as
  a root with its columns underneath — useful when you know you want a
  specific table and don't want to hunt through the group hierarchy.

Single-clicking a row just highlights it. **Double-click a table or scalar
to open it in a new tab** — that's the only thing that actually loads
something into a tab. Right-clicking a table also offers "Open in new tab"
from a context menu.

### 3. Set the connection details

Each tab has its own connection fields at the top:

| Field | Notes |
|---|---|
| Address | Hostname or IP of the device |
| Port | Defaults to `161` (standard SNMP) |
| Version | `v1`, `v2c`, or `v3` |
| Community | For v1/v2c — defaults to `public` |
| Security User / Auth / Priv | For v3 only, replaces Community |

### 4. Fetch data

The **Fetch** button is disabled until a node is selected and the
connection fields above are filled in — hover it to see what's missing.

Click the small chevron next to Fetch to choose the fetch mode:
- **Manual** (default) — fetch only when you click the button.
- **Auto-refresh (10s)** — fetches every 10 seconds; the chevron turns into
  a small ring that drains down to empty as the next fetch approaches.

### 5. Read the results

- Click a column header to sort by it; click again to reverse.
- Drag a column's right edge to resize it.
- **Diff mode** highlights what changed between fetches — added rows in
  green, removed rows struck through in red, changed cells in yellow. Handy
  when watching a table for changes over time with auto-refresh on.
- **Readable names** turns raw MIB identifiers like
  `dcpLinkviewLocalHostname` into `Local Hostname` in the column headers
  (strips the shared table-name prefix, splits the rest into words).
- Status-like values (`up`/`down`/etc.) get a colored dot for a quick read.

## Working with tabs and panes

- Double-clicking a node in the sidebar opens it in a new tab in the active
  pane.
- **×** on a tab closes it.
- **⊟** splits the pane in two, side by side (up to two panes) — handy for
  comparing two tables, or the same table on two devices, at once.
- **✕** on a split pane merges back down to one.

## A couple of things worth knowing

- A table's index column (e.g. `dcpLinkviewIndex`) is intentionally never
  shown as its own column — SNMP agents don't return values for it during a
  table walk, so its value only ever appears in the **Index** column, which
  is where you'll find it.
- MIB directories are remembered between launches; connection details
  (address/port/etc.) are per-tab and are not saved once a tab is closed.
