// Thin wrapper around the official Tauri updater plugin, which does the real
// work: checking the signed manifest at the endpoint configured in
// tauri.conf.json (plugins.updater), verifying downloaded artifacts against
// the embedded public key, and installing them. See .github/workflows/release.yml
// for where release artifacts get signed.

import { check, type Update } from "@tauri-apps/plugin-updater";

/**
 * Checks the updater endpoint for a newer release. Returns null both when
 * already up to date and on any failure (offline, rate-limited, malformed
 * manifest) - this is a routine background check, not something worth
 * surfacing errors for on every run. The returned `Update`, if any, carries
 * `downloadAndInstall()` for the caller to invoke once the user opts in.
 */
export async function checkForUpdate(): Promise<Update | null> {
  try {
    return await check();
  } catch {
    return null;
  }
}
