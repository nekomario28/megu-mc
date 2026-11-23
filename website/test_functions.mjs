// SPDX-License-Identifier: LGPL-3.0-or-later
//
// Node unit tests for the zero-dependency Cloudflare Pages Functions in
// functions/api/. Run:  node website/test_functions.mjs
//
// The functions are ESM .js files; Node only treats .js as ESM under
// "type": "module", so each is copied to a temp .mjs (byte-identical)
// before import. All GitHub/Turnstile/KV access is mocked — no network.

import assert from "node:assert/strict";
import { mkdtempSync, copyFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const TMP = mkdtempSync(join(tmpdir(), "megu-fn-"));

async function loadFunction(rel) {
  const src = join(HERE, rel);
  const dst = join(TMP, rel.replaceAll("/", "_") + ".mjs");
  copyFileSync(src, dst);
  return import(pathToFileURL(dst).href);
}

const report = await loadFunction("functions/api/report.js");
const latest = await loadFunction("functions/api/latest.js");

let passed = 0;
let failed = 0;
let testIpIndex = 0;
async function test(name, fn) {
  testIpIndex++;
  try {
    await fn();
    passed++;
    console.log(`ok   ${name}`);
  } catch (err) {
    failed++;
    console.log(`FAIL ${name}`);
    console.log(String(err && err.stack || err).split("\n")
      .map(l => "     " + l).join("\n"));
  }
}

// ---------------------------------------------------------------- helpers

function validPayload(overrides = {}) {
  return {
    schema: "megu-report/1",
    kind: "crash",
    fingerprint: "MEGU-E-1A2B3C4D",
    mc_version: "1.21.1",
    engine_version: "alpha-2026.11.22",
    os: "Linux x86_64",
    gpu_class: "Vulkan RTX",
    culprit: "create-1.21.1.jar",
    subsystem: "kinetics",
    summary: "Server crashed during chunk tick",
    top_frames: ["at com.example.Foo.bar(Foo.java:12)"],
    mod_count: 14,
    ...overrides,
  };
}

function makeKv() {
  const map = new Map();
  const puts = [];
  return {
    map,
    puts,
    async get(key) { return map.has(key) ? map.get(key) : null; },
    async put(key, value, options) {
      map.set(key, value);
      puts.push({ key, options });
    },
  };
}

function reportKeys(kv) {
  return [...kv.map.keys()].filter(key => key.startsWith("report:"));
}

// Mock fetch: GitHub issues/comments/releases + Turnstile siteverify.
function mockFetch(state) {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url, init = {}) => {
    const u = String(url);
    state.calls.push({ url: u, method: init.method || "GET",
      headers: init.headers || {},
      body: init.body ? String(init.body) : null });
    const json = (obj, status = 200) => new Response(JSON.stringify(obj), {
      status, headers: { "content-type": "application/json" } });

    const failure = state.githubFailure;
    if (failure && u.startsWith("https://api.github.com/") &&
        u.endsWith(failure.path) &&
        (init.method || "GET") === failure.method) {
      if (failure.network) throw new Error(failure.message);
      return json({ message: failure.message }, failure.status);
    }

    if (u.includes("challenges.cloudflare.com")) {
      return json({ success: state.turnstileOk !== false });
    }
    if (u.includes("/releases/latest")) {
      return json({
        tag_name: "v0.9.1",
        published_at: "2026-11-20T10:00:00Z",
        html_url: "https://github.com/o/r/releases/tag/v0.9.1",
      });
    }
    if (u.includes("/issues/") && u.includes("/comments") &&
        (init.method || "GET") === "POST") {
      state.comments++;
      return json({ id: 1 }, 201);
    }
    if (u.includes("/labels/") && (init.method || "GET") === "GET") {
      return json({ message: "not found" }, 404);
    }
    if (u.endsWith("/labels") && init.method === "POST") {
      return json({ name: "x" }, 201);
    }
    if (u.includes("/issues") && init.method === "POST") {
      state.issues++;
      const n = state.issues;
      return json({
        number: n,
        html_url: `https://github.com/o/r/issues/${n}`,
      }, 201);
    }
    return json({ message: "unmocked " + u }, 500);
  };
  return () => { globalThis.fetch = realFetch; };
}

