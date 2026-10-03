"""Which build is running: a release, a development snapshot, a local image or a source checkout.

Images get MIDIR_BUILD_CHANNEL, MIDIR_BUILD_COMMIT and MIDIR_BUILD_DATE baked in at build time (docker/Dockerfile,
from the workflows or the poe deploy tasks); a source checkout asks git. Only a release reports the bare version;
everything else carries semver build metadata, so a snapshot is never mistaken for the release it came after:

    release   0.0.1
    dev       0.0.1+dev.a817822              image built from develop
    local     0.0.1+local.a817822.dirty      image built on this machine (".dirty": uncommitted changes)
    source    0.0.1+src.a817822              `uv run midir` in a git checkout
"""
from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

from midir import __version__

CHANNEL_LABELS = {"release": "", "dev": "development build", "local": "local build", "source": "running from source", "unknown": "unknown build"}
SUFFIX = {"dev": "dev", "local": "local", "source": "src", "unknown": "unknown"}


@dataclass(frozen=True)
class BuildInfo:
    version: str  # the package version (pyproject.toml)
    channel: str  # release | dev | local | source | unknown
    commit: str = ""  # short commit hash, "" when unknown
    dirty: bool = False
    date: str = ""

    @property
    def full_version(self) -> str:
        if self.channel == "release":
            return self.version
        parts = [SUFFIX.get(self.channel, self.channel)] + ([self.commit] if self.commit else []) + (["dirty"] if self.dirty else [])
        return f"{self.version}+{'.'.join(parts)}"

    @property
    def label(self) -> str:
        """Human description for the banner: empty for a release."""
        return CHANNEL_LABELS.get(self.channel, f"{self.channel} build")

    def as_dict(self) -> dict:
        return {"version": self.full_version, "channel": self.channel, "commit": self.commit or None, "dirty": self.dirty, "date": self.date or None}


def _git(*args: str, cwd: Path) -> str | None:
    try:
        r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, timeout=2)
    except (OSError, subprocess.SubprocessError):
        return None
    return r.stdout.strip() if r.returncode == 0 else None


def detect(env: dict[str, str] | None = None, source_dir: Path | None = None) -> BuildInfo:
    e = os.environ if env is None else env
    channel = (e.get("MIDIR_BUILD_CHANNEL") or "").strip().lower()
    if channel:
        commit = (e.get("MIDIR_BUILD_COMMIT") or "").strip()
        dirty = commit.endswith("-dirty")
        return BuildInfo(__version__, channel, commit.removesuffix("-dirty")[:7], dirty, (e.get("MIDIR_BUILD_DATE") or "").strip())
    here = source_dir or Path(__file__).resolve().parent
    commit = _git("rev-parse", "--short=7", "HEAD", cwd=here)
    if commit:
        return BuildInfo(__version__, "source", commit, bool(_git("status", "--porcelain", "--untracked-files=no", cwd=here)))
    return BuildInfo(__version__, "unknown")


BUILD = detect()
