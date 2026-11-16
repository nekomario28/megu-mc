#!/usr/bin/env python3
"""Negative controls for refresh.compare().

The validator is only useful if it rejects a stale row. These tests feed it a
deliberately corrupted copy of the inventory and require a FAIL, plus the
baseline case that must stay clean.

  python3 refresh_test.py
"""
import copy
import json
import os
import unittest

from refresh import compare

HERE = os.path.dirname(os.path.abspath(__file__))


def load_inventory():
    with open(os.path.join(HERE, "inventory.json")) as f:
        return json.load(f)


def facts_for(inv):
    """Minimal facts that agree with the inventory as recorded."""
    heads = {r["ref"]: r["sha_observed"] for r in inv["refs"]}
    wts = {}
    for w in inv["worktrees"]:
        wts[w["worktree_id"]] = {
            "id": w["worktree_id"], "path": "/nonexistent", "branch": w["branch"],
            "head": w["head_observed"],
            "dirty_rows": [] if w["clean"] else [" M example"] * w["dirty_count"],
            "dirty_count": 0 if w["clean"] else w["dirty_count"],
            "status_readable": True,
        }
    return {"heads": heads, "worktrees": wts}


def codes(issues, level="FAIL"):
    return {i["code"] for i in issues if i["level"] == level}


class RefreshCompareTest(unittest.TestCase):
    def setUp(self):
        self.inv = load_inventory()
        self.facts = facts_for(self.inv)

    def test_baseline_is_clean(self):
        self.assertEqual(compare(self.inv, self.facts), [])

    def test_moved_sha_on_a_stable_row_is_rejected(self):
        row = next(r for r in self.inv["refs"]
                   if not r["moving"] and r["disposition"] != "CANONICAL")
        self.facts["heads"][row["ref"]] = "0" * 40
        issues = compare(self.inv, self.facts)
        self.assertIn("REF_MOVED", codes(issues))
        self.assertEqual(issues[0]["target"], row["ref"])

    def test_moved_active_row_only_warns(self):
        row = next(r for r in self.inv["refs"] if r["moving"] and r["disposition"] == "ACTIVE")
        self.facts["heads"][row["ref"]] = "1" * 40
        issues = compare(self.inv, self.facts)
        self.assertNotIn("REF_MOVED", codes(issues))
        self.assertIn("REF_MOVED_ACTIVE", codes(issues, "WARN"))

    def test_dirty_worktree_row_is_rejected(self):
        row = next(w for w in self.inv["worktrees"] if w["clean"])
        self.facts["worktrees"][row["worktree_id"]]["dirty_rows"] = [" M a", " M b", " M c"]
        self.facts["worktrees"][row["worktree_id"]]["dirty_count"] = 3
        issues = compare(self.inv, self.facts)
        self.assertIn("WORKTREE_DIRTY_MISMATCH", codes(issues))

    def test_clean_worktree_row_that_became_dirty_is_rejected(self):
        row = next(w for w in self.inv["worktrees"] if not w["clean"])
        self.facts["worktrees"][row["worktree_id"]]["dirty_rows"] = []
        self.facts["worktrees"][row["worktree_id"]]["dirty_count"] = 0
        issues = compare(self.inv, self.facts)
        self.assertIn("WORKTREE_DIRTY_MISMATCH", codes(issues))

    def test_missing_worktree_is_rejected(self):
        wid = self.inv["worktrees"][0]["worktree_id"]
        del self.facts["worktrees"][wid]
        issues = compare(self.inv, self.facts)
        self.assertIn("WORKTREE_MISSING", codes(issues))

    def test_missing_ref_is_rejected(self):
        ref = self.inv["refs"][0]["ref"]
        del self.facts["heads"][ref]
        issues = compare(self.inv, self.facts)
        self.assertIn("REF_MISSING", codes(issues))

    def test_corrupted_copy_does_not_mutate_the_input(self):
        before = copy.deepcopy(self.inv)
        self.facts["heads"][self.inv["refs"][0]["ref"]] = "2" * 40
        compare(self.inv, self.facts)
        self.assertEqual(before, self.inv)


class RefreshBoundaryTest(unittest.TestCase):
    """The documented limits, pinned so nobody reads `0 fail` as "safe to delete".

    These are properties of the validator, not bugs: it compares the rows it holds,
    with a sampled `git status`. Each case below is clean on purpose.
    """

    def setUp(self):
        self.inv = load_inventory()
        self.facts = facts_for(self.inv)

    def test_a_ref_that_is_not_in_the_inventory_is_invisible(self):
        self.facts["heads"]["t3/a-ref-created-after-the-snapshot"] = "3" * 40
        self.assertEqual(compare(self.inv, self.facts), [])

    def test_a_worktree_that_is_not_in_the_inventory_is_invisible(self):
        self.facts["worktrees"]["t3-a-worktree-created-after-the-snapshot"] = {
            "id": "t3-later", "path": "/nonexistent", "branch": "t3/later",
            "head": "4" * 40, "dirty_rows": [" M new"], "dirty_count": 1,
            "status_readable": True,
        }
        self.assertEqual(compare(self.inv, self.facts), [])

    def test_same_dirty_count_with_different_content_is_invisible(self):
        row = next(w for w in self.inv["worktrees"] if not w["clean"])
        live = self.facts["worktrees"][row["worktree_id"]]
        live["dirty_rows"] = [" M completely different"] * len(live["dirty_rows"])
        self.assertEqual(compare(self.inv, self.facts), [])

    def test_an_unreadable_status_is_a_warning_not_a_failure(self):
        row = self.inv["worktrees"][0]
        self.facts["worktrees"][row["worktree_id"]]["status_readable"] = False
        issues = compare(self.inv, self.facts)
        self.assertEqual(codes(issues), set())
        self.assertIn("WORKTREE_STATUS_UNREADABLE", codes(issues, "WARN"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
