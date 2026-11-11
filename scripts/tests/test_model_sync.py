"""Self-tests for scripts/check-model-sync.py (both checks) and the
repository's own trace event mapping."""
import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("check_model_sync", ROOT / "scripts" / "check-model-sync.py")
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


def run(*argv, env=None):
    out = io.StringIO()
    environ = {k: v for k, v in os.environ.items() if not k.startswith("GITHUB_")}
    environ.update(env or {})
    with patch.dict(os.environ, environ, clear=True), contextlib.redirect_stdout(out):
        code = sync.main(list(argv))
    return code, out.getvalue()


class RepoCopy(unittest.TestCase):
    def copy(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        shutil.copytree(ROOT / "concurrency" / "src", tmp / "concurrency" / "src")
        (tmp / "concurrency" / "tests").mkdir()
        shutil.copy(ROOT / "concurrency" / "tests" / "cohort_trace.rs", tmp / "concurrency" / "tests")
        shutil.copytree(ROOT / "verification", tmp / "verification")
        return tmp


class TraceEvents(RepoCopy):
    def test_repository_events_are_mapped(self):
        code, out = run("--root", str(ROOT), "events")
        self.assertEqual(code, 0, out)

    def inject(self, tmp, code):
        path = tmp / "concurrency" / "src" / "supervision.rs"
        path.write_text(path.read_text() + "\nfn injected() {\n" + code + "\n}\n")

    def test_unmapped_event_fails(self):
        tmp = self.copy()
        self.inject(tmp, 'let _ = Event::new(\n    "BogusStep", 1, "");')
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 1, out)
        self.assertIn("event `BogusStep`", out)

    def test_unmapped_event_in_a_branch_fails(self):
        tmp = self.copy()
        self.inject(tmp, 'let _ = Event::new(if x { "Close" } else { "BogusBranch" }, 0, "");')
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 1, out)
        self.assertIn("event `BogusBranch`", out)

    def test_non_literal_action_fails(self):
        tmp = self.copy()
        self.inject(tmp, "let _ = Event::new(name, 0, \"\");")
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 1, out)
        self.assertIn("cannot tell which action", out)

    def test_comments_and_strings_are_not_events(self):
        tmp = self.copy()
        self.inject(tmp, '// Event::new("InComment", 0, "")\nlet _ = "Event::new(\\"InString\\", 0, \\"\\")";')
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 0, out)

    def test_unconfigured_event_struct_fails(self):
        tmp = self.copy()
        path = tmp / "concurrency" / "src" / "lifecycle_trace.rs"
        path.write_text(path.read_text() + "\npub struct NewEvent { pub action: &'static str }\n")
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 1, out)
        self.assertIn("`NewEvent`", out)

    def test_cohort_filter_drift_fails(self):
        tmp = self.copy()
        path = tmp / "concurrency" / "src" / "lifecycle_trace.rs"
        path.write_text(path.read_text().replace('"CDone",\n', "", 1))
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 1, out)
        self.assertIn("missing ['CDone']", out)

    def test_spec_arm_without_emitter_warns(self):
        tmp = self.copy()
        path = tmp / "verification" / "tla" / "TraceNested.tla"
        path.write_text(path.read_text().replace(' [] OTHER -> FALSE', ' [] e.a = "TGhost" -> FALSE\n [] OTHER -> FALSE'))
        code, out = run("--root", str(tmp), "events")
        self.assertEqual(code, 0, out)
        self.assertIn("replays `TGhost`, which no code emits", out)


class ModelInventory(RepoCopy):
    def test_repository_models_pass(self):
        code, out = run("--root", str(ROOT), "models")
        self.assertEqual(code, 0, out)
        self.assertIn("TLC-only models", out)

    def edit_map(self, tmp, old, new):
        path = tmp / "verification" / "model-map.toml"
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def test_unmapped_model_fails(self):
        tmp = self.copy()
        (tmp / "verification" / "tla" / "Brand.tla").write_text("---- MODULE Brand ----\n====\n")
        code, out = run("--root", str(tmp), "models")
        self.assertEqual(code, 1, out)
        self.assertIn("model Brand has no [models.Brand] entry", out)

    def test_tlc_only_model_without_allowlist_fails(self):
        tmp = self.copy()
        self.edit_map(tmp, "[models.Coordinator.tlc_only]", "[models.Coordinator.not_allowlisted]")
        code, out = run("--root", str(tmp), "models")
        self.assertEqual(code, 1, out)
        self.assertIn("Coordinator has no trace validation", out)

    def test_allowlist_needs_tracking(self):
        tmp = self.copy()
        self.edit_map(tmp, 'tracking = "work item to be filed', 'untracked = "work item to be filed')
        code, out = run("--root", str(tmp), "models")
        self.assertEqual(code, 1, out)
        self.assertIn("needs both `reason` and `tracking`", out)

    def test_unrun_trace_spec_is_not_validation(self):
        tmp = self.copy()
        path = tmp / "verification" / "tla" / "trace_check.py"
        path.write_text(path.read_text().replace("TraceNested.tla", "TraceTree.tla"))
        code, out = run("--root", str(tmp), "models")
        self.assertEqual(code, 1, out)
        self.assertIn("NestedSupervision has no trace validation", out)

    def test_coverage_below_floor_fails(self):
        tmp = self.copy()
        path = tmp / "verification" / "tla" / "coverage-baseline.json"
        data = json.loads(path.read_text())
        data["Supervision"] = data["Supervision"][1:]
        path.write_text(json.dumps(data))
        code, out = run("--root", str(tmp), "models")
        self.assertEqual(code, 1, out)
        self.assertIn("below the floor 36", out)