function postContext(payload, env, ip = `192.0.2.${testIpIndex}`) {
  return {
    request: new Request("https://megu.example/api/report", {
      method: "POST",
      headers: { "content-type": "application/json", "CF-Connecting-IP": ip },
      body: typeof payload === "string" ? payload : JSON.stringify(payload),
    }),
    env,
  };
}

function baseEnv(kv) {
  return {
    CRASH_INDEX: kv,
    GITHUB_TOKEN: "test-token",
    GITHUB_REPO: "o/r",
    ALLOW_DEV_UNVERIFIED_REPORTS: "1",
  };
}

// ---------------------------------------------------------------- tests

await test("validateReportPayload accepts a compact valid report", () => {
  const r = report.validateReportPayload(validPayload());
  assert.equal(r.ok, true);
  assert.equal(r.report.fingerprint, "MEGU-E-1A2B3C4D");
  assert.equal(r.report.kind, "crash");
});

await test("validateReportPayload rejects payloads over 4 KiB", () => {
  const r = report.validateReportPayload(
    validPayload({ blob: "x".repeat(5000) }));
  assert.equal(r.ok, false);
  assert.match(r.error, /4 KiB/);
});

await test("validateReportPayload rejects >6 top frames", () => {
  const frames = Array.from({ length: 7 }, (_, i) => `frame ${i}`);
  const r = report.validateReportPayload(validPayload({ top_frames: frames }));
  assert.equal(r.ok, false);
  assert.match(r.error, /top_frames/);
});

await test("validateReportPayload rejects invalid fingerprint", () => {
  for (const fp of ["MEGU-E-XYZ", "MEGU-E-1A2B3C4", "megu-e-1a2b3c4d!",
                     "OTHER-1A2B3C4D", 12345]) {
    const r = report.validateReportPayload(validPayload({ fingerprint: fp }));
    assert.equal(r.ok, false, `fingerprint ${fp} should fail`);
  }
  // crash without fingerprint
  const nofp = validPayload();
  delete nofp.fingerprint;
  assert.equal(report.validateReportPayload(nofp).ok, false);
});

await test("validateReportPayload enforces field length caps", () => {
  assert.equal(report.validateReportPayload(
    validPayload({ summary: "x".repeat(241) })).ok, false);
  assert.equal(report.validateReportPayload(
    validPayload({ mc_version: "1".repeat(17) })).ok, false);
  assert.equal(report.validateReportPayload(
    validPayload({ mod_count: 5000 })).ok, false);
  assert.equal(report.validateReportPayload(
    validPayload({ mod_count: 1.5 })).ok, false);
});

await test("redactSensitiveText strips paths, IPs, tokens", () => {
  const dirty =
    "crash in /home/alice/.minecraft/mods and C:\\Users\\bob\\app " +
    "from 192.168.1.10 with Bearer abc.def.ghi " +
    "rcon.password=s3cret ghp_ABCDEFGHIJKLMNOPQRSTUVWX " +
    "github_pat_11ABCDEFG0_abcdefghijklmnop";
  const clean = report.redactSensitiveText(dirty);
  for (const leak of ["alice", "bob", "192.168.1.10", "abc.def.ghi",
                      "s3cret", "ghp_", "github_pat_"]) {
    assert.ok(!clean.includes(leak), `leaked '${leak}' in: ${clean}`);
  }
  assert.ok(clean.includes("[REDACTED]"));
});

