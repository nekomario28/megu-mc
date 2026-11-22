// SPDX-License-Identifier: LGPL-3.0-or-later
//
// POST /api/report — zero-dependency Cloudflare Pages Function.
//
// Accepts a compact "megu-report/1" payload (hard cap 4 KiB), redacts
// sensitive strings server-side, and deduplicates crash/bug/idea reports
// through CRASH_INDEX KV (one GitHub Issue per key, milestone comments).
//
// Bindings / vars (see website/wrangler.toml):
//   env.CRASH_INDEX        KV namespace — required for dedup; also rate limits
//   env.GITHUB_TOKEN       secret — fine-grained PAT, issues:write
//   env.GITHUB_REPO        var    — "owner/name", default nekomario28/megu-mc
//   env.TURNSTILE_SECRET_KEY secret — required for production reports;
//                          every kind must supply a valid turnstile_token
//   env.ALLOW_DEV_UNVERIFIED_REPORTS var — set exactly '1' to allow local
//                          development without TURNSTILE_SECRET_KEY only
// Missing Turnstile configuration returns 503 unless that dev override is set.

const MAX_BODY_BYTES = 4096;
const MAX_TOP_FRAMES = 6;
const MAX_FRAME_LEN = 120;
const MAX_SUMMARY_LEN = 240;
const MAX_TRACKED = 8;
const REPORT_SCHEMA = "megu-report/1";
const FINGERPRINT_RE = /^MEGU-E-[0-9A-Fa-f]{8}$/;
const GITHUB_API = "https://api.github.com";
const DEFAULT_REPO = "nekomario28/megu-mc";
const TURNSTILE_VERIFY_URL =
  "https://challenges.cloudflare.com/turnstile/v0/siteverify";
// Five reports per Cloudflare-provided client IP per ten-minute window.
// KV is eventually consistent; this isolate-local map also limits bursts and
// provides a best-effort fallback when KV is absent or temporarily unavailable.
const RATE_LIMIT = 5;
const RATE_WINDOW_MS = 10 * 60 * 1000;
const isolateRates = new Map();

const KINDS = new Set(["crash", "bug", "idea"]);
const KIND_LABEL = { crash: "auto-crash", bug: "triage", idea: "idea" };
const KIND_TITLE = { crash: "[Crash]", bug: "[Bug]", idea: "[Idea]" };

// Milestone comment counts: 1, 5, 10, then 25 * 2^k (25, 50, 100, 200, ...).
function shouldPostMilestoneComment(count) {
  if (!Number.isInteger(count) || count < 1) return false;
  if (count === 1 || count === 5 || count === 10) return true;
  if (count >= 25) {
    let m = 25;
    while (m < count) m *= 2;
    return m === count;
  }
  return false;
}

