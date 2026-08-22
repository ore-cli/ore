#!/usr/bin/env python3
"""Apply the ore rebrand as data.

The rebrand is *generated*, never hand-merged: every user-visible string, URL and
package name that must say "ore" is described as a rule in
``fork/substitutions.yaml`` and re-applied from scratch on top of each upstream
tag.  That keeps the hand-maintained upstream diff (the ``delta`` series) small
and keeps the rebrand from ever producing a merge conflict.

Modes
-----
``--apply``   rewrite the tree; fail if an enabled rule matches nothing (a rule
              that has gone stale against upstream text is a silent brand leak).
``--check``   re-apply in memory and fail if anything would still change; this is
              the idempotence / drift gate that the assemble pipeline runs after
              the substitution pass.
``--audit``   scan for surviving brand leaks (``codex <subcommand>``, ``Codex``,
              ``@openai/codex``) outside the allowlist, with the CLI subcommand
              list re-derived from the source so upstream's new subcommands are
              covered automatically.
``--list``    print the resolved rules (documentation / debugging).

In Rust sources only string literals and comments are rewritten -- never bare
tokens -- because identifiers, crate names and wire constants must stay
byte-identical.  Format placeholders (``{name}``) inside literals are protected
so a rule can never corrupt a format string.
"""

from __future__ import annotations

import argparse
import bisect
import fnmatch
import functools
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _yamlmin  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "fork" / "substitutions.yaml"

VAR_RE = re.compile(r"\$\{([a-z0-9_]+)\}")


# --------------------------------------------------------------------------
# Rust literal / comment scanning
# --------------------------------------------------------------------------

def rust_spans(text: str) -> list[tuple[int, int, str]]:
    """Return ``(start, end, kind)`` spans of Rust string literals and comments.

    ``kind`` is ``"lit"`` for string (and raw string) literal bodies and
    ``"comment"`` for line and block comments.  Char literals, lifetimes and code
    are deliberately excluded: rewriting a bare token would rename an identifier.
    """
    spans: list[tuple[int, int, str]] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        # line comment
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            j = n if j == -1 else j
            spans.append((i + 2, j, "comment"))
            i = j
            continue
        # block comment (nesting is legal in Rust)
        if ch == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            spans.append((i + 2, max(i + 2, j - 2), "comment"))
            i = j
            continue
        # raw string: r"..." / r#"..."# / br#"..."#
        m = re.match(r'(?:b?r)(#*)"', text[i:])
        if m and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            hashes = m.group(1)
            body_start = i + m.end()
            terminator = '"' + hashes
            j = text.find(terminator, body_start)
            if j == -1:
                break
            spans.append((body_start, j, "lit"))
            i = j + len(terminator)
            continue
        # char literal or lifetime -- skip without recording
        if ch == "'":
            m = re.match(r"'(\\.|[^\\'])'", text[i:])
            if m:
                i += m.end()
                continue
            i += 1
            continue
        # normal string literal
        if ch == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            spans.append((i + 1, min(j, n), "lit"))
            i = min(j + 1, n)
            continue
        i += 1
    return spans


# Rust format placeholders are `{}`, `{0}`, `{name}`, `{name:?}`, `{:>8}` -- never
# containing spaces, quotes or commas.  A looser pattern also matches JSON objects
# embedded in raw strings, which would exempt them from the rebrand.
_PLACEHOLDER_RE = re.compile(r"\{[A-Za-z0-9_.:#?><^+\-]*\}")


def _sub_in_chunk(pattern: re.Pattern[str], repl: str, chunk: str, protect_placeholders: bool) -> tuple[str, int]:
    if not protect_placeholders:
        return pattern.subn(repl, chunk)
    out: list[str] = []
    count = 0
    last = 0
    for m in _PLACEHOLDER_RE.finditer(chunk):
        piece, c = pattern.subn(repl, chunk[last : m.start()])
        out.append(piece)
        count += c
        out.append(m.group(0))  # a format placeholder is never rewritten
        last = m.end()
    piece, c = pattern.subn(repl, chunk[last:])
    out.append(piece)
    count += c
    return "".join(out), count


def sub_in_rust_literals(pattern: re.Pattern[str], repl: str, text: str) -> tuple[str, int]:
    """Substitute only inside Rust literal/comment spans, protecting placeholders."""
    return _sub_in_spans(pattern, repl, text, rust_spans(text))


