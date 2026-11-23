// SPDX-License-Identifier: LGPL-3.0-or-later
//
// GET /api/latest — zero-dependency Cloudflare Pages Function.
//
// Returns the latest GitHub Release plus anti-inflated download telemetry
// as a small JSON document for the landing page. Two upstream endpoints
// are queried: /releases/latest decides which release the site calls
// "latest" (GitHub's stable channel), while /releases?per_page=20 feeds
// per-release download telemetry and the prerelease fallback.
//
// Telemetry is deliberately conservative — only real user-downloadable
// distribution artifacts count. Checksum, signature, SBOM and manifest
// assets (which CI and verification tooling probe constantly) are
// excluded so automated fetches never inflate the public counter, and
// draft or test/ci/dryrun/dev-tagged releases are ignored entirely.
// Telemetry keys appear only when a real count exists: a zero-download
// launch stays a tiny document and the page hides its counter.
//
// Vars (see website/wrangler.toml):
//   env.GITHUB_REPO  var — "owner/name", default nekomario28/megu-mc

const GITHUB_API = "https://api.github.com";
const DEFAULT_REPO = "nekomario28/megu-mc";
const MC_TARGETS = ["1.21.1", "26.3"];

// CI/test tag segments never count toward public download telemetry and
// are never offered as the site's "latest" release.
const TEST_TAG = /(^|[-.])(test|ci|dryrun|dev)([-.\d]|$)/i;

// Only real user-downloadable distribution artifacts count toward
// totals and appear in the assets array.
const DISTRIBUTION_EXTENSIONS = [
  ".exe", ".jar", ".zip", ".tar.gz", ".mrpack", ".dmg", ".appimage", ".sh",
];
const STANDALONE_INSTALLER_BIN = /^megu-installer-(?:linux|macos|windows)-[a-z0-9_.-]+$/i;

// Checksum, signature, SBOM and manifest files are metadata, not
// downloads — excluded even though their extensions never match the
// allowlist anyway.
const METADATA_ASSET = /(?:^|[-_./])sha256sums$|\.(?:sha256|sig|asc|sbom\.json|json|txt|md)$/i;

function isEligibleRelease(r) {
  return Boolean(r) && r.draft !== true &&
    typeof r.tag_name === "string" && !TEST_TAG.test(r.tag_name);
}

function assetOs(name) {
  if (/(?:windows|win64|win32|\.exe$)/i.test(name)) {
    return "windows";
  }
  if (/(?:macos|darwin|osx|\.dmg$)/i.test(name)) {
    return "macos";
  }
  if (/(?:linux|\.appimage$|\.sh$)/i.test(name)) {
    return "linux";
  }
  return "";
}

function classifyAsset(name) {
  const n = name.toLowerCase();
  const os = assetOs(n);
  if (n.endsWith(".mrpack")) {
    return "mrpack";
  }
  if (n.includes("prism")) {
    return "prism_zip";
  }
  if (n.endsWith(".jar")) {
    return "installer_jar";
  }
  if (n.includes("installer") || n.includes("install-") || n.includes("setup") ||
      n.includes("bootstrapper") || n.endsWith(".exe") ||
      n.endsWith(".msi") || n.endsWith(".dmg")) {
    return "installer_" + (os || "windows");
  }
  if (os) {
    return "bundle_" + os;
  }
  return "other";
}

function isDistributionAsset(name) {
  const n = String(name || "").toLowerCase();
  if (!n || METADATA_ASSET.test(n)) {
    return false;
  }
  return DISTRIBUTION_EXTENSIONS.some((ext) => n.endsWith(ext)) ||
    STANDALONE_INSTALLER_BIN.test(n);
}

// Structured, fail-closed view of one release's downloadable assets:
// metadata is dropped and only https://github.com/ download URLs pass.
function distributionAssets(release) {
  const raw = release && Array.isArray(release.assets) ? release.assets : [];
  const out = [];
  for (const a of raw) {
    const name = a && typeof a.name === "string" ? a.name : "";
    const url = a && typeof a.browser_download_url === "string"
      ? a.browser_download_url : "";
    if (!name || !url.startsWith("https://github.com/") ||
        !isDistributionAsset(name)) {
      continue;
    }
    out.push({
      name,
      size: Number.isFinite(a.size) ? a.size : 0,
      download_count: Number.isFinite(a.download_count) ? a.download_count : 0,
      url,
      kind: classifyAsset(name),
    });
  }
  return out;
}

export async function onRequestGet(context) {
  const env = context.env || {};
  const repo = env.GITHUB_REPO || DEFAULT_REPO;

  const headers = {
    accept: "application/vnd.github+json",
    "x-github-api-version": "2022-11-28",
    "user-agent": "megu-site-functions",
  };

  let latestRes = null;
  let listRes = null;
  try {
    latestRes = await fetch(`${GITHUB_API}/repos/${repo}/releases/latest`, { headers });
  } catch (_) {}
  try {
    listRes = await fetch(`${GITHUB_API}/repos/${repo}/releases?per_page=20`, { headers });
  } catch (_) {}

  if (!latestRes && !listRes) {
    return jsonError(502, "github unreachable");
  }

  let latest = null;
  if (latestRes && latestRes.ok) {
    try {
      latest = await latestRes.json();
    } catch (_) {
      return jsonError(502, "github returned invalid JSON");
    }
  }

  let releases = null;
  if (listRes && listRes.ok) {
    try {
      const parsed = await listRes.json();
      if (Array.isArray(parsed)) {
        releases = parsed;
      } else if (!latest) {
        return jsonError(502, "github returned invalid release list");
      }
    } catch (_) {
      if (!latest) {
        return jsonError(502, "github returned invalid JSON");
      }
    }
  }

  const eligible = releases ? releases.filter(isEligibleRelease) : [];

  // The headline release: GitHub's /releases/latest wins when usable;
  // otherwise the newest non-draft, non-test published release — e.g.
  // while only prereleases exist (/releases/latest returns 404). When the
  // same tag appears in the list, the list entry is used so version and
  // telemetry share one source.
  let release = null;
  if (latest && isEligibleRelease(latest)) {
    release = eligible.find((r) => r.tag_name === latest.tag_name) || latest;
  } else if (eligible.length) {
    release = eligible[0];
  }

  if (!release) {
    if (latestRes && !latestRes.ok && latestRes.status !== 404) {
      return jsonError(502,
        `github releases/latest returned ${latestRes.status}`);
    }
    if (listRes && !listRes.ok) {
      return jsonError(502, `github releases returned ${listRes.status}`);
    }
    return jsonError(404, "no published releases");
  }

  const assets = distributionAssets(release);
  const releaseDownloads = assets.reduce((sum, a) => sum + a.download_count, 0);
  let totalDownloads = 0;
  for (const r of eligible) {
    for (const a of distributionAssets(r)) {
      totalDownloads += a.download_count;
    }
  }

  const body = {
    ok: true,
    version: release.tag_name || null,
    published_at: release.published_at || null,
    html_url: release.html_url || null,
    mc_targets: MC_TARGETS,
  };
  if (release.prerelease === true) {
    body.prerelease = true;
  }
  if (totalDownloads > 0 || assets.length) {
    body.total_downloads = totalDownloads;
    body.release_downloads = releaseDownloads;
    body.assets = assets;
  }

  return new Response(JSON.stringify(body), {
    status: 200,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "public, max-age=600",
    },
  });
}

function jsonError(status, error) {
  return new Response(JSON.stringify({ ok: false, error }), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}
