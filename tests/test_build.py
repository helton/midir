"""Build identification: only a release reports the bare version; snapshots, local images and source checkouts carry
semver build metadata with the commit, so they are never mistaken for a release."""
from __future__ import annotations

import subprocess

import pytest

from midir import __version__
from midir.build import BuildInfo, detect


@pytest.mark.parametrize("env,full,label", [
    ({"MIDIR_BUILD_CHANNEL": "release", "MIDIR_BUILD_COMMIT": "a817822f00d"}, __version__, ""),
    ({"MIDIR_BUILD_CHANNEL": "dev", "MIDIR_BUILD_COMMIT": "a817822f00dbeef"}, f"{__version__}+dev.a817822", "development build"),
    ({"MIDIR_BUILD_CHANNEL": "local", "MIDIR_BUILD_COMMIT": "a817822-dirty"}, f"{__version__}+local.a817822.dirty", "local build"),
    ({"MIDIR_BUILD_CHANNEL": "dev"}, f"{__version__}+dev", "development build"),
])
def test_baked_build_info(env, full, label):
    b = detect(env)
    assert b.full_version == full and b.label == label


def _git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


def test_source_checkout_reads_git(tmp_path):
    _git(tmp_path, "init", "-q")
    (tmp_path / "f.txt").write_text("x")
    _git(tmp_path, "add", "f.txt")
    _git(tmp_path, "-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "x")
    clean = detect({}, tmp_path)
    assert clean.channel == "source" and len(clean.commit) == 7 and not clean.dirty
    assert clean.full_version == f"{__version__}+src.{clean.commit}"
    (tmp_path / "f.txt").write_text("changed")
    assert detect({}, tmp_path).full_version.endswith(".dirty")


def test_no_git_and_nothing_baked_is_unknown(tmp_path):
    b = detect({}, tmp_path)
    assert b.channel == "unknown" and b.full_version == f"{__version__}+unknown"


def test_health_and_ready_report_the_build(app_client):
    for path in ("/health", "/ready"):
        body = app_client.get(path).json()
        assert body["build"]["version"] == body["version"] and body["build"]["channel"] in ("release", "dev", "local", "source", "unknown")


def test_as_dict():
    assert BuildInfo("1.2.3", "dev", "abc1234", False, "2026-10-03T12:00:00Z").as_dict() == {
        "version": "1.2.3+dev.abc1234", "channel": "dev", "commit": "abc1234", "dirty": False, "date": "2026-10-03T12:00:00Z"}
