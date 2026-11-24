#!/usr/bin/env python3
"""Fixture tests for tools/wiring_connectivity_audit.py.

Drives the real entry point against synthetic Rust trees to prove the
channel-symmetry check resolves ``const NAME: &str = "megu:…"`` bindings —
the shape shared crates use to publish a channel id both sides reference —
without letting a genuinely one-sided channel go green.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
AUDIT = ROOT / "tools/wiring_connectivity_audit.py"


def put(root, name, content):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def run(root, *args):
    return subprocess.run(
        [sys.executable, str(AUDIT), "--root", str(root), *args],
        text=True, capture_output=True, check=False, timeout=120,
    )


# The channel literal lives only inside the shared crate's const
# declaration; both sides reference the identifier, never the literal.
SHARED_CONST = """\
pub const GHOST_SYNC_CHANNEL: &str = "megu:ghost_sync_v1";
"""

SERVER_REF = """\
use megu_proto::channels::GHOST_SYNC_CHANNEL;

fn channel_frame() -> &'static str {
    GHOST_SYNC_CHANNEL
}
"""

CLIENT_REF = """\
use megu_proto::channels::GHOST_SYNC_CHANNEL;

fn handle(channel: &str) -> bool {
    channel == GHOST_SYNC_CHANNEL
}
"""

# A reference that exists only under `#[cfg(test)]` is masked like every
# other check, so it cannot launder a one-sided channel.
CLIENT_TEST_ONLY_REF = """\
fn handle(channel: &str) -> bool {
    channel.is_empty()
}

#[cfg(test)]
mod tests {
    use megu_proto::channels::GHOST_SYNC_CHANNEL;

    #[test]
    fn matches() {
        assert!(handle(GHOST_SYNC_CHANNEL));
    }
}
"""

# The save-tag check fails closed when src/save_v2.rs is absent, so every
# exit-0 fixture carries a symmetric minimal file.
SAVE_V2_STUB = """\
fn roundtrip(out: &mut Vec<u8>, b: &[u8]) -> bool {
    out.extend_from_slice(b"STB1");
    b.ends_with(b"STB1")
}
"""


class WiringConnectivityAuditTests(unittest.TestCase):
    def fixture(self, files):
        tmp = tempfile.TemporaryDirectory(prefix="wiring-conn-audit-")
        root = Path(tmp.name)
        for rel, text in files.items():
            put(root, rel, text)
        self.addCleanup(tmp.cleanup)
        return root

    def test_const_bound_references_wire_the_channel(self):
        root = self.fixture({
            "proto/src/channels.rs": SHARED_CONST,
            "src/channels.rs": SERVER_REF,
            "client-app/src/handler.rs": CLIENT_REF,
            "src/save_v2.rs": SAVE_V2_STUB,
        })
        result = run(root)
        self.assertEqual(
            result.returncode, 0,
            f"const-resolved channel should be symmetric\n{result.stdout}"
            f"{result.stderr}")
        self.assertNotIn("megu:ghost_sync_v1 @", result.stdout)

    def test_one_sided_const_channel_is_caught(self):
        root = self.fixture({
            "proto/src/channels.rs": SHARED_CONST,
            "src/channels.rs": SERVER_REF,
            "src/save_v2.rs": SAVE_V2_STUB,
        })
        result = run(root)
        self.assertEqual(
            result.returncode, 1,
            f"server-only channel must stay a finding\n{result.stdout}"
            f"{result.stderr}")
        self.assertIn("megu:ghost_sync_v1", result.stdout)

    def test_test_only_reference_does_not_wire_the_channel(self):
        root = self.fixture({
            "proto/src/channels.rs": SHARED_CONST,
            "src/channels.rs": SERVER_REF,
            "client-app/src/handler.rs": CLIENT_TEST_ONLY_REF,
            "src/save_v2.rs": SAVE_V2_STUB,
        })
        result = run(root)
        self.assertEqual(
            result.returncode, 1,
            f"a cfg(test)-only reference must not satisfy the check\n"
            f"{result.stdout}{result.stderr}")
        self.assertIn("megu:ghost_sync_v1", result.stdout)


if __name__ == "__main__":
    unittest.main()
