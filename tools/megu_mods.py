#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""Safe Modrinth v2 mod search, one-command add, and missing-dependency installer.

  tools/megu_mods.py search <query> [--mc-version 1.21.1|26.3] [--loader neoforge|fabric] [--limit 10] [--json]
  tools/megu_mods.py add <slug> --mods-dir <dir> [--mc-version 1.21.1|26.3] [--loader neoforge|fabric] [--yes] [--dry-run] [--json] [--force]
  tools/megu_mods.py resolve-deps --mods-dir <dir> [--mc-version 1.21.1|26.3] [--loader neoforge|fabric] [--yes] [--dry-run] [--json]

Safety contract — a pure-stdlib mirror of the invariants enforced by
`mod-adapter-core/src/mod_sync.rs` (`validate_download_url`,
`validate_jar_file_name`, `MAX_*` bounds, `ConsentToken`) and
`mod-adapter-core/src/dep_resolver.rs` (`parse_modrinth_versions_json`,
`install_resolved_artifact`, `RUNTIME_PROVIDED_MOD_IDS`):

1. Strict origin allowlist — downloads are only fetched from
   `cdn.modrinth.com` over HTTPS (exact host match, never a suffix match;
   no credentials-in-URL, no IP literals, no non-443 port, no
   local/internal hosts). `--api-base` may point the *API* at a loopback
   test server (`127.0.0.1`/`localhost`/`::1` only); only then are plain
   `http://` download URLs on loopback hosts accepted. Arbitrary remote
   hosts and non-loopback `http://` are always refused.
2. Strict `.jar` filename jail — ASCII basename, no path separators, no
   `..`, no leading `.`, no control/shell-metacharacter bytes.
3. Verified bytes before materialization — the server-declared `size`,
   `hashes.sha512`, and `hashes.sha256` (when present) are all verified in
   constant time (`hmac.compare_digest`) before anything is written; the
   JAR is capped at 64 MiB and installed atomically (`*.tmp` then
   `os.replace`). Nothing partial is left behind on failure.
4. Explicit consent — downloads only happen after an affirmative answer:
   `--yes` for non-interactive use, or an interactive `[y/N]` prompt. On
   a non-TTY stdin without `--yes` the command fails closed (default NO).
   `--dry-run` prints the install plan and downloads nothing.
5. Superseded performance mods — ids matching
   `BUILTIN_SUPERSEDED_PERF_MODS` (exact mirror of
   `mod-adapter-core/src/fallback.rs`) are skipped up front with a
   friendly explanation: mc-spine already builds that optimization in
   natively, so no download happens unless `--force` is passed.

Clean-room note: matching a mod id against BUILTIN_SUPERSEDED_PERF_MODS
only *skips downloading* a mod — it copies, links, bundles, or derives
nothing from that mod. Every built-in optimization is a 100% clean-room
original Rust implementation inside mc-spine.

