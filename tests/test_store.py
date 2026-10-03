"""Responses store (previous_response_id): chains on disk survive a restart, blobs are shared, permissions are
owner-only, purge by age and by size, and broken chains answer 404 instead of a wrong history."""
from __future__ import annotations

import os
import time

import pytest

from conftest import MIDIR_TOML, RESP_TOOLS, tool_call_text


def chain(openai_client, upstream, n: int = 3) -> list[str]:
    ids, prev = [], None
    for i in range(n):
        upstream.add(f"answer {i}")
        kw = {"previous_response_id": prev} if prev else {"instructions": "be brief", "tools": RESP_TOOLS}
        r = openai_client.responses.create(model="gpt-5.1", input=f"question {i}", **kw)
        ids.append(r.id)
        prev = r.id
    return ids


def test_chain_survives_a_restart(openai_client, upstream, gateway):
    ids = chain(openai_client, upstream)
    gateway.store.memory.clear()  # what a restart does to memory
    upstream.add("final")
    openai_client.responses.create(model="gpt-5.1", previous_response_id=ids[-1], input="question 3")
    p = upstream.prompts[-1]
    for i in range(3):
        assert f"question {i}" in p and f"answer {i}" in p
    assert "be brief" in p and '"read_file"' in p  # instructions and tools inherited through the chain


def test_files_are_owner_only_and_blobs_shared(openai_client, upstream, gateway):
    chain(openai_client, upstream)
    files = list(gateway.store.dir.glob("resp_*.json"))
    blobs = list((gateway.store.dir / "blobs").glob("*.json"))
    assert len(files) == 3 and len(blobs) == 2  # one system blob, one tools blob for the whole chain
    assert oct(gateway.store.dir.stat().st_mode & 0o777) == "0o700"
    assert all(oct(f.stat().st_mode & 0o777) == "0o600" for f in files + blobs)


def test_new_instructions_and_tools_replace_the_inherited_ones(openai_client, upstream, gateway):
    ids = chain(openai_client, upstream, 1)
    upstream.add("x")
    openai_client.responses.create(model="gpt-5.1", previous_response_id=ids[0], input="q", instructions="NEW RULES", tools=[])
    p = upstream.prompts[-1]
    assert "NEW RULES" in p and "be brief" not in p and "# Tools" not in p


def test_tool_call_round_trip_through_the_store(openai_client, upstream, gateway):
    upstream.add(tool_call_text("read_file", {"path": "a.py"}), "done")
    r1 = openai_client.responses.create(model="gpt-5.1", input="read a.py", tools=RESP_TOOLS)
    gateway.store.memory.clear()
    call = r1.output[-1]
    openai_client.responses.create(model="gpt-5.1", previous_response_id=r1.id, input=[{"type": "function_call_output", "call_id": call.call_id, "output": "print(1)"}])
    p = upstream.prompts[-1]
    assert f'<tool_call id="{call.call_id}">' in p and f'<tool_result id="{call.call_id}" name="read_file">' in p


def test_store_false_is_not_kept(openai_client, upstream, gateway):
    upstream.add("x")
    r = openai_client.responses.create(model="gpt-5.1", input="x", store=False)
    assert r.id not in gateway.store.memory and not list(gateway.store.dir.glob(f"{r.id}.json"))


def test_purge_by_age_breaks_the_chain_with_a_404(openai_client, upstream, gateway):
    import openai
    ids = chain(openai_client, upstream)
    old = time.time() - gateway.store.retention_s - 10
    os.utime(gateway.store.dir / f"{ids[0]}.json", (old, old))
    assert gateway.store.purge() >= 1
    gateway.store.memory.clear()
    with pytest.raises(openai.NotFoundError):
        openai_client.responses.create(model="gpt-5.1", previous_response_id=ids[-1], input="next")


def test_purge_by_size_removes_the_oldest(openai_client, upstream, gateway, monkeypatch):
    ids = chain(openai_client, upstream, 5)
    for k, rid in enumerate(ids):
        t = time.time() - 1000 + k
        os.utime(gateway.store.dir / f"{rid}.json", (t, t))
    total = sum(f.stat().st_size for f in gateway.store.dir.rglob("*.json"))
    monkeypatch.setattr(gateway.store, "max_bytes", total - 1)
    gateway.store.purge()
    left = {f.stem for f in gateway.store.dir.glob("resp_*.json")}
    assert ids[0] not in left and ids[-1] in left


def test_corrupt_file_is_a_404_not_a_500(openai_client, upstream, gateway):
    import openai
    ids = chain(openai_client, upstream, 2)
    (gateway.store.dir / f"{ids[0]}.json").write_text("{broken")
    gateway.store.memory.clear()
    with pytest.raises(openai.NotFoundError):
        openai_client.responses.create(model="gpt-5.1", previous_response_id=ids[-1], input="next")


def test_ids_that_are_not_ours_never_touch_the_disk(openai_client):
    import openai
    for bad in ("../../etc/passwd", "resp_xyz", "resp_" + "a" * 23):
        with pytest.raises((openai.NotFoundError, openai.BadRequestError)):
            openai_client.responses.create(model="gpt-5.1", previous_response_id=bad, input="x")


def test_memory_only_mode(make_cfg):
    assert make_cfg(MIDIR_TOML.replace('responses_dir = "{responses_dir}"', 'responses_dir = ""')).server.responses_dir is None