ACTOR_CLEANUP = """//! Shared terminal publication.
pub fn publish(x: u32) -> u32 {
    x + 1
}

#[cfg(test)]
mod tests {
    #[test]
    fn publishes() {
        assert_eq!(super::publish(1), 2);
    }
}
"""


class ModelPaths(unittest.TestCase):
    def setUp(self):
        self.repo = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.repo)
        (self.repo / "verification" / "tla").mkdir(parents=True)
        (self.repo / "concurrency" / "src").mkdir(parents=True)
        shutil.copy(ROOT / "verification" / "model-map.toml", self.repo / "verification")
        self.write("verification/tla/Lifecycle.tla", "---- MODULE Lifecycle ----\n====\n")
        self.write("concurrency/src/actor_cleanup.rs", ACTOR_CLEANUP)
        self.write("concurrency/src/registry.rs", "pub fn lookup() {}\n")
        self.git("init", "-q")
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True, text=True).stdout

    def write(self, rel, text):
        (self.repo / rel).write_text(text)

    def edit(self, rel, old, new):
        path = self.repo / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def check(self, labels="", body="", env=None):
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "change", "--allow-empty")
        return run("--root", str(self.repo), "paths", "--base", self.base, "--labels", labels, "--body", body, env=env)

    def test_code_change_without_model_fails(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("MISSING concurrency/src/actor_cleanup.rs: covered by Lifecycle", out)

    def test_code_change_with_model_passes(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        self.edit("verification/tla/Lifecycle.tla", "====", "\\* updated\n====")
        code, out = self.check()
        self.assertEqual(code, 0, out)

    def test_label_and_reason_waive(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check("no-model-change", "Refactor.\n\nModel: no change because the publication order is unchanged.\n")
        self.assertEqual(code, 0, out)
        self.assertIn("waived", out)

    def test_label_alone_does_not_waive(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check("no-model-change", "Refactor.")
        self.assertEqual(code, 1, out)

    def test_reason_alone_does_not_waive(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check("", "Model: no change because nothing changed.")
        self.assertEqual(code, 1, out)

    def test_template_placeholder_is_not_a_reason(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check("no-model-change", "Model: no change because <reason>")
        self.assertEqual(code, 1, out)

    def test_comment_only_change_passes(self):
        self.edit("concurrency/src/actor_cleanup.rs", "//! Shared terminal publication.", "//! Shared terminal publication,\n//! for futures and threads.")
        code, out = self.check()
        self.assertEqual(code, 0, out)
        self.assertIn("comments only", out)

    def test_test_module_change_passes(self):
        self.edit("concurrency/src/actor_cleanup.rs", "assert_eq!(super::publish(1), 2);", "assert_eq!(super::publish(1), 2);\n        assert_eq!(super::publish(2), 3);")
        code, out = self.check()
        self.assertEqual(code, 0, out)
        self.assertIn("tests only", out)

    def test_code_and_test_change_fails(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        self.edit("concurrency/src/actor_cleanup.rs", "publish(1), 2", "publish(1), 3")
        code, out = self.check()
        self.assertEqual(code, 1, out)

    def test_unmapped_code_passes(self):
        self.edit("concurrency/src/registry.rs", "{}", "{ let _ = 1; }")
        code, out = self.check()
        self.assertEqual(code, 0, out)

    def test_merge_queue_passes(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        code, out = self.check(env={"GITHUB_EVENT_NAME": "merge_group"})
        self.assertEqual(code, 0, out)

    def test_actions_event_payload(self):
        self.edit("concurrency/src/actor_cleanup.rs", "x + 1", "x + 2")
        event = self.repo / "event.json"
        event.write_text('{"pull_request": {"labels": [{"name": "no-model-change"}], "body": "Model: no change because tests."}}')
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "change")
        code, out = run("--root", str(self.repo), "paths", "--base", self.base,
                        env={"GITHUB_EVENT_NAME": "pull_request", "GITHUB_EVENT_PATH": str(event)})
        self.assertEqual(code, 0, out)


class TomlSubset(unittest.TestCase):
    def test_fallback_parser_matches_tomllib(self):
        try:
            import tomllib
        except ImportError:
            self.skipTest("tomllib needs Python 3.11")
        text = (ROOT / "verification" / "model-map.toml").read_text()
        self.assertEqual(sync.parse_toml(text), tomllib.loads(text))


if __name__ == "__main__":
    unittest.main()
