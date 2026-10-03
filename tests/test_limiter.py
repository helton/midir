"""Upstream queue: concurrency, requests per minute, queue timeout, pause after a 429 and the adaptive budget.
A fake clock drives time.monotonic and asyncio.sleep, so nothing here waits for real."""
from __future__ import annotations

import asyncio

import pytest

import midir.limiter as limiter_module
from conftest import Reply
from midir.config import LimitSettings
from midir.errors import QueueTimeout
from midir.limiter import UpstreamLimiter


_real_sleep = asyncio.sleep


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def monotonic(self) -> float:
        return self.now

    async def sleep(self, s: float) -> None:
        self.now += s
        await _real_sleep(0)


class _Shim:
    """The server's view of a module with some attributes replaced (the event loop keeps the real clock)."""

    def __init__(self, module, **over) -> None:
        self._m, self._over = module, over

    def __getattr__(self, name):
        return self._over[name] if name in self._over else getattr(self._m, name)


@pytest.fixture
def clock(monkeypatch) -> Clock:
    c = Clock()
    monkeypatch.setattr(limiter_module, "time", _Shim(limiter_module.time, monotonic=c.monotonic))
    monkeypatch.setattr(limiter_module, "asyncio", _Shim(limiter_module.asyncio, sleep=c.sleep))
    return c


def limiter(rpm: int = 3, conc: int = 2, timeout: int = 600, cooldown: int = 15) -> UpstreamLimiter:
    return UpstreamLimiter(LimitSettings(max_concurrent=conc, requests_per_minute=rpm, queue_timeout_s=timeout, cooldown_on_429_s=cooldown))


def test_requests_per_minute_window(clock):
    lim = limiter(rpm=3)

    async def go():
        starts = []
        for _ in range(5):
            await lim.start(clock.now + 600)
            starts.append(clock.now - 1000)
        return starts

    starts = asyncio.run(go())
    assert starts[:3] == [0, 0, 0] and starts[3] >= 60 and starts[4] >= 60


def test_queue_timeout_is_a_429_that_is_not_retried(clock):
    lim = limiter(rpm=1, timeout=10)

    async def go():
        await lim.start(clock.now + 10)
        await lim.start(clock.now + 10)

    with pytest.raises(QueueTimeout) as e:
        asyncio.run(go())
    assert e.value.status == 429 and not e.value.retryable


def test_concurrency_slots(clock):
    lim = limiter(conc=2)

    async def go():
        await lim.acquire_slot(clock.now + 5)
        await lim.acquire_slot(clock.now + 5)
        assert lim.in_flight == 2
        with pytest.raises(QueueTimeout):
            await asyncio.wait_for(lim.acquire_slot(clock.now + 0.01), 1)
        lim.release_slot()
        await lim.acquire_slot(clock.now + 5)
        assert lim.in_flight == 2 and lim.waiting == 0

    asyncio.run(go())


def test_429_pauses_new_starts(clock):
    lim = limiter(rpm=100, cooldown=15)

    async def go():
        lim.on_429()
        t = clock.now
        await lim.start(clock.now + 600)
        return clock.now - t

    assert asyncio.run(go()) >= 15


def test_429_lowers_the_budget_and_it_recovers(clock):
    """Another gateway or client on the same account shares the 100/min: after a 429 the local budget halves, then
    grows back by one request per quiet minute."""
    lim = limiter(rpm=90)
    lim.on_429()
    assert lim.effective_rpm() == 45
    lim.on_429()
    assert lim.effective_rpm() >= 10
    low = lim.effective_rpm()
    clock.now += 600  # ten quiet minutes
    assert lim.effective_rpm() == min(90, low + 10)
    clock.now += 3600
    assert lim.effective_rpm() == 90


def test_health_reports_queue_state(app_client):
    q = app_client.get("/health").json()["backends"]["stackspot"]["queue"]
    assert {"in_flight", "waiting", "starts_last_60s", "paused_s", "requests_per_minute", "effective_rpm"} <= set(q)


def test_slot_released_after_a_stop_sequence_cut(app_client, upstream, stackspot):
    for _ in range(6):  # 4 slots in the test config: a leaked slot per request would block the 5th
        upstream.add(Reply("aaa STOP bbb ccc", chunk=2))
        r = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": [{"role": "user", "content": "x"}], "stop": ["STOP"], "stream": True})
        assert r.status_code == 200
    assert stackspot.limiter.in_flight == 0
