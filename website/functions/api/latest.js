// SPDX-License-Identifier: LGPL-3.0-or-later
//
// GET /api/latest — zero-dependency Cloudflare Pages Function.
//
// Returns the latest GitHub Release as a tiny JSON document for the
// landing page's "latest" badge. Public releases endpoint — no token
// required. Cached at the edge for 10 minutes.
//
// Vars (see website/wrangler.toml):
//   env.GITHUB_REPO  var — "owner/name", default nekomario28/mc-spine

const GITHUB_API = "https://api.github.com";
const DEFAULT_REPO = "nekomario28/mc-spine";
const MC_TARGETS = ["1.21.1", "26.3"];

export async function onRequestGet(context) {
  const env = context.env || {};
  const repo = env.GITHUB_REPO || DEFAULT_REPO;

  const headers = {
    accept: "application/vnd.github+json",
    "x-github-api-version": "2022-11-28",
    "user-agent": "megu-site-functions",
  };

  let res;
  try {
    res = await fetch(`${GITHUB_API}/repos/${repo}/releases/latest`, { headers });
  } catch (_) {
    return jsonError(502, "github unreachable");
  }

  let release = null;
  if (res.status === 404) {
    // Stable /releases/latest returns 404 while only pre-releases (e.g.
    // v0.1.0-alpha) exist; fall back to the newest published non-draft release.
    let listRes;
    try {
      listRes = await fetch(`${GITHUB_API}/repos/${repo}/releases?per_page=5`, { headers });
    } catch (_) {
      return jsonError(502, "github unreachable");
    }
    if (!listRes.ok) {
      return jsonError(502, `github releases returned ${listRes.status}`);
    }
    let list;
    try {
      list = await listRes.json();
    } catch (_) {
      return jsonError(502, "github returned invalid JSON");
    }
    if (!Array.isArray(list)) {
      return jsonError(502, "github returned invalid release list");
    }
    release = list.find((r) => r && !r.draft && typeof r.tag_name === "string") || null;
    if (!release) {
      return jsonError(404, "no published releases");
    }
  } else if (!res.ok) {
    return jsonError(502, `github releases/latest returned ${res.status}`);
  } else {
    try {
      release = await res.json();
    } catch (_) {
      return jsonError(502, "github returned invalid JSON");
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
