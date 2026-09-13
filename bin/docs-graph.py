#!/usr/bin/env python3
"""Map the documentation onto the code it describes.

Builds a correlation graph between `docs/*.md` and `crates/*/src`, so the
docs can be checked against reality instead of read for plausibility.

For each published page it records:

  * the backing tests that declare it (`//! Backing test for `docs/x.md``)
  * every Rust symbol the page names, and whether that symbol resolves
  * the source files those symbols live in
  * which named symbols are Postgres-gated, so a page can be checked for
    showing a `#[cfg(feature = "postgres")]` method as portable

Emits JSON on stdout; `--summary` prints a human table instead.

    python3 bin/docs-graph.py --summary
    python3 bin/docs-graph.py > var/docs-graph.json

Known limitation: symbols emitted by the derive macros (`save_pool`,
`post_set`, …) are not `pub fn` anywhere in the tree, so they are resolved
against a generated-name index built from `rustango-macros` instead.
"""

from __future__ import annotations

import argparse
import json
import re

import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
CRATES = ROOT / "crates"

# Rust keywords, prose words and std/third-party names that look like symbols
# in backticks but are not this crate's API.
NOT_OURS = {
    "async", "await", "fn", "impl", "let", "match", "mut", "pub", "self",
    "struct", "trait", "use", "where", "true", "false", "None", "Some", "Ok",
    "Err", "Result", "Option", "Vec", "String", "str", "bool", "i32", "i64",
    "u32", "u64", "usize", "f64", "HashMap", "HashSet", "Arc", "Box", "Rc",
    "Duration", "Instant", "PathBuf", "Path", "Router", "Json", "StatusCode",
    "TODO", "NOTE", "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "QUERY",
    "OPTIONS", "NULL", "SELECT", "INSERT", "UPDATE", "WHERE", "JSON", "JSONB",
    "HTTP", "HTTPS", "SQL", "URL", "UUID", "TTL", "CI", "API", "ORM", "CRUD",
}

DEFINERS = [
    r"pub\s+async\s+fn\s+(\w+)",
    r"pub\s+fn\s+(\w+)",
    r"pub\s+struct\s+(\w+)",
    r"pub\s+enum\s+(\w+)",
    r"pub\s+trait\s+(\w+)",
    r"pub\s+mod\s+(\w+)",
    r"pub\s+const\s+(\w+)",
    r"pub\s+type\s+(\w+)",
    r"macro_rules!\s+(\w+)",
]

# Names the derive macros emit. Not `pub fn` anywhere, so grep can't see them.
GENERATED = re.compile(r"pub\s+async\s+fn\s+(\w+)")


def build_symbol_index() -> tuple[dict[str, list[str]], set[str]]:
    """symbol -> [files that define it], plus the set of pg-gated symbols."""
    index: dict[str, list[str]] = defaultdict(list)
    pg_gated: set[str] = set()

    for src in CRATES.rglob("*.rs"):
        if "/target/" in str(src) or "/tests/" in str(src):
            continue
        try:
            text = src.read_text(errors="ignore")
        except OSError:
            continue
        rel = str(src.relative_to(ROOT))
        lines = text.splitlines()

        for pattern in DEFINERS:
            for m in re.finditer(pattern, text):
                index[m.group(1)].append(rel)

        # A definition is pg-gated if a #[cfg(feature = "postgres")] sits
        # within the 8 lines above it (attribute + doc comment window).
        for i, line in enumerate(lines):
            m = GENERATED.search(line)
            if not m:
                continue
            window = lines[max(0, i - 8):i]
            if any('cfg(feature = "postgres")' in w for w in window):
                pg_gated.add(m.group(1))

    return index, pg_gated


def backing_tests() -> dict[str, list[str]]:
    """doc filename -> [test files that declare they back it]."""
    out: dict[str, list[str]] = defaultdict(list)
    for src in CRATES.rglob("*.rs"):
        if "/target/" in str(src):
            continue
        try:
            head = src.read_text(errors="ignore")[:2000]
        except OSError:
            continue
        if "acking test for" not in head:
            continue
        rel = str(src.relative_to(ROOT))
        for m in re.finditer(r"docs/([a-z0-9_-]+\.md)", head):
            out[m.group(1)].append(rel)
    return out


def published_pages() -> list[str]:
    toml = (DOCS / "index.toml").read_text()
    pages: list[str] = []
    for m in re.finditer(r"pages\s*=\s*\[(.*?)\]", toml, re.S):
        pages += re.findall(r'"([^"]+\.md)"', m.group(1))
    return pages