await test("formatCompactIssueBody stays tiny and structured", () => {
  const body = report.formatCompactIssueBody(report.validateReportPayload(
    validPayload()).report);
  const bytes = new TextEncoder().encode(body).length;
  assert.ok(bytes < 2048, `body too big: ${bytes} bytes`);
  assert.match(body, /MEGU-E-1A2B3C4D/);
  assert.match(body, /Server crashed during chunk tick/);
  assert.match(body, /```\nat com\.example/);
});

await test("shouldPostMilestoneComment hits 1,5,10,25,50,100", () => {
  for (const n of [1, 5, 10, 25, 50, 100, 200, 400]) {
    assert.equal(report.shouldPostMilestoneComment(n), true, `count ${n}`);
  }
  for (const n of [0, 2, 3, 4, 6, 7, 9, 11, 20, 24, 26, 30, 49, 51, 99,
                    101, 150, 300]) {
    assert.equal(report.shouldPostMilestoneComment(n), false, `count ${n}`);
  }
});

await test("onRequestPost creates a fresh issue then dedups", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const env = baseEnv(kv);
    const first = await report.onRequestPost(postContext(validPayload(), env));
    assert.equal(first.status, 200);
    const j1 = await first.json();
    assert.equal(j1.ok, true);
    assert.equal(j1.duplicate, false);
    assert.equal(j1.total_reports, 1);
    assert.equal(j1.issue_url, "https://github.com/o/r/issues/1");
    assert.equal(state.issues, 1);
    assert.equal(state.comments, 0); // creation is not a milestone comment

    // 4 more sightings -> count 5 hits a milestone comment, still 1 issue.
    let milestone;
    for (let i = 0; i < 4; i++) {
      const res = await report.onRequestPost(
        postContext(validPayload(), env));
      const j = await res.json();
      assert.equal(j.duplicate, true);
      milestone = j;
    }
    assert.equal(state.issues, 1, "duplicate fingerprint must not " +
      "create a second issue");
    assert.equal(state.comments, 1, "count 5 should post one comment");
    assert.equal(milestone.comment_posted, true);

    const rec = JSON.parse(kv.map.get("report:fp:MEGU-E-1A2B3C4D"));
    assert.equal(rec.count, 5);
    assert.ok(rec.versions.includes("1.21.1"));
    assert.ok(rec.os_list.includes("Linux x86_64"));

    const githubCalls = state.calls.filter(c =>
      c.url.startsWith("https://api.github.com/"));
    for (const [method, suffix] of [
      ["GET", "/labels/auto-crash"],
      ["POST", "/labels"],
      ["POST", "/issues"],
      ["POST", "/issues/1/comments"],
    ]) {
      assert.ok(githubCalls.some(c => c.method === method &&
        c.url.endsWith(suffix)), `${method} ${suffix} not called`);
    }
    for (const call of githubCalls) {
      assert.equal(call.headers.authorization, "Bearer test-token", call.url);
      assert.equal(call.headers.accept, "application/vnd.github+json", call.url);
      assert.equal(call.headers["x-github-api-version"], "2022-11-28", call.url);
      assert.equal(call.headers["user-agent"], "megu-site-functions", call.url);
      if (call.method === "POST") {
        assert.equal(call.headers["content-type"], "application/json", call.url);
      }
    }
  } finally {
    restore();
  }
});

await test("onRequestPost returns redacted 502 on GitHub API errors", async () => {
  for (const failure of [
    { method: "GET", path: "/labels/auto-crash", status: 401 },
    { method: "POST", path: "/labels", status: 500 },
    { method: "POST", path: "/issues", status: 401 },
    { method: "POST", path: "/issues", status: 500, kind: "idea" },
    { method: "POST", path: "/issues", network: true },
  ]) {
    const kv = makeKv();
    const state = { calls: [], issues: 0, comments: 0,
      githubFailure: { ...failure, message: "test-token upstream failure" } };
    const restore = mockFetch(state);
    try {
      const env = baseEnv(kv);
      const payload = validPayload(failure.kind ? { kind: failure.kind } : {});
      const res = await report.onRequestPost(postContext(payload, env));
      assert.equal(res.status, 502, JSON.stringify(failure));
      assert.equal(res.headers.get("content-type"),
        "application/json; charset=utf-8");
      const body = await res.text();
      assert.deepEqual(JSON.parse(body), {
        ok: false, error: "github issue creation failed",
      });
      assert.ok(!body.includes(env.GITHUB_TOKEN), "GitHub token leaked");
      assert.equal(reportKeys(kv).length, 0, "failed issue must not be recorded");
    } finally {
      restore();
    }
  }
});

await test("onRequestPost keeps recorded counts on comment failure", async () => {
  for (const [caseIndex, network] of [false, true].entries()) {
    const kv = makeKv();
    const state = { calls: [], issues: 0, comments: 0 };
    const restore = mockFetch(state);
    try {
      const env = baseEnv(kv);
      const payload = validPayload();
      const ip = `198.51.100.${caseIndex + 200}`;
      const first = await report.onRequestPost(postContext(payload, env, ip));
      assert.equal(first.status, 200);
      state.githubFailure = {
        method: "POST", path: "/issues/1/comments", status: 500,
        network, message: "test-token comment failure",
      };
      let res;
      for (let i = 0; i < 4; i++) {
        res = await report.onRequestPost(postContext(payload, env, ip));
      }
      assert.equal(res.status, 200);
      const j = await res.json();
      assert.equal(j.ok, true);
      assert.equal(j.duplicate, true);
      assert.equal(j.issue_url, "https://github.com/o/r/issues/1");
      assert.equal(j.fingerprint, "MEGU-E-1A2B3C4D");
      assert.equal(j.total_reports, 5);
      assert.equal(j.comment_posted, false);
      assert.equal(JSON.parse(kv.map.get("report:fp:MEGU-E-1A2B3C4D")).count, 5);
    } finally {
      restore();
    }
  }
});

await test("onRequestPost stacks bug reports without fingerprint", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const env = baseEnv(kv);
    const bug = validPayload({ kind: "bug" });
    delete bug.fingerprint;
    const r1 = await report.onRequestPost(postContext(bug, env));
    const j1 = await r1.json();
    assert.equal(j1.duplicate, false);
    const r2 = await report.onRequestPost(postContext(bug, env));
    const j2 = await r2.json();
    assert.equal(j2.duplicate, true);
    assert.equal(j2.total_reports, 2);
    assert.equal(state.issues, 1);
    assert.equal(reportKeys(kv).length, 1);
  } finally {
    restore();
  }
});

await test("onRequestPost routes ideas to a compact [Idea] issue", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const idea = validPayload({
      kind: "idea", summary: "Add a /megu status command",
    });
    delete idea.fingerprint;
    const res = await report.onRequestPost(postContext(idea, baseEnv(kv)));
    const j = await res.json();
    assert.equal(j.ok, true);
    assert.equal(j.issue_url, "https://github.com/o/r/issues/1");
    const create = state.calls.find(c =>
      c.url.includes("/issues") && c.method === "POST");
    const sent = JSON.parse(create.body);
    assert.match(sent.title, /^\[Idea\]/);
    assert.ok(sent.body.length < 2048);
    const duplicate = await report.onRequestPost(postContext({
      ...idea, summary: "  add a  /MEGU STATUS command  ",
    }, baseEnv(kv)));
    assert.equal(duplicate.status, 200);
    const second = await duplicate.json();
    assert.equal(second.duplicate, true);
    assert.equal(second.total_reports, 2);
    assert.equal(second.issue_url, j.issue_url);
    assert.equal(state.issues, 1);
    assert.equal(reportKeys(kv).length, 1);
  } finally {
    restore();
  }
});

await test("onRequestPost refuses oversized bodies with 413", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const big = JSON.stringify(validPayload({ blob: "x".repeat(5000) }));
    const res = await report.onRequestPost(
      postContext(big, baseEnv(kv)));
    assert.equal(res.status, 413);
    assert.equal(state.issues, 0);
  } finally {
    restore();
  }
});

await test("onRequestPost fails closed on bad payloads", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const env = baseEnv(kv);
    for (const [body, want] of [
      ["{not json", 400],
      [JSON.stringify({ schema: "other/9" }), 400],
      [JSON.stringify(validPayload({ kind: "weird" })), 400],
    ]) {
      const res = await report.onRequestPost(postContext(body, env));
      assert.equal(res.status, want, `body ${body.slice(0, 40)}`);
    }
    assert.equal(state.issues, 0);
  } finally {
    restore();
  }
});

await test("onRequestPost enforces Turnstile when configured", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0, turnstileOk: true };
  const restore = mockFetch(state);
  try {
    const env = Object.assign(baseEnv(kv), {
      TURNSTILE_SECRET_KEY: "ts-secret",
    });
    // Missing token -> 403, fail closed.
    const denied = await report.onRequestPost(
      postContext(validPayload(), env));
    assert.equal(denied.status, 403);
    assert.equal(state.issues, 0);

    // Valid token -> passes verification and files the issue.
    const okRes = await report.onRequestPost(
      postContext(validPayload({ turnstile_token: "tok-123" }), env));
    assert.equal(okRes.status, 200);
    assert.equal(state.issues, 1);
    assert.ok(state.calls.some(c =>
      c.url.includes("challenges.cloudflare.com")));

    // Siteverify says no -> 403.
    state.turnstileOk = false;
    const bad = await report.onRequestPost(
      postContext(validPayload({ turnstile_token: "bad" }), env));
    assert.equal(bad.status, 403);
    assert.equal(state.issues, 1);
  } finally {
    restore();
  }
});

await test("onRequestPost requires Turnstile for all kinds and rejects bad tokens", async () => {
  const state = { calls: [], issues: 0, comments: 0, turnstileOk: false };
  const restore = mockFetch(state);
  try {
    for (const [i, kind] of ["crash", "bug", "idea"].entries()) {
      const env = { ...baseEnv(makeKv()), TURNSTILE_SECRET_KEY: "secret" };
      const payload = validPayload({ kind });
      if (kind !== "crash") delete payload.fingerprint;
      const ip = `203.0.113.${i + 1}`;
      const missing = await report.onRequestPost(postContext(payload, env, ip));
      assert.equal(missing.status, 403, `${kind} missing token`);
      const invalid = await report.onRequestPost(postContext({
        ...payload, turnstile_token: "invalid",
      }, env, ip));
      assert.equal(invalid.status, 403, `${kind} invalid token`);
    }
    assert.equal(state.issues, 0);
  } finally {
    restore();
  }
});

await test("onRequestPost returns 503 without a secret or explicit development override", async () => {
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    for (const kind of ["crash", "bug", "idea"]) {
      const payload = validPayload({ kind });
      if (kind !== "crash") delete payload.fingerprint;
      const env = baseEnv(makeKv());
      delete env.ALLOW_DEV_UNVERIFIED_REPORTS;
      const res = await report.onRequestPost(postContext(payload, env));
      assert.equal(res.status, 503, kind);
      assert.match((await res.json()).error, /turnstile/);
    }
    assert.equal(state.calls.length, 0);
  } finally {
    restore();
  }
});

await test("onRequestPost allows unverified reports only with explicit development override", async () => {
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const env = baseEnv(makeKv());
    for (const kind of ["crash", "bug", "idea"]) {
      const payload = validPayload({ kind });
      if (kind !== "crash") delete payload.fingerprint;
      const res = await report.onRequestPost(postContext(payload, env));
      assert.equal(res.status, 200, kind);
    }
    assert.equal(state.issues, 3);
    assert.ok(state.calls.every(c => !c.url.includes("challenges.cloudflare.com")));
  } finally {
    restore();
  }
});

await test("onRequestPost rate limits each kind to five reports per IP in ten minutes", async () => {
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    for (const [i, kind] of ["crash", "bug", "idea"].entries()) {
      const kv = makeKv();
      const env = baseEnv(kv);
      const payload = validPayload({ kind });
      if (kind !== "crash") delete payload.fingerprint;
      const ip = `198.51.100.${i + 1}`;
      for (let n = 0; n < 5; n++) {
        const res = await report.onRequestPost(postContext(payload, env, ip));
        assert.equal(res.status, 200, `${kind} attempt ${n + 1}`);
      }
      const denied = await report.onRequestPost(postContext(payload, env, ip));
      assert.equal(denied.status, 429, kind);
      assert.deepEqual(await denied.json(), {
        ok: false, error: "report rate limit exceeded",
      });
      const limiterPut = kv.puts.find(p => p.key === `rl:${ip}`);
      assert.equal(limiterPut.options.expirationTtl, 600);
      // A different IP can still submit a report.
      const other = await report.onRequestPost(postContext(payload, env,
        `198.51.100.${i + 10}`));
      assert.equal(other.status, 200);
    }
    assert.equal(state.issues, 3);
  } finally {
    restore();
  }
});

await test("onRequestPost falls back to an isolate rate limit when KV is absent", async () => {
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const env = baseEnv(null);
    for (let n = 0; n < 5; n++) {
      const res = await report.onRequestPost(postContext(validPayload(), env));
      assert.equal(res.status, 503); // dedup itself still needs CRASH_INDEX
    }
    const denied = await report.onRequestPost(postContext(validPayload(), env));
    assert.equal(denied.status, 429);
    assert.equal(state.issues, 0);
  } finally {
    restore();
  }
});

await test("onRequestPost requires GITHUB_TOKEN and CRASH_INDEX", async () => {
  const kv = makeKv();
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const noToken = await report.onRequestPost(postContext(
      validPayload(), { CRASH_INDEX: kv, GITHUB_REPO: "o/r",
        ALLOW_DEV_UNVERIFIED_REPORTS: "1" }));
    assert.equal(noToken.status, 500);

    const noKv = await report.onRequestPost(postContext(
      validPayload(), { GITHUB_TOKEN: "t", GITHUB_REPO: "o/r",
        ALLOW_DEV_UNVERIFIED_REPORTS: "1" }));
    assert.equal(noKv.status, 503);
  } finally {
    restore();
  }
});

await test("onRequestGet /api/latest returns tiny release JSON", async () => {
  const state = { calls: [], issues: 0, comments: 0 };
  const restore = mockFetch(state);
  try {
    const res = await latest.onRequestGet({
      request: new Request("https://megu.example/api/latest"),
      env: { GITHUB_REPO: "o/r" },
    });
    assert.equal(res.status, 200);
    assert.equal(res.headers.get("cache-control"), "public, max-age=600");
    const j = await res.json();
    assert.deepEqual(j, {
      ok: true,
      version: "v0.9.1",
      published_at: "2026-11-20T10:00:00Z",
      html_url: "https://github.com/o/r/releases/tag/v0.9.1",
      mc_targets: ["1.21.1", "26.3"],
    });
    assert.ok(JSON.stringify(j).length < 512);
  } finally {
    restore();
  }
});

await test("onRequestGet /api/latest falls back to newest prerelease on 404", async () => {
  const realFetch = globalThis.fetch;
  const urls = [];
  globalThis.fetch = async (url) => {
    urls.push(String(url));
    if (String(url).endsWith("/releases/latest")) {
      return new Response("Not Found", { status: 404 });
    }
    return new Response(JSON.stringify([
      { draft: true, prerelease: true, tag_name: "v0.1.0-draft" },
      {
        draft: false,
        prerelease: true,
        tag_name: "v0.1.0-alpha",
        published_at: "2026-11-22T09:00:00Z",
        html_url: "https://github.com/o/r/releases/tag/v0.1.0-alpha",
      },
    ]), { status: 200, headers: { "content-type": "application/json" } });
  };
  try {
    const res = await latest.onRequestGet({
      request: new Request("https://megu.example/api/latest"),
      env: { GITHUB_REPO: "o/r" },
    });
    assert.equal(res.status, 200);
    const j = await res.json();
    assert.deepEqual(j, {
      ok: true,
      version: "v0.1.0-alpha",
      published_at: "2026-11-22T09:00:00Z",
      html_url: "https://github.com/o/r/releases/tag/v0.1.0-alpha",
      mc_targets: ["1.21.1", "26.3"],
      prerelease: true,
    });
    assert.equal(urls.length, 2);
  } finally {
    globalThis.fetch = realFetch;
  }
});

await test("onRequestGet /api/latest fails closed on upstream errors", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response("nope", { status: 503 });
  try {
    const res = await latest.onRequestGet({ request: null, env: {} });
    assert.equal(res.status, 502);
    const j = await res.json();
    assert.equal(j.ok, false);
  } finally {
    globalThis.fetch = realFetch;
  }
});

// ------------------------------------------------- /api/latest telemetry

function releaseFixture(overrides = {}) {
  return {
    draft: false,
    prerelease: false,
    tag_name: "v1.0.0",
    published_at: "2026-11-20T10:00:00Z",
    html_url: "https://github.com/o/r/releases/tag/v1.0.0",
    assets: [],
    ...overrides,
  };
}

function assetFixture(name, download_count, url) {
  return {
    name,
    size: 1024,
    download_count,
    browser_download_url: url ||
      `https://github.com/o/r/releases/download/v1.0.0/${name}`,
  };
}

// Mock both release endpoints: `latest` is served at /releases/latest
// (latestStatus when null, 404 by default) and `releases` at the list
// endpoint queried for per-release download telemetry.
function mockLatestFetch({ latest = null, latestStatus = 404, releases = [] }) {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    const u = String(url);
    if (u.endsWith("/releases/latest")) {
      if (latest === null) {
        return new Response("Not Found", { status: latestStatus });
      }
      return new Response(JSON.stringify(latest), {
        status: 200, headers: { "content-type": "application/json" } });
    }
    if (u.includes("/releases")) {
      return new Response(JSON.stringify(releases), {
        status: 200, headers: { "content-type": "application/json" } });
    }
    return new Response("unmocked " + u, { status: 500 });
  };
  return () => { globalThis.fetch = realFetch; };
}

