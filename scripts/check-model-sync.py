#!/usr/bin/env python3
"""Check that the TLA+ models track the code (verification/model-map.toml).

    check-model-sync.py paths [--base REF] [--head REF] [--files PATH...]
                              [--labels L,...] [--body TEXT] [--pr N]
    check-model-sync.py events
    check-model-sync.py models

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

`models`: every model in verification/tla must be in the map and have trace
validation (a Trace*.tla spec that trace_check.py runs and recorded events
reach), unless it has a `tlc_only` allowlist entry with a reason and a
tracking item; and each model's covered action keys in
coverage-baseline.json may not drop below its `coverage_floor`.

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
    line = re.compile(r"[ \t]*(?:#[^\n]*)?(?:\n|$)|[ \t]*(\[\[?)([A-Za-z0-9_.-]+)\]\]?[ \t]*(?:#[^\n]*)?(?:\n|$)|[ \t]*([A-Za-z0-9_-]+)[ \t]*=[ \t]*")
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
            if m.end() == i:
                break  # end of text
            i = m.end()
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
                j = n - 1 if j < 0 else j
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


def action_values(masked, strings, a, b):
    """Action names an argument expression can evaluate to: a string literal
    or `if .. { <values> } else { <values> }` (also `else if`). None if
    anything else could decide the value (a variable, a call, a match)."""
    while a < b and masked[a].isspace():
        a += 1
    while b > a and masked[b - 1].isspace():
        b -= 1
    if a >= b:
        return None
    if masked[a] == '"' and a in strings:
        if masked.find('"', a + 1) == b - 1:
            return [strings[a]]
        return None
    if re.match(r"if\b", masked[a:b]):
        k = a + 2
        depth = 0
        while k < b:
            c = masked[k]
            if c in "([":
                depth += 1
            elif c in ")]":
                depth -= 1
            elif c == "{" and depth == 0:
                break
            k += 1
        close = matching(masked, k) if k < b else None
        if close is None or close >= b:
            return None
        then = action_values(masked, strings, k + 1, close)
        rest = re.match(r"\s*else\b\s*", masked[close + 1:b])
        if then is None or not rest:
            return None
        start = close + 1 + rest.end()
        if masked[start:start + 1] == "{":
            end = matching(masked, start)
            if end != b - 1:
                return None
            other = action_values(masked, strings, start + 1, end)
        else:
            other = action_values(masked, strings, start, b)
        return None if other is None else then + other
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


def production_lines(src):
    """Non-blank, non-`//` lines outside `#[cfg(test)] mod` blocks."""
    ranges = test_module_lines(src)
    return [l.strip() for n, l in enumerate(src.splitlines(), 1)
            if l.strip() and not l.strip().startswith("//") and not any(x <= n <= y for x, y in ranges)]


def exempt_change(root, base, head, path):
    """'comments' or 'tests' if the change leaves the file's production code
    (outside `#[cfg(test)] mod` blocks, ignoring `//` comment lines and
    blank lines) identical, else None. Removing `#[cfg(test)]` or moving
    code into a test module changes the production lines, so it is not
    exempt; an unparseable file has no test blocks (no exemption)."""
    if not path.endswith(".rs"):
        return None
    old, new = show(root, base, path), show(root, head, path)
    if production_lines(old) != production_lines(new):
        return None
    strip = lambda src: [l.strip() for l in src.splitlines() if l.strip() and not l.strip().startswith("//")]
    return "comments" if strip(old) == strip(new) else "tests"


def tla_content(src):
    """A TLA+ module without comments and blank lines."""
    src = re.sub(r"\(\*.*?\*\)", " ", src, flags=re.S)
    src = re.sub(r"\\\*[^\n]*", "", src)
    return [l.strip() for l in src.splitlines() if l.strip()]


def model_file_changed(root, base, head, path):
    """A model-side file counts as updated unless its change is comments only."""
    if base is None or not path.endswith(".tla"):
        return True
    return tla_content(show(root, base, path)) != tla_content(show(root, head, path))


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
        try:
            base = git(root, "merge-base", args.base or "origin/main", head).strip()
        except subprocess.CalledProcessError:
            print(f"model-sync: no merge base between {args.base or 'origin/main'} and {head}; fetch it or pass --base")
            return 2
        changed = git(root, "diff", "--name-only", "--no-renames", f"{base}...{head}").split()
    model_changes = [p for p in changed
                     if any(glob_match(p, g) for m in models.values() for g in m["files"])]
    for p in model_changes:
        if not model_file_changed(root, base, head, p):
            print(f"note    {p}: comments only, does not count as a model update")
    model_changes = [p for p in model_changes if model_file_changed(root, base, head, p)]
    touched = {name for name, m in models.items()
               if any(glob_match(p, g) for p in model_changes for g in m["files"])}
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
    check_py = (root / "verification/tla/trace_check.py").read_text()
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
                    literals = action_values(masked, strings, *span) if span else None
                    if not literals or not all(re.fullmatch(r"\w+", v) for v in literals):
                        errors.append(f"{rel}:{line}: cannot tell which action `{m.group(0)}` records "
                                      f"(pass a string literal or `if .. {{ \"A\" }} else {{ \"B\" }}`)")
                        continue
                    for v in literals:
                        emitted.setdefault(v, []).append(f"{rel}:{line}")
            # A struct literal setting the action field directly bypasses the emitters.
            for m in re.finditer(r"\b%s\s*\{" % re.escape(name), masked):
                before = masked[max(0, m.start() - 40):m.start()]
                if rel == "concurrency/src/lifecycle_trace.rs" or re.search(r"(struct|impl|for|->|\blet|\bif let|\bwhile let|matches!\(\s*[\w.]+\s*,)\s*(\w+::)*$", before):
                    continue
                close = matching(masked, m.end() - 1)
                if close is not None and re.match(r"\s*(=>|=[^=])", masked[close + 1:close + 4]):
                    continue  # a pattern, not a construction
                body = masked[m.end():close] if close else ""
                if re.search(r"(^|[,{\s])action\s*[:,}]", body) or not close:
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
            if not (root / f["file"]).exists():
                errors.append(f"{name}: filter file {f['file']} does not exist")
                continue
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


# --------------------------------------------------------------------------
# models

BASELINE = "verification/tla/coverage-baseline.json"


def base_models(root):
    """Models proper: verification/tla/*.tla minus MC*, Trace*, Cov*, *Ind and
    *Coverage helper modules."""
    names = []
    for path in sorted((root / "verification/tla").glob("*.tla")):
        name = path.stem
        if re.match(r"(MC|Trace|Cov)[A-Z]", name) or name.endswith(("Ind", "Coverage")):
            continue
        names.append(name)
    return names


def trace_validation(root, config, model):
    """(spec, mapped event kinds) of a trace spec that replays recorded Rust
    events against `model`, or (None, reason)."""
    check_py = (root / "verification/tla/trace_check.py").read_text()
    trace_rs = (root / "concurrency/src/lifecycle_trace.rs").read_text()
    structs = set(re.findall(r"pub struct (\w*Event)\b", trace_rs))
    sources, reasons = None, []
    for spec_path in sorted((root / "verification/tla").glob("Trace*.tla")):
        text = spec_path.read_text()
        ext = re.search(r"EXTENDS\s+([\w, ]+)", text)
        if not ext or model not in [m.strip() for m in ext.group(1).split(",")]:
            continue
        rel = spec_path.relative_to(root).as_posix()
        if spec_path.name not in check_py:
            reasons.append(f"{rel} exists but trace_check.py never runs it")
            continue
        arms = spec_arms(text)
        for t in config.get("trace", []):
            if rel not in t["specs"] or t["struct"] not in structs:
                continue
            if sources is None:
                sources = {p: rust_mask(p.read_text()) for p in (root / "concurrency/src").rglob("*.rs")}
            emitted = set()
            for masked, strings in sources.values():
                for e in t["emitters"]:
                    for m in re.finditer(e["call"], masked):
                        span = call_argument(masked, m.end() - 1, e["arg"])
                        emitted.update((action_values(masked, strings, *span) or []) if span else [])
            for action, how in t.get("translated", {}).items():
                if action in emitted and isinstance(how, dict):
                    emitted.update(how.get("into", []))
            mapped = emitted & arms
            if mapped:
                return rel, mapped
        reasons.append(f"{rel} exists but no recorded event reaches it")
    return None, "; ".join(reasons) or "no trace spec"


def check_models(args, root):
    config = load_map(root)
    models = config["models"]
    baseline_path = root / BASELINE
    baseline = json.loads(baseline_path.read_text()) if baseline_path.exists() else {}
    errors, warnings, rows, tlc_only = [], [], [], set()
    for model in base_models(root):
        entry = models.get(model)
        if entry is None:
            errors.append(f"model {model} has no [models.{model}] entry in {MAP} (which code does it cover?)")
            continue
        spec, detail = trace_validation(root, config, model)
        allow = entry.get("tlc_only")
        if spec:
            status = f"trace-validated by {spec} ({len(detail)} event kinds)"
            if allow:
                warnings.append(f"{model} has trace validation now; remove its `tlc_only` allowlist entry")
        elif allow:
            tlc_only.add(model)
            if not allow.get("reason") or not allow.get("tracking"):
                errors.append(f"{model}: a `tlc_only` entry needs both `reason` and `tracking`")
            status = f"TLC ONLY (allowlisted: {allow.get('reason', '?')}; tracking: {allow.get('tracking', '?')})"
        else:
            errors.append(f"{model} has no trace validation ({detail}): a model no real run is checked against can drift "
                          f"from the code. Add a trace spec and recorded traces, or a [models.{model}.tlc_only] "
                          f"entry with `reason` and `tracking`")
            status = f"TLC ONLY, not allowlisted ({detail})"
        floor = entry.get("coverage_floor")
        key = entry.get("coverage_key", model)
        if floor is not None:
            covered = sum(1 for k in baseline.get(key, []) if k.startswith("A|"))
            status += f"; covered action keys {covered} (floor {floor})"
            if covered < floor:
                errors.append(f"{model}: {BASELINE} covers {covered} action keys of {key}, below the floor {floor}. "
                              f"Trace coverage may only grow: restore the lost tests, or lower `coverage_floor` "
                              f"in {MAP} with a reason in the PR")
            elif covered > floor:
                warnings.append(f"{model}: coverage grew to {covered} action keys; raise `coverage_floor` to {covered}")
        elif spec and key in baseline:
            errors.append(f"{model}: trace-validated with a coverage baseline but no `coverage_floor` in {MAP}")
        rows.append(f"{model:18} {status}")
    for r in rows:
        print(r)
    for w in warnings:
        print(f"WARNING {w}")
        if os.environ.get("GITHUB_ACTIONS"):
            print(f"::warning title=model-sync models::{w}")
    for e in errors:
        print(f"ERROR   {e}")
    allowlisted = [m for m in base_models(root) if m in tlc_only]
    if allowlisted:
        print(f"\nTLC-only models (no trace validation; these can drift from the code): {', '.join(allowlisted)}")
    if errors:
        print(f"\nmodel-sync models: {len(errors)} error(s)")
        return 1
    print("model-sync models: every model is trace-validated or allowlisted, and no coverage floor is broken")
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
    sub.add_parser("events")
    sub.add_parser("models")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    if args.command == "paths":
        return check_paths(args, args.root)
    if args.command == "models":
        return check_models(args, args.root)
    return check_events(args, args.root)


if __name__ == "__main__":
    sys.exit(main())
