"""Focused live NeoForge S1 listener/cancellation wire boundary regression.

The Rust java_compat unit tests verify host CAS atomicity and S3/S4 frame
bounds. These tests exercise the real pinned Java event bus and encoded S1
frames using the existing fixture rather than a synthetic listener.
"""

import unittest

import neoforge_s1_events_test as s1_fixture


class NeoForgeS1S4BoundaryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        s1_fixture.NeoForgeS1EventsTests.setUpClass()

    @classmethod
    def tearDownClass(cls):
        s1_fixture.NeoForgeS1EventsTests.tearDownClass()

    def run_fixture(self, mode, expected):
        runner = s1_fixture.NeoForgeS1EventsTests(methodName="test_six_events_encoded_batch")
        result = runner.run_java(
            "net.mcspine.neoforge.S1Checks", mode,
            s1_fixture.NeoForgeS1EventsTests.mod_dir.parent,
        )
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertIn(expected, result.stdout)

    def test_listener_order_and_canceled_frames(self):
        self.run_fixture("encoded", "PASS encoded S1 six events")

    def test_nested_cancellation_and_failed_scope_cleanup(self):
        self.run_fixture("scopes", "PASS scoped mutation rollback")
        self.run_fixture("overflow", "PASS real event bus reentrancy")


if __name__ == "__main__":
    unittest.main()
