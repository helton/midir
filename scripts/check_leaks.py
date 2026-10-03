"""Scan every file git would publish for things that must never leave the machine: the real values of the secret
and account keys in .env,
token-shaped strings, StackSpot agent ids, home-directory paths and e-mail addresses. Exits 1 on a hit.

    uv run poe check-leaks

Inside a git repository the file list is `git ls-files --cached --others --exclude-standard` (tracked + would-be
added); outside one, the tree minus the ignored paths below. Run it before every push.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
IGNORED = (".git/", ".venv/", ".internal/", ".secret", ".env", "docker/data/", "config/midir.toml", ".pytest_cache/", "__pycache__/")
ALLOWED_EMAILS = re.compile(r"@(anthropic\.com|users\.noreply\.github\.com|example\.(com|org)|midir.local)$")
PATTERNS = {
    "GitHub token": re.compile(r"gh[opsu]_[A-Za-z0-9]{20,}"),
    "StackSpot agent id": re.compile(r"\b01[A-Z0-9]{24}\b"),
    "home path": re.compile(r"/home/[a-z][\w-]*/|/Users/[A-Za-z][\w-]*/|C:\\\\Users\\\\"),
    "secret assignment": re.compile(r"(CLIENT_SECRET|API_KEY|TOKEN|PASSWORD)[ \t]*[=:][ \t]*['\"]?[A-Za-z0-9_\-]{12,}"),
}
SENSITIVE_KEY = re.compile(r"SECRET|TOKEN|KEY|PASSWORD|CLIENT_ID|AGENT_ID|REALM|GIST", re.I)
EMAIL = re.compile(r"[\w.+-]+@[\w-]+\.[\w.-]+")


def env_values() -> dict[str, str]:
    out = {}
    for f in (ROOT / ".env",):
        for line in (f.read_text().splitlines() if f.is_file() else []):
            m = re.match(r"\s*([A-Z0-9_]+)\s*=\s*(.*)", line)
            if m:
                v = m.group(2).split(" #")[0].strip().strip("'\"")
                if len(v) >= 8 and SENSITIVE_KEY.search(m.group(1)):  # TZ, ports and the like are not secrets
                    out[m.group(1)] = v
    return out


def files() -> list[Path]:
    git = subprocess.run(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=ROOT, capture_output=True, text=True)
    if git.returncode == 0:
        return [ROOT / p for p in git.stdout.split("\0") if p]
    out = []
    for f in ROOT.rglob("*"):
        rel = f.relative_to(ROOT).as_posix()
        if f.is_file() and not any(rel == i.rstrip("/") or rel.startswith(i) or f"/{i}" in f"/{rel}" for i in IGNORED):
            out.append(f)
    return out


def main() -> int:
    secrets = env_values()
    hits = []
    scanned = 0
    for f in sorted(files()):
        try:
            text = f.read_text()
        except (UnicodeDecodeError, OSError):
            continue
        scanned += 1
        rel = f.relative_to(ROOT).as_posix()
        for name, value in secrets.items():
            if value in text:
                hits.append(f"{rel}: contains the value of {name}")
        for what, rx in PATTERNS.items():
            if m := rx.search(text):
                hits.append(f"{rel}: {what}: {m.group()[:40]!r}")
        for m in EMAIL.finditer(text):
            if not ALLOWED_EMAILS.search(m.group()) and not m.group().endswith((".py", ".md", ".json")):
                hits.append(f"{rel}: e-mail {m.group()!r}")
                break
    print(f"scanned {scanned} files, {len(hits)} finding(s)")
    for h in hits:
        print("  " + h)
    return 1 if hits else 0


if __name__ == "__main__":
    sys.exit(main())