// Server-side double redaction: strips home paths, IPv4 addresses,
// bearer tokens, rcon passwords, and common GitHub token shapes.
function redactSensitiveText(text) {
  if (typeof text !== "string") return text;
  return text
    .replace(/(rcon\.password\s*[=:]\s*)[^\s&"']+/gi, "$1[REDACTED]")
    .replace(/\bBearer\s+[^\s"']+/gi, "Bearer [REDACTED]")
    .replace(/\b(?:ghp_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,})\b/g,
      "[REDACTED]")
    .replace(/\b(?:\d{1,3}\.){3}\d{1,3}\b/g, "[REDACTED]")
    .replace(/(\/home\/)[^/\s"']+/g, "$1[REDACTED]")
    .replace(/(\/Users\/)[^/\s"']+/g, "$1[REDACTED]")
    .replace(/(C:[\\/]Users[\\/])[^\\/\s"']+/gi, "$1[REDACTED]");
}

function isShortString(value, maxLen) {
  return typeof value === "string" && value.length <= maxLen;
}

function redactString(value, maxLen) {
  return redactSensitiveText(value).slice(0, maxLen);
}

// Validates an already-parsed payload object. Returns
// { ok: true, report } with a sanitized, redacted, schema-only object,
// or { ok: false, error }.
function validateReportPayload(payload) {
  if (payload === null || typeof payload !== "object" ||
      Array.isArray(payload)) {
    return { ok: false, error: "payload must be a JSON object" };
  }
  // Hard cap on the raw payload, before any field is trusted or stripped.
  let rawBytes;
  try {
    rawBytes = new TextEncoder().encode(JSON.stringify(payload)).length;
  } catch (_) {
    return { ok: false, error: "payload is not JSON-serializable" };
  }
  if (rawBytes > MAX_BODY_BYTES) {
    return { ok: false, error: "payload exceeds 4 KiB cap" };
  }
  if (payload.schema !== REPORT_SCHEMA) {
    return { ok: false, error: `schema must be "${REPORT_SCHEMA}"` };
  }
  if (!KINDS.has(payload.kind)) {
    return { ok: false, error: "kind must be crash, bug, or idea" };
  }

  let fingerprint = null;
  if (payload.fingerprint !== undefined && payload.fingerprint !== null) {
    if (typeof payload.fingerprint !== "string" ||
        !FINGERPRINT_RE.test(payload.fingerprint)) {
      return { ok: false, error: "invalid fingerprint (want MEGU-E-XXXXXXXX)" };
    }
    fingerprint = payload.fingerprint.toUpperCase();
  }
  if (payload.kind === "crash" && !fingerprint) {
    return { ok: false, error: "crash reports require a fingerprint" };
  }

  if (!isShortString(payload.mc_version, 16)) {
    return { ok: false, error: "mc_version must be a string <= 16 chars" };
  }
  if (!isShortString(payload.engine_version, 32)) {
    return { ok: false, error: "engine_version must be a string <= 32 chars" };
  }
  if (!isShortString(payload.os, 32)) {
    return { ok: false, error: "os must be a string <= 32 chars" };
  }
  if (typeof payload.summary !== "string" ||
      payload.summary.trim().length === 0 ||
      payload.summary.length > MAX_SUMMARY_LEN) {
    return { ok: false,
      error: `summary must be a non-empty string <= ${MAX_SUMMARY_LEN} chars` };
  }

  const report = {
    schema: REPORT_SCHEMA,
    kind: payload.kind,
    mc_version: redactString(payload.mc_version, 16),
    engine_version: redactString(payload.engine_version, 32),
    os: redactString(payload.os, 32),
    summary: redactString(payload.summary.trim(), MAX_SUMMARY_LEN),
  };
  if (fingerprint) report.fingerprint = fingerprint;

  for (const [key, maxLen] of [["gpu_class", 32], ["culprit", 80],
                               ["subsystem", 40]]) {
    if (payload[key] !== undefined && payload[key] !== null) {
      if (!isShortString(payload[key], maxLen)) {
        return { ok: false, error: `${key} must be a string <= ${maxLen} chars` };
      }
      report[key] = redactString(payload[key], maxLen);
    }
  }

  if (payload.top_frames !== undefined && payload.top_frames !== null) {
    if (!Array.isArray(payload.top_frames) ||
        payload.top_frames.length > MAX_TOP_FRAMES ||
        payload.top_frames.some(f => !isShortString(f, MAX_FRAME_LEN))) {
      return { ok: false,
        error: `top_frames must be an array of <= ${MAX_TOP_FRAMES} ` +
               `strings <= ${MAX_FRAME_LEN} chars` };
    }
    report.top_frames = payload.top_frames
      .map(f => redactString(f, MAX_FRAME_LEN));
  }

  if (payload.mod_count !== undefined && payload.mod_count !== null) {
    if (!Number.isInteger(payload.mod_count) ||
        payload.mod_count < 0 || payload.mod_count > 4096) {
      return { ok: false,
        error: "mod_count must be an integer between 0 and 4096" };
    }
    report.mod_count = payload.mod_count;
  }

  return { ok: true, report };
}

function truncate(text, max) {
  return text.length <= max ? text : text.slice(0, max - 1) + "…";
}

function issueTitleFor(report) {
  const head = KIND_TITLE[report.kind] +
    (report.fingerprint ? " " + report.fingerprint : "");
  return truncate(head + " " + report.summary, 140);
}

// Compact, LLM-friendly markdown body — stays well under ~1.5 KiB.
function formatCompactIssueBody(report) {
  const lines = [
    `**Kind:** ${report.kind}` +
      (report.fingerprint ? ` · **Fingerprint:** \`${report.fingerprint}\`` : ""),
    `**MC / Engine / OS:** ${report.mc_version} · ${report.engine_version} · ${report.os}` +
      (report.gpu_class ? ` · **GPU:** ${report.gpu_class}` : ""),
  ];
  if (report.culprit) lines.push(`**Culprit:** ${report.culprit}`);
  if (report.subsystem) lines.push(`**Subsystem:** ${report.subsystem}`);
  if (report.mod_count !== undefined) lines.push(`**Mods:** ${report.mod_count}`);
  lines.push("", `**Summary:** ${report.summary}`);
  if (report.top_frames && report.top_frames.length) {
    lines.push("", "**Top frames:**", "```", ...report.top_frames, "```");
  }
  lines.push("", "_Reported via /api/report (megu-report/1)_");
  return lines.join("\n");
}

function jsonResponse(data, status, extraHeaders) {
  return new Response(JSON.stringify(data), {
    status,
    headers: Object.assign({
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    }, extraHeaders || {}),
  });
}

function errorResponse(status, error) {
  return jsonResponse({ ok: false, error }, status);
}

function githubHeaders(env) {
  return {
    accept: "application/vnd.github+json",
    "x-github-api-version": "2022-11-28",
    "user-agent": "megu-site-functions",
    authorization: `Bearer ${env.GITHUB_TOKEN}`,
  };
}

async function githubFetch(env, path, init) {
  const opts = Object.assign({}, init || {});
  opts.headers = Object.assign(githubHeaders(env), opts.headers || {});
  return fetch(GITHUB_API + path, opts);
}

// Ensures a label exists so issue creation never 422s on a missing label.
async function ensureLabel(env, repo, name) {
  const res = await githubFetch(env,
    `/repos/${repo}/labels/${encodeURIComponent(name)}`);
  if (res.ok) return;
  if (res.status !== 404) {
    throw new Error(`github label lookup failed: ${res.status}`);
  }
  const created = await githubFetch(env, `/repos/${repo}/labels`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      name,
      color: name === "auto-crash" ? "b60205" : "fbca04",
    }),
  });
  if (!created.ok) {
    throw new Error(`github label creation failed: ${created.status}`);
  }
}

async function createIssue(env, repo, report) {
  await ensureLabel(env, repo, KIND_LABEL[report.kind]);
  const res = await githubFetch(env, `/repos/${repo}/issues`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      title: issueTitleFor(report),
      body: formatCompactIssueBody(report),
      labels: [KIND_LABEL[report.kind]],
    }),
  });
  if (!res.ok) {
    throw new Error(`github issue create failed: ${res.status}`);
  }
  return res.json();
}

async function postMilestoneComment(env, repo, issueNumber, report, count) {
  const seen = [];
  if (report.mc_version) seen.push("MC " + report.mc_version);
  if (report.os) seen.push(report.os);
  if (report.engine_version) seen.push("engine " + report.engine_version);
  const body =
    `Report #${count}` +
    (report.fingerprint ? ` for \`${report.fingerprint}\`` : "") +
    ` — seen on ${seen.join(", ") || "unknown env"}.`;
  const res = await githubFetch(env, `/repos/${repo}/issues/${issueNumber}/comments`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ body }),
  });
  return res.ok;
}

