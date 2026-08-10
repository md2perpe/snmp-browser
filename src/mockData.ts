import type { HostProfile } from "./types";

// Host profiles remain frontend-only for now (no persistence pass yet) - the
// MIB tree and table data come from the Rust backend (see src-tauri/src/mib.rs
// and snmp.rs).

export const mockHostProfiles: HostProfile[] = [
  { id: "h1", label: "core-switch-01", addr: "10.0.1.5", port: "161", community: "public", v3User: "admin" },
  { id: "h2", label: "edge-router-02", addr: "10.0.2.14", port: "161", community: "public", v3User: "netops" },
  { id: "h3", label: "dist-switch-03", addr: "10.0.3.8", port: "1161", community: "private", v3User: "admin" },
];

export const DEFAULT_COL_WIDTH = 140;
