// Exports the currently displayed table (respecting sort order and the
// "Readable names" / "Display hint" toggles) to a CSV or PNG file. Under
// Tauri this goes through a native save dialog plus a backend write
// command; in a plain browser tab (dev mode outside Tauri) it falls back to
// a client-side download, mirroring the isTauri branching already used
// elsewhere (e.g. `pickDirectory` in api.ts).

import { save } from "@tauri-apps/plugin-dialog";
import { invoke, isTauri } from "./api";
import { humanizeColumnNames, resolveCellValue } from "./render";
import type { Store } from "./state";
import type { TabState } from "./types";

function sanitizeFilename(name: string): string {
  return (name.trim() || "table").replace(/[/\\?%*:|"<>]/g, "_");
}

interface ExportTable {
  headers: string[];
  rows: string[][];
}

/** Builds the export table in the table's normal (non-transposed) orientation, since that's the useful shape for data interchange regardless of how it's currently displayed on screen. */
function buildExportTable(store: Store, tab: TabState): ExportTable {
  const columnLabels = tab.humanReadableColumns ? humanizeColumnNames(tab.columns) : null;
  const displayHints = tab.useDisplayHints ? tab.displayHints : null;
  const enumLabels = tab.useDisplayHints ? tab.enumLabels : null;
  const headers = tab.columns.map((col) => (columnLabels ? columnLabels[col] : col));
  const rows = store
    .getSortedRows(tab)
    .map((row) => tab.columns.map((col) => resolveCellValue(row[col] ?? "", displayHints?.[col], enumLabels?.[col]).value));
  return { headers, rows };
}

function csvField(value: string): string {
  return /[",\r\n]/.test(value) ? `"${value.replace(/"/g, '""')}"` : value;
}

function toCsv({ headers, rows }: ExportTable): string {
  return [headers, ...rows].map((r) => r.map(csvField).join(",")).join("\r\n") + "\r\n";
}

async function writeFileBytes(path: string, bytes: Uint8Array) {
  await invoke("write_export_file", { path, data: Array.from(bytes) });
}

/** Browser-only fallback: triggers a client-side download via a throwaway anchor element. */
function downloadBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export async function exportTableCsv(store: Store, tab: TabState, labelHint: string) {
  const csv = toCsv(buildExportTable(store, tab));
  const filename = `${sanitizeFilename(labelHint)}.csv`;
  if (isTauri) {
    const path = await save({ defaultPath: filename, filters: [{ name: "CSV", extensions: ["csv"] }] });
    if (!path) return;
    await writeFileBytes(path, new TextEncoder().encode(csv));
  } else {
    downloadBlob(new Blob([csv], { type: "text/csv;charset=utf-8" }), filename);
  }
}

// ---------- PNG ----------

const ROW_HEIGHT = 28;
const CELL_PAD_X = 10;
const MIN_COL_WIDTH = 60;
const MAX_COL_WIDTH = 320;
const BODY_FONT = "13px -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";
const HEADER_FONT = "600 13px -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif";

interface PngColors {
  background: string;
  headerBg: string;
  border: string;
  text: string;
  altRowBg: string;
}

/** Reads the live theme's CSS custom properties so the exported PNG matches whichever theme (dark/classic) is currently active, without duplicating color values here. */
function currentPngColors(): PngColors {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string) => cs.getPropertyValue(name).trim();
  return {
    background: v("--bg-app"),
    headerBg: v("--bg-sidebar"),
    border: v("--divider-strong"),
    text: v("--text-primary"),
    altRowBg: v("--hover-weak"),
  };
}

function truncateToWidth(ctx: CanvasRenderingContext2D, text: string, maxWidth: number): string {
  if (ctx.measureText(text).width <= maxWidth) return text;
  const ellipsis = "…";
  let lo = 0;
  let hi = text.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (ctx.measureText(text.slice(0, mid) + ellipsis).width <= maxWidth) lo = mid;
    else hi = mid - 1;
  }
  return lo === 0 ? ellipsis : text.slice(0, lo) + ellipsis;
}

function measureColWidths(ctx: CanvasRenderingContext2D, table: ExportTable): number[] {
  return table.headers.map((header, i) => {
    ctx.font = HEADER_FONT;
    let max = ctx.measureText(header).width;
    ctx.font = BODY_FONT;
    for (const row of table.rows) max = Math.max(max, ctx.measureText(row[i] ?? "").width);
    return Math.min(MAX_COL_WIDTH, Math.max(MIN_COL_WIDTH, Math.ceil(max) + CELL_PAD_X * 2));
  });
}

/** Draws the table (header row + data rows, with grid lines) onto a fresh canvas at up to 2x device pixel ratio, and returns it ready for `toBlob`. */
function renderTableCanvas(table: ExportTable, colors: PngColors): HTMLCanvasElement {
  const measureCtx = document.createElement("canvas").getContext("2d")!;
  const colWidths = measureColWidths(measureCtx, table);
  const width = colWidths.reduce((a, b) => a + b, 0);
  const height = ROW_HEIGHT * (table.rows.length + 1);
  const dpr = Math.min(2, window.devicePixelRatio || 1);

  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(width * dpr));
  canvas.height = Math.max(1, Math.round(height * dpr));
  const ctx = canvas.getContext("2d")!;
  ctx.scale(dpr, dpr);
  ctx.textBaseline = "middle";

  ctx.fillStyle = colors.background;
  ctx.fillRect(0, 0, width, height);

  ctx.fillStyle = colors.headerBg;
  ctx.fillRect(0, 0, width, ROW_HEIGHT);
  ctx.font = HEADER_FONT;
  ctx.fillStyle = colors.text;
  let x = 0;
  table.headers.forEach((header, i) => {
    ctx.fillText(truncateToWidth(ctx, header, colWidths[i] - CELL_PAD_X * 2), x + CELL_PAD_X, ROW_HEIGHT / 2);
    x += colWidths[i];
  });

  ctx.font = BODY_FONT;
  table.rows.forEach((row, r) => {
    const y = ROW_HEIGHT * (r + 1);
    if (r % 2 === 1) {
      ctx.fillStyle = colors.altRowBg;
      ctx.fillRect(0, y, width, ROW_HEIGHT);
    }
    ctx.fillStyle = colors.text;
    let cx = 0;
    row.forEach((cell, i) => {
      ctx.fillText(truncateToWidth(ctx, cell, colWidths[i] - CELL_PAD_X * 2), cx + CELL_PAD_X, y + ROW_HEIGHT / 2);
      cx += colWidths[i];
    });
  });

  ctx.strokeStyle = colors.border;
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let r = 0; r <= table.rows.length + 1; r++) {
    const y = Math.round(ROW_HEIGHT * r) + 0.5;
    ctx.moveTo(0, y);
    ctx.lineTo(width, y);
  }
  let gx = 0;
  for (let i = 0; i <= colWidths.length; i++) {
    const lx = Math.round(gx) + 0.5;
    ctx.moveTo(lx, 0);
    ctx.lineTo(lx, height);
    if (i < colWidths.length) gx += colWidths[i];
  }
  ctx.stroke();

  return canvas;
}

function canvasToPngBlob(canvas: HTMLCanvasElement): Promise<Blob> {
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("PNG encoding failed"))), "image/png");
  });
}

export async function exportTablePng(store: Store, tab: TabState, labelHint: string) {
  const table = buildExportTable(store, tab);
  const canvas = renderTableCanvas(table, currentPngColors());
  const blob = await canvasToPngBlob(canvas);
  const filename = `${sanitizeFilename(labelHint)}.png`;
  if (isTauri) {
    const path = await save({ defaultPath: filename, filters: [{ name: "PNG Image", extensions: ["png"] }] });
    if (!path) return;
    await writeFileBytes(path, new Uint8Array(await blob.arrayBuffer()));
  } else {
    downloadBlob(blob, filename);
  }
}