// Small deterministic hash for bug reports that arrive without a
// fingerprint, so identical bug spam still stacks onto one issue.
function fnv1a(text) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16).padStart(8, "0").toUpperCase();
}

function dedupKeyFor(report) {
  if (report.kind !== "idea" && report.fingerprint) {
    return "fp:" + report.fingerprint;
  }
  if (report.kind === "bug") {
    return "bug:" + fnv1a(
      [report.summary, report.culprit || "", report.mc_version].join("|"));
  }
  // Ideas have no fingerprint. Normalize user-authored text so case and
  // whitespace variations of the same suggestion share a GitHub Issue.
  const normalized = [report.summary, report.culprit || "",
    report.subsystem || ""].map(s => s.trim().toLowerCase().replace(/\s+/g, " "));
  return "idea:" + fnv1a(JSON.stringify(normalized));
}

async function verifyTurnstile(env, token) {
  if (typeof token !== "string" || token.length === 0) return false;
  const res = await fetch(TURNSTILE_VERIFY_URL, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      secret: env.TURNSTILE_SECRET_KEY,
      response: token,
    }).toString(),
  });
  if (!res.ok) return false;
  const data = await res.json().catch(() => null);
  return !!(data && data.success === true);
}

async function overRateLimit(env, request) {
  // CF-Connecting-IP is supplied by Cloudflare Pages; requests without it
  // share one conservative "unknown" bucket rather than bypassing limits.
  const ip = request.headers.get("CF-Connecting-IP") || "unknown";
  const key = "rl:" + ip;
  const now = Date.now();
  let local = isolateRates.get(key);
  if (!local || local.expires <= now) {
    local = { count: 0, expires: now + RATE_WINDOW_MS };
    isolateRates.set(key, local);
  }
  if (local.count >= RATE_LIMIT) return true;
  // Reserve the slot synchronously before KV I/O to limit concurrent bursts
  // within one isolate. Bound the fallback map's memory usage.
  local.count++;
  if (isolateRates.size > 10000) {
    for (const [oldKey, entry] of isolateRates) {
      if (entry.expires <= now || isolateRates.size > 10000) {
        isolateRates.delete(oldKey);
      }
      if (isolateRates.size <= 10000) break;
    }
  }
  if (!env.CRASH_INDEX) return false;

  try {
    const raw = await env.CRASH_INDEX.get(key);
    let stored = null;
    try { stored = raw ? JSON.parse(raw) : null; } catch (_) { /* expired/corrupt */ }
    const count = stored && stored.expires > now &&
      Number.isInteger(stored.count) && stored.count >= 0 ? stored.count : 0;
    if (count >= RATE_LIMIT) return true;
    await env.CRASH_INDEX.put(key, JSON.stringify({
      count: count + 1,
      expires: stored && stored.expires > now ? stored.expires : local.expires,
    }), { expirationTtl: RATE_WINDOW_MS / 1000 });
  } catch (_) {
    // KV failure degrades to the isolate-local limiter, never unlimited.
  }
  return false;
}

