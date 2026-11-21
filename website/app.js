// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//
// mc-spine landing page script.
//
// Two halves, one file:
//
// 1. A faithful browser-side port of the modpack pre-flight analyzer in
//    mod-adapter-core/src/modpack.rs (+ the version-range engine in
//    version_compat.rs and the 36-entry BUILTIN_SUPERSEDED_PERF_MODS catalog
//    in fallback.rs, which tools/mod_launcher.py also mirrors). It parses
//    Modrinth `modrinth.index.json` and CurseForge `manifest.json`
//    documents fail-closed — malformed JSON, wrong
//    formatVersion/manifestType/game, and path-traversal entries are hard
//    errors, never silent skips — and emits the same
//    `mc-spine-modpack-preflight/1` report shape the CLIs print.
//
// 2. The DOM wiring: SITE_CONFIG brand binding, the drag-and-drop checker
//    UI, and copy-to-clipboard buttons. Everything runs 100% client-side;
//    no manifest bytes ever leave the tab.
//
// Clean-room note (same contract as fallback.rs/modpack.rs): matching file
// names and mod ids against BUILTIN_SUPERSEDED_PERF_MODS only decides
// whether a user-supplied JAR is loaded or skipped. No code, assets, or
// data from any third-party mod is copied, linked, bundled, or derived —
// every built-in optimization is an original clean-room Rust
// implementation inside mc-spine, and no affiliation with any named mod is
// claimed.
(function (root) {
  "use strict";

  // ---------------------------------------------------------------------
  // Brand configuration (site-config.js -> window.SITE_CONFIG)
  // ---------------------------------------------------------------------

  var DEFAULT_CONFIG = {
    projectName: "mc-spine",
    cliPrefix: "mc-spine",
    tagline: "A Rust-native Minecraft server & client runtime",
    statusBadge: "Experimental Alpha",
    valueProp:
      "A Rust-native Minecraft server & client runtime with tiered " +
      "NeoForge/Fabric mod compatibility and Velocity forwarding support.",
    mcTargets: ["1.21.1", "26.3"],
    githubUrl: "https://github.com/nekomario28/mc-spine",
    releasesUrl: "https://github.com/nekomario28/mc-spine/releases",
    docsBaseUrl: "https://github.com/nekomario28/mc-spine/tree/main/docs",
    customDomain: "",
    flagshipMods: ["create"],
    profileMods: [],
  };

  function siteConfig() {
    var cfg = {};
    var key;
    for (key in DEFAULT_CONFIG) {
      if (Object.prototype.hasOwnProperty.call(DEFAULT_CONFIG, key)) {
        cfg[key] = DEFAULT_CONFIG[key];
      }
    }
    var ext = root.SITE_CONFIG;
    if (ext && typeof ext === "object") {
      for (key in ext) {
        if (Object.prototype.hasOwnProperty.call(ext, key)) {
          cfg[key] = ext[key];
        }
      }
    }
    return cfg;
  }

  // ---------------------------------------------------------------------
  // Errors (ModpackError / VersionRangeError — fail closed, never throw
  // anything else out of the parsers)
  // ---------------------------------------------------------------------

  function ModpackError(message) {
    this.name = "ModpackError";
    this.message = message;
  }
  ModpackError.prototype = Object.create(Error.prototype);
  ModpackError.prototype.constructor = ModpackError;

  function VersionRangeError(message) {
    this.name = "VersionRangeError";
    this.message = message;
  }
  VersionRangeError.prototype = Object.create(Error.prototype);
  VersionRangeError.prototype.constructor = VersionRangeError;

  // ---------------------------------------------------------------------
  // Multi-version compatibility engine — port of
  // mod-adapter-core/src/version_compat.rs (via tools/mod_launcher.py).
  //
  // Version keys are packed as major*2^32 + minor*2^16 + patch. Components
  // are u16, so every key is < 2^48 — exact in IEEE-754 doubles. KEY_MAX
  // stands in for u64::MAX; all real keys compare far below it.
  // ---------------------------------------------------------------------

  var KEY_MAX = Number.MAX_SAFE_INTEGER;
  var ASCII_WS = " \t\n\r\f";

  function isAsciiWs(ch) {
    return ASCII_WS.indexOf(ch) !== -1;
  }

  function vkey(v) {
    return v[0] * 0x100000000 + v[1] * 0x10000 + v[2];
  }

  function formatMcVersion(v) {
    return v[0] + "." + v[1] + "." + v[2];
  }

  // 1-3 all-digit components (parse_numeric_version).
  function parseNumericVersion(text) {
    if (!text) {
      throw new VersionRangeError("empty version");
    }
    var parts = text.split(".");
    if (parts.length > 3) {
      throw new VersionRangeError(
        "version '" + text + "' has more than three components");
    }
    var n = [0, 0, 0];
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i];
      if (!/^[0-9]+$/.test(part)) {
        throw new VersionRangeError(
          "version '" + text + "' has non-numeric component '" + part + "'");
      }
      n[i] = parseInt(part, 10);
      if (n[i] > 0xffff) {
        throw new VersionRangeError(
          "version '" + text + "' component '" + part + "' out of range");
      }
    }
    return n;
  }

  // Parse "1.21.1", "1.21" (= 1.21.0), "26.3" (= 26.3.0), or "1" (= 1.0.0);
  // null on invalid (McVersion::parse).
  function parseMcVersion(text) {
    try {
      return parseNumericVersion(text.trim());
    } catch (e) {
      if (e instanceof VersionRangeError) {
        return null;
      }
      throw e;
    }
  }

  // SemVer-ish version spec -> {lo: [maj,min,pat], specified} (parse_spec).
  function parseSpec(text) {
    var s = text.trim();
    var first = s.charAt(0);
    if (first === "v" || first === "V") {
      s = s.slice(1);
    }
    s = s.split("+")[0];
    if (!s) {
      throw new VersionRangeError("empty version spec");
    }
    if (s.indexOf("-") !== -1) {
      throw new VersionRangeError(
        "prerelease/hyphenated spec '" + s +
          "' is unsupported (use an explicit range)");
    }
    var parts = s.split(".");
    if (parts.length > 3) {
      throw new VersionRangeError(
        "version spec '" + s + "' has more than three components");
    }
    var lo = [0, 0, 0];
    var specified = parts.length;
    var sawWildcard = false;
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i];
      if (!part) {
        throw new VersionRangeError(
          "version spec '" + s + "' has an empty component");
      }
      if (part === "x" || part === "X" || part === "*") {
        sawWildcard = true;
        if (specified === parts.length) {
          specified = i;
        }
        continue;
      }
      if (sawWildcard) {
        throw new VersionRangeError(
          "version spec '" + s +
            "' has a numeric component after a wildcard");
      }
      if (!/^[0-9]+$/.test(part)) {
        throw new VersionRangeError(
          "version spec '" + s +
            "' has non-numeric component '" + part + "'");
      }
      lo[i] = parseInt(part, 10);
      if (lo[i] > 0xffff) {
        throw new VersionRangeError(
          "version spec '" + s + "' component '" + part + "' out of range");
      }
    }
    return { lo: lo, specified: specified };
  }

  function specLoKey(lo) {
    return lo[0] * 0x100000000 + lo[1] * 0x10000 + lo[2];
  }

  // Exclusive upper key of the spec band: 1.21.1 -> 1.21.2, 1.21.x -> 1.22.0.
  function specBandHi(lo, specified) {
    if (specified === 0) {
      return KEY_MAX;
    }
    if (specified === 1) {
      return (lo[0] + 1) * 0x100000000;
    }
    if (specified === 2) {
      return lo[0] * 0x100000000 + (lo[1] + 1) * 0x10000;
    }
    return specLoKey(lo) + 1;
  }

  var PREDICATE_OPS = ["~", "^", ">=", "<=", ">", "<", "="];

  // One SemVer predicate token -> inclusive [lo, hi] key interval.
  function predicateInterval(tok) {
    if (tok === "*" || tok === "x" || tok === "X") {
      return [0, KEY_MAX];
    }
    var op = "";
    var body = tok;
    for (var i = 0; i < PREDICATE_OPS.length; i++) {
      if (tok.indexOf(PREDICATE_OPS[i]) === 0) {
        op = PREDICATE_OPS[i];
        body = tok.slice(op.length);
        break;
      }
    }
    if (!body) {
      throw new VersionRangeError(
        "predicate '" + tok + "' is missing a version");
    }
    var spec = parseSpec(body);
    if (spec.specified === 0) {
      // `*`, `~*`, `^*`, `>=1.x` style: wildcard-only spec matches anything.
      return [0, KEY_MAX];
    }
    var lo = specLoKey(spec.lo);
    var hiBand = specBandHi(spec.lo, spec.specified);
    var hi;
    switch (op) {
      case ">=":
        return [lo, KEY_MAX];
      case ">":
        return [hiBand, KEY_MAX];
      case "<=":
        return spec.specified === 3 ? [0, lo] : [0, hiBand - 1];
      case "<":
        if (lo === 0) {
          return [1, 0]; // `<0.0.0` matches nothing; empty interval marker.
        }
        return [0, lo - 1];
      case "=":
        return [lo, hiBand - 1];
      case "~":
        // Tilde keeps the minor band: `~1.21.1` is `>=1.21.1 <1.22.0`.
        hi = spec.specified >= 2
          ? spec.lo[0] * 0x100000000 + (spec.lo[1] + 1) * 0x10000
          : (spec.lo[0] + 1) * 0x100000000;
        return [lo, hi - 1];
      case "^":
        // Caret allows everything below the leftmost non-zero component's
        // increment.
        if (spec.lo[0] > 0) {
          hi = (spec.lo[0] + 1) * 0x100000000;
        } else if (spec.specified >= 2 && spec.lo[1] > 0) {
          hi = (spec.lo[1] + 1) * 0x10000;
        } else if (spec.specified === 3) {
          hi = spec.lo[2] + 1;
        } else if (spec.specified === 2) {
          hi = 0x10000;
        } else {
          hi = 0x100000000;
        }
        return [lo, hi - 1];
      default:
        // Bare spec: full pin or its whole band (SemVer range convention).
        return [lo, hiBand - 1];
    }
  }

  // One bracketed Maven interval member starting at text[i].
  function parseMavenMember(text, i) {
    var openIncl = text.charAt(i) === "[";
    var close = -1;
    for (var j = i + 1; j < text.length; j++) {
      var ch = text.charAt(j);
      if (ch === "]" || ch === ")") {
        close = j;
        break;
      }
    }
    if (close < 0) {
      throw new VersionRangeError("unterminated interval in '" + text + "'");
    }
    var closeIncl = text.charAt(close) === "]";
    var inner = text.slice(i + 1, close);
    var parts = inner.split(",");
    if (parts.length === 1) {
      var pin = parts[0].trim();
      if (!pin) {
        throw new VersionRangeError(
          "empty interval '" + inner + "' in '" + text + "'");
      }
      if (!openIncl) {
        throw new VersionRangeError(
          "exact pin '" + inner + "' in '" + text +
            "' must use square brackets");
      }
      var k = vkey(parseNumericVersion(pin));
      return { lo: k, hi: k, next: close + 1 };
    }
    if (parts.length === 2) {
      var loS = parts[0].trim();
      var hiS = parts[1].trim();
      var lo;
      var hi;
      if (loS) {
        var kLo = vkey(parseNumericVersion(loS));
        lo = openIncl ? kLo : kLo + 1;
      } else {
        lo = 0;
      }
      if (hiS) {
        var kHi = vkey(parseNumericVersion(hiS));
        hi = closeIncl ? kHi : Math.max(kHi - 1, 0);
      } else {
        hi = KEY_MAX;
      }
      return { lo: lo, hi: hi, next: close + 1 };
    }
    throw new VersionRangeError(
      "interval '" + inner + "' in '" + text +
        "' has more than two bounds");
  }

  // Maven / NeoForge range -> union of inclusive version-key intervals.
  function parseMavenRange(text) {
    var intervals = [];
    var i = 0;
    var n = text.length;
    for (;;) {
      while (i < n && isAsciiWs(text.charAt(i))) {
        i++;
      }
      if (i >= n) {
        if (!intervals.length) {
          throw new VersionRangeError("empty version range");
        }
        throw new VersionRangeError("trailing separator in '" + text + "'");
      }
      var ch = text.charAt(i);
      if (ch === "[" || ch === "(") {
        var member = parseMavenMember(text, i);
        intervals.push([member.lo, member.hi]);
        i = member.next;
      } else {
        // Bare member inside the list: exact pin (`(,1.0],1.2` style).
        var end = text.indexOf(",", i);
        if (end < 0) {
          end = n;
        }
        var memberStr = text.slice(i, end).trim();
        if (!memberStr) {
          throw new VersionRangeError("empty list member in '" + text + "'");
        }
        var k = vkey(parseNumericVersion(memberStr));
        intervals.push([k, k]);
        i = end;
      }
      while (i < n && isAsciiWs(text.charAt(i))) {
        i++;
      }
      if (i >= n) {
        break;
      }
      if (text.charAt(i) !== ",") {
        throw new VersionRangeError(
          "expected ',' between intervals in '" + text + "'");
      }
      i++;
    }
    return intervals;
  }

  // Fabric/SemVer range -> union of inclusive version-key intervals.
  function parseSemverRange(text) {
    var intervals = [];
    var arms = text.split("||");
    for (var a = 0; a < arms.length; a++) {
      var arm = arms[a].trim();
      if (!arm) {
        throw new VersionRangeError("empty '||' arm in '" + text + "'");
      }
      var lo = 0;
      var hi = KEY_MAX;
      var tokens = arm.split(/[ \t\n\r\f]+/);
      var j = 0;
      while (j < tokens.length) {
        var tok = tokens[j];
        // Tolerate `>= 1.21.1`: a lone operator binds the next token.
        if (PREDICATE_OPS.indexOf(tok) !== -1 && j + 1 < tokens.length) {
          tok += tokens[j + 1];
          j++;
        }
        var iv = predicateInterval(tok);
        if (iv[0] > lo) {
          lo = iv[0];
        }
        if (iv[1] < hi) {
          hi = iv[1];
        }
        j++;
      }
      intervals.push([lo, hi]);
    }
    return intervals;
  }

  // Parse `declared`: Maven when it opens with '['/'(', else Fabric SemVer.
  function parseVersionRange(declared) {
    var trimmed = declared.trim();
    if (!trimmed) {
      throw new VersionRangeError("empty version range");
    }
    var first = trimmed.charAt(0);
    var intervals = first === "[" || first === "("
      ? parseMavenRange(trimmed)
      : parseSemverRange(trimmed);
    return { declared: trimmed, intervals: intervals };
  }

  function rangeContains(range, v) {
    var k = vkey(v);
    return range.intervals.some(function (iv) {
      return iv[0] <= k && k <= iv[1];
    });
  }

  // Whether the range admits any version on v's major.minor line.
  function overlapsMinorBand(range, v) {
    var lo = vkey([v[0], v[1], 0]);
    var hi = vkey([v[0], v[1], 0xffff]);
    return range.intervals.some(function (iv) {
      return iv[0] <= hi && iv[1] >= lo;
    });
  }

  // Check one mod's declared range against the server target
  // (check_mod_mc_version): kind is "compatible" | "minor_mismatch" |
  // "major_mismatch" | "invalid_range".
  function checkModMcVersion(target, declaredRange) {
    var declared = declaredRange.trim();
    var range;
    try {
      range = parseVersionRange(declaredRange);
    } catch (e) {
      if (e instanceof VersionRangeError) {
        return { kind: "invalid_range", declared: declared, reason: e.message };
      }
      throw e;
    }
    if (rangeContains(range, target)) {
      return { kind: "compatible", declared: null, reason: null };
    }
    if (overlapsMinorBand(range, target)) {
      return { kind: "minor_mismatch", declared: declared, reason: null };
    }
    return { kind: "major_mismatch", declared: declared, reason: null };
  }

  // ---------------------------------------------------------------------
  // Built-in superseded performance mods — EXACT mirror of
  // BUILTIN_SUPERSEDED_PERF_MODS in mod-adapter-core/src/fallback.rs.
  // website/check_site.py keeps this list in sync entry-for-entry.
  // ---------------------------------------------------------------------

  var BUILTIN_SUPERSEDED_PERF_MODS = [
    // Rendering / mesh / LOD.
    "sodium",
    "embeddium",
    "rubidium",
    "xenon",
    "magnesium",
    "immediatelyfast",
    "entityculling",
    "moreculling",
    "distanthorizons",
    "voxy",
    "bobby",
    "farsight",
    "indium",
    "reeses-sodium-options",
    "sodium-extra",
    "embeddium_extra",
    // Server tick / lighting / memory / startup.
    "lithium",
    "radium",
    "canary",
    "starlight",
    "phosphor",
    "scalpel",
    "ferritecore",
    "modernfix",
    "noisium",
    "c2me",
    "krypton",
    "lazydfu",
    "memoryleakfix",
    "alternate-current",
    "clumps",
    "saturn",
    "smoothboot",
    "fastload",
    "dashloader",
    "servercore",
  ];

  var MODRINTH_LOADER_DEPENDENCIES = [
    "neoforge",
    "forge",
    "fabric-loader",
    "quilt-loader",
    "fabric-api",
  ];

  // Entry class spellings mirror the mc-spine-modpack-preflight/1 schema.
  var CLASS_SUPERSEDED_PERF_MOD = "superseded_perf_mod";
  var CLASS_CLIENT_ONLY_SKIPPED = "client_only_skipped";
  var CLASS_NATIVE_FLAGSHIP = "native_flagship";
  var CLASS_UNIVERSAL_NATIVE = "universal_native";
  var CLASS_NEEDS_INSPECTION = "needs_inspection";

  var READINESS_READY_NATIVE = "ready_native";
  var READINESS_READY_WITH_AUTO_SKIPS = "ready_with_auto_skips";
  var READINESS_NEEDS_JAR_INSPECTION = "needs_jar_inspection";
  var READINESS_VERSION_MISMATCH = "version_mismatch";

  // Standalone-manifest size cap, mirroring MAX_MANIFEST_MEMBER_BYTES.
  var MAX_MANIFEST_MEMBER_BYTES = 16 * 1024 * 1024;

  // ---------------------------------------------------------------------
  // JSON field helpers (req_obj/req_str/req_u64/req_array)
  // ---------------------------------------------------------------------

  function isObj(v) {
    return typeof v === "object" && v !== null && !Array.isArray(v);
  }

  function hasKey(obj, key) {
    return Object.prototype.hasOwnProperty.call(obj, key);
  }

  // JSON value -> u64, or null (JsonValue::as_u64).
  function optU64(v) {
    return typeof v === "number" && isFinite(v) &&
      Math.floor(v) === v && v >= 0 ? v : null;
  }

  function reqObj(obj, key, ctx) {
    var v = obj[key];
    if (!isObj(v)) {
      throw new ModpackError(ctx + ": missing object '" + key + "'");
    }
    return v;
  }

  function reqStr(obj, key, ctx) {
    var v = obj[key];
    if (typeof v !== "string") {
      throw new ModpackError(ctx + ": missing string '" + key + "'");
    }
    return v;
  }

  function reqU64(obj, key, ctx) {
    var v = optU64(obj[key]);
    if (v === null) {
      throw new ModpackError(ctx + ": missing number '" + key + "'");
    }
    return v;
  }

  function reqArray(obj, key, ctx) {
    var v = obj[key];
    if (!Array.isArray(v)) {
      throw new ModpackError(ctx + ": missing array '" + key + "'");
    }
    return v;
  }

  function parseJsonDoc(text, ctx) {
    var doc;
    try {
      doc = JSON.parse(text);
    } catch (e) {
      throw new ModpackError(ctx + ": " + e.message);
    }
    if (!isObj(doc)) {
      throw new ModpackError(ctx + ": root must be an object");
    }
    return doc;
  }

  // ---------------------------------------------------------------------
  // Manifest parsing (from_modrinth_json / from_curseforge_json)
  // ---------------------------------------------------------------------

  // Modrinth `env.client`/`env.server` flag -> required/optional/unsupported.
  function parseEnvSupport(value, ctx) {
    if (value === undefined) {
      return "required";
    }
    if (value === "required" || value === "optional" ||
        value === "unsupported") {
      return value;
    }
    if (typeof value === "string") {
      throw new ModpackError(
        ctx + ": unknown env value '" + value +
          "' (expected required/optional/unsupported)");
    }
    throw new ModpackError(ctx + ": env value must be a string");
  }

  // Fail-closed manifest member path validation (validate_member_path):
  // rejects `..` traversal, absolute paths, Windows drive letters, backslash
  // separators, empty/`.` segments, and NUL bytes.
  function validateMemberPath(path, ctx) {
    if (!path) {
      throw new ModpackError(ctx + ": empty path");
    }
    if (path.indexOf("\u0000") !== -1) {
      throw new ModpackError(ctx + ": NUL byte in path '" + path + "'");
    }
    if (path.indexOf("\\") !== -1) {
      throw new ModpackError(
        ctx + ": backslash separator not allowed in path '" + path + "'");
    }
    if (path.charAt(0) === "/") {
      throw new ModpackError(ctx + ": absolute path '" + path + "'");
    }
    if (path.length >= 2 && /^[a-zA-Z]$/.test(path.charAt(0)) &&
        path.charAt(1) === ":") {
      throw new ModpackError(
        ctx + ": Windows drive-letter path '" + path + "'");
    }
    var segments = path.split("/");
    for (var i = 0; i < segments.length; i++) {
      var seg = segments[i];
      if (seg === "") {
        throw new ModpackError(
          ctx + ": empty path segment in '" + path + "'");
      }
      if (seg === ".") {
        throw new ModpackError(ctx + ": '.' path segment in '" + path + "'");
      }
      if (seg === "..") {
        throw new ModpackError(
          ctx + ": '..' traversal segment in '" + path + "'");
      }
    }
  }

  var VERSIONISH_PREFIXES = [
    "snapshot", "release", "build", "beta", "alpha", "rev", "pre",
    "mc", "rc", "v", "b", "r",
  ];
  var VERSIONISH_MARKERS = [
    "b", "beta", "alpha", "rc", "pre", "preview", "release", "final",
    "snapshot",
  ];
  var LOADERISH_TOKENS = [
    "fabric", "forge", "neoforge", "quilt", "liteloader", "rift",
    "common", "universal", "multiloader", "all",
  ];

  // is_versionish_token: digits/dots, prefixed versions, lone markers.
  function isVersionishToken(tok) {
    if (!tok) {
      return false;
    }
    if (/^[0-9.]+$/.test(tok) && /[0-9]/.test(tok)) {
      return true;
    }
    for (var i = 0; i < VERSIONISH_PREFIXES.length; i++) {
      var prefix = VERSIONISH_PREFIXES[i];
      if (tok.indexOf(prefix) === 0) {
        var rest = tok.slice(prefix.length);
        if (rest && /^[0-9.]+$/.test(rest)) {
          return true;
        }
      }
    }
    return VERSIONISH_MARKERS.indexOf(tok) !== -1;
  }

  // Case-insensitive `.jar` check (is_jar_name) — an uppercase `MOD.JAR` is
  // still a Java JAR and must never slip through as a data file.
  function isJarName(name) {
    return name.length > 4 &&
      name.slice(-4).toLowerCase() === ".jar";
  }

  // Normalized candidate mod id from a JAR path's basename, or null
  // (infer_candidate_mod_id): lowercase, `_`/`+` folded to `-`, trailing
  // version and loader tokens stripped.
  function inferCandidateModId(path) {
    var name = path.split("/").pop();
    if (!isJarName(name)) {
      return null;
    }
    var normalized = name.slice(0, -4).toLowerCase()
      .replace(/_/g, "-").replace(/\+/g, "-");
    var tokens = normalized.split("-").filter(function (t) {
      return t.length > 0;
    });
    while (tokens.length &&
           (isVersionishToken(tokens[tokens.length - 1]) ||
            LOADERISH_TOKENS.indexOf(tokens[tokens.length - 1]) !== -1)) {
      tokens.pop();
    }
    return tokens.length ? tokens.join("-") : null;
  }

  // Mod id comparison key (normalized_mod_key): case-folded with `-`/`_`
  // removed.
  function normalizedModKey(modId) {
    return modId.toLowerCase().replace(/[-_]/g, "");
  }

  // CurseForge slug -> the same id space infer_candidate_mod_id produces.
  function normalizedSlugId(slug) {
    return slug.toLowerCase().replace(/_/g, "-");
  }

  // Parse a Modrinth `modrinth.index.json` document (from_modrinth_json).
  function parseModrinthManifest(text) {
    var ctx = "modrinth.index.json";
    var root = parseJsonDoc(text, ctx);

    var formatVersion = reqU64(root, "formatVersion", ctx);
    if (formatVersion !== 1) {
      throw new ModpackError(
        ctx + ": unsupported formatVersion " + formatVersion +
          " (expected 1)");
    }
    var game = reqStr(root, "game", ctx);
    if (game !== "minecraft") {
      throw new ModpackError(
        ctx + ": unsupported game '" + game + "' (expected 'minecraft')");
    }
    var name = reqStr(root, "name", ctx);
    var version = reqStr(root, "versionId", ctx);

    var deps = reqObj(root, "dependencies", ctx);
    var mcVersion = reqStr(deps, "minecraft", ctx);
    var loaders = [];
    Object.keys(deps).sort().forEach(function (depId) {
      if (depId === "minecraft" ||
          MODRINTH_LOADER_DEPENDENCIES.indexOf(depId) === -1) {
        return;
      }
      var depVersion = deps[depId];
      if (typeof depVersion !== "string") {
        throw new ModpackError(
          ctx + ": dependencies." + depId + " must be a version string");
      }
      loaders.push({ id: depId, version: depVersion, primary: true });
    });

    var files = [];
    reqArray(root, "files", ctx).forEach(function (entry, i) {
      var fctx = ctx + ": files[" + i + "]";
      if (!isObj(entry)) {
        throw new ModpackError(fctx + ": must be an object");
      }
      var path = reqStr(entry, "path", fctx);
      validateMemberPath(path, fctx);

      var clientSupport = "required";
      var serverSupport = "required";
      if (hasKey(entry, "env")) {
        var env = entry.env;
        if (!isObj(env)) {
          throw new ModpackError(fctx + ": env must be an object");
        }
        clientSupport = parseEnvSupport(env.client, fctx);
        serverSupport = parseEnvSupport(env.server, fctx);
      }

      var hashes = isObj(entry.hashes) ? entry.hashes : {};
      files.push({
        path: path,
        candidate_mod_id: inferCandidateModId(path),
        client_support: clientSupport,
        server_support: serverSupport,
        required: true,
        file_size: optU64(entry.fileSize),
        sha1: typeof hashes.sha1 === "string" ? hashes.sha1 : null,
        sha512: typeof hashes.sha512 === "string" ? hashes.sha512 : null,
        project_id: null,
        file_id: null,
      });
    });

    return {
      format: "modrinth",
      name: name,
      version: version,
      mc_version: mcVersion,
      loaders: loaders,
      files: files,
    };
  }

  // Parse a CurseForge `manifest.json` document (from_curseforge_json).
  function parseCurseforgeManifest(text) {
    var ctx = "manifest.json";
    var root = parseJsonDoc(text, ctx);

    var manifestType = reqStr(root, "manifestType", ctx);
    if (manifestType !== "minecraftModpack") {
      throw new ModpackError(
        ctx + ": unsupported manifestType '" + manifestType +
          "' (expected 'minecraftModpack')");
    }
    var manifestVersion = reqU64(root, "manifestVersion", ctx);
    if (manifestVersion !== 1) {
      throw new ModpackError(
        ctx + ": unsupported manifestVersion " + manifestVersion +
          " (expected 1)");
    }
    var name = reqStr(root, "name", ctx);
    var version = reqStr(root, "version", ctx);

    var mc = reqObj(root, "minecraft", ctx);
    var mcVersion = reqStr(mc, "version", ctx);
    var loaders = [];
    if (Array.isArray(mc.modLoaders)) {
      mc.modLoaders.forEach(function (entry, i) {
        var lctx = ctx + ": minecraft.modLoaders[" + i + "]";
        if (!isObj(entry)) {
          throw new ModpackError(lctx + ": must be an object");
        }
        var idField = reqStr(entry, "id", lctx);
        // Ids look like "neoforge-21.1.93": loader + '-' + version.
        var split = idField.indexOf("-");
        var loaderId = split === -1 ? idField : idField.slice(0, split);
        var loaderVersion = split === -1 ? "" : idField.slice(split + 1);
        loaders.push({
          id: loaderId,
          version: loaderVersion,
          primary: typeof entry.primary === "boolean" ? entry.primary : false,
        });
      });
    }

    var files = [];
    reqArray(root, "files", ctx).forEach(function (entry, i) {
      var fctx = ctx + ": files[" + i + "]";
      if (!isObj(entry)) {
        throw new ModpackError(fctx + ": must be an object");
      }
      var projectId = reqU64(entry, "projectID", fctx);
      var fileId = reqU64(entry, "fileID", fctx);
      var required = typeof entry.required === "boolean"
        ? entry.required : true;

      var slug = typeof entry.slug === "string" ? entry.slug : null;
      var fileName = typeof entry.fileName === "string"
        ? entry.fileName : null;
      [["slug", slug], ["fileName", fileName]].forEach(function (pair) {
        var v = pair[1];
        if (v !== null &&
            (!v || v.indexOf("/") !== -1 || v.indexOf("\\") !== -1 ||
             v.indexOf("..") !== -1 || v.indexOf("\u0000") !== -1)) {
          throw new ModpackError(
            fctx + ": unsafe " + pair[0] + " '" + v +
              "' (must be a bare name)");
        }
      });

      var path;
      var candidate;
      if (fileName !== null) {
        path = "mods/" + fileName;
        candidate = slug !== null
          ? normalizedSlugId(slug)
          : inferCandidateModId(fileName);
      } else if (slug !== null) {
        path = "mods/" + slug + ".jar";
        candidate = normalizedSlugId(slug);
      } else {
        path = "mods/curseforge-project-" + projectId +
          "-file-" + fileId + ".jar";
        candidate = null;
      }
      files.push({
        path: path,
        candidate_mod_id: candidate,
        client_support: "required",
        server_support: "required",
        required: required,
        file_size: null,
        sha1: null,
        sha512: null,
        project_id: projectId,
        file_id: fileId,
      });
    });

    return {
      format: "curseforge",
      name: name,
      version: version,
      mc_version: mcVersion,
      loaders: loaders,
      files: files,
    };
  }

  // ---------------------------------------------------------------------
  // Analysis (classify_entry / analyze_modpack / report helpers)
  // ---------------------------------------------------------------------

  function classifyEntry(f, superseded, flagship, profile) {
    if (f.server_support === "unsupported") {
      return {
        cls: CLASS_CLIENT_ONLY_SKIPPED,
        note: "client-side only; a dedicated server never installs it",
      };
    }
    if (f.candidate_mod_id !== null) {
      var key = normalizedModKey(f.candidate_mod_id);
      if (superseded[key]) {
        return {
          cls: CLASS_SUPERSEDED_PERF_MOD,
          note: "redundant performance mod — mc-spine already ships an " +
            "equivalent clean-room optimization",
        };
      }
      if (flagship[key]) {
        return {
          cls: CLASS_NATIVE_FLAGSHIP,
          note: "handled by a flagship native Rust adapter (no Java)",
        };
      }
      if (profile[key]) {
        return {
          cls: CLASS_UNIVERSAL_NATIVE,
          note: "covered by a declarative offload profile (no Java)",
        };
      }
    }
    if (f.path.indexOf("datapacks/") === 0) {
      return {
        cls: CLASS_UNIVERSAL_NATIVE,
        note: "datapack — pure data, runs natively",
      };
    }
    if (!isJarName(f.path.split("/").pop())) {
      return {
        cls: CLASS_UNIVERSAL_NATIVE,
        note: "data/resource file — no Java bytecode to adapt",
      };
    }
    return {
      cls: CLASS_NEEDS_INSPECTION,
      note: "unrecognized Java mod — its bytecode still needs a " +
        "compatibility inspection before launch",
    };
  }

  function keySet(ids) {
    var set = {};
    ids.forEach(function (id) {
      set[normalizedModKey(id)] = true;
    });
    return set;
  }

  function entryDisplayName(e) {
    return e.candidate_mod_id || e.path.split("/").pop();
  }

  function namesOfClass(entries, classes) {
    return entries
      .filter(function (e) {
        return classes.indexOf(e["class"]) !== -1;
      })
      .map(entryDisplayName)
      .sort();
  }

  // analyze_modpack: classify a parsed manifest against the server target,
  // flagship adapter ids, and declarative profile ids.
  function analyzeModpack(manifest, target, flagshipMods, profileMods) {
    var verdict = checkModMcVersion(target, manifest.mc_version);
    var superseded = keySet(BUILTIN_SUPERSEDED_PERF_MODS);
    var flagship = keySet(flagshipMods);
    var profile = keySet(profileMods);

    var entries = manifest.files.map(function (f) {
      var c = classifyEntry(f, superseded, flagship, profile);
      return {
        path: f.path,
        candidate_mod_id: f.candidate_mod_id,
        "class": c.cls,
        note: c.note,
      };
    });

    var readiness;
    if (verdict.kind !== "compatible") {
      readiness = READINESS_VERSION_MISMATCH;
    } else if (entries.some(function (e) {
        return e["class"] === CLASS_NEEDS_INSPECTION;
      })) {
      readiness = READINESS_NEEDS_JAR_INSPECTION;
    } else if (entries.some(function (e) {
        return e["class"] === CLASS_SUPERSEDED_PERF_MOD ||
          e["class"] === CLASS_CLIENT_ONLY_SKIPPED;
      })) {
      readiness = READINESS_READY_WITH_AUTO_SKIPS;
    } else {
      readiness = READINESS_READY_NATIVE;
    }

    return {
      schema: "mc-spine-modpack-preflight/1",
      pack_name: manifest.name,
      pack_version: manifest.version,
      format: manifest.format,
      mc_version_declared: manifest.mc_version,
      target: formatMcVersion(target),
      loaders: manifest.loaders,
      version_verdict: verdict,
      entries: entries,
      readiness: readiness,
      skipped_perf_mods: namesOfClass(entries, [CLASS_SUPERSEDED_PERF_MOD]),
      client_only_mods: namesOfClass(entries, [CLASS_CLIENT_ONLY_SKIPPED]),
      native_mods: namesOfClass(
        entries, [CLASS_NATIVE_FLAGSHIP, CLASS_UNIVERSAL_NATIVE]),
      needs_inspection_mods: namesOfClass(entries, [CLASS_NEEDS_INSPECTION]),
    };
  }

  // Jargon-free operator summary (format_human_summary port).
  function formatHumanSummary(report) {
    var formatName = report.format === "modrinth"
      ? "Modrinth pack" : "CurseForge pack";
    var lines = [
      "Pre-flight report for \"" + report.pack_name + "\" " +
        report.pack_version + " (" + formatName + ")",
    ];
    var verdict = report.version_verdict;
    if (verdict.kind === "compatible") {
      lines.push(
        "The pack targets Minecraft " + report.mc_version_declared +
          ", which matches this server.");
    } else if (verdict.kind === "minor_mismatch" ||
               verdict.kind === "major_mismatch") {
      lines.push(
        "The pack targets Minecraft " + report.mc_version_declared +
          " but this server runs " + report.target +
          " — the versions do not match.");
    } else {
      lines.push(
        "The pack declares an unreadable Minecraft version '" +
          verdict.declared +
          "', so it cannot be confirmed against this server (" +
          report.target + ").");
    }

    lines.push("Checked " + report.entries.length + " pack file(s):");
    var skipped = report.skipped_perf_mods;
    if (skipped.length) {
      lines.push(
        "  - " + skipped.length +
          " performance mod(s) skipped automatically — mc-spine already " +
          "has the same optimizations built in (clean-room): " +
          skipped.join(", "));
    }
    var clientOnly = report.client_only_mods;
    if (clientOnly.length) {
      lines.push(
        "  - " + clientOnly.length +
          " client-side mod(s) a dedicated server does not install: " +
          clientOnly.join(", "));
    }
    var native = report.native_mods;
    if (native.length) {
      lines.push(
        "  - " + native.length +
          " mod(s) run natively with no Java needed: " +
          native.join(", "));
    }
    var inspect = report.needs_inspection_mods;
    if (inspect.length) {
      lines.push(
        "  - " + inspect.length +
          " Java mod(s) still need a compatibility inspection before " +
          "launch: " + inspect.join(", "));
    }

    var result;
    switch (report.readiness) {
      case READINESS_READY_NATIVE:
        result = "ready — every file in this pack runs natively on " +
          "mc-spine.";
        break;
      case READINESS_READY_WITH_AUTO_SKIPS:
        result = "ready — redundant and client-only files are skipped " +
          "automatically; the rest runs natively.";
        break;
      case READINESS_NEEDS_JAR_INSPECTION:
        result = "almost ready — the Java mod(s) listed above still need " +
          "a compatibility check before launch.";
        break;
      default:
        result = "not compatible — this pack was built for a different " +
          "Minecraft version.";
        break;
    }
    lines.push("Result: " + result);
    return lines.join("\n");
  }

  // ---------------------------------------------------------------------
  // Public entry point: analyze a manifest's JSON text.
  //
  // `text` is the raw file contents; `opts` may carry:
  //   fileName     — original file name, used in errors and to sniff the
  //                  format when root keys are ambiguous
  //   target       — server Minecraft version ("1.21.1" or "26.3";
  //                  defaults to SITE_CONFIG.mcTargets[0])
  //   flagshipMods — flagship adapter ids (default SITE_CONFIG.flagshipMods)
  //   profileMods  — declarative profile ids (default SITE_CONFIG.profileMods)
  //
  // Returns the mc-spine-modpack-preflight/1 report object; throws
  // ModpackError on any malformed or unsafe input (fail closed).
  // ---------------------------------------------------------------------

  function analyzeModpackJson(text, opts) {
    opts = opts || {};
    var cfg = siteConfig();
    var fileName = opts.fileName || "";
    var display = fileName || "input.json";
    if (typeof text !== "string") {
      throw new ModpackError(display + ": manifest is not UTF-8 text");
    }
    var byteLen = typeof TextEncoder !== "undefined"
      ? new TextEncoder().encode(text).length
      : text.length;
    if (byteLen > MAX_MANIFEST_MEMBER_BYTES) {
      throw new ModpackError(
        display + ": manifest exceeds the 16 MiB limit");
    }

    var targetStr = opts.target ||
      (cfg.mcTargets && cfg.mcTargets[0]) || "1.21.1";
    var target = parseMcVersion(String(targetStr));
    if (target === null) {
      throw new ModpackError(
        "invalid target '" + targetStr +
          "' (expected a numeric release like 1.21.1)");
    }
    var flagship = opts.flagshipMods || cfg.flagshipMods || ["create"];
    var profile = opts.profileMods || cfg.profileMods || [];

    var doc;
    try {
      doc = JSON.parse(text);
    } catch (e) {
      throw new ModpackError(display + ": invalid JSON (" + e.message + ")");
    }
    if (!isObj(doc)) {
      throw new ModpackError(display + ": root must be an object");
    }
    var base = fileName.split("/").pop();
    var manifest;
    if (hasKey(doc, "manifestType") || base === "manifest.json") {
      manifest = parseCurseforgeManifest(text);
    } else if (hasKey(doc, "formatVersion") || hasKey(doc, "game") ||
               base === "modrinth.index.json") {
      manifest = parseModrinthManifest(text);
    } else {
      throw new ModpackError(
        display + ": cannot determine pack format (expected a Modrinth " +
          "modrinth.index.json or a CurseForge manifest.json)");
    }
    return analyzeModpack(manifest, target, flagship, profile);
  }

  // ---------------------------------------------------------------------
  // Built-in sample pack: a realistic 1.21.1 Modrinth index exercising
  // every classification — flagship `create`, several superseded perf
  // mods, a client-only shader mod, a datapack, a resourcepack, and one
  // unrecognized Java JAR for the inspection bucket.
  // ---------------------------------------------------------------------

  var SAMPLE_MODRINTH_INDEX_JSON = JSON.stringify({
    formatVersion: 1,
    game: "minecraft",
    name: "Create + Perf Sample Pack",
    versionId: "1.0.0",
    summary: "Sample pack demonstrating the mc-spine pre-flight checker",
    dependencies: {
      minecraft: "1.21.1",
      neoforge: "21.1.93",
    },
    files: [
      {
        path: "mods/create-1.21.1-6.0.6.jar",
        fileSize: 15234567,
        hashes: {
          sha1: "0123456789abcdef0123456789abcdef01234567",
          sha512: "abcdef0123456789",
        },
      },
      {
        path: "mods/sodium-fabric-0.6.13+mc1.21.1.jar",
        fileSize: 1987654,
      },
      {
        path: "mods/lithium-fabric-0.15.0+mc1.21.1.jar",
        fileSize: 876543,
      },
      {
        path: "mods/modernfix-neoforge-5.20.0+mc1.21.1.jar",
        fileSize: 1234567,
      },
      {
        path: "mods/ferritecore-7.0.2-neoforge.jar",
        fileSize: 234567,
      },
      {
        path: "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
        fileSize: 3456789,
      },
      {
        path: "mods/iris-1.8.0-beta.4+1.21.1.jar",
        fileSize: 2987654,
        env: {
          client: "required",
          server: "unsupported",
        },
      },
      {
        path: "mods/waystones-neoforge-1.21.1-21.1.10.jar",
        fileSize: 456789,
      },
      {
        path: "datapacks/create-compat-tweaks.zip",
        fileSize: 34567,
      },
      {
        path: "resourcepacks/faithful-32x.zip",
        fileSize: 8765432,
      },
    ],
  }, null, 2);

  // ---------------------------------------------------------------------
  // DOM wiring — only inside a real document.
  // ---------------------------------------------------------------------

  var api = {
    analyzeModpackJson: analyzeModpackJson,
    formatHumanSummary: formatHumanSummary,
    parseModrinthManifest: parseModrinthManifest,
    parseCurseforgeManifest: parseCurseforgeManifest,
    inferCandidateModId: inferCandidateModId,
    checkModMcVersion: checkModMcVersion,
    parseMcVersion: parseMcVersion,
    BUILTIN_SUPERSEDED_PERF_MODS: BUILTIN_SUPERSEDED_PERF_MODS,
    SAMPLE_MODRINTH_INDEX_JSON: SAMPLE_MODRINTH_INDEX_JSON,
    ModpackError: ModpackError,
    VersionRangeError: VersionRangeError,
  };

  root.McSpinePreflight = api;
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
  if (typeof document === "undefined") {
    return; // Node / worker import: analyzer only, no UI.
  }

  var CFG = siteConfig();

  function $(id) {
    return document.getElementById(id);
  }

  // -- SITE_CONFIG binding -------------------------------------------------

  function bindConfig() {
    // Text bindings: <span data-cfg="projectName">fallback</span>.
    Array.prototype.forEach.call(
      document.querySelectorAll("[data-cfg]"),
      function (el) {
        var key = el.getAttribute("data-cfg");
        var value = CFG[key];
        if (typeof value === "string") {
          el.textContent = value;
        }
      });

    // Link bindings: <a data-cfg-href="githubUrl" href="fallback">.
    Array.prototype.forEach.call(
      document.querySelectorAll("[data-cfg-href]"),
      function (el) {
        var key = el.getAttribute("data-cfg-href");
        var value = CFG[key];
        if (typeof value === "string" && value) {
          el.setAttribute("href", value);
        }
      });

    // CLI templates: <code data-cli="{{cli}}-init ...">fallback</code>.
    Array.prototype.forEach.call(
      document.querySelectorAll("[data-cli]"),
      function (el) {
        var tpl = el.getAttribute("data-cli");
        el.textContent = tpl.replace(/\{\{cli\}\}/g, CFG.cliPrefix);
      });

    // Hero target pills: first target = LTS, last = Latest.
    var pills = $("target-pills");
    if (pills && Array.isArray(CFG.mcTargets) && CFG.mcTargets.length) {
      pills.textContent = "";
      CFG.mcTargets.forEach(function (target, i) {
        var tag = i === 0 && CFG.mcTargets.length > 1
          ? "LTS"
          : i === CFG.mcTargets.length - 1 ? "Latest" : "";
        var pill = document.createElement("span");
        pill.className = "pill" + (i === CFG.mcTargets.length - 1 &&
          CFG.mcTargets.length > 1 ? " latest" : "");
        var dot = document.createElement("span");
        dot.className = "dot";
        pill.appendChild(dot);
        pill.appendChild(document.createTextNode(target + " "));
        if (tag) {
          var tagEl = document.createElement("span");
          tagEl.className = "pill-tag";
          tagEl.textContent = tag;
          pill.appendChild(tagEl);
        }
        pills.appendChild(pill);
      });
    }

    // Checker target select follows mcTargets.
    var select = $("mc-target-select");
    if (select && Array.isArray(CFG.mcTargets) && CFG.mcTargets.length) {
      select.textContent = "";
      CFG.mcTargets.forEach(function (target, i) {
        var opt = document.createElement("option");
        opt.value = target;
        opt.textContent = target;
        if (i === 0) {
          opt.selected = true;
        }
        select.appendChild(opt);
      });
    }

    // <title> and custom domain note.
    document.title = CFG.projectName + " — " + CFG.tagline;
    var deployNote = $("deploy-note");
    if (deployNote && CFG.customDomain) {
      deployNote.textContent = "";
      deployNote.appendChild(
        document.createTextNode("Serving at "));
      var code = document.createElement("code");
      code.textContent = "https://" + CFG.customDomain;
      deployNote.appendChild(code);
      deployNote.classList.add("visible");
    }
  }

  // -- Copy buttons ---------------------------------------------------------

  function copyText(text, btn) {
    function done() {
      btn.classList.add("copied");
      btn.textContent = "copied";
      setTimeout(function () {
        btn.classList.remove("copied");
        btn.textContent = "copy";
      }, 1600);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () {
        legacyCopy(text);
        done();
      });
    } else {
      legacyCopy(text);
      done();
    }
  }

  function legacyCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "absolute";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } catch (e) {
      // Clipboard unavailable (e.g. file:// with no permission): non-fatal.
    }
    document.body.removeChild(ta);
  }

  function bindCopyButtons() {
    document.addEventListener("click", function (ev) {
      var btn = ev.target;
      if (!btn.classList || !btn.classList.contains("copy-btn")) {
        return;
      }
      var code = btn.parentElement
        ? btn.parentElement.querySelector("code") : null;
      if (code) {
        copyText(code.textContent, btn);
      }
    });
  }

  // -- Pre-flight checker UI ------------------------------------------------

  var lastInput = null; // {text, fileName}

  function showError(message) {
    var box = $("checker-error");
    box.textContent = message;
    box.classList.add("visible");
    $("report").classList.remove("visible");
  }

  function clearError() {
    var box = $("checker-error");
    box.textContent = "";
    box.classList.remove("visible");
  }

  var READINESS_LABEL = {};
  READINESS_LABEL[READINESS_READY_NATIVE] = {
    icon: "✓",
    text: "ready — every file in this pack runs natively on " +
      CFG.projectName + ".",
  };
  READINESS_LABEL[READINESS_READY_WITH_AUTO_SKIPS] = {
    icon: "✓",
    text: "ready — redundant and client-only files are skipped " +
      "automatically; the rest runs natively.",
  };
  READINESS_LABEL[READINESS_NEEDS_JAR_INSPECTION] = {
    icon: "⚠",
    text: "almost ready — the Java mod(s) listed below still need a " +
      "compatibility check before launch.",
  };
  READINESS_LABEL[READINESS_VERSION_MISMATCH] = {
    icon: "✕",
    text: "not compatible — this pack was built for a different " +
      "Minecraft version.",
  };

  function addGroup(container, cls, title, names, extra) {
    if (!names.length) {
      return;
    }
    var group = document.createElement("div");
    group.className = "report-group " + cls;
    var h = document.createElement("h4");
    h.textContent = title + " ";
    var count = document.createElement("span");
    count.className = "count";
    count.textContent = "(" + names.length + ")";
    h.appendChild(count);
    group.appendChild(h);
    var ul = document.createElement("ul");
    names.forEach(function (name) {
      var li = document.createElement("li");
      li.textContent = name;
      ul.appendChild(li);
    });
    group.appendChild(ul);
    if (extra) {
      var p = document.createElement("p");
      p.className = "cleanroom";
      p.textContent = extra;
      group.appendChild(p);
    }
    container.appendChild(group);
  }

  function renderReport(report) {
    clearError();
    var reportEl = $("report");
    reportEl.classList.add("visible");

    var banner = $("readiness");
    var label = READINESS_LABEL[report.readiness];
    banner.className = "readiness " + report.readiness;
    $("readiness-icon").textContent = label.icon;
    $("readiness-text").textContent =
      "Pre-flight for \"" + report.pack_name + "\" " + report.pack_version +
      " (" + report.format + ", declared MC " + report.mc_version_declared +
      ", server " + report.target + "): " + label.text;

    var groups = $("report-groups");
    groups.textContent = "";
    addGroup(
      groups, "superseded", "Skipped perf mods — already built in",
      report.skipped_perf_mods,
      "Neutral id match only: " + CFG.projectName + " ships its own " +
        "clean-room Rust implementations of these optimizations. No " +
        "affiliation with the named mods is claimed, and no mod code or " +
        "assets are copied, ported, or bundled.");
    addGroup(
      groups, "client-only", "Client-side only — server skips",
      report.client_only_mods);
    addGroup(
      groups, "native", "Runs natively — no Java",
      report.native_mods);
    addGroup(
      groups, "inspect", "Needs compatibility inspection",
      report.needs_inspection_mods);

    var body = $("entries-body");
    body.textContent = "";
    report.entries.forEach(function (entry) {
      var tr = document.createElement("tr");
      var tdPath = document.createElement("td");
      tdPath.className = "path";
      tdPath.textContent = entry.path;
      var tdId = document.createElement("td");
      tdId.className = "mono";
      tdId.textContent = entry.candidate_mod_id || "—";
      var tdClass = document.createElement("td");
      var chip = document.createElement("span");
      chip.className = "class-chip " + entry["class"];
      chip.textContent = entry["class"];
      tdClass.appendChild(chip);
      var tdNote = document.createElement("td");
      tdNote.className = "note";
      tdNote.textContent = entry.note;
      tr.appendChild(tdPath);
      tr.appendChild(tdId);
      tr.appendChild(tdClass);
      tr.appendChild(tdNote);
      body.appendChild(tr);
    });

    $("report-json").textContent = JSON.stringify(report, null, 2);
    $("report-summary").textContent = formatHumanSummary(report);
  }

  function runAnalysis(text, fileName) {
    lastInput = { text: text, fileName: fileName };
    var select = $("mc-target-select");
    var target = select && select.value ? select.value : undefined;
    try {
      var report = analyzeModpackJson(text, {
        fileName: fileName,
        target: target,
      });
      renderReport(report);
    } catch (e) {
      if (e instanceof ModpackError) {
        showError(fileName + ": " + e.message);
      } else {
        showError(fileName + ": unexpected error (" + e.message + ")");
      }
    }
  }

  function bindChecker() {
    var dropzone = $("dropzone");
    var fileInput = $("file-input");
    var sampleBtn = $("sample-pack-btn");
    var select = $("mc-target-select");

    dropzone.addEventListener("click", function () {
      fileInput.click();
    });
    dropzone.addEventListener("keydown", function (ev) {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        fileInput.click();
      }
    });
    fileInput.addEventListener("change", function () {
      if (fileInput.files && fileInput.files.length) {
        readManifestFile(fileInput.files[0]);
      }
      fileInput.value = "";
    });

    ["dragenter", "dragover"].forEach(function (type) {
      dropzone.addEventListener(type, function (ev) {
        ev.preventDefault();
        dropzone.classList.add("dragover");
      });
    });
    ["dragleave", "drop"].forEach(function (type) {
      dropzone.addEventListener(type, function (ev) {
        ev.preventDefault();
        dropzone.classList.remove("dragover");
      });
    });
    dropzone.addEventListener("drop", function (ev) {
      var files = ev.dataTransfer && ev.dataTransfer.files;
      if (files && files.length) {
        readManifestFile(files[0]);
      }
    });

    sampleBtn.addEventListener("click", function () {
      runAnalysis(SAMPLE_MODRINTH_INDEX_JSON, "modrinth.index.json");
    });

    select.addEventListener("change", function () {
      if (lastInput) {
        runAnalysis(lastInput.text, lastInput.fileName);
      }
    });
  }

  function readManifestFile(file) {
    var reader = new FileReader();
    reader.onload = function () {
      runAnalysis(String(reader.result), file.name);
    };
    reader.onerror = function () {
      showError(file.name + ": could not read the file");
    };
    reader.readAsText(file);
  }

  // -- Boot ------------------------------------------------------------------

  bindConfig();
  bindCopyButtons();
  bindChecker();
})(typeof window !== "undefined" ? window : globalThis);
