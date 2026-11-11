#!/usr/bin/env python3
"""Check that the TLA+ models track the code (verification/model-map.toml).

    check-model-sync.py paths [--base REF] [--head REF] [--files PATH...]
                              [--labels L,...] [--body TEXT] [--pr N]
    check-model-sync.py events [--root DIR]

`paths`: fails if the PR changes a code file a model covers without changing
any of that file's models (model, MC configurations, inductive/coverage
modules or trace spec). Exempt: changes to comments only or only inside a
`#[cfg(test)] mod` block, and PRs with the `no-model-change` label AND a body
line `Model: no change because <reason>`. Labels and body come from
`GITHUB_EVENT_PATH` in Actions, `--pr N` (gh CLI) or `--labels`/`--body`.
In a merge queue (`GITHUB_EVENT_NAME=merge_group`) it passes: the PR-level
run already enforced it.

`events`: every action string the code can record in a lifecycle trace must
be replayed by a trace spec arm (or be declared translated/rejected in the
map); unmapped events, non-literal action names and unconfigured event
structs fail. Spec arms no code emits and model actions no trace reaches are
printed as warnings.

Standard library only; Python 3.9+ (a TOML subset parser is used when
`tomllib` is unavailable).
"""
import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAP = "verification/model-map.toml"
LABEL = "no-model-change"
REASON = re.compile(r"^\s*Model:\s*no change because\s+(?!<reason>)\S", re.I | re.M)


# --------------------------------------------------------------------------
# Config

def _toml_value(text, i):
    """Parse one TOML value (string, int, bool, array, inline table)."""
    def skip(i):
        while i < len(text):
            if text[i] in " \t\r\n,":
                i += 1
            elif text[i] == "#":
                while i < len(text) and text[i] != "\n":
                    i += 1
            else:
                break
        return i
    c = text[i]
    if c == "'":
        j = text.index("'", i + 1)
        return text[i + 1:j], j + 1
    if c == '"':
        j = i + 1
        out = []
        while text[j] != '"':
            if text[j] == "\\":
                out.append({"n": "\n", "t": "\t"}.get(text[j + 1], text[j + 1]))
                j += 2
            else:
                out.append(text[j])
                j += 1
        return "".join(out), j + 1
    if c == "[":
        items, i = [], skip(i + 1)
        while text[i] != "]":
            value, i = _toml_value(text, i)
            items.append(value)
            i = skip(i)
        return items, i + 1
    if c == "{":
        table, i = {}, skip(i + 1)
        while text[i] != "}":
            m = re.compile(r"\s*([A-Za-z0-9_-]+)\s*=\s*").match(text, i)
            value, i = _toml_value(text, m.end())
            table[m.group(1)] = value
            i = skip(i)
        return table, i + 1
    m = re.compile(r"true|false|-?\d+").match(text, i)
    word = m.group(0)
    return (word == "true" if word in ("true", "false") else int(word)), m.end()


def parse_toml(text):
    """The TOML subset model-map.toml uses: [a.b], [[a.b]], key = value."""
    data, i = {}, 0
    current = data
    line = re.compile(r"[ \t]*(?:#[^\n]*)?\n|[ \t]*(\[\[?)([A-Za-z0-9_.-]+)\]\]?[ \t]*(?:#[^\n]*)?(?:\n|$)|[ \t]*([A-Za-z0-9_-]+)[ \t]*=[ \t]*")
    while i < len(text):
        m = line.match(text, i)
        if not m:
            raise ValueError(f"model map: cannot parse at {text[i:i + 40]!r}")
        if m.group(2):
            parts = m.group(2).split(".")
            node = data
            for part in parts[:-1]:
                node = node.setdefault(part, {})
                if isinstance(node, list):
                    node = node[-1]
            if m.group(1) == "[[":
                node.setdefault(parts[-1], []).append({})
                current = node[parts[-1]][-1]
            else:
                current = node.setdefault(parts[-1], {})
            i = m.end()
        elif m.group(3):
            value, i = _toml_value(text, m.end())
            current[m.group(3)] = value
        else:
            i = m.end()
            if m.end() == m.start():
                i += 1
    return data


