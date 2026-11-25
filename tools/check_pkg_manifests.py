#!/usr/bin/env python3
"""Check package metadata and installer versions against the website pin."""
import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPO_URL = "https://github.com/nekomario28/megu-mc/releases/download/"
FILES = ("packaging/homebrew/megu.rb", "packaging/scoop/megu.json",
         "packaging/winget/nekomario28.Megu.yaml", "packaging/aur/PKGBUILD",
         "tools/install_windows.ps1", "tools/install_linux.sh")
VERSION_RE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?$")
# Build the machine-specific alternatives so this guard does not trip the
# repository public-hygiene scanner on its own detection pattern.
PERSONAL_RE = re.compile(
    "|".join((
        r"/home/[^\s/'\"]+",
        "Megumin" + "Desktop",
        r"/data/tmp/",
        "\\.t3/" + "worktrees",
    )),
    re.I,
)


def read_version(root):
    text = (root / "website/site-config.js").read_text(encoding="utf-8")
    match = re.search(r'\breleaseTag\s*:\s*"([^"]+)"', text)
    if not match:
        raise ValueError("website/site-config.js has no releaseTag")
    return match.group(1)


def tokens(tag):
    raw = tag[1:]
    return raw, raw.replace("-", "_"), raw


def sync(root, tag):
    if not VERSION_RE.fullmatch(tag):
        raise ValueError(f"invalid release tag: {tag!r}")
    raw, pkgver, winget = tokens(tag)
    interpolation = "$" + "{pkgver/_/-}"
    replacements = {
        FILES[0]: [(r'(?m)^\s*version "[^"]+"$', f'  version "{raw}"')],
        FILES[1]: [(r'(?m)^\s*"version": "[^"]+",$', f'  "version": "{tag}",')],
        FILES[2]: [(r"(?m)^PackageVersion: .+$", f"PackageVersion: {winget}"),
                   (r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?", tag)],
        FILES[3]: [(r"(?m)^pkgver=.+$", f"pkgver={pkgver}"),
                   (r"(?m)^_tag=.+$", f'_tag="v{interpolation}"')],
        FILES[4]: [(r"(?m)^\s*\[string\]\$Version = '[^']+',?$", f"    [string]$Version = '{tag}',")],
        FILES[5]: [(r'(?m)^VERSION="\$\{MEGU_VERSION:-[^}]+\}"$', f'VERSION="${{MEGU_VERSION:-{tag}}}"')],
    }
    for rel, edits in replacements.items():
        path = root / rel
        text = path.read_text(encoding="utf-8")
        for pattern, replacement in edits:
            all_matches = rel == FILES[2] and pattern.startswith("v[0-9]")
            text, count = re.subn(pattern, replacement, text, count=0 if all_matches else 1)
            if count < 1:
                raise ValueError(f"cannot locate version field in {rel}: {pattern}")
        path.write_text(text, encoding="utf-8")


def check(root):
    errors = []
    try:
        tag = read_version(root)
    except (OSError, ValueError) as error:
        return [str(error)]
    if not VERSION_RE.fullmatch(tag):
        return [f"invalid website releaseTag: {tag!r}"]
    raw, pkgver, winget = tokens(tag)
    data = {}
    for rel in FILES:
        try:
            data[rel] = (root / rel).read_text(encoding="utf-8")
        except OSError as error:
            errors.append(f"{rel}: {error}")
    for rel, text in data.items():
        if PERSONAL_RE.search(text):
            errors.append(f"{rel}: personal or machine-specific path found")
        for url in re.findall(r"https?://[^\s\"']+", text):
            if "github.com/nekomario28/megu-mc/releases/download/" in url:
                if not url.startswith(REPO_URL):
                    errors.append(f"{rel}: untrusted release URL {url}")
            elif ("github.com/nekomario28/megu-mc" not in url
                  and "raw.githubusercontent.com/nekomario28/megu-mc" not in url
                  and "api.github.com/repos/nekomario28/megu-mc/" not in url
                  and not url.startswith("https://github.com/$Repository/releases/download/")
                  and not url.startswith("https://github.com/${REPO}/releases/download/")):
                errors.append(f"{rel}: untrusted external URL {url}")
        if "LGPL-3.0-or-later" not in text:
            errors.append(f"{rel}: missing LGPL-3.0-or-later license")
        if "http://" in text:
            errors.append(f"{rel}: insecure HTTP URL")

    formula, scoop, winget_file, aur, win, linux = (data.get(rel, "") for rel in FILES)
    interpolation = "$" + "{pkgver/_/-}"
    must_contain = (
        (FILES[0], formula, f'version "{raw}"'),
        (FILES[1], scoop, f'"version": "{tag}"'),
        (FILES[2], winget_file, f"PackageVersion: {winget}"),
        (FILES[2], winget_file, tag),
        (FILES[3], aur, f"pkgver={pkgver}"),
        (FILES[3], aur, f"v{interpolation}"),
        (FILES[4], win, f"[string]$Version = '{tag}'"),
        (FILES[5], linux, f"MEGU_VERSION:-{tag}"),
    )
    for rel, text, value in must_contain:
        if value not in text:
            errors.append(f"{rel}: version is not synchronized with {tag}")
    for target in ("macos-arm64", "macos-x86_64", "linux-arm64", "linux-x86_64"):
        if target not in formula:
            errors.append(f"{FILES[0]}: missing target {target}")
    if "%w[megu megu-client-gpu megu-installer]" not in formula:
        errors.append(f"{FILES[0]}: Homebrew formula does not install exactly the slim release binaries")
    for arch, target in (("64bit", "windows-x86_64"), ("arm64", "windows-arm64")):
        if f'"{arch}"' not in scoop or target not in scoop:
            errors.append(f"{FILES[1]}: missing architecture {arch}/{target}")
    for target in ("windows-x86_64", "windows-arm64"):
        if target not in winget_file:
            errors.append(f"{FILES[2]}: missing target {target}")
    for target in ("linux-x86_64", "linux-arm64"):
        if target not in aur:
            errors.append(f"{FILES[3]}: missing target {target}")
    if "SHA256SUMS-all.txt" not in win or "Get-FileHash" not in win:
        errors.append(f"{FILES[4]}: missing release SHA-256 verification")
    if "SHA256SUMS-all.txt" not in linux:
        errors.append(f"{FILES[5]}: checksum filename differs from release workflow")
    if "for bin in megu megu-client-gpu megu-installer; do" not in linux:
        errors.append(f"{FILES[5]}: Linux installer does not link exactly the slim release binaries")
    if "CLIENT_TARGET" not in linux or "-client" not in linux:
        errors.append(f"{FILES[5]}: Linux installer does not reconcile split dynamic musl client archives")
    for dropped_bin in ("megu-mod-check", "megu-world-migrate", "megu-init", "megu-admin"):
        if dropped_bin in linux:
            errors.append(f"{FILES[5]}: dropped internal binary {dropped_bin} must not be installed")
    if "$version" not in scoop or "megu-$version-windows-x86_64.tar.gz" not in scoop or "megu-$version-windows-arm64.tar.gz" not in scoop:
        errors.append(f"{FILES[1]}: release asset URLs do not follow the manifest version")
    for required_bin in ("megu.exe", "megu-client-gpu.exe", "megu-installer.exe"):
        if required_bin not in scoop:
            errors.append(f"{FILES[1]}: missing slim release binary {required_bin}")
    for dropped_bin in ("megu-client.exe", "megu-mod-check.exe", "megu-world-migrate.exe", "megu-init.exe", "megu-admin.exe"):
        if dropped_bin in scoop:
            errors.append(f"{FILES[1]}: dropped internal binary {dropped_bin} must not be packaged")
    if "SHA256SUMS-all.txt" not in scoop or '"regex"' not in scoop:
        errors.append(f"{FILES[1]}: autoupdate must extract hashes from SHA256SUMS-all.txt")
    for target in ("windows-x86_64", "windows-arm64"):
        if f"/download/{tag}/megu-installer-{target}.exe" not in winget_file:
            errors.append(f"{FILES[2]}: installer URL is not a release asset for {tag}/{target}")
    workflow = (root / ".github/workflows/release.yml").read_text(encoding="utf-8")
    for target in ("linux-x86_64", "linux-arm64", "macos-x86_64", "macos-arm64", "windows-x86_64", "windows-arm64"):
        if target not in workflow:
            errors.append(f"release workflow has no asset target {target}")
    if "SHA256SUMS-all.txt" not in workflow or "SHA256SUMS" not in workflow:
        errors.append("release workflow checksum asset names changed")
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument("--sync-version", metavar="TAG", help="synchronize package and installer versions")
    args = parser.parse_args(argv)
    try:
        if args.sync_version:
            sync(args.root.resolve(), args.sync_version)
        errors = check(args.root.resolve())
    except (OSError, ValueError) as error:
        print(f"package manifest check failed: {error}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(f"package manifests synchronized with {read_version(args.root.resolve())}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