function latestContext() {
  return {
    request: new Request("https://megu.example/api/latest"),
    env: { GITHUB_REPO: "o/r" },
  };
}

await test("onRequestGet /api/latest never counts metadata, drafts or test tags", async () => {
  const restore = mockLatestFetch({
    releases: [
      releaseFixture({
        tag_name: "v1.0.0",
        assets: [
          assetFixture("megu-installer.exe", 100),
          assetFixture("megu-bootstrapper.jar", 50),
          assetFixture("SHA256SUMS", 900),
          assetFixture("release-manifest.json", 800),
          assetFixture("megu.sbom.json", 700),
          assetFixture("megu-installer.exe.sig", 600),
          assetFixture("megu-installer.exe.sha256", 500),
          assetFixture("megu-installer.exe.asc", 400),
          assetFixture("README.txt", 300),
          assetFixture("CHANGELOG.md", 200),
        ],
      }),
      releaseFixture({
        draft: true, tag_name: "v1.0.1-rc",
        assets: [assetFixture("megu-installer.exe", 9999)],
      }),
      releaseFixture({
        tag_name: "v0.9.9-test",
        assets: [assetFixture("megu-installer.exe", 9999)],
      }),
      releaseFixture({
        tag_name: "v0.9.8-ci.3",
        assets: [assetFixture("megu-installer.exe", 9999)],
      }),
      releaseFixture({
        tag_name: "v0.9.7-dryrun",
        assets: [assetFixture("megu-installer.exe", 9999)],
      }),
      releaseFixture({
        tag_name: "v0.9.6.dev",
        assets: [assetFixture("megu-installer.exe", 9999)],
      }),
      releaseFixture({
        tag_name: "v0.9.5",
        assets: [
          assetFixture("megu-server-linux-x86_64.tar.gz", 25),
          assetFixture("notes.txt", 888),
        ],
      }),
    ],
  });
  try {
    const res = await latest.onRequestGet(latestContext());
    assert.equal(res.status, 200);
    assert.equal(res.headers.get("cache-control"), "public, max-age=600");
    const j = await res.json();
    assert.equal(j.ok, true);
    assert.equal(j.version, "v1.0.0");
    // 100 (.exe) + 50 (.jar) + 25 (.tar.gz from the older release) —
    // checksums, signatures, SBOMs, manifests, drafts and test/ci/dryrun/
    // dev tags contribute nothing.
    assert.equal(j.total_downloads, 175);
    assert.equal(j.release_downloads, 150);
    assert.deepEqual(j.assets.map(a => a.name),
      ["megu-installer.exe", "megu-bootstrapper.jar"]);
    assert.ok(j.assets.every(a => a.url.startsWith("https://github.com/")));
  } finally {
    restore();
  }
});

