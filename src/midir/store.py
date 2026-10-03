"""The Responses API state (previous_response_id): an in-memory cache plus, when a directory is configured, one small
JSON file per response holding only what that response added (parent id, new input turns, output); system prompts and
tool lists are stored once per distinct value (by content hash). A chain is rebuilt by following the parent ids, so it
survives restarts. Retention is fixed from creation, with a size cap; purged at startup and hourly. Files are
owner-only (0600, folders 0700) and not encrypted.
CAVEAT: an id older than the retention (or from another machine) answers 404 previous_response_not_found."""
from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import time
from collections import OrderedDict
from pathlib import Path
from typing import Any

from midir.canonical import CanonicalRequest, CanonicalResponse, ToolCall, ToolResult, ToolSpec, Turn

log = logging.getLogger(__name__)

Entry = tuple[float, CanonicalRequest, CanonicalResponse]  # (created, request with history, response)


class ResponseStore:
    MEMORY_MAX = 500
    ID_RE = re.compile(r"resp_[0-9a-f]{24}")

    def __init__(self, directory: Path | None = None, retention_days: float = 30, max_mb: float = 500) -> None:
        self.dir = directory
        self.retention_s = retention_days * 86400
        self.max_bytes = int(max_mb * 1024 * 1024)
        self.memory: "OrderedDict[str, Entry]" = OrderedDict()
        if self.dir:
            try:
                (self.dir / "blobs").mkdir(parents=True, exist_ok=True)
                for d in (self.dir, self.dir / "blobs"):
                    d.chmod(0o700)  # conversation content: readable by the owner only
            except OSError as e:  # e.g. the container started without its data volume: run, but say so
                log.warning("responses store: cannot use %s (%s); previous_response_id is kept in memory only (lost on restart)", self.dir, e)
                self.dir = None
                return
            log.info("responses store: %s (retention %g days from creation, max %g MB)", self.dir, retention_days, max_mb)
            self.purge()

    def describe(self) -> str:
        return f"kept {self.retention_s / 86400:g} days in {self.dir}" if self.dir else "kept in memory only; a restart forgets them"

    def session_of(self, rid: str) -> str | None:
        """The telemetry session of a stored response (memory only: sessions are labels, not persisted)."""
        hit = self.memory.get(rid)
        return hit[1].meta.get("session") if hit else None

    # ---- serialization ----
    @staticmethod
    def _turn_out(t: Turn) -> dict:
        return {"role": t.role, "text": t.text, "tool_calls": [c.__dict__ for c in t.tool_calls], "tool_results": [r.__dict__ for r in t.tool_results], "after": t.after}

    @staticmethod
    def _turn_in(d: dict) -> Turn:
        return Turn(d["role"], d["text"], [ToolCall(**c) for c in d["tool_calls"]], [ToolResult(**r) for r in d["tool_results"]], d.get("after", ""))

    def _blob(self, data: Any) -> str | None:
        """Store a JSON value once by content hash (tool lists and system prompts repeat across every turn)."""
        if not data:
            return None
        text = json.dumps(data, ensure_ascii=False, sort_keys=True)
        h = hashlib.sha256(text.encode()).hexdigest()[:32]
        f = self.dir / "blobs" / f"{h}.json"
        if f.exists():
            os.utime(f)  # keep shared blobs alive while chains use them
        else:
            self._write(f, data)
        return h

    def _unblob(self, h: str) -> Any:
        f = self.dir / "blobs" / f"{h}.json"
        os.utime(f)
        return json.loads(f.read_text())

    def _write(self, path: Path, data: Any) -> None:
        tmp = path.with_suffix(".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)  # owner-only from creation
        with os.fdopen(fd, "w") as fh:
            fh.write(json.dumps(data, ensure_ascii=False))
        tmp.replace(path)

    # ---- state ----
    def load(self, rid: str) -> tuple[float, CanonicalRequest, CanonicalResponse] | None:
        hit = self.memory.get(rid)
        if hit and time.time() - hit[0] <= self.retention_s:
            return hit
        if hit:
            self.memory.pop(rid, None)
        return self._load_disk(rid)

    def _load_disk(self, rid: str) -> tuple[float, CanonicalRequest, CanonicalResponse] | None:
        if not self.dir or not self.ID_RE.fullmatch(rid):
            return None
        chain: list[dict] = []
        cur: str | None = rid
        try:
            while cur:
                f = self.dir / f"{cur}.json"
                if not f.exists():
                    if chain:
                        log.warning("responses store: %s is missing from the chain of %s (purged?)", cur, rid)
                    return None
                rec = json.loads(f.read_text())
                chain.append(rec)
                cur = rec.get("parent")
                if len(chain) > 10000:
                    raise ValueError("chain too long")
            chain.reverse()
            req = CanonicalRequest()
            for k, rec in enumerate(chain):
                if rec.get("system"):
                    req.system = list(self._unblob(rec["system"]))
                if rec.get("tools"):
                    req.tools = [ToolSpec(**t) for t in self._unblob(rec["tools"])]
                req.turns += [self._turn_in(t) for t in rec["turns"]]
                if k < len(chain) - 1:
                    o = rec["resp"]
                    req.turns.append(Turn("assistant", o["text"], [ToolCall(**c) for c in o["tool_calls"]], []))
            o = chain[-1]["resp"]
            resp = CanonicalResponse(text=o["text"], tool_calls=[ToolCall(**c) for c in o["tool_calls"]], finish=o["finish"], usage=o["usage"])
            entry = (chain[-1]["ts"], req, resp)
            self.memory[rid] = entry
            return entry
        except Exception as e:
            log.warning("responses store: could not rebuild %s: %r", rid, e)
            return None

    def remember(self, rid: str, req: CanonicalRequest, resp: CanonicalResponse) -> None:
        now = time.time()
        self.memory[rid] = (now, req, resp)
        while len(self.memory) > self.MEMORY_MAX:
            self.memory.popitem(last=False)
        if not self.dir:
            return
        try:
            parent = req.meta.get("prev_id")
            record = {"v": 1, "id": rid, "ts": now, "parent": parent,
                      "system": self._blob(req.system), "tools": self._blob([t.__dict__ for t in req.tools]),
                      "turns": [self._turn_out(t) for t in req.turns[req.meta.get("prev_turns", 0):]],
                      "resp": {"text": resp.text, "tool_calls": [c.__dict__ for c in resp.tool_calls], "finish": resp.finish, "usage": resp.usage}}
            self._write(self.dir / f"{rid}.json", record)
        except Exception as e:
            log.warning("responses store: could not persist %s: %r", rid, e)

    def purge(self) -> int:
        """Retention is fixed from creation: response files older than responses_retention_days go first (a chain
        longer than that loses its oldest part and answers 404). Shared blobs are refreshed whenever a response uses
        them, so they go only once no recent response needs them. Then, above responses_max_mb, the oldest responses
        go until the store is back under 90% of the cap. Runs at startup and hourly."""
        if not self.dir:
            return 0
        cutoff, n = time.time() - self.retention_s, 0
        files: list[tuple[float, int, Path]] = []
        for f in list(self.dir.glob("resp_*.json")) + list((self.dir / "blobs").glob("*.json")):
            try:
                st = f.stat()
                if st.st_mtime < cutoff:
                    f.unlink(missing_ok=True)
                    n += 1
                elif f.name.startswith("resp_"):
                    files.append((st.st_mtime, st.st_size, f))
            except OSError:
                pass
        total = sum(sz for _, sz, _ in files) + sum(b.stat().st_size for b in (self.dir / "blobs").glob("*.json"))
        if total > self.max_bytes:
            for _, sz, f in sorted(files):
                if total <= self.max_bytes * 0.9:
                    break
                f.unlink(missing_ok=True)
                self.memory.pop(f.stem, None)
                total -= sz
                n += 1
            log.warning("responses store: above %d MB, removed the oldest responses", self.max_bytes // (1024 * 1024))
        if n:
            log.info("responses store: purged %d files", n)
        return n
