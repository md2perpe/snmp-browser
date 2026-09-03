// Best-effort "is a newer version out?" check against GitHub Releases. No
// installing, no signing - just a courtesy notice with a link, since releases
// are manually-published GitHub Actions builds (see .github/workflows/release.yml).

import { getVersion } from "@tauri-apps/api/app";
import type { UpdateInfo } from "./types";

const REPO = "md2perpe/snmp-browser";

/** Compares two dotted version strings part by part numerically (e.g. "0.10.0" > "0.9.0"); a missing part counts as 0. */
function isNewer(latest: string, current: string): boolean {
  const a = latest.split(".").map(Number);
  const b = current.split(".").map(Number);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const av = a[i] ?? 0;
    const bv = b[i] ?? 0;
    if (av !== bv) return av > bv;
  }
  return false;
}

/**
 * Checks GitHub's "latest release" for this repo - which already excludes
 * drafts and prereleases - against the running app version. Returns null on
 * any failure (offline, rate-limited, malformed response) or when already up
 * to date; this is a courtesy check, not something worth surfacing errors for.
 */
export async function checkForUpdate(): Promise<UpdateInfo | null> {
  try {
    const [current, res] = await Promise.all([
      getVersion(),
      fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
        headers: { Accept: "application/vnd.github+json" },
      }),
    ]);
    if (!res.ok) return null;
    const data = (await res.json()) as { tag_name?: string; html_url?: string };
    const latest = data.tag_name?.replace(/^v/, "");
    if (!latest || !data.html_url) return null;
    return isNewer(latest, current) ? { version: latest, url: data.html_url } : null;
  } catch {
    return null;
  }
}