await test("onRequestGet /api/latest classifies kinds and fails closed on non-GitHub URLs", async () => {
  const restore = mockLatestFetch({
    releases: [
      releaseFixture({
        tag_name: "v2.0.0",
        assets: [
          assetFixture("megu-installer.exe", 100),
          assetFixture("megu-installer.dmg", 40),
          assetFixture("megu-bootstrapper.jar", 30),
          assetFixture("megu-prism-1.21.1.zip", 20),
          assetFixture("megu-1.21.1.mrpack", 15),
          assetFixture("megu-server-linux-x86_64.tar.gz", 10),
          assetFixture("megu-x86_64.AppImage", 5),
          assetFixture("megu-symbols.zip", 2),
          assetFixture("megu-stealer.exe", 7777,
            "https://evil-cdn.example/megu-stealer.exe"),
          assetFixture("megu-relative.exe", 7777,
            "/o/r/releases/download/v2.0.0/megu-relative.exe"),
        ],
      }),
    ],
  });
  try {
    const res = await latest.onRequestGet(latestContext());
    assert.equal(res.status, 200);
    const j = await res.json();
    assert.equal(j.version, "v2.0.0");
    assert.equal(j.total_downloads, 222);
    assert.equal(j.release_downloads, 222);
    const kinds = {};
    for (const a of j.assets) {
      kinds[a.name] = a.kind;
      assert.equal(typeof a.size, "number");
      assert.equal(typeof a.download_count, "number");
      assert.ok(a.url.startsWith("https://github.com/"), a.name);
    }
    assert.equal(j.assets.length, 8);
    assert.equal(kinds["megu-installer.exe"], "installer_windows");
    assert.equal(kinds["megu-installer.dmg"], "installer_macos");
    assert.equal(kinds["megu-bootstrapper.jar"], "installer_jar");
    assert.equal(kinds["megu-prism-1.21.1.zip"], "prism_zip");
    assert.equal(kinds["megu-1.21.1.mrpack"], "mrpack");
    assert.equal(kinds["megu-server-linux-x86_64.tar.gz"], "bundle_linux");
    assert.equal(kinds["megu-x86_64.AppImage"], "bundle_linux");
    assert.equal(kinds["megu-symbols.zip"], "other");
  } finally {
    restore();
  }
});