export {
  MAX_BODY_BYTES,
  MAX_TOP_FRAMES,
  shouldPostMilestoneComment,
  redactSensitiveText,
  validateReportPayload,
  formatCompactIssueBody,
  issueTitleFor,
};

export async function onRequestPost(context) {
  const { request, env } = context;

  if (!env.TURNSTILE_SECRET_KEY &&
      env.ALLOW_DEV_UNVERIFIED_REPORTS !== "1") {
    return errorResponse(503, "turnstile is not configured");
  }

  const lenHeader = request.headers.get("content-length");
  if (lenHeader !== null && Number(lenHeader) > MAX_BODY_BYTES) {
    return errorResponse(413, "payload exceeds 4 KiB cap");
  }

  let payload;
  try {
    const text = await request.text();
    if (new TextEncoder().encode(text).length > MAX_BODY_BYTES) {
      return errorResponse(413, "payload exceeds 4 KiB cap");
    }
    payload = JSON.parse(text);
  } catch (_) {
    return errorResponse(400, "invalid JSON body");
  }

  if (await overRateLimit(env, request)) {
    return errorResponse(429, "report rate limit exceeded");
  }

  // The development override only applies when no secret is configured.
  const turnstileToken = payload && typeof payload === "object" &&
    !Array.isArray(payload) ? payload.turnstile_token : undefined;
  if (env.TURNSTILE_SECRET_KEY) {
    try {
      if (!(await verifyTurnstile(env, turnstileToken))) {
        return errorResponse(403, "turnstile verification failed");
      }
    } catch (_) {
      return errorResponse(403, "turnstile verification failed");
    }
  }

  const checked = validateReportPayload(payload);
  if (!checked.ok) return errorResponse(400, checked.error);
  const report = checked.report;

  if (!env.GITHUB_TOKEN) {
    return errorResponse(500, "GITHUB_TOKEN is not configured");
  }
  const repo = env.GITHUB_REPO || DEFAULT_REPO;
  const dedupKey = dedupKeyFor(report);

  // --- Dedup path: crash, bug, and idea ----------------------------------
  if (dedupKey) {
    if (!env.CRASH_INDEX) {
      return errorResponse(503, "CRASH_INDEX KV binding is not configured");
    }
    const kvKey = "report:" + dedupKey;
    const raw = await env.CRASH_INDEX.get(kvKey);
    let record = null;
    if (raw) {
      try { record = JSON.parse(raw); } catch (_) { record = null; }
    }

    if (record && record.issue_url) {
      const count = (record.count || 0) + 1;
      const versions = Array.isArray(record.versions) ? record.versions : [];
      const osList = Array.isArray(record.os_list) ? record.os_list : [];
      if (!versions.includes(report.mc_version) &&
          versions.length < MAX_TRACKED) versions.push(report.mc_version);
      if (!osList.includes(report.os) && osList.length < MAX_TRACKED) {
        osList.push(report.os);
      }
      record.count = count;
      record.versions = versions;
      record.os_list = osList;
      await env.CRASH_INDEX.put(kvKey, JSON.stringify(record));

      let commentPosted = false;
      if (shouldPostMilestoneComment(count)) {
        try {
          commentPosted = await postMilestoneComment(
            env, repo, record.issue_number, report, count);
        } catch (_) {
          // The report count is already stored; a comment failure is optional.
        }
      }
      return jsonResponse({
        ok: true,
        fingerprint: report.fingerprint || null,
        issue_url: record.issue_url,
        duplicate: true,
        total_reports: count,
        comment_posted: commentPosted,
      });
    }

    // First sighting: file the issue, then seed the dedup record.
    let issue;
    try {
      issue = await createIssue(env, repo, report);
    } catch (_) {
      return errorResponse(502, "github issue creation failed");
    }
    await env.CRASH_INDEX.put(kvKey, JSON.stringify({
      count: 1,
      versions: [report.mc_version],
      os_list: [report.os],
      issue_number: issue.number,
      issue_url: issue.html_url,
    }));
    return jsonResponse({
      ok: true,
      fingerprint: report.fingerprint || null,
      issue_url: issue.html_url,
      duplicate: false,
      total_reports: 1,
    });
  }

}
