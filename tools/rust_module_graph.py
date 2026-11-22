"""Project explicit module paths from rustc's production macro/configuration expansion.

This is not type/name resolution: parent-facade bindings outside the generation
family are not expanded into symbol dependencies. Generation code must reference
siblings explicitly. Cargo/rustc, not a handwritten cfg or macro interpreter,
chooses the actual production token stream. No server build inherits bootstrap.
"""
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / "evidence/foundation/module-layers/policy.json"
IDENT = re.compile(r"[A-Za-z_][A-Za-z_0-9]*")
RAW = re.compile(r'(?:br|cr|r)(#*)"')
CHAR = re.compile(r"'(?:[^'\\\n]|\\(?:[nrt0'\"\\]|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}))'")


def tokens(text):
    out, i = [], 0
    while i < len(text):
        if text[i].isspace():
            i += 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            i = len(text) if end < 0 else end + 1
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while depth:
                if i >= len(text): raise ValueError("unterminated Rust comment")
                if text.startswith("/*", i): depth, i = depth + 1, i + 2
                elif text.startswith("*/", i): depth, i = depth - 1, i + 2
                else: i += 1
        elif raw := RAW.match(text, i):
            end = text.find('"' + raw[1], raw.end())
            if end < 0: raise ValueError("unterminated Rust raw string")
            i = end + 1 + len(raw[1])
        elif text[i] == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            if i >= len(text): raise ValueError("unterminated Rust string")
            i += 1
        elif char := CHAR.match(text, i): i = char.end()
        elif ident := IDENT.match(text, i):
            out.append(ident[0]); i = ident.end()
        elif text.startswith("::", i):
            out.append("::"); i += 2
        else:
            out.append(text[i]); i += 1
    return out


def balanced_end(items, start, left, right):
    depth = 0
    for i in range(start, len(items)):
        if items[i] == left: depth += 1
        elif items[i] == right:
            depth -= 1
            if not depth: return i + 1
    raise ValueError(f"unbalanced Rust {left}{right}")


def strip_macro_definitions(items):
    # Unused macro definitions can remain in expanded output; their token trees
    # are not resolved code. Used macros have their expansion at the call site.
    out, i = [], 0
    while i < len(items):
        if items[i:i + 2] == ["macro_rules", "!"]:
            start = i + 3
            if start >= len(items) or items[start] not in {"{", "(", "["}:
                raise ValueError("unsupported expanded macro definition")
            left = items[start]
            i = balanced_end(items, start, left, {"{": "}", "(": ")", "[": "]"}[left])
        else:
            out.append(items[i]); i += 1
    return out


def split_modules(items):
    modules, main, i, depth = {}, [], 0, 0
    while i < len(items):
        if not depth and items[i] == "mod":
            if i + 2 >= len(items) or not IDENT.fullmatch(items[i + 1]) or items[i + 2] != "{":
                raise ValueError("unsupported expanded root module")
            name = items[i + 1]
            end = balanced_end(items, i + 2, "{", "}")
            if name == "main" or name in modules:
                raise ValueError(f"duplicate or reserved module owner {name}")
            modules[name] = items[i + 3:end - 1]
            i = end
        else:
            if items[i] == "{": depth += 1
            elif items[i] == "}": depth -= 1
            main.append(items[i]); i += 1
    modules["main"] = main
    return modules


def expanded_modules(root):
    env = dict(os.environ)
    # This private diagnostic flag is isolated to this subprocess and this crate.
    # The dedicated cache never supplies a runtime executable or substitutes a
    # prior source dump. Empty/failed expansion is a hard failure.
    env["RUSTC_BOOTSTRAP"] = "megu"
    cache = Path(env.get("CARGO_TARGET_DIR", str(root / "target"))).resolve() / "module-graph"
    env["CARGO_TARGET_DIR"] = str(cache)
    result = subprocess.run(
        ["cargo", "rustc", "--offline", "--locked", "--all-features", "--bin", "megu",
         "--", "-Zunpretty=expanded"], cwd=root, env=env, capture_output=True, text=True, timeout=120)
    if result.returncode or not result.stdout.strip():
        raise ValueError(f"Rust expansion unavailable (exit {result.returncode}): {result.stderr[-3000:]}")
    return split_modules(strip_macro_definitions(tokens(result.stdout)))


def group_names(items, start):
    end = balanced_end(items, start, "{", "}")
    names, i = [], start + 1
    while i < end - 1:
        if items[i] == ",": i += 1
        elif items[i] == "{":
            names.extend(group_names(items, i))
            i = balanced_end(items, i, "{", "}")
        else:
            names.append(items[i]); i += 1
            while i < end - 1 and items[i] != ",":
                i = balanced_end(items, i, "{", "}") if items[i] == "{" else i + 1
    return names


def references(items, modules, strict=False):
    found = set()
    for i, item in enumerate(items):
        if item == "mod":
            raise ValueError("nested production modules require an explicit graph policy")
        if strict and ((item == "super" and items[i + 1:i + 2] == ["::"]) or
                       (item == "crate" and items[i + 1:i + 2] == ["as"])):
            raise ValueError("generation module must use explicit sibling-module paths")
        if item in modules and items[i + 1:i + 2] == ["::"] and (i == 0 or items[i - 1] != "::"):
            found.add(item)
        if item not in {"crate", "super"} or items[i + 1:i + 2] != ["::"]:
            continue
        start = i + 2
        if start >= len(items): raise ValueError("incomplete module path")
        names = group_names(items, start) if items[start] == "{" else [items[start]]
        for name in names:
            if name in modules: found.add(name)
            elif strict: raise ValueError(f"generation dependency hidden behind root binding {name!r}")
    return found


def source_graph(root=ROOT):
    policy = json.loads((root / "evidence/foundation/module-layers/policy.json").read_text())
    bodies = expanded_modules(root)
    if any(items[i:i + 3] == ["extern", "crate", "self"]
           for items in bodies.values() for i in range(len(items) - 2)):
        raise ValueError("indirect self-crate aliases require a resolved-symbol graph")
    modules = set(bodies)
    expected = {name for layer in policy["layers"] for name in layer}
    if modules != expected:
        raise ValueError(f"module policy mismatch: added={sorted(modules - expected)}, missing={sorted(expected - modules)}")
    edges = {("main", name) for name in modules - {"main"}}
    for name, items in bodies.items():
        for target in references(items, modules, name in policy["explicit_generation_modules"]):
            if name != target: edges.add((name, target))
    return dict(modules=sorted(modules), depends=sorted(edges), roots=["main"],
                layers=policy["layers"], forbidden=policy["forbidden"])


if __name__ == "__main__":
    print(json.dumps(source_graph(), indent=2))