await test("onRequestGet /api/latest prefers stable latest and sums every eligible release", async () => {
  const restore = mockLatestFetch({
    latest: releaseFixture({ tag_name: "v1.0.0" }),
    releases: [
      releaseFixture({
        tag_name: "v1.1.0-alpha.1",
        prerelease: true,
        published_at: "2026-11-22T09:00:00Z",
        html_url: "https://github.com/o/r/releases/tag/v1.1.0-alpha.1",
        assets: [assetFixture("megu-installer.exe", 7)],
      }),
      releaseFixture({
        tag_name: "v1.0.0",
        assets: [
          assetFixture("megu-installer.exe", 100),
          assetFixture("megu-bootstrapper.jar", 23),
        ],
      }),
    ],
  });
  try {
    const res = await latest.onRequestGet(latestContext());
    assert.equal(res.status, 200);
    const j = await res.json();
    // The stable /releases/latest wins over the newer alpha, while the
    // alpha's real downloads still count toward the total.
    assert.equal(j.version, "v1.0.0");
    assert.equal(j.prerelease, undefined);
    assert.equal(j.release_downloads, 123);
    assert.equal(j.total_downloads, 130);
    assert.deepEqual(j.assets.map(a => a.name),
      ["megu-installer.exe", "megu-bootstrapper.jar"]);
  } finally {
    restore();
  }
});

// ---------------------------------------------------------------- summary

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