def _sub_in_spans(
    pattern: re.Pattern[str], repl: str, text: str, spans: list[tuple[int, int, str]]
) -> tuple[str, int]:
    if not spans:
        return text, 0
    out: list[str] = []
    count = 0
    cursor = 0
    for start, end, kind in spans:
        if start < cursor:  # overlapping span (e.g. comment inside skipped text)
            continue
        out.append(text[cursor:start])
        chunk, c = _sub_in_chunk(pattern, repl, text[start:end], protect_placeholders=(kind == "lit"))
        out.append(chunk)
        count += c
        cursor = end
    out.append(text[cursor:])
    return "".join(out), count


# --------------------------------------------------------------------------
# Manifest model
# --------------------------------------------------------------------------

@functools.lru_cache(maxsize=4096)
def _glob_re(pattern: str) -> re.Pattern[str]:
    """Translate a glob to a regex where ``**/`` also matches zero directories.

    Python's :mod:`fnmatch` has no globstar, so ``codex-cli/**/*.json`` would not
    match ``codex-cli/package.json`` -- a silent way to lose rules.
    """
    out = ["(?s:"]
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:[^/]+/)*")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    out.append(r")\Z")
    return re.compile("".join(out))


def glob_match(path: str, pattern: str) -> bool:
    return bool(_glob_re(pattern).match(path))


@dataclass
class Rule:
    name: str
    category: str
    pattern: re.Pattern[str]
    replace: str
    files: list[str] = field(default_factory=list)
    globs: list[str] = field(default_factory=list)
    exclude: list[str] = field(default_factory=list)
    rust_literals_only: bool | None = None
    optional: bool = False
    note: str = ""

    def targets(self, path: str) -> bool:
        if self.files:
            if path not in self.files:
                return False
        elif self.globs:
            if not any(glob_match(path, g) for g in self.globs):
                return False
        for pat in self.exclude:
            if glob_match(path, pat) or path.startswith(pat.rstrip("*")):
                return False
        return True


@dataclass
class Engine:
    skip_dirs: set[str]
    skip_files: set[str]
    skip_paths: list[str]
    skip_suffixes: tuple[str, ...]
    rust_literals_only_default: bool


@dataclass
class Manifest:
    vars: dict[str, str]
    engine: Engine
    rules: list[Rule]
    path_moves: list[tuple[str, str]]
    allowlist: dict
    audit: dict


def _interp(value: str, vars_: dict[str, str]) -> str:
    def repl(m: re.Match[str]) -> str:
        key = m.group(1)
        if key not in vars_:
            raise SystemExit(f"substitutions.yaml: unknown var ${{{key}}}")
        return vars_[key]

    return VAR_RE.sub(repl, value)


def load_manifest(path: Path = MANIFEST) -> Manifest:
    doc = _yamlmin.safe_load(path.read_text(encoding="utf-8"))
    raw_vars = doc.get("vars") or {}
    vars_: dict[str, str] = {}
    for key, value in raw_vars.items():
        vars_[key] = _interp(str(value), vars_)
    eng = doc.get("engine") or {}
    engine = Engine(
        skip_dirs=set(eng.get("skip_dirs") or []),
        skip_files=set(eng.get("skip_files") or []),
        skip_paths=list(eng.get("skip_paths") or []),
        skip_suffixes=tuple(eng.get("skip_suffixes") or []),
        rust_literals_only_default=bool(eng.get("rust_literals_only", True)),
    )
    rules: list[Rule] = []
    for category, entries in (doc.get("categories") or {}).items():
        for entry in entries or []:
            pattern = _interp(str(entry["pattern"]), vars_)
            rules.append(
                Rule(
                    name=str(entry["name"]),
                    category=str(category),
                    pattern=re.compile(pattern),
                    replace=_interp(str(entry.get("replace", "")), vars_),
                    files=[_interp(str(f), vars_) for f in (entry.get("files") or [])],
                    globs=[_interp(str(g), vars_) for g in (entry.get("globs") or [])],
                    exclude=[_interp(str(g), vars_) for g in (entry.get("exclude") or [])],
                    rust_literals_only=entry.get("rust_literals_only"),
                    optional=bool(entry.get("optional", False)),
                    note=str(entry.get("note", "")),
                )
            )
    moves = [(str(a), _interp(str(b), vars_)) for a, b in (doc.get("path_moves") or [])]
    return Manifest(
        vars=vars_,
        engine=engine,
        rules=rules,
        path_moves=moves,
        allowlist=doc.get("allowlist") or {},
        audit=doc.get("audit") or {},
    )


# --------------------------------------------------------------------------
# File walking
# --------------------------------------------------------------------------

