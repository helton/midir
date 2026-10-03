"""Emulation layer for text-only backends: tool calling, structured output, stop sequences and max_tokens built on a
backend that takes one text prompt and returns text."""
from midir.emulation.engine import EmulationEngine

__all__ = ["EmulationEngine"]