After a successful install, if a compiled `mc-spine-mod-check` binary is
on PATH or under `target/`, it is run as
`mc-spine-mod-check --adapt-mods-dir <dir> --json` and its verdict is
summarized for the operator along with the reload/restart hint.
"""

import argparse
import hashlib
import hmac
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile


ROOT = Path(__file__).resolve().parents[1]
PROG = "megu_mods.py"

# Mirrors MODRINTH_API_BASE in mod-adapter-core/src/dep_resolver.rs.
MODRINTH_API_BASE = "https://api.modrinth.com/v2"
# Mirrors TRUSTED_MOD_CDN_HOSTS restricted to the Modrinth CDN — this tool
# only talks to Modrinth, so only its CDN is a legal download origin.
TRUSTED_DOWNLOAD_HOSTS = frozenset({"cdn.modrinth.com"})
# Loopback hosts that may serve a test --api-base / download URL.
LOOPBACK_HOSTS = frozenset({"localhost", "127.0.0.1", "::1"})
# Bounds mirrored from mod-adapter-core/src/mod_sync.rs; the JAR cap is the
# lane-mandated 64 MiB (stricter than mod_sync's 256 MiB ceiling).
MAX_JAR_BYTES = 64 * 1024 * 1024
MAX_URL_LEN = 2048
MAX_FILE_NAME_LEN = 255
MAX_HTTP_BODY = 16 * 1024 * 1024
MAX_MANIFEST_BYTES = 8 * 1024 * 1024
HTTP_TIMEOUT = 30
# Recursion cap for transitive `dependency_type == "required"` resolution.
MAX_DEP_DEPTH = 4
MAX_SEARCH_LIMIT = 100
DEFAULT_MC_VERSION = "1.21.1"
USER_AGENT = f"mc-spine/{PROG} (Modrinth v2 dependency installer)"

MOD_ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
SLUG_RE = re.compile(r"^[a-z0-9_-]{1,64}$")

# Exact mirror of BUILTIN_SUPERSEDED_PERF_MODS in
# mod-adapter-core/src/fallback.rs. Compared case-insensitively after
# stripping `-`/`_` so Modrinth spellings like `distant-horizons` still
# match the canonical `distanthorizons` id.
BUILTIN_SUPERSEDED_PERF_MODS = (
    # Rendering / mesh / LOD.
    "sodium", "embeddium", "rubidium", "xenon", "magnesium",
    "vulkanmod", "nvidium",
    "immediatelyfast", "entityculling", "moreculling", "distanthorizons",
    "voxy", "bobby", "farsight", "indium", "reeses-sodium-options",
    "sodium-extra", "embeddium_extra",
    # Server tick / lighting / memory / startup.
    "lithium", "radium", "canary", "starlight", "scalablelux", "phosphor", "scalpel",
    "ferritecore", "modernfix", "noisium", "c2me", "krypton", "lazydfu",
    "memoryleakfix", "alternate-current", "clumps", "saturn",
    "smoothboot", "fastload", "dashloader", "servercore",
)
_SUPERSEDED_NORMALIZED = frozenset(
    re.sub(r"[^a-z0-9]", "", mod_id) for mod_id in BUILTIN_SUPERSEDED_PERF_MODS
)

# Mirror of REASON_SUPERSEDED in mod-adapter-core/src/fallback.rs.
REASON_SUPERSEDED = "already covered by mc-spine built-in optimizations"

# Exact mirror of RUNTIME_PROVIDED_MOD_IDS in
# mod-adapter-core/src/ecosystem_compat.rs — never reported as missing.
RUNTIME_PROVIDED_MOD_IDS = frozenset({
    "minecraft", "java", "fabricloader", "fabric-loader", "fabric",
    "fabric-api", "fabric_api", "neoforge", "forge", "javafml",
})

# Minimal TOML line-scan patterns for the no-tomllib fallback, mirroring
# tools/mod_launcher.py.
TABLE_HEADER = re.compile(r"^\s*\[\[?\s*([^\]]+)\s*\]?\]")
MODID_LINE = re.compile(r"""^\s*modId\s*=\s*['"]([^'"]+)['"]""")
DEP_TYPE_LINE = re.compile(r"""^\s*type\s*=\s*['"]([^'"]+)['"]""")
MANDATORY_LINE = re.compile(r"""^\s*mandatory\s*=\s*(true|false)""")
VERSION_RANGE_LINE = re.compile(r"""^\s*versionRange\s*=\s*['"]([^'"]*)['"]""")
BARE_MODID_LINE = re.compile(r"""^\s*modId\s*=\s*['"]([^'"]+)['"]""")

KNOWN_LOADERS = ("neoforge", "fabric", "forge", "quilt")


class MeguModsError(Exception):
    """Fail-closed operator-facing error; message is safe to print."""


def is_builtin_superseded(name):
    """Whether `name` names a built-in-superseded optimization mod id."""
    normalized = re.sub(r"[^a-z0-9]", "", (name or "").lower())
    return bool(normalized) and normalized in _SUPERSEDED_NORMALIZED


def is_runtime_provided(mod_id):
    return mod_id.lower() in RUNTIME_PROVIDED_MOD_IDS


# ---------------------------------------------------------------------------
# URL allowlist & filename jail (mirror of mod_sync.rs)
# ---------------------------------------------------------------------------

def normalize_host(host):
    """Lowercase, strip one trailing root dot, require a strict DNS name."""
    if not host or len(host) > 253:
        raise MeguModsError("URL host length out of bounds")
    h = host.lower()
    if h.endswith("."):
        h = h[:-1]
    if not h or not all(c in "abcdefghijklmnopqrstuvwxyz0123456789.-" for c in h):
        raise MeguModsError(f"URL host '{host}' is not a strict DNS name")
    for label in h.split("."):
        if not label or label.startswith("-") or label.endswith("-"):
            raise MeguModsError(f"URL host '{host}' has a malformed DNS label")
    return h


def is_ip_literal_host(host):
    """Raw IP literal in any textual form (dotted, truncated, hex, dword)."""
    try:
        ipaddress.ip_address(host)
        return True
    except ValueError:
        pass
    return all(c.isdigit() or c == "." for c in host) or host.startswith("0x")


def is_local_or_internal_host(host):
    return (
        host == "localhost"
        or host.endswith(".localhost")
        or host == "local"
        or host.endswith(".local")
        or host.endswith(".internal")
        or host.endswith(".lan")
        or host.endswith(".home")
        or host.endswith(".corp")
        or host.endswith(".intranet")
    )


def is_loopback_host(host):
    """Whether `host` names a loopback interface (127/8, ::1, localhost)."""
    h = host.lower()
    if h == "localhost" or h.endswith(".localhost"):
        return True
    try:
        return ipaddress.ip_address(h).is_loopback
    except ValueError:
        return False


def _split_authority(authority):
    """`authority` -> (host, port); userinfo/@ and IPv6 literals rejected."""
    if not authority:
        raise MeguModsError("URL has an empty authority")
    if "@" in authority:
        raise MeguModsError("URL embeds credentials or userinfo")
    if authority.startswith("["):
        raise MeguModsError("URL uses an IPv6 literal")
    host = authority
    port = None
    if ":" in authority:
        host, _, raw_port = authority.rpartition(":")
        if not raw_port.isdigit():
            raise MeguModsError("URL has a malformed port")
        port = int(raw_port)
    return host, port


def validate_url_syntax(url):
    """Scheme/authority/SSRF checks independent of the allowlist.

    Returns the normalized hostname on success — mirrors
    `validate_url_syntax` in mod_sync.rs: ASCII only, no control/space
    bytes, https:// only, no userinfo, no IPv6 literal, port must be 443
    when present, no raw-IP or local/internal hosts.
    """
    if not url or len(url) > MAX_URL_LEN:
        raise MeguModsError(f"URL length out of bounds (1..={MAX_URL_LEN})")
    if not url.isascii():
        raise MeguModsError("URL must be pure ASCII")
    if any(ord(c) <= 0x20 or ord(c) == 0x7F for c in url):
        raise MeguModsError("URL contains a control/space byte")
    if not url[:8].lower() == "https://":
        raise MeguModsError(f"URL '{url}' is not an https:// URL")
    rest = url[8:]
    end = len(rest)
    for c in "/?#":
        i = rest.find(c)
        if i != -1:
            end = min(end, i)
    host, port = _split_authority(rest[:end])
    if port is not None and port != 443:
        raise MeguModsError(f"URL '{url}' uses a non-443 port")
    host = normalize_host(host)
    if is_ip_literal_host(host):
        raise MeguModsError(f"URL '{url}' uses a raw IP literal host")
    if is_local_or_internal_host(host):
        raise MeguModsError(f"URL '{url}' targets a local/internal host")
    return host


def validate_download_url(url, loopback_ok=False):
    """Full download-URL policy check — fail closed on every deviation.

    Canonical policy: https:// + exact TRUSTED_DOWNLOAD_HOSTS match. With a
    loopback `--api-base` override (`loopback_ok`), http(s) URLs whose host
    is loopback are additionally accepted for tests — never remote hosts.
    """
    if not loopback_ok:
        host = validate_url_syntax(url)
        if host not in TRUSTED_DOWNLOAD_HOSTS:
            raise MeguModsError(f"URL host '{host}' is not a trusted mod CDN")
        return
    if not url or len(url) > MAX_URL_LEN or not url.isascii():
        raise MeguModsError("download URL failed bounds/ASCII check")
    if any(ord(c) <= 0x20 or ord(c) == 0x7F for c in url):
        raise MeguModsError("download URL contains a control/space byte")
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https"):
        raise MeguModsError(f"download URL '{url}' is not http(s)")
    if "@" in parts.netloc:
        raise MeguModsError("download URL embeds credentials or userinfo")
    host = parts.hostname or ""
    if host.lower() in TRUSTED_DOWNLOAD_HOSTS and parts.scheme == "https":
        return
    if is_loopback_host(host):
        return
    raise MeguModsError(
        f"URL host '{host}' is not a trusted mod CDN or test loopback")


def validate_jar_file_name(name):
    """The `.jar` basename jail, mirrored from mod_sync.rs exactly."""
    if not name or len(name) > MAX_FILE_NAME_LEN:
        raise MeguModsError(
            f"file name length out of bounds (1..={MAX_FILE_NAME_LEN})")
    if not name.isascii():
        raise MeguModsError(f"file name '{name}' is not ASCII")
    for c in name:
        o = ord(c)
        if o <= 0x1F or o == 0x7F:
            raise MeguModsError(f"file name '{name}' has a control byte")
        if c in "/\\":
            raise MeguModsError(
                f"file name '{name}' contains a path separator")
        if c == ":":
            raise MeguModsError(f"file name '{name}' contains ':'")
        if c in "`$(){}[]&;|<>*?!'\"~#":
            raise MeguModsError(
                f"file name '{name}' contains a shell metacharacter")
    if name.startswith("."):
        raise MeguModsError(f"file name '{name}' is a hidden dotfile")
    if ".." in name:
        raise MeguModsError(f"file name '{name}' contains '..'")
    if len(name) <= 4 or name[-4:].lower() != ".jar":
        raise MeguModsError(f"file name '{name}' is not a .jar basename")
    return name


# ---------------------------------------------------------------------------
# Modrinth API client
# ---------------------------------------------------------------------------

class _SafeRedirect(urllib.request.HTTPRedirectHandler):
    """Re-validate every redirect target before urllib follows it."""

    def __init__(self, loopback_ok):
        super().__init__()
        self._loopback_ok = loopback_ok

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        validate_download_url(newurl, self._loopback_ok)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


class ModrinthClient:
    """Tiny urllib client bound to one validated API base."""

    def __init__(self, api_base=None):
        raw = (api_base or MODRINTH_API_BASE).rstrip("/")
        if api_base is None:
            self.loopback = False
        else:
            parts = urllib.parse.urlsplit(raw)
            host = parts.hostname or ""
            if (parts.scheme not in ("http", "https")
                    or not is_loopback_host(host)
                    or "@" in parts.netloc):
                raise MeguModsError(
                    "--api-base may only point at a loopback test server "
                    f"(127.0.0.1/localhost/::1), got '{api_base}'")
            self.loopback = True
        self.api_base = raw
        self._opener = urllib.request.build_opener(
            _SafeRedirect(self.loopback))

    def _fetch(self, url, max_bytes):
        request = urllib.request.Request(
            url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
        try:
            with self._opener.open(request, timeout=HTTP_TIMEOUT) as resp:
                chunks = []
                total = 0
                while True:
                    chunk = resp.read(1 << 16)
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > max_bytes:
                        raise MeguModsError(
                            f"response from {url} exceeds {max_bytes} bytes")
                    chunks.append(chunk)
                return b"".join(chunks)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                raise MeguModsError(f"not found on Modrinth: {url}")
            raise MeguModsError(
                f"Modrinth request failed: HTTP {error.code} for {url}")
        except urllib.error.URLError as error:
            raise MeguModsError(
                f"Modrinth request failed: {error.reason} ({url})")

    def get_json(self, path):
        body = self._fetch(f"{self.api_base}{path}", MAX_HTTP_BODY)
        try:
            return json.loads(body)
        except json.JSONDecodeError as error:
            raise MeguModsError(f"invalid Modrinth response: {error}")

    def download(self, url):
        """Fetch verified-artifact bytes with the JAR cap enforced inline."""
        validate_download_url(url, self.loopback)
        return self._fetch(url, MAX_JAR_BYTES)


def encode_query(params):
    return urllib.parse.urlencode(params)


def search_projects(client, query, mc_version=None, loader=None, limit=10):
    facets = [["project_type:mod"]]
    if mc_version:
        facets.append([f"versions:{mc_version}"])
    if loader:
        facets.append([f"categories:{loader}"])
    params = {
        "query": query,
        "limit": str(limit),
        "index": "relevance",
        "facets": json.dumps(facets, separators=(",", ":")),
    }
    doc = client.get_json(f"/search?{encode_query(params)}")
    hits = doc.get("hits") if isinstance(doc, dict) else None
    if not isinstance(hits, list):
        raise MeguModsError("invalid Modrinth response: search hits missing")
    return hits


def get_versions(client, slug_or_id):
    if not MOD_ID_RE.match(slug_or_id):
        raise MeguModsError(
            f"mod id '{slug_or_id}' is not a valid [A-Za-z0-9_-]{{1,64}} id")
    doc = client.get_json(f"/project/{slug_or_id}/version")
    if not isinstance(doc, list):
        raise MeguModsError(
            "invalid Modrinth response: version list is not an array")
    return doc


# ---------------------------------------------------------------------------
# Version / file selection (mirror of parse_modrinth_versions_json)
# ---------------------------------------------------------------------------

def _is_hex(text, length):
    return (isinstance(text, str) and len(text) == length
            and all(c in "0123456789abcdefABCDEF" for c in text))


def _extract_candidate_file(obj, loopback_ok):
    """Validate one `files[]` entry; ineligible files return None."""
    if not isinstance(obj, dict):
        return None
    file_name = obj.get("filename")
    download_url = obj.get("url")
    size = obj.get("size")
    hashes = obj.get("hashes")
    if not isinstance(file_name, str) or not isinstance(download_url, str):
        return None
    try:
        validate_jar_file_name(file_name)
        validate_download_url(download_url, loopback_ok)
    except MeguModsError:
        return None
    if not isinstance(size, int) or isinstance(size, bool):
        return None
    if size <= 0 or size > MAX_JAR_BYTES:
        return None
    if not isinstance(hashes, dict):
        return None
    sha256 = hashes.get("sha256")
    sha512 = hashes.get("sha512")
    sha256 = sha256.lower() if _is_hex(sha256, 64) else None
    sha512 = sha512.lower() if _is_hex(sha512, 128) else None
    if not sha256 and not sha512:
        return None
    file_type = obj.get("file_type")
    file_type_rank = {
        "required_mod": 2, "optional_mod": 2,
        "required_resource": 1, "optional_resource": 1,
    }.get(file_type, 0)
    return {
        "file_name": file_name,
        "download_url": download_url,
        "size_bytes": size,
        "sha256": sha256,
        "sha512": sha512,
        "primary": bool(obj.get("primary")),
        "file_type_rank": file_type_rank,
    }


_VERSION_TYPE_RANK = {"release": 0, "beta": 1, "alpha": 2}


def pick_artifact(versions, mc_version, loaders, loopback_ok):
    """Select the best eligible listed release, or None.

    Eligibility mirrors parse_modrinth_versions_json: status "listed",
    `game_versions` contains the target tag (when one is set), `loaders`
    intersects the preferred set, and at least one file passes the URL
    allowlist, `.jar` jail, size bound, and digest check. Ranking:
    release < beta < alpha < other, then latest `date_published`, then
    highest `version_number`, then lowest `id`.
    """
    scored = []
    for version in versions:
        if not isinstance(version, dict):
            continue
        if version.get("status") != "listed":
            continue
        game_versions = version.get("game_versions")
        if mc_version is not None:
            if not isinstance(game_versions, list) \
                    or mc_version not in game_versions:
                continue
        version_loaders = version.get("loaders")
        if loaders:
            if not isinstance(version_loaders, list) or not (
                    {str(l).lower() for l in version_loaders}
                    & set(loaders)):
                continue
        version_number = version.get("version_number")
        version_id = version.get("id")
        if not isinstance(version_number, str) or not isinstance(version_id, str):
            continue
        files = version.get("files")
        if not isinstance(files, list):
            continue
        best = None
        for index, file_obj in enumerate(files):
            candidate = _extract_candidate_file(file_obj, loopback_ok)
            if candidate is None:
                continue
            key = (candidate["primary"], candidate["file_type_rank"], -index)
            if best is None or key > best[0]:
                best = (key, candidate)
        if best is None:
            continue
        candidate = best[1]
        deps = version.get("dependencies")
        required_ids = []
        if isinstance(deps, list):
            for dep in deps:
                if not isinstance(dep, dict):
                    continue
                if dep.get("dependency_type") != "required":
                    continue
                project_id = dep.get("project_id")
                if isinstance(project_id, str) and MOD_ID_RE.match(project_id):
                    required_ids.append(project_id)
        scored.append({
            "project_id": version.get("project_id") or "",
            "version_id": version_id,
            "version_number": version_number,
            "version_type": version.get("version_type") or "",
            "date_published": version.get("date_published") or "",
            "required_project_ids": required_ids,
            **candidate,
        })
    if not scored:
        return None
    scored.sort(key=lambda a: _VERSION_TYPE_RANK.get(a["version_type"], 3))
    best_rank = _VERSION_TYPE_RANK.get(scored[0]["version_type"], 3)
    top = [a for a in scored
           if _VERSION_TYPE_RANK.get(a["version_type"], 3) == best_rank]
    # Latest published wins within the best release tier.
    top.sort(key=lambda a: (a["date_published"], a["version_number"],
                            a["version_id"]), reverse=True)
    return top[0]


# ---------------------------------------------------------------------------
# Verified download + atomic install
# ---------------------------------------------------------------------------

def verify_artifact_bytes(artifact, data):
    """Exact size + sha512/sha256 constant-time; fail closed on any miss."""
    if len(data) != artifact["size_bytes"]:
        raise MeguModsError(
            f"downloaded {len(data)} bytes, artifact declares "
            f"{artifact['size_bytes']}")
    verified = False
    if artifact.get("sha512"):
        digest = hashlib.sha512(data).digest()
        if not hmac.compare_digest(
                digest, bytes.fromhex(artifact["sha512"])):
            raise MeguModsError("downloaded bytes failed sha512 verification")
        verified = True
    if artifact.get("sha256"):
        digest = hashlib.sha256(data).digest()
        if not hmac.compare_digest(
                digest, bytes.fromhex(artifact["sha256"])):
            raise MeguModsError("downloaded bytes failed sha256 verification")
        verified = True
    if not verified:
        raise MeguModsError(
            "artifact has no usable sha256/sha512 digest to verify against")


def install_verified_jar(mods_dir, file_name, data):
    """Atomically install `data` as `mods_dir/file_name` (.tmp -> replace)."""
    validate_jar_file_name(file_name)
    target = mods_dir / file_name
    fd, tmp = tempfile.mkstemp(
        dir=str(mods_dir), prefix=".megu-mods-", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, target)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return target


def existing_matches(mods_dir, artifact):
    """Whether an identical verified file is already installed."""
    target = mods_dir / artifact["file_name"]
    if not target.is_file():
        return False
    try:
        data = target.read_bytes()
    except OSError:
        return False
    try:
        verify_artifact_bytes(artifact, data)
    except MeguModsError:
        return False
    return True


# ---------------------------------------------------------------------------
# Recursive required-dependency resolution
# ---------------------------------------------------------------------------

def resolve_with_deps(client, slug_or_id, mc_version, loaders, notes):
    """Resolve a project and its transitive required deps, depth-capped and
    cycle-safe. Returns artifacts in install order (deps before users)."""
    plan = []
    visited = set()
    planned_files = set()

    def resolve(key, depth):
        if key in visited:
            return
        visited.add(key)
        if depth > MAX_DEP_DEPTH:
            notes.append(
                f"dependency depth cap ({MAX_DEP_DEPTH}) reached — "
                f"'{key}' and its deeper dependencies were not resolved; "
                "install them manually")
            return
        versions = get_versions(client, key)
        artifact = pick_artifact(versions, mc_version, loaders, client.loopback)
        if artifact is None:
            raise MeguModsError(
                f"no verified compatible release found for '{key}' "
                f"(mc {mc_version or 'any'}, loader "
                f"{'/'.join(loaders)})")
        artifact = dict(artifact)
        artifact["slug"] = key
        for dep_id in artifact["required_project_ids"]:
            resolve(dep_id, depth + 1)
        if artifact["file_name"] in planned_files:
            notes.append(
                f"'{key}' resolves to already-planned file "
                f"'{artifact['file_name']}' — skipped duplicate")
            return
        planned_files.add(artifact["file_name"])
        plan.append(artifact)

    resolve(slug_or_id, 0)
    return plan


# ---------------------------------------------------------------------------
# mods/ JAR manifest scan (fabric.mod.json + *.mods.toml) for resolve-deps
# ---------------------------------------------------------------------------

def _parse_fabric_manifest(raw):
    """(mod_ids, required_deps, error) from fabric.mod.json bytes."""
    try:
        doc = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        return [], [], f"fabric.mod.json: {error}"
    if not isinstance(doc, dict) or doc.get("schemaVersion") != 1 \
            or not isinstance(doc.get("id"), str):
        return [], [], "fabric.mod.json: missing schemaVersion 1 + id"
    ids = [doc["id"]]
    provides = doc.get("provides")
    if isinstance(provides, list):
        ids.extend(p for p in provides if isinstance(p, str))
    deps = []
    depends = doc.get("depends")
    if isinstance(depends, dict):
        for dep_id, declared in depends.items():
            if isinstance(dep_id, str):
                deps.append((dep_id, True,
                             declared if isinstance(declared, str) else None))
    recommends = doc.get("recommends")
    if isinstance(recommends, dict):
        for dep_id in recommends:
            if isinstance(dep_id, str):
                deps.append((dep_id, False, None))
    return ids, deps, None


def _toml_dep_entry(entry, ctx, deps):
    """Validate one `[[dependencies.*]]` table (tomllib or line-scan dict)."""
    dep_id = entry.get("modId")
    if not isinstance(dep_id, str):
        return f"{ctx}: missing string 'modId'"
    dep_type = entry.get("type", "required")
    if dep_type in ("incompatible", "disc"):
        return None
    if dep_type not in ("required", "optional"):
        return f"{ctx}: unknown type '{dep_type}'"
    required = dep_type == "required" and entry.get("mandatory") is not False
    deps.append((dep_id, required, entry.get("versionRange")
                 if isinstance(entry.get("versionRange"), str) else None))
    return None


def _parse_forge_manifest(raw, ctx):
    """(mod_ids, deps, error) from neoforge.mods.toml / mods.toml bytes."""
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return [], [], f"{ctx}: manifest is not UTF-8"
    document = None
    try:
        import tomllib
        document = tomllib.loads(text)
    except Exception:
        document = None
    if isinstance(document, dict):
        mod_ids = []
        mods = document.get("mods")
        if isinstance(mods, list):
            mod_ids = [m["modId"] for m in mods
                       if isinstance(m, dict) and isinstance(m.get("modId"), str)]
        deps = []
        dependencies = document.get("dependencies")
        if isinstance(dependencies, dict):
            for owner, entries in dependencies.items():
                if not isinstance(entries, list):
                    continue
                for i, entry in enumerate(entries):
                    if not isinstance(entry, dict):
                        continue
                    error = _toml_dep_entry(
                        entry, f"{ctx}: dependencies.{owner}[{i}]", deps)
                    if error:
                        return mod_ids, deps, error
        return mod_ids, deps, None
    # Line-scan fallback when tomllib is unavailable or the parse failed.
    mod_ids = []
    deps = []
    in_mods = False
    in_dep = False
    entry = None
    for line in text.splitlines():
        header = TABLE_HEADER.match(line)
        if header:
            name = header.group(1).strip()
            array = "[[" in line
            if in_dep and entry is not None:
                error = _toml_dep_entry(entry, ctx, deps)
                if error:
                    return mod_ids, deps, error
                entry = None
            in_mods = array and name == "mods"
            in_dep = array and name.startswith("dependencies.")
            continue
        if in_mods:
            match = BARE_MODID_LINE.match(line)
            if match:
                mod_ids.append(match.group(1))
        elif in_dep:
            if entry is None:
                entry = {}
            for pattern, key, conv in (
                    (MODID_LINE, "modId", str),
                    (DEP_TYPE_LINE, "type", str),
                    (VERSION_RANGE_LINE, "versionRange", str),
                    (MANDATORY_LINE, "mandatory", lambda s: s == "true")):
                match = pattern.match(line)
                if match:
                    entry[key] = conv(match.group(1))
    if in_dep and entry is not None:
        error = _toml_dep_entry(entry, ctx, deps)
        if error:
            return mod_ids, deps, error
    return mod_ids, deps, None


def scan_mods_dir(mods_dir):
    """Scan installed JARs for provided ids and required dependencies.

    Returns (provided_ids, missing, rejected, skipped_superseded) where
    `missing` is [(requiring_mod_id, required_mod_id, version_range)] —
    mirroring plan_dual_loader_mods_dir's missing_dependencies rule.
    """
    provided = set()
    required_deps = []
    rejected = []
    skipped = []
    for jar in sorted(mods_dir.iterdir()):
        if not jar.is_file() or jar.suffix.lower() != ".jar":
            continue
        ids = []
        deps = []
        try:
            with zipfile.ZipFile(jar) as archive:
                names = set(archive.namelist())
                lower = {n.lower(): n for n in names}
                errors = []
                if "fabric.mod.json" in names:
                    info = archive.getinfo("fabric.mod.json")
                    if info.file_size > MAX_MANIFEST_BYTES:
                        errors.append("fabric.mod.json: manifest too large")
                    else:
                        f_ids, f_deps, err = _parse_fabric_manifest(
                            archive.read("fabric.mod.json"))
                        ids.extend(f_ids)
                        deps.extend(f_deps)
                        if err:
                            errors.append(err)
                toml_name = lower.get("meta-inf/neoforge.mods.toml") \
                    or lower.get("meta-inf/mods.toml")
                if toml_name:
                    info = archive.getinfo(toml_name)
                    if info.file_size > MAX_MANIFEST_BYTES:
                        errors.append(f"{toml_name}: manifest too large")
                    else:
                        t_ids, t_deps, err = _parse_forge_manifest(
                            archive.read(toml_name), toml_name)
                        ids.extend(t_ids)
                        deps.extend(t_deps)
                        if err:
                            errors.append(err)
                if not ids and not deps:
                    rejected.append(
                        (jar.name, "no loader manifest "
                         "(fabric.mod.json / META-INF/*.mods.toml)"))
                    continue
                if errors and not ids:
                    rejected.append((jar.name, "; ".join(errors)))
                    continue
        except (zipfile.BadZipFile, OSError) as error:
            rejected.append((jar.name, f"unreadable JAR: {error}"))
            continue
        ids = [i for i in dict.fromkeys(ids)]
        if any(is_builtin_superseded(i) for i in ids):
            skipped.append((jar.name, ids[0] if ids else jar.name))
            continue
        provided.update(ids)
        # Filename-derived fallback id mirrors infer_candidate_mod_id use.
        if not ids:
            provided.add(jar.name[:-4])
        required_deps.append((ids[0] if ids else jar.name[:-4], deps))
    missing = []
    provided_lower = {p.lower() for p in provided}
    for mod_id, deps in required_deps:
        merged = {}
        for dep_id, required, rng in deps:
            key = dep_id.lower()
            if key in merged:
                merged[key] = (dep_id, merged[key][1] or required,
                               merged[key][2] or rng)
            else:
                merged[key] = (dep_id, required, rng)
        for dep_id, required, rng in merged.values():
            if not required or is_runtime_provided(dep_id):
                continue
            if dep_id.lower() not in provided_lower:
                missing.append((mod_id, dep_id, rng))
    missing.sort()
    return provided, missing, rejected, skipped


# ---------------------------------------------------------------------------
# mc-spine-mod-check integration
# ---------------------------------------------------------------------------

def find_mod_check_binary():
    for rel in ("target/debug/mc-spine-mod-check",
                "target/release/mc-spine-mod-check"):
        candidate = ROOT / rel
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return shutil.which("mc-spine-mod-check")


def run_mod_check(mods_dir):
    """Run `mc-spine-mod-check --adapt-mods-dir <dir> --json` when available.
    Returns (binary_path_or_None, report_dict_or_None, warning_or_None)."""
    binary = find_mod_check_binary()
    if binary is None:
        return None, None, None
    try:
        proc = subprocess.run(
            [binary, "--adapt-mods-dir", str(mods_dir), "--json"],
            capture_output=True, text=True, timeout=180, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        return binary, None, f"adapt check could not run: {error}"
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return binary, None, (
            f"adapt check exited {proc.returncode} with no JSON report")
    if report.get("schema") != "mc-spine-mods-dir-adaptation/1":
        return binary, None, "adapt check returned an unexpected schema"
    return binary, report, None


def format_adapt_summary(report):
    """Operator-friendly compatibility + reload/restart summary."""
    totals = report.get("totals") or {}
    lines = ["compatibility check (mc-spine-mod-check):"]
    lines.append(
        "  verdict: {verdict} — {native} native bundle(s), "
        "{shim} tier-2 shimmed, {skipped} auto-skipped, "
        "{fallback} tier-3 fallback".format(
            verdict=report.get("verdict", "?"),
            native=totals.get("native_bundle", 0),
            shim=totals.get("tier2_lifted_shim", 0),
            skipped=totals.get("auto_skipped", 0),
            fallback=totals.get("tier3_fallback_required", 0)))
    blockers = []
    for jar in report.get("jars") or []:
        if not isinstance(jar, dict):
            continue
        if jar.get("tier") == "tier3_fallback_required":
            reasons = jar.get("blockers") or jar.get("rejection_reasons") or []
            blockers.append((jar.get("jar", "?"), reasons))
    for name, reasons in blockers[:10]:
        shown = reasons[:2] if isinstance(reasons, list) else [reasons]
        for reason in shown:
            lines.append(f"  fallback blocker {name}: {reason}")
    if report.get("verdict") == "requires_fallback":
        lines.append("  next: fix/replace the tier-3 JAR(s), then restart "
                     "mc-spine to load the rest.")
    else:
        lines.append("  next: restart mc-spine (or run '/mods reload' for "
                     "hot-reloadable data mods) to load the new JARs.")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------

def human_size(n):
    if n >= 1 << 20:
        return f"{n / (1 << 20):.1f} MiB"
    if n >= 1 << 10:
        return f"{n / (1 << 10):.1f} KiB"
    return f"{n} B"


def print_json(doc):
    print(json.dumps(doc, indent=2, sort_keys=True))


def emit_plan(plan, mods_dir, lang="en"):
    """Print the install plan; returns total bytes."""
    total = sum(a["size_bytes"] for a in plan)
    if lang == "ja":
        print(f"インストール計画（mods ディレクトリ: {mods_dir}）:")
    else:
        print(f"install plan (mods dir: {mods_dir}):")
    for artifact in plan:
        print("  {slug} {ver} ({kind}) -> {file} ({size})".format(
            slug=artifact["slug"],
            ver=artifact["version_number"],
            kind=artifact["version_type"] or "release",
            file=artifact["file_name"],
            size=human_size(artifact["size_bytes"])))
    if lang == "ja":
        print(f"合計: {human_size(total)} / {len(plan)} ファイル")
    else:
        print(f"total: {human_size(total)} in {len(plan)} file(s)")
    return total


def request_consent(args):
    """Explicit-consent gate — fail closed (default NO) without --yes."""
    if getattr(args, "yes", False):
        return True
    if not sys.stdin.isatty():
        raise MeguModsError(
            "explicit consent is required before downloading; "
            "re-run with --yes for non-interactive use (default is NO)")
    try:
        answer = input("Proceed with download and install? [y/N] ")
    except (EOFError, KeyboardInterrupt):
        return False
    return answer.strip().lower() in ("y", "yes")


def install_plan(client, plan, mods_dir):
    """Download, verify, and atomically install every planned artifact."""
    mods_dir.mkdir(parents=True, exist_ok=True)
    installed = []
    for artifact in plan:
        if existing_matches(mods_dir, artifact):
            installed.append({**artifact, "status": "already_installed"})
            continue
        data = client.download(artifact["download_url"])
        verify_artifact_bytes(artifact, data)
        target = install_verified_jar(mods_dir, artifact["file_name"], data)
        installed.append({**artifact, "status": "installed",
                          "path": str(target)})
    return installed


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------

def cmd_search(args):
    client = ModrinthClient(args.api_base)
    hits = search_projects(client, args.query, args.mc_version,
                           args.loader, args.limit)
    rows = []
    for hit in hits:
        if not isinstance(hit, dict):
            continue
        slug = str(hit.get("slug") or "")
        versions = hit.get("versions")
        mc_match = (None if args.mc_version is None
                    else isinstance(versions, list)
                    and args.mc_version in versions)
        categories = [str(c).lower() for c in hit.get("categories") or []
                      if isinstance(c, str)]
        loaders_found = [c for c in categories if c in KNOWN_LOADERS]
        rows.append({
            "slug": slug,
            "title": str(hit.get("title") or slug),
            "project_id": str(hit.get("project_id") or ""),
            "downloads": hit.get("downloads") if isinstance(
                hit.get("downloads"), int) else 0,
            "mc_match": mc_match,
            "loaders": loaders_found,
            "builtin_superseded": is_builtin_superseded(slug),
        })
    if args.json:
        print_json({
            "schema": "megu-mods-search/1",
            "query": args.query,
            "mc_version": args.mc_version,
            "loader": args.loader,
            "hits": rows,
        })
        return 0
    print(f"{'slug':<34} {'title':<40} {'mc':<5} {'loaders':<16} downloads")
    for row in rows:
        slug = row["slug"] + (" [built-in]" if row["builtin_superseded"] else "")
        mc = ("-" if row["mc_match"] is None
              else ("yes" if row["mc_match"] else "no"))
        loaders = ",".join(row["loaders"]) or "-"
        print(f"{slug:<34.34} {row['title']:<40.40} {mc:<5} "
              f"{loaders:<16.16} {row['downloads']:,}")
    if not rows:
        print("(no results)")
    return 0


def cmd_add(args):
    slug = args.slug.strip().lower()
    if not SLUG_RE.match(slug):
        raise MeguModsError(
            f"slug '{args.slug}' is not a valid [a-z0-9_-]{{1,64}} id")
    if is_builtin_superseded(slug) and not args.force:
        if args.json:
            print_json({
                "schema": "megu-mods-add/1",
                "status": "builtin_superseded",
                "slug": slug,
                "reason": REASON_SUPERSEDED,
            })
        else:
            print(f"'{slug}' is a performance mod that is "
                  f"{REASON_SUPERSEDED} — mc-spine builds this "
                  "optimization in natively, so it is skipped and nothing "
                  "was downloaded. Pass --force to install it anyway.")
        return 0
    if is_builtin_superseded(slug):
        print(f"warning: '{slug}' is {REASON_SUPERSEDED}; installing anyway "
              "because --force was given", file=sys.stderr)
    client = ModrinthClient(args.api_base)
    loaders = [args.loader] if args.loader else ["neoforge", "fabric"]
    notes = []
    plan = resolve_with_deps(client, slug, args.mc_version, loaders, notes)
    mods_dir = Path(args.mods_dir)
    if args.dry_run:
        if args.json:
            print_json({"schema": "megu-mods-add/1", "status": "dry_run",
                        "mods_dir": str(mods_dir), "notes": notes,
                        "plan": plan})
        else:
            emit_plan(plan, mods_dir, args.lang)
            for note in notes:
                print(f"note: {note}")
            print("dry run: nothing downloaded or installed")
        return 0
    if not args.json:
        emit_plan(plan, mods_dir, args.lang)
        for note in notes:
            print(f"note: {note}")
    if not request_consent(args):
        print("aborted — nothing downloaded")
        return 1
    installed = install_plan(client, plan, mods_dir)
    binary, report, warning = run_mod_check(mods_dir)
    if args.json:
        print_json({
            "schema": "megu-mods-add/1",
            "status": "installed",
            "mods_dir": str(mods_dir),
            "installed": installed,
            "notes": notes,
            "adapt_check": report,
            "adapt_check_binary": binary,
            "adapt_check_warning": warning,
        })
    else:
        for item in installed:
            state = item["status"]
            print(f"{state}: {item['file_name']} "
                  f"({human_size(item['size_bytes'])})")
        if report is not None:
            print(format_adapt_summary(report))
        elif warning:
            print(f"note: {warning}")
    return 0


def cmd_resolve_deps(args):
    mods_dir = Path(args.mods_dir)
    if not mods_dir.is_dir():
        raise MeguModsError(f"--mods-dir '{mods_dir}' is not a directory")
    provided, missing, rejected, skipped = scan_mods_dir(mods_dir)
    if not args.json:
        if rejected:
            for name, reason in rejected:
                print(f"rejected: {name}: {reason}")
        if skipped:
            for name, mod_id in skipped:
                print(f"skipped (built-in): {name} [{mod_id}] — "
                      f"{REASON_SUPERSEDED}")
        if not missing:
            msg = ("No missing mod dependencies — every required mod is "
                   "provided." if args.lang != "ja" else
                   "不足している必須依存 Mod はありません。")
            print(msg)
    if not missing:
        if args.json:
            print_json({
                "schema": "megu-mods-resolve-deps/1",
                "status": "clean",
                "mods_dir": str(mods_dir),
                "missing": [],
                "rejected": [{"jar": n, "reason": r} for n, r in rejected],
                "skipped_superseded": [
                    {"jar": n, "mod_id": i} for n, i in skipped],
            })
        return 0
    client = ModrinthClient(args.api_base)
    loaders = [args.loader] if args.loader else ["neoforge", "fabric"]
    plan = []
    notes = []
    manual = []
    visited = set()
    planned_files = set()

    def resolve(key, depth):
        if key in visited:
            return
        visited.add(key)
        if depth > MAX_DEP_DEPTH:
            notes.append(
                f"dependency depth cap ({MAX_DEP_DEPTH}) reached at '{key}'")
            return
        try:
            versions = get_versions(client, key)
        except MeguModsError:
            manual.append(key)
            return
        artifact = pick_artifact(
            versions, args.mc_version, loaders, client.loopback)
        if artifact is None:
            manual.append(key)
            return
        artifact = dict(artifact)
        artifact["slug"] = key
        for dep_id in artifact["required_project_ids"]:
            resolve(dep_id, depth + 1)
        if artifact["file_name"] in planned_files:
            return
        planned_files.add(artifact["file_name"])
        plan.append(artifact)

    for _mod_id, dep_id, rng in missing:
        dep_key = dep_id.lower()
        if not SLUG_RE.match(dep_key):
            manual.append(dep_id)
            continue
        resolve(dep_key, 1)

    if args.json and (not plan or args.dry_run):
        print_json({
            "schema": "megu-mods-resolve-deps/1",
            "status": "dry_run" if args.dry_run else
                      ("planned" if plan else "manual_required"),
            "mods_dir": str(mods_dir),
            "missing": [{"mod_id": m, "required_mod_id": d, "range": r}
                        for m, d, r in missing],
            "plan": plan,
            "manual": manual,
            "notes": notes,
            "rejected": [{"jar": n, "reason": r} for n, r in rejected],
        })
    elif not args.json:
        for mod_id, dep_id, rng in missing:
            print(f"missing: '{dep_id}' required by '{mod_id}'"
                  + (f" (declared {rng})" if rng else ""))
        for dep_id in manual:
            if args.lang == "ja":
                print(f"検証済みの Modrinth 候補がありません — '{dep_id}' "
                      "の JAR を手動でダウンロードして mods/ に配置し、"
                      "'/mods reload' を実行してください")
            else:
                print(f"no verified Modrinth candidate — download the "
                      f"'{dep_id}' JAR manually, drop it into mods/, then "
                      "run '/mods reload'")
        if plan:
            emit_plan(plan, mods_dir, args.lang)
        for note in notes:
            print(f"note: {note}")
    if not plan or args.dry_run:
        if not args.json and args.dry_run:
            print("dry run: nothing downloaded or installed")
        return 0
    if not request_consent(args):
        print("aborted — nothing downloaded")
        return 1
    installed = install_plan(client, plan, mods_dir)
    binary, report, warning = run_mod_check(mods_dir)
    if args.json:
        print_json({
            "schema": "megu-mods-resolve-deps/1",
            "status": "installed",
            "mods_dir": str(mods_dir),
            "installed": installed,
            "manual": manual,
            "adapt_check": report,
            "adapt_check_binary": binary,
            "adapt_check_warning": warning,
        })
    else:
        for item in installed:
            print(f"{item['status']}: {item['file_name']} "
                  f"({human_size(item['size_bytes'])})")
        if report is not None:
            print(format_adapt_summary(report))
        elif warning:
            print(f"note: {warning}")
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def build_parser():
    parser = argparse.ArgumentParser(
        prog=PROG,
        description="Safe Modrinth v2 mod search, add, and missing-dependency "
                    "installer (consent + CDN allowlist + SHA-512 verified, "
                    "atomic installs).")
    parser.add_argument("--api-base", default=None,
                        help=argparse.SUPPRESS)  # loopback test override only
    sub = parser.add_subparsers(dest="command", required=True)

    def common(p, mods_dir=False):
        p.add_argument("--mc-version", default=DEFAULT_MC_VERSION,
                       help="target Minecraft version tag (default "
                            f"{DEFAULT_MC_VERSION}; e.g. 26.3)")
        p.add_argument("--loader", choices=KNOWN_LOADERS, default=None,
                       help="preferred loader (default: neoforge+fabric)")
        p.add_argument("--json", action="store_true",
                       help="print the machine-readable report")
        p.add_argument("--lang", choices=("en", "ja"), default="en")
        if mods_dir:
            p.add_argument("--mods-dir", required=True,
                           help="target mods/ directory")
            p.add_argument("--yes", action="store_true",
                           help="non-interactive consent to download+install")
            p.add_argument("--dry-run", action="store_true",
                           help="print the install plan only; no downloads")

    p_search = sub.add_parser("search", help="search Modrinth v2 projects")
    p_search.add_argument("query")
    p_search.add_argument("--limit", type=int, default=10,
                          help=f"max hits (1..={MAX_SEARCH_LIMIT})")
    common(p_search)
    p_search.set_defaults(func=cmd_search)

    p_add = sub.add_parser("add", help="resolve + install one mod and its "
                                       "required dependencies")
    p_add.add_argument("slug", help="Modrinth project slug or id")
    p_add.add_argument("--force", action="store_true",
                       help="install even a built-in-superseded perf mod")
    common(p_add, mods_dir=True)
    p_add.set_defaults(func=cmd_add)

    p_deps = sub.add_parser("resolve-deps",
                            help="find missing required deps of installed "
                                 "JARs and install them")
    common(p_deps, mods_dir=True)
    p_deps.set_defaults(func=cmd_resolve_deps)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    if getattr(args, "limit", 10) < 1 or getattr(args, "limit", 10) \
            > MAX_SEARCH_LIMIT:
        parser.error(f"--limit must be 1..={MAX_SEARCH_LIMIT}")
    try:
        return args.func(args)
    except MeguModsError as error:
        print(f"{PROG}: error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