def iter_files(root: Path, engine: Engine) -> list[str]:
    """Walk the tree, pruning skipped directories instead of filtering after the fact."""
    out: list[str] = []
    root_str = str(root)
    for dirpath, dirnames, filenames in os.walk(root_str):
        rel_dir = os.path.relpath(dirpath, root_str)
        rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
        dirnames[:] = sorted(
            d
            for d in dirnames
            if d not in engine.skip_dirs
            and not _skipped(f"{rel_dir}/{d}" if rel_dir else d, engine.skip_paths)
        )
        for name in sorted(filenames):
            if name in engine.skip_files:
                continue
            rel = f"{rel_dir}/{name}" if rel_dir else name
            if rel.endswith(engine.skip_suffixes):
                continue
            if _skipped(rel, engine.skip_paths):
                continue
            path = Path(dirpath) / name
            if path.is_symlink() or not path.is_file():
                continue
            out.append(rel)
    return out


def _skipped(rel: str, skip_paths: list[str]) -> bool:
    for pat in skip_paths:
        if glob_match(rel, pat):
            return True
        prefix = pat.rstrip("*")
        if prefix and prefix != pat and rel.startswith(prefix):
            return True
        if rel == pat:
            return True
    return False


def read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return None


# --------------------------------------------------------------------------
# Apply / check
# --------------------------------------------------------------------------

def apply_rules(root: Path, manifest: Manifest, write: bool) -> tuple[dict[str, int], dict[str, int]]:
    """Return (per-rule replacement counts, per-file change counts)."""
    counts: dict[str, int] = {rule.name: 0 for rule in manifest.rules}
    changed: dict[str, int] = {}
    files = iter_files(root, manifest.engine)
    # One cheap pass first: a file with no upstream brand token anywhere cannot be
    # matched by any rule, and skipping it avoids ~50 regex passes over the tree.
    interesting = re.compile(r"codex|Codex|openai|OpenAI|rust-v|chatgpt|ChatGPT")
    for rel in files:
        candidates = [rule for rule in manifest.rules if rule.targets(rel)]
        if not candidates:
            continue
        path = root / rel
        original = read_text(path)
        if original is None or not interesting.search(original):
            continue
        text = original
        is_rust = rel.endswith(".rs")
        spans: list[tuple[int, int, str]] | None = None
        spans_for = ""
        for rule in candidates:
            literal_only = rule.rust_literals_only
            if literal_only is None:
                literal_only = manifest.engine.rust_literals_only_default and is_rust
            if literal_only and is_rust:
                # Cheap pre-filter: no match anywhere means no match inside a literal
                # either, and it lets us skip the (expensive) span scan entirely.
                if not rule.pattern.search(text):
                    continue
                if spans is None or spans_for != text:
                    spans = rust_spans(text)
                    spans_for = text
                text, c = _sub_in_spans(rule.pattern, rule.replace, text, spans)
                if c:
                    spans = None  # offsets moved; rescan lazily on the next rule
            else:
                text, c = rule.pattern.subn(rule.replace, text)
            counts[rule.name] += c
        if text != original:
            changed[rel] = sum(1 for a, b in zip(original.splitlines(), text.splitlines()) if a != b) or 1
            if write:
                path.write_text(text, encoding="utf-8")
    return counts, changed


def apply_path_moves(root: Path, manifest: Manifest, write: bool) -> list[tuple[str, str]]:
    done: list[tuple[str, str]] = []
    for src, dst in manifest.path_moves:
        src_path = root / src
        dst_path = root / dst
        if not src_path.exists():
            continue
        done.append((src, dst))
        if not write:
            continue
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        moved = subprocess.run(
            ["git", "-C", str(root), "mv", src, dst],
            capture_output=True,
            text=True,
        )
        if moved.returncode != 0:
            src_path.replace(dst_path)
    return done


# --------------------------------------------------------------------------
# Audit
# --------------------------------------------------------------------------

def declared_subcommands(root: Path, source: str) -> list[str]:
    """Re-derive the CLI subcommand list from ``enum Subcommand`` in the source."""
    text = read_text(root / source) or ""
    match = re.search(r"enum Subcommand \{(.*?)\n\}", text, re.S)
    if not match:
        raise SystemExit(f"--audit: could not find `enum Subcommand` in {source}")
    body = match.group(1)
    names: list[str] = []
    for m in re.finditer(r'#\[(?:clap|command)\([^)]*name\s*=\s*"([^"]+)"', body):
        names.append(m.group(1))
    for m in re.finditer(r"^\s{4}([A-Z][A-Za-z0-9]*)\s*[({,]", body, re.M):
        variant = m.group(1)
        kebab = re.sub(r"(?<!^)(?=[A-Z])", "-", variant).lower()
        names.append(kebab)
    return sorted(set(names))