def load_map(root):
    text = (root / MAP).read_text()
    try:
        import tomllib
        return tomllib.loads(text)
    except ImportError:
        return parse_toml(text)


def glob_match(path, pattern):
    regex = ""
    k = 0
    while k < len(pattern):
        if pattern.startswith("**", k):
            regex += ".*"
            k += 2
        elif pattern[k] == "*":
            regex += "[^/]*"
            k += 1
        elif pattern[k] == "?":
            regex += "[^/]"
            k += 1
        else:
            regex += re.escape(pattern[k])
            k += 1
    return re.fullmatch(regex, path) is not None


# --------------------------------------------------------------------------
# Rust source scanning

def rust_mask(src):
    """Copy of src with comments, string and char literal contents blanked
    (same length, newlines kept), so brace and call matching ignores them.
    Returns (masked, strings) where strings maps literal start -> value."""
    out = list(src)
    strings = {}
    i, n = 0, len(src)

    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c in "rb" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")) and \
                re.match(r"b?r(#*)\"", src[i:i + 260]):
            m = re.match(r"b?r(#*)\"", src[i:])
            close = '"' + m.group(1)
            j = src.find(close, i + m.end())
            j = n if j < 0 else j
            strings[i] = src[i + m.end():j]
            blank(i + m.end(), j)
            i = j + len(close)
        elif c == '"':
            j, value = i + 1, []
            while j < n and src[j] != '"':
                if src[j] == "\\":
                    value.append(src[j:j + 2])
                    j += 2
                else:
                    value.append(src[j])
                    j += 1
            strings[i] = "".join(value)
            blank(i + 1, j)
            i = j + 1
        elif c == "'":
            if src.startswith("\\", i + 1):
                j = src.find("'", i + 3)
                blank(i + 1, j)
                i = j + 1
            elif i + 2 < n and src[i + 2] == "'":
                blank(i + 1, i + 2)
                i += 3
            else:
                i += 1  # lifetime
        else:
            i += 1
    return "".join(out), strings


def matching(masked, open_at):
    pairs = {"(": ")", "{": "}", "[": "]"}
    stack = []
    for k in range(open_at, len(masked)):
        c = masked[k]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")}]":
            if not stack or stack.pop() != c:
                return None
            if not stack:
                return k
    return None


def call_argument(masked, open_paren, index):
    """(start, end) of the index-th top-level argument of the call whose '('
    is at open_paren, or None."""
    close = matching(masked, open_paren)
    if close is None:
        return None
    depth, start, arg = 0, open_paren + 1, 0
    for k in range(open_paren + 1, close + 1):
        c = masked[k]
        if c in "([{":
            depth += 1
        elif c in ")]}" and k != close:
            depth -= 1
        elif (c == "," and depth == 0) or k == close:
            if arg == index:
                return start, k
            arg, start = arg + 1, k + 1
    return None


def test_module_lines(src):
    """1-based inclusive line ranges of `#[cfg(test)] mod name { ... }`."""
    masked, _ = rust_mask(src)
    ranges = []
    for m in re.finditer(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", masked):
        close = matching(masked, m.end() - 1)
        if close is None:
            return []  # cannot tell: no exemption
        ranges.append((masked.count("\n", 0, m.start()) + 1, masked.count("\n", 0, close) + 1))
    return ranges


# --------------------------------------------------------------------------
# paths

def git(root, *args):
    return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout


def show(root, rev, path):
    r = subprocess.run(["git", "show", f"{rev}:{path}"], cwd=root, capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else ""


def exempt_change(root, base, head, path):
    """'comments' or 'tests' if every changed line of path is a comment line
    or inside a #[cfg(test)] module (on its side of the diff), else None."""
    diff = git(root, "diff", "-U0", "--no-color", "--no-ext-diff", f"{base}...{head}", "--", path)
    old_tests = test_module_lines(show(root, base, path)) if path.endswith(".rs") else []
    new_tests = test_module_lines(show(root, head, path)) if path.endswith(".rs") else []
    within = lambda line, ranges: any(a <= line <= b for a, b in ranges)
    kinds = set()
    old_line = new_line = 0
    seen = False
    for line in diff.splitlines():
        hunk = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
        if hunk:
            old_line, new_line = int(hunk.group(1)), int(hunk.group(2))
            continue
        if line.startswith(("+++", "---", "diff ", "index ", "new file", "deleted file", "similarity", "rename", "old mode", "new mode", "Binary")):
            if line.startswith("Binary"):
                return None
            continue
        if line.startswith("\\"):
            continue
        if line[:1] not in "+-":
            continue
        seen = True
        text = line[1:].strip()
        number = new_line if line[0] == "+" else old_line
        if line[0] == "+":
            new_line += 1
        else:
            old_line += 1
        if text == "" or text.startswith("//"):
            kinds.add("comments")
        elif within(number, new_tests if line[0] == "+" else old_tests):
            kinds.add("tests")
        else:
            return None
    if not seen:
        return "comments"  # mode change or no content change
    return "tests" if "tests" in kinds else "comments"


def pr_context(args):
    """(labels, body, event_name) from flags, Actions or gh."""
    event = os.environ.get("GITHUB_EVENT_NAME", "")
    labels, body = None, None
    if args.labels is not None:
        labels = [l for l in args.labels.split(",") if l]
    if args.body is not None:
        body = args.body
    if (labels is None or body is None) and os.environ.get("GITHUB_EVENT_PATH"):
        with open(os.environ["GITHUB_EVENT_PATH"]) as f:
            pr = json.load(f).get("pull_request") or {}
        if labels is None:
            labels = [l["name"] for l in pr.get("labels", [])]
        if body is None:
            body = pr.get("body") or ""
    if (labels is None or body is None) and args.pr:
        info = json.loads(subprocess.check_output(["gh", "pr", "view", str(args.pr), "--json", "labels,body"], text=True))
        if labels is None:
            labels = [l["name"] for l in info["labels"]]
        if body is None:
            body = info["body"] or ""
    return labels or [], body or "", event


def check_paths(args, root):
    labels, body, event = pr_context(args)
    if event == "merge_group":
        print("model-sync: merge queue run; the pull_request run already enforced the check")
        return 0
    config = load_map(root)
    models = config["models"]
    if args.files is not None:
        changed, base, head = list(args.files), None, None
    else:
        head = args.head
        base = args.base or git(root, "merge-base", "origin/main", head).strip()
        changed = git(root, "diff", "--name-only", "--no-renames", f"{base}...{head}").split()
    touched = {name for name, m in models.items()
               if any(glob_match(p, g) for p in changed for g in m["files"])}
    missing = {}
    for path in changed:
        covering = sorted(name for name, m in models.items() if any(glob_match(path, g) for g in m["code"]))
        if not covering:
            continue
        if touched.intersection(covering):
            print(f"ok      {path}: model updated ({', '.join(sorted(touched.intersection(covering)))})")
            continue
        kind = exempt_change(root, base, head, path) if base else None
        if kind:
            print(f"ok      {path}: {kind} only")
            continue
        missing[path] = covering
    if not missing:
        print("model-sync: every changed modelled file has its model updated (or is exempt)")
        return 0
    has_label = LABEL in labels
    has_reason = bool(REASON.search(body))
    for path, covering in missing.items():
        print(f"{'waived' if has_label and has_reason else 'MISSING'} {path}: covered by {', '.join(covering)}, none changed")
    if has_label and has_reason:
        line = body[REASON.search(body).start():].strip().splitlines()[0]
        print(f"model-sync: waived by the `{LABEL}` label and: {line}")
        return 0
    print(f"""
model-sync: this PR changes modelled code without changing its model.
Models track the code: update the model (verification/tla/) in this PR, or,
if the behaviour the model specifies is unchanged, add the `{LABEL}` label
AND a PR body line `Model: no change because <reason>`.
Missing now: {'' if has_label else f'the `{LABEL}` label; '}{'' if has_reason else 'the `Model: no change because ...` line'}
Mapping: {MAP}""")
    return 1


# --------------------------------------------------------------------------
# events

def spec_arms(text):
    return set(re.findall(r"\be\.a\s*=\s*\"([A-Za-z0-9_]+)\"", text))


def model_actions(model_text):
    """Actions reachable from Next: expand operator references from Next,
    stopping at an operator that is not a pure disjunction (its body primes
    a variable, says UNCHANGED or has a conjunction)."""
    model_text = re.sub(r"\(\*.*?\*\)", lambda m: re.sub(r"[^\n]", " ", m.group(0)), model_text, flags=re.S)
    model_text = re.sub(r"\\\*[^\n]*", "", model_text)
    defs = {}
    for m in re.finditer(r"^([A-Za-z_]\w*)(?:\([^)]*\))?\s*==", model_text, re.M):
        defs[m.group(1)] = m.end()
    names = sorted(defs, key=defs.get)
    bodies = {}
    for k, name in enumerate(names):
        end = len(model_text)
        nxt = re.compile(r"^(?:[A-Za-z_]\w*(?:\([^)]*\))?\s*==|-{4,}|={4,}|\\\* )", re.M).search(model_text, defs[name])
        if nxt:
            end = nxt.start()
        bodies[name] = model_text[defs[name]:end]
    leaves, todo, seen = set(), ["Next"], set()
    while todo:
        name = todo.pop()
        if name in seen or name not in bodies:
            continue
        seen.add(name)
        body = bodies[name]
        if name != "Next" and ("'" in body or "UNCHANGED" in body or "/\\" in body):
            leaves.add(name)
            continue
        todo += [w for w in re.findall(r"\b[A-Za-z_]\w*\b", body) if w in bodies]
    return leaves


def check_events(args, root):
    config = load_map(root)
    trace_rs = (root / "concurrency/src/lifecycle_trace.rs").read_text()
    structs = set(re.findall(r"pub struct (\w*Event)\b", trace_rs))
    errors, warnings, notes = [], [], []
    configured = {t["struct"] for t in config.get("trace", [])}
    for s in sorted(structs - configured):
        errors.append(f"event struct `{s}` in lifecycle_trace.rs has no [[trace]] table in {MAP}")
    sources = {}
    for path in sorted((root / "concurrency/src").rglob("*.rs")):
        src = path.read_text()
        sources[path.relative_to(root).as_posix()] = (src, *rust_mask(src))
    for t in config.get("trace", []):
        name = t["struct"]
        if name not in structs:
            notes.append(f"{name}: not in lifecycle_trace.rs yet, skipped")
            continue
        emitted = {}
        for rel, (src, masked, strings) in sources.items():
            done = set()
            for e in t["emitters"]:
                for m in re.finditer(e["call"], masked):
                    if m.start() in done:
                        continue
                    done.add(m.start())
                    if re.search(r"\bfn\s+\w*$", masked[max(0, m.start() - 20):m.start()]):
                        continue
                    line = masked.count("\n", 0, m.start()) + 1
                    span = call_argument(masked, m.end() - 1, e["arg"])
                    literals = [v for k, v in strings.items() if span and span[0] <= k < span[1]]
                    if not literals or not all(re.fullmatch(r"\w+", v) for v in literals):
                        errors.append(f"{rel}:{line}: cannot tell which action `{m.group(0)}` records (use string literals)")
                        continue
                    for v in literals:
                        emitted.setdefault(v, []).append(f"{rel}:{line}")
            # A struct literal setting the action field directly bypasses the emitters.
            for m in re.finditer(r"\b%s\s*\{" % re.escape(name), masked):
                if rel == "concurrency/src/lifecycle_trace.rs" or re.search(r"(struct|impl|for)\s+$", masked[max(0, m.start() - 8):m.start()]):
                    continue
                close = matching(masked, m.end() - 1)
                body = masked[m.end():close] if close else ""
                if re.search(r"(^|[,{\s])(action|a)\s*[:,}]", body) or not close:
                    line = masked.count("\n", 0, m.start()) + 1
                    errors.append(f"{rel}:{line}: `{name} {{ .. }}` sets the action directly; build it with an emitter so the action can be checked")
        specs = {}
        for spec in t["specs"]:
            path = root / spec
            if not path.exists():
                errors.append(f"{name}: trace spec {spec} does not exist")
                continue
            specs[spec] = path.read_text()
        arms = {a: spec for spec, text in specs.items() for a in spec_arms(text)}
        rejected = t.get("rejected", {})
        translated = t.get("translated", {})
        check_py = (root / "verification/tla/trace_check.py").read_text()
        for action, sites in sorted(emitted.items()):
            if action in arms:
                continue
            if action in rejected:
                notes.append(f"{name}: `{action}` is rejected by design ({rejected[action]}); emitted at {', '.join(sites)}")
            elif action in translated:
                how = translated[action]
                into = how.get("into", []) if isinstance(how, dict) else []
                if f'"{action}"' not in check_py:
                    errors.append(f"{name}: `{action}` is declared translated but trace_check.py never mentions it ({', '.join(sites)})")
                for target in into:
                    if target not in arms:
                        errors.append(f"{name}: `{action}` is translated into `{target}`, which no spec arm replays")
            else:
                errors.append(f"{name}: event `{action}` (emitted at {', '.join(sites)}) is not mapped by {' or '.join(t['specs'])}")
        produced = set(emitted)
        for action, how in translated.items():
            if action in emitted and isinstance(how, dict):
                produced.update(how.get("into", []))
        for action in sorted(set(arms) - produced):
            warnings.append(f"{name}: {arms[action]} replays `{action}`, which no code emits")
        for f in t.get("filters", []):
            text = (root / f["file"]).read_text()
            start = re.search(f["start"], text)
            if not start:
                errors.append(f"{name}: filter start `{f['start']}` not found in {f['file']}")
                continue
            end = re.compile(f["end"]).search(text, start.end())
            listed = set(re.findall(r'"(\w+)"', text[start.end():end.start() if end else len(text)]))
            want = spec_arms(specs.get(f["spec"], ""))
            if listed != want:
                errors.append(f"{name}: the event filter in {f['file']} differs from the arms of {f['spec']}: "
                              f"missing {sorted(want - listed)}, extra {sorted(listed - want)}")
        unlogged = t.get("unlogged", {})
        for spec, text in specs.items():
            ext = re.search(r"EXTENDS\s+([\w, ]+)", text)
            model = next((m.strip() for m in ext.group(1).split(",") if (root / f"verification/tla/{m.strip()}.tla").exists()), None) if ext else None
            if not model:
                continue
            refs = set(re.findall(r"\b[A-Za-z_]\w*\b", re.sub(r"\(\*.*?\*\)|\\\*[^\n]*", "", text, flags=re.S)))
            for action in sorted(model_actions((root / f"verification/tla/{model}.tla").read_text()) - refs):
                if action in unlogged:
                    notes.append(f"{name}: {model} action `{action}` has no trace event by design ({unlogged[action]})")
                else:
                    warnings.append(f"{name}: {model} action `{action}` is never taken by {spec}: no trace can reach it")
        print(f"{name}: {len(emitted)} event kinds emitted, {len(arms)} spec arms ({', '.join(t['specs'])})")
    for n in notes:
        print(f"note    {n}")
    for w in warnings:
        print(f"WARNING {w}")
        if os.environ.get("GITHUB_ACTIONS"):
            print(f"::warning title=model-sync events::{w}")
    for e in errors:
        print(f"ERROR   {e}")
    if errors:
        print(f"\nmodel-sync events: {len(errors)} error(s). Every event the code records must be replayed by its trace spec; see {MAP}.")
        return 1
    print("model-sync events: every emitted trace event is mapped")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    paths = sub.add_parser("paths")
    paths.add_argument("--base", help="base revision (default: merge-base with origin/main)")
    paths.add_argument("--head", default="HEAD")
    paths.add_argument("--files", nargs="*", help="changed files instead of a git diff (no comment/test exemption)")
    paths.add_argument("--labels", help="comma-separated PR labels")
    paths.add_argument("--body", help="PR body")
    paths.add_argument("--pr", type=int, help="read labels and body with gh")
    events = sub.add_parser("events")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    if args.command == "paths":
        return check_paths(args, args.root)
    return check_events(args, args.root)


if __name__ == "__main__":
    sys.exit(main())
