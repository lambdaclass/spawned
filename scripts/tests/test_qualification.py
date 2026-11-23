"""Self-tests of the client qualification harness (benchmarks/qualification):
the single-spawned check and the pass/fail analysis, on synthetic inputs."""
import csv
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

HARNESS = Path(__file__).resolve().parents[2] / "benchmarks" / "qualification"
sys.path.insert(0, str(HARNESS))
import analyze  # noqa: E402
import qualify  # noqa: E402

CRATES = b"/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/spawned-concurrency-%s/src/tasks/actor.rs"


class SpawnedCheck(unittest.TestCase):
    def binary(self, *paths):
        f = tempfile.NamedTemporaryFile(delete=False)
        f.write(b"\0junk\0".join(paths))
        f.close()
        self.addCleanup(os.unlink, f.name)
        return f.name

    def test_one_crates_io_copy_passes(self):
        r = qualify.check_spawned(self.binary(CRATES % b"0.8.0", CRATES % b"0.8.0"), expect="0.8.0")
        self.assertTrue(r["ok"], r)
        self.assertEqual(r["binary"], ["crates.io 0.8.0"])

    def test_hidden_second_copy_fails(self):
        # The 0.8.0 canary: ethlambda linked spawned 0.5.0 through ethrex-p2p.
        r = qualify.check_spawned(self.binary(CRATES % b"0.8.0", CRATES % b"0.5.0"))
        self.assertFalse(r["ok"])
        self.assertEqual(r["binary"], ["crates.io 0.5.0", "crates.io 0.8.0"])

    def test_git_and_path_copies(self):
        git = b"/home/u/.cargo/git/checkouts/spawned-7f29ee23d00bc2a8/433fa63/concurrency/src/lib.rs"
        path = b"/home/u/run/src/spawned/rt/src/tasks/mod.rs"
        self.assertEqual(qualify.check_spawned(self.binary(git, git))["binary"], ["git 433fa63"])
        self.assertEqual(qualify.check_spawned(self.binary(path))["binary"], ["path /home/u/run/src/spawned"])
        self.assertFalse(qualify.check_spawned(self.binary(git, path))["ok"])

    def test_wrong_expected_version_fails(self):
        self.assertFalse(qualify.check_spawned(self.binary(CRATES % b"0.8.0"), expect="0.9.0")["ok"])


class Analysis(unittest.TestCase):
    """A 4 h paired run: soak from t=1800, stop at 4 h, 30 s samples."""

    def make_run(self, cand_rss_slope=0.0, cand_fd_slope=0.0, rejected=0, panic_line=False,
                 timed_out=False, outcome="complete"):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(d))
        cfg = dict(qualify.DEFAULTS, CLIENT="ethlambda", NETWORK="mainnet", PAIRED="1", PRESET="")
        (d / "config.json").write_text(json.dumps(cfg))
        (d / "manifest.json").write_text(json.dumps({"labels": {"candidate": {"spawned": {
            "binary": ["crates.io 0.8.0"], "ok": True, "problems": []}}}}))
        (d / "run").mkdir()
        (d / "run/outcome").write_text(outcome + "\n")
        R = d / "results"
        (R / "logs").mkdir(parents=True)
        t0 = 1_800_000_000
        events = [{"epoch": t0, "phase": "start"}, {"epoch": t0 + 1800, "phase": "soak"},
                  {"epoch": t0 + 4 * 3600, "phase": "stopping"}]
        (R / "events.jsonl").write_text("".join(json.dumps(e) + "\n" for e in events))
        with open(R / "samples.csv", "w", newline="") as f:
            w = csv.DictWriter(f, ["epoch", "label", "pid", "cpu_pct", "rss_mb", "fds", "threads", "api_ok",
                                   "head", "sync_distance", "peers", "sp_messages_handled_total",
                                   "sp_messages_rejected_total", "sp_requests_timed_out_total",
                                   "sp_handlers_panicked_total", "sp_actors_active", "sp_mailbox_depth"])
            w.writeheader()
            for i in range(0, 4 * 3600, 30):
                h = i / 3600
                for label, pid in (("control", "10"), ("candidate", "20")):
                    cand = label == "candidate"
                    w.writerow({"epoch": t0 + i, "label": label, "pid": pid, "cpu_pct": 20, "api_ok": 1,
                                "rss_mb": 2000 + (cand_rss_slope * h if cand else 0) + (i % 7),
                                "fds": 500 + (cand_fd_slope * h if cand else 0), "threads": 50, "head": i // 12,
                                "sync_distance": 0, "peers": 50, "sp_messages_handled_total": i * 10,
                                "sp_messages_rejected_total": rejected if cand and i > 7200 else 0,
                                "sp_requests_timed_out_total": 0, "sp_handlers_panicked_total": 0,
                                "sp_actors_active": 5, "sp_mailbox_depth": 0})
        stops = [{"utc": "x", "label": "candidate", "pid": 20, "signal": "TERM", "elapsed_s": 41 if timed_out else 0.3,
                  "timed_out": timed_out, "rc": 0}]
        (R / "shutdown.jsonl").write_text("".join(json.dumps(s) + "\n" for s in stops))
        (R / "logs/candidate.0.log").write_text(
            "INFO ok\n" + ("thread 'x' panicked at src/lib.rs:1:1:\n" if panic_line else ""))
        return d

    def verdict(self, d):
        with redirect_stdout(io.StringIO()):
            rc = analyze.Analysis(d).run()
        v = json.loads((d / "results/verdict.json").read_text())
        return rc, v["verdict"], {c["check"]: c["result"] for c in v["checks"]}

    def test_clean_run_passes(self):
        rc, verdict, checks = self.verdict(self.make_run())
        self.assertEqual((rc, verdict), (0, "PASS"), checks)
        self.assertEqual(checks["fault injection"], "SKIP")

    def test_rss_growth_against_control_fails(self):
        _, verdict, checks = self.verdict(self.make_run(cand_rss_slope=40))
        self.assertEqual(verdict, "FAIL")
        self.assertEqual(checks["RSS drift"], "FAIL")

    def test_fd_growth_fails(self):
        _, _, checks = self.verdict(self.make_run(cand_fd_slope=12))
        self.assertEqual(checks["fd slope"], "FAIL")

    def test_rejected_messages_fail(self):
        _, _, checks = self.verdict(self.make_run(rejected=3))
        self.assertEqual(checks["spawned counters"], "FAIL")

    def test_panic_and_unbounded_stop_fail(self):
        _, _, checks = self.verdict(self.make_run(panic_line=True, timed_out=True))
        self.assertEqual(checks["panics outside injected faults"], "FAIL")
        self.assertEqual(checks["bounded shutdown"], "FAIL")

    def test_early_stop_is_incomplete(self):
        rc, verdict, _ = self.verdict(self.make_run(outcome="stopped early: STOP file"))
        self.assertEqual((rc, verdict), (2, "INCOMPLETE"))


if __name__ == "__main__":
    unittest.main()