def audit(root: Path, manifest: Manifest) -> int:
    cfg = manifest.audit or {}
    allow_paths = [str(p) for p in (cfg.get("allow_paths") or [])]
    allow_literals = [str(p) for p in (manifest.allowlist.get("literals") or [])]
    allow_prefixes = [str(p) for p in (manifest.allowlist.get("literal_prefixes") or [])]
    allow_globs = [str(p) for p in (manifest.allowlist.get("paths") or [])]
    subcommands = declared_subcommands(root, str(cfg.get("cli_subcommands_from", "codex-rs/cli/src/main.rs")))
    leak_res = [
        ("cli-command", re.compile(r"(?<![\w./-])codex(?= (?:%s)\b)" % "|".join(map(re.escape, subcommands)))),
        ("npm-package", re.compile(r"@openai/codex")),
        ("product-name", re.compile(r"\bCodex\b")),
    ]
    findings: list[str] = []
    for rel in iter_files(root, manifest.engine):
        if any(rel.startswith(p) for p in allow_paths):
            continue
        if any(glob_match(rel, g) or rel.startswith(g.rstrip("*")) for g in allow_globs):
            continue
        text = read_text(root / rel)
        if text is None:
            continue
        # Rust identifiers are allowed to keep the upstream name; only literals and
        # comments are user-visible, so restrict the scan to those spans.
        spans = rust_spans(text) if rel.endswith(".rs") else None
        line_starts = [0]
        for m in re.finditer(r"\n", text):
            line_starts.append(m.end())
        for kind, rx in leak_res:
            for m in rx.finditer(text):
                offset = m.start()
                if spans is not None and not any(s <= offset < e for s, e, _ in spans):
                    continue
                lineno = bisect.bisect_right(line_starts, offset)
                line = text[line_starts[lineno - 1] : (line_starts[lineno] if lineno < len(line_starts) else len(text))]
                frag = line.strip()
                if any(lit in frag for lit in allow_literals):
                    continue
                if any(pfx in frag for pfx in allow_prefixes):
                    continue
                findings.append(f"{rel}:{lineno}: [{kind}] {frag[:140]}")
    if findings:
        print(f"substitute --audit: {len(findings)} brand leak(s) outside the allowlist", file=sys.stderr)
        for line in findings[:200]:
            print(f"  {line}", file=sys.stderr)
        if len(findings) > 200:
            print(f"  ... and {len(findings) - 200} more", file=sys.stderr)
        return 1
    print(f"substitute --audit: clean ({len(subcommands)} subcommands derived)")
    return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--apply", action="store_true", help="rewrite the tree")
    mode.add_argument("--check", action="store_true", help="fail if applying would still change anything")
    mode.add_argument("--audit", action="store_true", help="scan for brand leaks outside the allowlist")
    mode.add_argument("--list", action="store_true", help="print resolved rules")
    parser.add_argument("--root", default=str(REPO_ROOT), help="tree to operate on (default: repo root)")
    parser.add_argument("--manifest", default=str(MANIFEST))
    parser.add_argument("--verbose", "-v", action="store_true")
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    manifest = load_manifest(Path(args.manifest))

    if args.list:
        print(f"# vars: {manifest.vars}")
        print(f"# yaml backend: {_yamlmin.loader_name()}")
        for rule in manifest.rules:
            scope = ",".join(rule.files or rule.globs) or "<all files>"
            print(f"{rule.category}/{rule.name}: {rule.pattern.pattern!r} -> {rule.replace!r}  [{scope}]")
        for src, dst in manifest.path_moves:
            print(f"path_move: {src} -> {dst}")
        return 0

    if args.audit:
        return audit(root, manifest)

    write = args.apply
    counts, changed = apply_rules(root, manifest, write=write)
    moves = apply_path_moves(root, manifest, write=write)

    if args.check:
        if changed or moves:
            print("substitute --check: tree is NOT idempotent; these would still change:", file=sys.stderr)
            for rel in sorted(changed)[:100]:
                print(f"  {rel}", file=sys.stderr)
            for src, dst in moves:
                print(f"  move {src} -> {dst}", file=sys.stderr)
            return 1
        print("substitute --check: clean (no further changes)")
        return 0

    stale = [r.name for r in manifest.rules if counts[r.name] == 0 and not r.optional]
    total = sum(counts.values())
    print(f"substitute --apply: {total} replacement(s) in {len(changed)} file(s), {len(moves)} path move(s)")
    if args.verbose:
        for rule in manifest.rules:
            print(f"  {counts[rule.name]:6d}  {rule.category}/{rule.name}")
    if stale:
        print(
            "substitute --apply: these enabled rules matched nothing (stale against upstream text):",
            file=sys.stderr,
        )
        for name in stale:
            print(f"  {name}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
