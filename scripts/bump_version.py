"""Bump the gateway version everywhere it is written: pyproject.toml (+ uv.lock) and the default image tag in
docker/compose.yml. The server reads its version from the package metadata, so it needs no edit.

    uv run poe bump            # patch: 0.0.1 -> 0.0.2
    uv run poe bump minor      # 0.0.1 -> 0.1.0
    uv run poe bump major      # 0.0.1 -> 1.0.0
    uv run poe bump 1.0.0      # explicit version
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COMPOSE = ROOT / "docker" / "compose.yml"
IMAGE_RE = re.compile(r"(image:\s*\$\{MIDIR_IMAGE:-[^}]*?midir:)([^}\s]+)(\})")


def uv_version(*args: str) -> str:
    out = subprocess.run(["uv", "version", "--short", *args], cwd=ROOT, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(out.stderr.strip() or f"uv version {' '.join(args)} failed")
    return out.stdout.strip().split()[-1]


def main(part: str = "patch") -> None:
    old = uv_version()
    if part in ("patch", "minor", "major"):
        new = uv_version("--bump", part)
    elif re.fullmatch(r"\d+\.\d+\.\d+([.-]?[0-9A-Za-z.]+)?", part):
        new = uv_version(part)
    else:
        sys.exit(f"usage: poe bump [patch|minor|major|X.Y.Z] (got {part!r})")
    text = COMPOSE.read_text()
    if not IMAGE_RE.search(text):
        sys.exit(f"{COMPOSE}: default image tag '${{MIDIR_IMAGE:-...midir:<version>}}' not found")
    COMPOSE.write_text(IMAGE_RE.sub(lambda m: m.group(1) + new + m.group(3), text))
    print(f"{old} -> {new}: pyproject.toml, uv.lock, {COMPOSE.name}")


if __name__ == "__main__":
    main(*sys.argv[1:2])
