"""Negative controls for the narrowly declared feature-split exception."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import os
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("check_api", Path(__file__).resolve().parents[1] / "check-api.py")
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


def diagnostic(item):
    return "--- failure module_missing: module removed ---\n\nFailed in:\n  " + item + ", previously in file baseline.rs:1\n\n"


class FeatureSplitAudit(unittest.TestCase):
    expected = [["module_missing", "mod spawned_rt::tasks"]]

    def audit(self, code, output):
        result = subprocess.CompletedProcess([], code, stdout=output)
        with patch.object(api.subprocess, "run", return_value=result):
            api.declared_break(["unused"], self.expected, Path.cwd())

    def test_only_the_declared_removal_is_accepted(self):
        self.audit(100, diagnostic("mod spawned_rt::tasks"))

    def test_another_removed_module_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "unexpected"):
            self.audit(100, diagnostic("mod spawned_rt::tasks") + diagnostic("mod spawned_rt::threads"))

    def test_a_tool_failure_is_not_accepted(self):
        with self.assertRaises(RuntimeError):
            self.audit(1, "compiler failed")

    def test_an_unreported_break_is_not_accepted(self):
        with self.assertRaises(RuntimeError):
            self.audit(100, "unknown diagnostic format")

    def test_diagnostic_format_changes_fail_closed(self):
        with self.assertRaises(ValueError):
            self.audit(100, "--- failure module_missing: bad format\n")

    def test_wrong_baseline_is_rejected_before_building(self):
        root = Path(__file__).resolve().parents[2]
        with patch.object(api, "run") as build, patch.object(api.subprocess, "run") as tool:
            with self.assertRaisesRegex(ValueError, "requires baseline"):
                api.semver(root, "v0.5.0")
            build.assert_not_called()
            tool.assert_not_called()

    def test_feature_checks_reuse_paths_and_refresh_only_the_copy(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "Cargo.toml").write_text("original")
            cache = root / "target"
            locations = []

            def fake_build(*command, cwd):
                locations.append((cwd, os.environ["CARGO_TARGET_DIR"]))
                self.assertEqual((cwd / "Cargo.toml").read_text(), "original")
                (cwd / "Cargo.toml").write_text("stripped by cargo-hack")

            with patch.object(api.subprocess, "check_output", return_value=b"Cargo.toml\0"), patch.object(api, "run", side_effect=fake_build), patch.dict(os.environ):
                api.features(root, cache, True)
                api.features(root, cache, True)
            self.assertEqual(locations[0], locations[1])
            self.assertEqual(locations[0], (cache / "feature-check/source", str(cache / "feature-check/build")))
            self.assertEqual((root / "Cargo.toml").read_text(), "original")


if __name__ == "__main__":
    unittest.main()
