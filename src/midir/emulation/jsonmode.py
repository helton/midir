"""Structured output for text backends: parse the model's text as JSON (tolerating code fences) and validate it."""
from __future__ import annotations

import json
from typing import Any

from midir.emulation.parser import FENCE_RE

def validate_json_schema(value: Any, schema: dict, path: str = "$") -> list[str]:
    """Basic validation (type, required, properties, items, enum, additionalProperties).
    CAVEAT: allOf/oneOf/anyOf, pattern and format are not checked."""
    errs: list[str] = []
    typ = schema.get("type")
    types = typ if isinstance(typ, list) else ([typ] if typ else [])
    py = {"object": dict, "array": list, "string": str, "integer": int, "number": (int, float), "boolean": bool, "null": type(None)}
    if types and not any(isinstance(value, py.get(t, object)) and not (t == "integer" and isinstance(value, bool)) for t in types):
        return [f"{path}: expected {typ}, got {type(value).__name__}"]
    if "enum" in schema and value not in schema["enum"]:
        errs.append(f"{path}: value not in enum")
    if isinstance(value, dict):
        errs += [f"{path}.{k}: required property missing" for k in schema.get("required", []) if k not in value]
        for k, sub in (schema.get("properties") or {}).items():
            if k in value and isinstance(sub, dict):
                errs += validate_json_schema(value[k], sub, f"{path}.{k}")
        if schema.get("additionalProperties") is False:
            extra = sorted(set(value) - set(schema.get("properties") or {}))
            if extra:
                errs.append(f"{path}: additional properties not allowed {extra}")
    if isinstance(value, list) and isinstance(schema.get("items"), dict):
        for i, v in enumerate(value):
            errs += validate_json_schema(v, schema["items"], f"{path}[{i}]")
    return errs


def check_json(text: str, schema: dict) -> tuple[str | None, list[str]]:
    """Parse the model output as JSON (tolerating code fences) and validate it. Returns (normalized JSON, errors)."""
    try:
        value = json.loads(FENCE_RE.sub("", text.strip()))
    except json.JSONDecodeError as e:
        return None, [f"not JSON: {e}"]
    errs = validate_json_schema(value, schema) if schema else []
    return (json.dumps(value, ensure_ascii=False) if not errs else None), errs
