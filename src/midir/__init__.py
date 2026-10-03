"""Midir: OpenAI- and Anthropic-compatible gateway for LLM backends.

Clients speak OpenAI Chat Completions, OpenAI Responses or Anthropic Messages; each request is translated to one
canonical form (:mod:`midir.canonical`), routed by its ``model`` to a backend (:mod:`midir.backends`) and answered in
the client's own protocol (:mod:`midir.protocols`). Backends that only take and return text (StackSpot AI agents today)
get tool calling, structured output and stop sequences emulated on top (:mod:`midir.emulation`).
"""
from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

try:
    __version__ = version("midir")
except PackageNotFoundError:  # running from a source tree that was not installed
    __version__ = "0.0.0+unknown"

__all__ = ["__version__"]
