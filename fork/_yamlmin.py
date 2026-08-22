"""Dependency-free loader for the YAML subset the fork tooling uses.

PyYAML is used when importable (GitHub runners usually have it); this module is
the fallback so `fork/` works on a bare Python 3.11+ install.

Supported: block mappings, block sequences, nested indentation, comments,
plain/single/double-quoted scalars, flow sequences of scalars (``[a, b]``),
flow mappings of scalars (``{a: b}``), block scalars (``|`` and ``>``), and the
booleans/null/int/float spellings YAML 1.1 uses in practice.

Deliberately unsupported (raises): anchors, aliases, tags, multi-document
streams, complex keys.  The fork's own data files stay inside this subset; real
GitHub workflow files are only ever parsed after being narrowed to a single
top-level block (see fork/verify/workflows_check.py).
"""

from __future__ import annotations

import re
from typing import Any

__all__ = ["load", "loader_name", "safe_load"]

_TRUE = {"true", "yes", "on"}
_FALSE = {"false", "no", "off"}
_NULL = {"", "~", "null"}
_INT_RE = re.compile(r"^[+-]?\d+$")
_FLOAT_RE = re.compile(r"^[+-]?(\d+\.\d*|\.\d+)([eE][+-]?\d+)?$")


class YamlError(ValueError):
    """Raised when the input leaves the supported subset."""


class _Line:
    __slots__ = ("indent", "text", "lineno")

    def __init__(self, indent: int, text: str, lineno: int) -> None:
        self.indent = indent
        self.text = text
        self.lineno = lineno

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return f"_Line({self.indent}, {self.text!r}, line {self.lineno})"


