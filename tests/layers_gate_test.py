#!/usr/bin/env python3
"""Compiler-backed source controls, and guards against absent/blind inspectors."""
import contextlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from rust_module_graph import references, split_modules, tokens


@contextlib.contextmanager
def fixture():
    with tempfile.TemporaryDirectory(prefix="spine-layers-") as tmp:
        root = Path(tmp)
        for name in ("src", "tools", "bin", "evidence/foundation/module-layers"):
            (root / name).mkdir(parents=True)
        for path in (ROOT / "src").glob("*.rs"):
            shutil.copy2(path, root / "src" / path.name)
        for name in ("Cargo.toml", "Cargo.lock", "config_packets.bin"):
            shutil.copy2(ROOT / name, root / name)
        (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
        for name in ("rust_module_graph.py", "check_layers.py"):
            shutil.copy2(ROOT / "tools" / name, root / "tools" / name)
        for name in ("deps.py", "deps_target.py", "policy.json", "gate_probe.py"):
            shutil.copy2(ROOT / "evidence/foundation/module-layers" / name,
                         root / "evidence/foundation/module-layers" / name)
        yield root


def run(root, path=None):
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    target = Path(env.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    if not target.is_absolute(): target = ROOT / target
    env["CARGO_TARGET_DIR"] = str(target / "layer-gate-tests")
    if path is not None: env["PATH"] = str(path)
    result = subprocess.run([sys.executable, "tools/check_layers.py"], cwd=root, env=env,
                            capture_output=True, text=True, timeout=180)
    return result.returncode, result.stdout + result.stderr


class LayerGateTests(unittest.TestCase):
    def test_real_source_control(self):
        with fixture() as root:
            code, output = run(root)
            self.assertEqual(code, 0, output)
            self.assertIn("source graph:", output)

    def test_forbidden_edge_is_read_from_rust(self):
        for imported in ["use crate::tree::TreeExtra;\n",
                         "use crate::{tree::TreeExtra, block_registry::BReg};\n",
                         "use crate::{{tree as hidden}};\n"]:
            with self.subTest(imported=imported), fixture() as root:
                p = root / "src/worldgen.rs"
                p.write_text(imported + p.read_text())
                code, output = run(root)
                self.assertNotEqual(code, 0, output)
                self.assertIn("deps.py: [forbidden-dependency] worldgen", output)

    def test_same_checkout_changes_and_restoration_reexpand(self):
        with fixture() as root:
            p = root / "src/worldgen.rs"
            original = p.read_text()
            code, output = run(root)
            self.assertEqual(code, 0, output)
            p.write_text("use crate::tree::TreeExtra;\n" + original)
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [forbidden-dependency] worldgen", output)
            p.write_text(original)
            code, output = run(root)
            self.assertEqual(code, 0, output)

    def test_java_backend_cannot_depend_on_its_dispatcher_again(self):
        with fixture() as root:
            p = root / "src/java_compat.rs"
            p.write_text(p.read_text() + "\nuse crate::landing::eligible;\n")
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [cycle] java_compat", output)

    def test_terrain_cannot_depend_on_population_implementation_again(self):
        with fixture() as root:
            p = root / "src/worldgen.rs"
            original = p.read_text()
            code, output = run(root)
            self.assertEqual(code, 0, output)
            p.write_text(original + "\nuse crate::feature::populate;\n")
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [layer-violation] worldgen", output)
            p.write_text(original)
            code, output = run(root)
            self.assertEqual(code, 0, output)

    def test_tree_cannot_depend_on_feature_again(self):
        with fixture() as root:
            p = root / "src/tree.rs"
            original = p.read_text()
            code, output = run(root)
            self.assertEqual(code, 0, output)
            p.write_text(original + "\nuse crate::feature::populate;\n")
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [layer-violation] tree", output)
            p.write_text(original)
            code, output = run(root)
            self.assertEqual(code, 0, output)

    def test_generation_context_cannot_depend_on_placement_implementations(self):
        with fixture() as root:
            p = root / "src/generation_context.rs"
            original = p.read_text()
            code, output = run(root)
            self.assertEqual(code, 0, output)
            p.write_text(original + "\nuse crate::tree::TreeExtra;\n")
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [layer-violation] generation_context", output)
            p.write_text(original)
            code, output = run(root)
            self.assertEqual(code, 0, output)

    def test_shared_primitives_cannot_depend_on_terrain_generation(self):
        for owner in ["json", "generation_math"]:
            with self.subTest(owner=owner), fixture() as root:
                p = root / f"src/{owner}.rs"
                original = p.read_text()
                code, output = run(root)
                self.assertEqual(code, 0, output)
                p.write_text(original + "\nuse crate::worldgen::Gen;\n")
                code, output = run(root)
                self.assertNotEqual(code, 0, output)
                self.assertIn(f"deps.py: [layer-violation] {owner}", output)
                p.write_text(original)
                code, output = run(root)
                self.assertEqual(code, 0, output)

    def test_macro_shadow_is_seen_at_the_expanded_call_site(self):
        with fixture() as root:
            p = root / "src/main.rs"
            macro = '''macro_rules! println {
                () => {{ let _: Option<crate::tree::TreeExtra> = None; }};
                ($($args:tt)+) => { ::std::println!($($args)*) };
            }
            '''
            p.write_text(p.read_text().replace("mod landing;", macro + "mod landing;", 1))
            p = root / "src/worldgen.rs"
            p.write_text(p.read_text() + "\nfn gate_shadow_proof() { println!(); }\n")
            code, output = run(root)
            self.assertNotEqual(code, 0, output)
            self.assertIn("deps.py: [forbidden-dependency] worldgen", output)

    def test_cfg_and_stringify_are_decided_by_rustc(self):
        with fixture() as root:
            p = root / "src/worldgen.rs"
            p.write_text(p.read_text() + '''
                #[cfg(all(test))] use crate::tree::TreeExtra;
                #[cfg(all(test, feature="java-compat"))]
                const ONLY_TEST: usize = { 0 } + crate::tree::NOT_A_VALUE;
                #[cfg(test)] const fn only_test<const N: usize = { 0 }>() { crate::tree::f(); }
                #[cfg(test)] static TEST_FN: fn() -> bool = || { true };
                #[path="not_a_file.rs"] #[cfg(test)] mod separate_tests;
                const ONLY_TEXT: &str = stringify!(crate::tree::NOT_A_TYPE);
                macro_rules! unused_tokens { () => { crate::tree::NOT_A_VALUE } }
            ''')
            code, output = run(root)
            self.assertEqual(code, 0, output)

    def test_new_module_requires_a_policy_identity(self):
        with fixture() as root:
            p = root / "src/main.rs"
            p.write_text(p.read_text() + "\nmod hidden;\n")
            (root / "src/hidden.rs").write_text("")
            code, output = run(root)
            self.assertNotEqual(code, 0)
            self.assertIn("module policy mismatch", output)

    def test_indirect_crate_namespace_is_loud_not_unresolved(self):
        with fixture() as root:
            p = root / "src/main.rs"
            p.write_text(p.read_text().replace("mod landing;", "extern crate self as alias;\nmod landing;", 1))
            code, output = run(root)
            self.assertNotEqual(code, 0)
            self.assertIn("indirect self-crate aliases", output)

    def test_empty_compiler_capture_cannot_use_the_declared_graph(self):
        with fixture() as root:
            compiler = root / "bin/cargo"
            compiler.write_text(f"#!{sys.executable}\n")
            compiler.chmod(0o755)
            code, output = run(root, str(root / "bin") + os.pathsep + os.environ["PATH"])
            self.assertNotEqual(code, 0)
            self.assertIn("Rust expansion unavailable", output)
            self.assertNotIn("deps.py: [forbidden-dependency] worldgen", output)

    def test_missing_tool_is_not_a_skip(self):
        with fixture() as root:
            code, output = run(root, root / "bin")
            self.assertNotEqual(code, 0)
            self.assertIn("depsgx is required", output)

    def test_inspector_must_detect_faults_and_preserve_exit_status(self):
        errors = [{"severity": "error", "code": c, "target": "lower", "message": "probe"}
                  for c in ("cycle", "layer-violation", "forbidden-dependency")]
        for report, status, marker in [({"errors": 0, "problems": []}, 0, "probe must report"),
                                       ({"errors": 3, "problems": errors}, 0, "exit status disagrees"),
                                       ({"errors": 3, "problems": errors}, 1, "probe must report")]:
            with self.subTest(report=report, status=status), fixture() as root:
                tool = root / "bin/depsgx"
                tool.write_text(f"#!{sys.executable}\nprint({json.dumps(report)!r})\nraise SystemExit({status})\n")
                tool.chmod(0o755)
                code, output = run(root, root / "bin")
                self.assertNotEqual(code, 0)
                self.assertIn(marker, output)

    def test_root_module_cannot_replace_the_synthetic_crate_owner(self):
        with self.assertRaisesRegex(ValueError, "reserved module owner main"):
            split_modules(tokens("mod main { use crate::tree::TreeExtra; } fn main() {}"))

    def test_lexical_data_and_nested_aliases(self):
        text = '''
        // crate::tree::not_code
        /* crate::tree::nope /* nested */ */
        let a = r###"crate::tree::not_code"###;
        let b = "crate::tree::not_code";
        use crate::{worldgen::{J, Xor}, feature as registry};
        fn borrowed<'a>(x: &'a J) { if !(true) { vec![1]; } }
        '''
        self.assertEqual(references(tokens(text), {"feature", "tree", "worldgen"}, strict=True),
                         {"worldgen", "feature"})
        for text in ['use super::*;', 'use crate::*;', 'use crate as hidden;']:
            with self.subTest(text=text), self.assertRaises(ValueError):
                references(tokens(text), {"feature", "tree", "worldgen"}, strict=True)


if __name__ == "__main__":
    unittest.main()
