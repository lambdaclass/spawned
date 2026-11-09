"""Reject Tokio anywhere in the isolated native example's normal/build graph."""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parent.parent
command = ["cargo", "tree", "--locked", "--manifest-path", str(root / "examples/thread_only/Cargo.toml"), "--features", "metrics,async", "--edges", "normal,build", "--prefix", "none"]
tree = subprocess.check_output(command, text=True, cwd=root)
packages = {line.split()[0] for line in tree.splitlines() if line.strip()}
assert not any(name == "tokio" or name.startswith("tokio-") for name in packages), tree
print(tree)
print("PASS: no Tokio package in the synchronous-only dependency graph")