def _strip_comment(text: str) -> str:
    out = []
    quote: str | None = None
    i = 0
    while i < len(text):
        ch = text[i]
        if quote:
            out.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(text):
                out.append(text[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#" and (not out or out[-1] in " \t"):
            break
        else:
            out.append(ch)
        i += 1
    return "".join(out).rstrip()


def _tokenize(text: str) -> list[_Line]:
    lines: list[_Line] = []
    for lineno, raw in enumerate(text.splitlines(), start=1):
        if "\t" in raw[: len(raw) - len(raw.lstrip("\t "))]:
            raise YamlError(f"line {lineno}: tabs are not valid YAML indentation")
        stripped = _strip_comment(raw)
        if not stripped.strip():
            continue
        indent = len(stripped) - len(stripped.lstrip(" "))
        lines.append(_Line(indent, stripped.strip(), lineno))
    return lines


def _scalar(token: str) -> Any:
    token = token.strip()
    if len(token) >= 2 and token[0] == token[-1] and token[0] in "\"'":
        body = token[1:-1]
        if token[0] == '"':
            return (
                body.replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace('\\"', '"')
                .replace("\\\\", "\\")
            )
        return body.replace("''", "'")
    if token.startswith("[") and token.endswith("]"):
        return _flow_seq(token[1:-1])
    if token.startswith("{") and token.endswith("}"):
        return _flow_map(token[1:-1])
    low = token.lower()
    if low in _NULL:
        return None
    if low in _TRUE:
        return True
    if low in _FALSE:
        return False
    if _INT_RE.match(token):
        return int(token)
    if _FLOAT_RE.match(token):
        return float(token)
    return token


def _split_flow(body: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    quote: str | None = None
    current: list[str] = []
    i = 0
    while i < len(body):
        ch = body[i]
        if quote:
            current.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(body):
                current.append(body[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            current.append(ch)
        elif ch in "[{":
            depth += 1
            current.append(ch)
        elif ch in "]}":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
        i += 1
    if current:
        parts.append("".join(current))
    return [p.strip() for p in parts if p.strip()]


def _flow_seq(body: str) -> list[Any]:
    return [_scalar(part) for part in _split_flow(body)]


def _flow_map(body: str) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for part in _split_flow(body):
        key, sep, value = part.partition(":")
        if not sep:
            raise YamlError(f"flow mapping entry without ':': {part!r}")
        out[str(_scalar(key))] = _scalar(value)
    return out


def _split_key(text: str) -> tuple[str, str] | None:
    """Split ``key: value`` honouring quotes; returns None when there is no key."""
    quote: str | None = None
    i = 0
    while i < len(text):
        ch = text[i]
        if quote:
            if ch == "\\" and quote == '"':
                i += 2
                continue
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch in "[{":
            return None  # a flow collection, not a mapping key
        elif ch == ":" and (i + 1 == len(text) or text[i + 1] in " \t"):
            return text[:i].strip(), text[i + 1 :].strip()
        i += 1
    return None


def _block_scalar(lines: list[_Line], index: int, header: str, parent_indent: int) -> tuple[str, int]:
    style = header[0]
    chomp = "clip"
    if "-" in header:
        chomp = "strip"
    elif "+" in header:
        chomp = "keep"
    body: list[str] = []
    indent: int | None = None
    while index < len(lines):
        line = lines[index]
        if line.indent <= parent_indent:
            break
        if indent is None:
            indent = line.indent
        body.append(" " * (line.indent - indent) + line.text)
        index += 1
    if style == ">":
        text = " ".join(body)
    else:
        text = "\n".join(body)
    if chomp != "strip":
        text += "\n"
    return text, index


def _parse_block(lines: list[_Line], index: int, indent: int) -> tuple[Any, int]:
    if index >= len(lines):
        return None, index
    if lines[index].text.startswith("- "):
        return _parse_seq(lines, index, indent)
    if lines[index].text == "-":
        return _parse_seq(lines, index, indent)
    return _parse_map(lines, index, indent)


def _parse_seq(lines: list[_Line], index: int, min_indent: int) -> tuple[list[Any], int]:
    items: list[Any] = []
    base = lines[index].indent
    if base < min_indent:
        raise YamlError(f"line {lines[index].lineno}: sequence is indented too far left")
    while index < len(lines):
        line = lines[index]
        if line.indent < base or not (line.text == "-" or line.text.startswith("- ")):
            break
        if line.indent > base:
            raise YamlError(f"line {line.lineno}: unexpected indentation in sequence")
        rest = line.text[1:].strip()
        index += 1
        if not rest:
            child, index = _parse_block(lines, index, base + 1)
            items.append(child)
            continue
        pair = _split_key(rest)
        if pair is not None:
            # "- key: value" starts an inline mapping whose remaining keys are
            # indented to the column the key started at.
            key, value = pair
            item_indent = line.indent + 2
            mapping: dict[str, Any] = {}
            if value:
                mapping[key] = _scalar(value)
            else:
                child, index = _parse_block(lines, index, item_indent + 1)
                mapping[key] = child
            while index < len(lines) and lines[index].indent >= item_indent:
                nxt = lines[index]
                if nxt.text.startswith("- ") or nxt.text == "-":
                    break
                sub = _split_key(nxt.text)
                if sub is None:
                    raise YamlError(f"line {nxt.lineno}: expected 'key: value'")
                sub_key, sub_value = sub
                index += 1
                if sub_value.startswith("|") or sub_value.startswith(">"):
                    mapping[sub_key], index = _block_scalar(lines, index, sub_value, nxt.indent)
                elif sub_value:
                    mapping[sub_key] = _scalar(sub_value)
                else:
                    child, index = _parse_block(lines, index, nxt.indent + 1)
                    mapping[sub_key] = child
            items.append(mapping)
            continue
        items.append(_scalar(rest))
    return items, index


def _parse_map(lines: list[_Line], index: int, min_indent: int) -> tuple[dict[str, Any], int]:
    mapping: dict[str, Any] = {}
    base = lines[index].indent
    if base < min_indent:
        raise YamlError(f"line {lines[index].lineno}: mapping is indented too far left")
    while index < len(lines):
        line = lines[index]
        if line.indent < base:
            break
        if line.indent > base:
            raise YamlError(f"line {line.lineno}: unexpected indentation in mapping")
        pair = _split_key(line.text)
        if pair is None:
            raise YamlError(f"line {line.lineno}: expected 'key: value', got {line.text!r}")
        key, value = pair
        index += 1
        if value.startswith("|") or value.startswith(">"):
            mapping[key], index = _block_scalar(lines, index, value, base)
        elif value:
            mapping[key] = _scalar(value)
        elif index < len(lines) and lines[index].indent > base:
            child, index = _parse_block(lines, index, base + 1)
            mapping[key] = child
        elif index < len(lines) and lines[index].indent == base and (
            lines[index].text.startswith("- ") or lines[index].text == "-"
        ):
            child, index = _parse_seq(lines, index, base)
            mapping[key] = child
        else:
            mapping[key] = None
    return mapping, index


def load(text: str) -> Any:
    """Parse the supported YAML subset."""
    lines = _tokenize(text)
    if not lines:
        return None
    value, index = _parse_block(lines, 0, lines[0].indent)
    if index != len(lines):
        raise YamlError(f"line {lines[index].lineno}: trailing content {lines[index].text!r}")
    return value


def safe_load(text: str) -> Any:
    """Parse with this module, always.

    This used to hand off to PyYAML when it was importable, on the theory that
    the better parser should win. What that actually bought was a fork whose
    checks behave differently depending on what happens to be installed -- and
    the two parsers disagree in a way that matters here. PyYAML implements
    YAML 1.1, where `on`, `off`, `yes` and `no` are booleans, so a workflow's
    `on:` key parses as True; this module keeps it the string "on".

    The fork's Macs have no PyYAML and ubuntu-24.04's runners ship it, so the
    workflow check passed every local run and raised KeyError('on') in CI on the
    first push. A tool this repository relies on to be deterministic should not
    change behaviour because a dependency is present.
    """
    return load(text)


def loader_name() -> str:
    return "fork/_yamlmin"