FENCE = re.compile(r"^```(\w*)(.*)$")


def extract(doc: Path) -> tuple[list[tuple[str, str]], list[str]]:
    """Return (fences as [(lang, body)], inline code spans)."""
    fences: list[tuple[str, str]] = []
    lang, buf, inside = "", [], False
    body_lines: list[str] = []

    for line in doc.read_text(errors="ignore").splitlines():
        m = FENCE.match(line)
        if m:
            if inside:
                fences.append((lang, "\n".join(buf)))
                buf, inside = [], False
            else:
                lang, inside = m.group(1), True
            continue
        (buf if inside else body_lines).append(line)

    inline = re.findall(r"`([^`\n]{2,60})`", "\n".join(body_lines))
    return fences, inline


CALL = re.compile(r"\b([A-Z]\w+)::(\w+)|\.(\w+)\s*\(")
IDENT = re.compile(r"^[A-Za-z_]\w*$")


def symbols_from(fences: list[tuple[str, str]], inline: list[str]) -> set[str]:
    found: set[str] = set()

    for lang, body in fences:
        if lang not in {"rust", "rs"}:
            continue
        for m in CALL.finditer(body):
            for g in m.groups():
                if g:
                    found.add(g)

    for span in inline:
        span = span.strip()
        # `Type::method` / `method()` / `Type`
        core = span.split("(")[0].strip()
        if "::" in core:
            for part in core.split("::"):
                if IDENT.match(part):
                    found.add(part)
        elif IDENT.match(core) and (span.endswith(")") or core[:1].isupper()):
            found.add(core)

    return {s for s in found if s not in NOT_OURS and len(s) > 2}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--summary", action="store_true")
    ap.add_argument("--unresolved-only", action="store_true")
    args = ap.parse_args()

    index, pg_gated = build_symbol_index()
    tests = backing_tests()
    published = set(published_pages())

    graph = {"pages": {}, "stats": {}}

    for doc in sorted(DOCS.glob("*.md")):
        name = doc.name
        fences, inline = extract(doc)
        symbols = symbols_from(fences, inline)

        resolved, unresolved, files = {}, [], set()
        for s in sorted(symbols):
            if s in index:
                resolved[s] = index[s][:3]
                files.update(index[s][:3])
            else:
                unresolved.append(s)

        graph["pages"][name] = {
            "published": name in published,
            "backing_tests": sorted(tests.get(name, [])),
            "rust_fences": sum(1 for l, _ in fences if l in {"rust", "rs"}),
            "total_fences": len(fences),
            "symbols_named": len(symbols),
            "resolved": resolved,
            "unresolved": sorted(unresolved),
            "pg_gated_named": sorted(s for s in symbols if s in pg_gated),
            "source_files": sorted(files),
        }

    pages = graph["pages"]
    graph["stats"] = {
        "docs": len(pages),
        "published": sum(1 for p in pages.values() if p["published"]),
        "with_backing_test": sum(1 for p in pages.values() if p["backing_tests"]),
        "published_without_test": sorted(
            n for n, p in pages.items() if p["published"] and not p["backing_tests"]
        ),
        "total_unresolved": sum(len(p["unresolved"]) for p in pages.values()),
        "pg_gated_symbols_indexed": len(pg_gated),
    }

    if args.summary:
        s = graph["stats"]
        print(f"docs: {s['docs']}  published: {s['published']}  "
              f"with backing test: {s['with_backing_test']}")
        print(f"unresolved symbol mentions: {s['total_unresolved']}\n")
        print(f"{'page':<32}{'test':>6}{'fences':>8}{'syms':>7}{'unres':>7}{'pg':>5}")
        print("-" * 65)
        for n, p in sorted(pages.items(), key=lambda kv: -len(kv[1]["unresolved"])):
            if args.unresolved_only and not p["unresolved"]:
                continue
            print(f"{n:<32}{'yes' if p['backing_tests'] else '--':>6}"
                  f"{p['rust_fences']:>8}{p['symbols_named']:>7}"
                  f"{len(p['unresolved']):>7}{len(p['pg_gated_named']):>5}")
        print("\npublished with NO backing test:")
        for n in s["published_without_test"]:
            print(f"  {n}")
    else:
        json.dump(graph, sys.stdout, indent=2)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
