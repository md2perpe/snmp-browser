// Talks to the Rust backend. Inside the Tauri shell this goes through Tauri's
// IPC `invoke`; anywhere else (e.g. `npm run dev` opened in a plain browser
// tab, like VSCode's Simple Browser) there's no IPC bridge, so it falls back
// to the standalone HTTP backend (`npm run dev:server`), which exposes the
// same commands under POST /api/invoke/:cmd.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
  }
}

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const API_BASE = import.meta.env.VITE_API_BASE ?? "http://127.0.0.1:8787";

export async function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isTauri) return tauriInvoke<T>(cmd, args);

  const res = await fetch(`${API_BASE}/api/invoke/${cmd}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  const text = await res.text();
  const data = text ? JSON.parse(text) : undefined;
  if (!res.ok) throw new Error(typeof data === "string" ? data : res.statusText);
  return data as T;
}

/** Directory picker: native dialog under Tauri, a plain prompt in the browser (no filesystem access there). */
export async function pickDirectory(): Promise<string | null> {
  if (isTauri) {
    const selected = await tauriOpen({ directory: true, multiple: false });
    if (!selected || Array.isArray(selected)) return null;
    return selected;
  }
  return window.prompt("Browser mode has no native file picker — enter the absolute path to a MIB directory on the machine running the standalone backend:");
}
