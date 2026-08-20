import type { HostProfile } from "./types";

// Host profiles are persisted by the Rust backend (see src-tauri/src/settings.rs);
// this is just the value the store holds before init() fetches the real list.
export const mockHostProfiles: HostProfile[] = [];

export const DEFAULT_COL_WIDTH = 140;
