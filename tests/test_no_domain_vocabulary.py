"""The neutrality invariant, enforced as a test (not a review habit).

Ephemeris Kernel is a deterministic time-and-truth runtime for discrete-event
systems: flow units move through a capacitated network of nodes, lanes, and
connections. No named application domain receives special status — in code
identifiers, wire columns, test names, or docs.

The kernel additionally must not reference any downstream consumer: it is
infrastructure, and it does not know who builds on it.

Any banned term appearing anywhere in the repository (outside this file)
fails CI.
"""

from __future__ import annotations

import pathlib
import re
import subprocess

BANNED = [
    # industry vocabulary — the kernel is domain-neutral. Enforced
    # EVERYWHERE, including the README.
    "parcel", "shipment", "freight", "logistics",
    "depot", "warehouse", "courier", "consignment",
    # 'hub' is banned as a word; \bhub\b avoids false hits inside e.g.
    # 'github' — repository URLs remain expressible.
    r"\bhubs?\b",
]

CONSUMER_BANNED = [
    # consumer names — kernel CODE and internal docs must not know the
    # consumers exist (the dependency arrow points strictly downward).
    # The top-level README alone may state the one-directional
    # relationship for portfolio navigation; nothing else may.
    # (concatenated so this file does not ban itself)
    "ep" + "ure",
    r"\b" + "arenas?" + r"\b",
    "tes" + "sar",
    "cen" + "acle",
]

ALLOWED_FILES = {
    "tests/test_no_domain_vocabulary.py",
}

CONSUMER_ALLOWED_FILES = ALLOWED_FILES | {"README.md"}

SCAN_SUFFIXES = {".py", ".rs", ".toml", ".md", ".yml", ".yaml", ".json"}


def repo_root() -> pathlib.Path:
    out = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                         capture_output=True, text=True, check=True)
    return pathlib.Path(out.stdout.strip())


def tracked_files(root: pathlib.Path) -> list[pathlib.Path]:
    out = subprocess.run(["git", "ls-files"], cwd=root,
                         capture_output=True, text=True, check=True)
    return [root / line for line in out.stdout.splitlines()
            if pathlib.Path(line).suffix in SCAN_SUFFIXES]


def _scan(root, banned, allowed):
    pattern = re.compile("|".join(banned), re.IGNORECASE)
    violations: list[str] = []
    for path in tracked_files(root):
        if str(path.relative_to(root)) in allowed:
            continue
        try:
            text = path.read_text(errors="ignore")
        except OSError:
            continue
        for i, line in enumerate(text.splitlines(), 1):
            if pattern.search(line):
                violations.append(f"{path.relative_to(root)}:{i}: {line.strip()[:100]}")
    return violations


def test_no_banned_vocabulary_anywhere():
    root = repo_root()
    violations = _scan(root, BANNED, ALLOWED_FILES)
    assert not violations, (
        "banned vocabulary found:\n" + "\n".join(violations[:40])
        + (f"\n... and {len(violations) - 40} more" if len(violations) > 40 else "")
    )


def test_no_consumer_references_outside_readme():
    root = repo_root()
    violations = _scan(root, CONSUMER_BANNED, CONSUMER_ALLOWED_FILES)
    assert not violations, (
        "consumer references found outside README.md:\n"
        + "\n".join(violations[:40])
        + (f"\n... and {len(violations) - 40} more" if len(violations) > 40 else "")
    )
