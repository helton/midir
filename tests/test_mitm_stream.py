"""The observability stack's mitmproxy must stream SSE through (docker/mitm/sse_stream.py): without the addon it buffers
whole bodies, so tokens and keepalives reach clients only when generation is over.

Opt-in (downloads mitmproxy with uvx on first use):  uv run poe test-mitm
"""
from __future__ import annotations

import os
import shutil
import subprocess
import time

import httpx
import pytest

from blackbox import HttpUpstream, ServerProcess, free_port
from conftest import MIDIR_TOML, ROOT, Reply

pytestmark = [pytest.mark.inprocess, pytest.mark.skipif(not os.environ.get("MIDIR_TEST_MITM"), reason="opt-in: uv run poe test-mitm")]

ADDON = ROOT / "docker" / "mitm" / "sse_stream.py"
KEEPALIVE = MIDIR_TOML.replace('responses_dir = "{responses_dir}"', 'responses_dir = "{responses_dir}"\nkeepalive_s = 0.2')


@pytest.fixture
def mitm_in_front(tmp_path):
    """Midir (as a process, on the HTTP stand-in) behind `mitmdump --mode reverse` with the addon."""
    if not shutil.which("uvx"):
        pytest.skip("uvx not found")
    upstream = HttpUpstream(Reply)
    server = ServerProcess("python", KEEPALIVE, upstream, tmp_path / "server")
    port = free_port()
    proxy = subprocess.Popen(["uvx", "--from", "mitmproxy>=12,<13", "mitmdump", "-q", "--mode", f"reverse:{server.url}", "-p", str(port), "-s", str(ADDON)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.monotonic() + 120  # first run downloads mitmproxy
    while time.monotonic() < deadline:
        try:
            if httpx.get(f"http://127.0.0.1:{port}/health", timeout=1).status_code == 200:
                break
        except httpx.HTTPError:
            time.sleep(0.2)
    else:
        proxy.kill()
        pytest.fail("mitmdump did not start: " + (proxy.stderr.read() or b"").decode()[-2000:])
    yield upstream, f"http://127.0.0.1:{port}"
    proxy.terminate()
    proxy.wait(10)
    server.stop()
    upstream.close()


def arrival_times(url: str, path: str, body: dict) -> tuple[float | None, float | None]:
    """(first keepalive, first content) arrival times in seconds, as the client sees them."""
    t0, keepalive, content = time.monotonic(), None, None
    with httpx.stream("POST", url + path, json=body, timeout=60) as r:
        for line in r.iter_lines():
            now = time.monotonic() - t0
            if keepalive is None and (line.startswith(": keepalive") or line == "event: ping"):
                keepalive = now
            if content is None and ("Hello" in line):
                content = now
    return keepalive, content


@pytest.mark.parametrize("path,body", [
    ("/v1/chat/completions", {"model": "gpt-5.1", "stream": True, "messages": [{"role": "user", "content": "hi"}]}),
    ("/v1/responses", {"model": "gpt-5.1", "stream": True, "input": "hi"}),
])
def test_keepalive_reaches_the_client_through_mitm_before_content(mitm_in_front, path, body):
    upstream, url = mitm_in_front
    upstream.add(Reply("Hello there.", delay=2.0))
    keepalive, content = arrival_times(url, path, body)
    assert keepalive is not None and content is not None
    assert keepalive < 1.0 and content > 1.8  # buffered, both would arrive together at the end
