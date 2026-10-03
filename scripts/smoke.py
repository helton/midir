"""
smoke.py: live acceptance of a running midir through the official SDKs, in the three protocols (25 checks,
real backend requests; the prompts are in Portuguese on purpose, as the gateway's users write).

    uv run poe smoke [--base http://127.0.0.1:18880]

Per protocol: plain text, streaming, a two-round tool-calling loop, forced tool_choice, structured JSON, and
unsupported content that must not break the request. Exits 1 if any check fails.
"""
from __future__ import annotations

import argparse
import json
import sys
import time

import httpx
from anthropic import Anthropic
from openai import OpenAI

TOOLS_OAI = [
    {"type": "function", "function": {"name": "get_weather", "description": "Temperatura atual em uma cidade.", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}},
    {"type": "function", "function": {"name": "convert", "description": "Converte Celsius para Fahrenheit.", "parameters": {"type": "object", "properties": {"celsius": {"type": "number"}}, "required": ["celsius"]}}},
]
TOOLS_RESP = [{"type": "function", "name": t["function"]["name"], "description": t["function"]["description"], "parameters": t["function"]["parameters"]} for t in TOOLS_OAI]
TOOLS_ANT = [{"name": t["function"]["name"], "description": t["function"]["description"], "input_schema": t["function"]["parameters"]} for t in TOOLS_OAI]
TASK = "Qual a temperatura em Curitiba agora, em Fahrenheit? Use as ferramentas: primeiro get_weather, depois convert. Responda em uma frase."
SCHEMA = {"type": "object", "properties": {"cidade": {"type": "string"}, "graus_c": {"type": "number"}}, "required": ["cidade", "graus_c"], "additionalProperties": False}
results: list[tuple[str, bool, str]] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    results.append((name, ok, detail))
    print(f"  {'✓' if ok else '✗'} {name}" + (f"  {detail}" if detail else ""), flush=True)


def fake_tool(name: str, args: dict) -> str:
    if name == "get_weather":
        return json.dumps({"city": args.get("city"), "celsius": 18})
    if name == "convert":
        return json.dumps({"fahrenheit": args.get("celsius", 0) * 9 / 5 + 32})
    return "unknown tool"


# ----------------------------------------------------------------------------- chat completions


def test_chat(base: str) -> None:
    print("[chat completions]")
    c = OpenAI(base_url=base + "/v1", api_key="gateway")
    check("models", "gpt-5.1" in [m.id for m in c.models.list().data])
    t0 = time.perf_counter()
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "system", "content": "Responda em maiúsculas."}, {"role": "user", "content": "Diga apenas: ok"}], temperature=0)
    check("text", r.choices[0].message.content.strip().upper().startswith("OK") and r.usage.prompt_tokens > 0, f"{time.perf_counter()-t0:.1f}s {r.choices[0].message.content!r}")
    parts = []
    usage = None
    for ch in c.chat.completions.create(model="gpt-5.1", stream=True, stream_options={"include_usage": True}, messages=[{"role": "user", "content": "Liste três frutas, uma por linha, sem numeração."}]):
        if ch.choices and ch.choices[0].delta.content:
            parts.append(ch.choices[0].delta.content)
        if ch.usage:
            usage = ch.usage
    check("stream", len("".join(parts).strip().splitlines()) >= 3 and usage is not None, repr("".join(parts))[:60])
    # loop de tools
    msgs: list = [{"role": "user", "content": TASK}]
    calls_seen = []
    for _ in range(4):
        r = c.chat.completions.create(model="gpt-5.1", messages=msgs, tools=TOOLS_OAI)
        m = r.choices[0].message
        if not m.tool_calls:
            break
        msgs.append({"role": "assistant", "content": m.content, "tool_calls": [tc.model_dump() for tc in m.tool_calls]})
        for tc in m.tool_calls:
            calls_seen.append(tc.function.name)
            msgs.append({"role": "tool", "tool_call_id": tc.id, "content": fake_tool(tc.function.name, json.loads(tc.function.arguments))})
    final = r.choices[0].message.content or ""
    check("tool loop", calls_seen[:2] == ["get_weather", "convert"] and "64" in final and r.choices[0].finish_reason == "stop", f"calls={calls_seen} → {final!r}"[:120])
    # stream com tool call
    tc_names = []
    for ch in c.chat.completions.create(model="gpt-5.1", stream=True, messages=[{"role": "user", "content": "Qual a temperatura em Lisboa? Use get_weather."}], tools=TOOLS_OAI):
        if ch.choices and ch.choices[0].delta.tool_calls:
            tc_names += [t.function.name for t in ch.choices[0].delta.tool_calls]
        if ch.choices and ch.choices[0].finish_reason:
            fin = ch.choices[0].finish_reason
    check("stream tool call", tc_names == ["get_weather"] and fin == "tool_calls", f"{tc_names} finish={fin}")
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "user", "content": "Oi, tudo bem?"}], tools=TOOLS_OAI, tool_choice={"type": "function", "function": {"name": "get_weather"}})
    check("tool_choice named", bool(r.choices[0].message.tool_calls) and r.choices[0].message.tool_calls[0].function.name == "get_weather")
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "user", "content": "Qual a temperatura em Lisboa? Use get_weather."}], tools=TOOLS_OAI, tool_choice="none")
    check("tool_choice none", not r.choices[0].message.tool_calls)
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "user", "content": "Curitiba está com 18 graus. Preencha o JSON."}], response_format={"type": "json_schema", "json_schema": {"name": "clima", "schema": SCHEMA}})
    try:
        j = json.loads(r.choices[0].message.content)
        check("json_schema", j.get("cidade", "").lower().startswith("curitiba") and j.get("graus_c") == 18, r.choices[0].message.content[:80])
    except Exception as e:
        check("json_schema", False, repr(e))
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "user", "content": "Escreva 20 linhas numeradas."}], max_tokens=20)
    check("max_tokens → length", r.choices[0].finish_reason == "length", f"{len(r.choices[0].message.content)} chars")
    r = c.chat.completions.create(model="gpt-5.1", messages=[{"role": "user", "content": [{"type": "text", "text": "Se houver uma imagem nesta mensagem, responda apenas: SEM IMAGEM. Caso contrário responda: OK."}, {"type": "image_url", "image_url": {"url": "http://x/y.png"}}]}])
    check("image becomes a placeholder (no error)", "IMAGEM" in (r.choices[0].message.content or "").upper() or "OK" in (r.choices[0].message.content or "").upper(), repr(r.choices[0].message.content)[:60])


# ----------------------------------------------------------------------------- responses


def test_responses(base: str) -> None:
    print("[responses]")
    c = OpenAI(base_url=base + "/v1", api_key="gateway")
    r = c.responses.create(model="gpt-5.1", instructions="Responda em maiúsculas.", input="Diga apenas: ok")
    check("text", r.output_text.strip().upper().startswith("OK") and r.usage.input_tokens > 0, repr(r.output_text)[:40])
    deltas = []
    completed = None
    with c.responses.stream(model="gpt-5.1", input="Liste três frutas, uma por linha.") as s:
        for ev in s:
            if ev.type == "response.output_text.delta":
                deltas.append(ev.delta)
            if ev.type == "response.completed":
                completed = ev.response
    check("stream", len("".join(deltas).strip().splitlines()) >= 3 and completed is not None and completed.usage.output_tokens > 0, repr("".join(deltas))[:60])
    # loop de tools com previous_response_id
    r = c.responses.create(model="gpt-5.1", input=TASK, tools=TOOLS_RESP)
    calls_seen = []
    for _ in range(4):
        fcs = [o for o in r.output if o.type == "function_call"]
        if not fcs:
            break
        outs = []
        for fc in fcs:
            calls_seen.append(fc.name)
            outs.append({"type": "function_call_output", "call_id": fc.call_id, "output": fake_tool(fc.name, json.loads(fc.arguments))})
        r = c.responses.create(model="gpt-5.1", previous_response_id=r.id, input=outs, tools=TOOLS_RESP)
    check("tool loop + previous_response_id", calls_seen[:2] == ["get_weather", "convert"] and "64" in r.output_text, f"calls={calls_seen} → {r.output_text!r}"[:120])
    # loop de tools stateless (input completo)
    inp: list = [{"role": "user", "content": TASK}]
    r = c.responses.create(model="gpt-5.1", input=inp, tools=TOOLS_RESP, store=False)
    n = 0
    while [o for o in r.output if o.type == "function_call"] and n < 4:
        for o in r.output:
            if o.type == "function_call":
                inp.append({"type": "function_call", "call_id": o.call_id, "name": o.name, "arguments": o.arguments})
                inp.append({"type": "function_call_output", "call_id": o.call_id, "output": fake_tool(o.name, json.loads(o.arguments))})
        r = c.responses.create(model="gpt-5.1", input=inp, tools=TOOLS_RESP, store=False)
        n += 1
    check("tool loop stateless", "64" in r.output_text, repr(r.output_text)[:80])
    names = []
    with c.responses.stream(model="gpt-5.1", input="Temperatura em Lisboa? Use get_weather.", tools=TOOLS_RESP) as s:
        for ev in s:
            if ev.type == "response.output_item.done" and ev.item.type == "function_call":
                names.append(ev.item.name)
    check("stream tool call", names == ["get_weather"], str(names))
    r = c.responses.create(model="gpt-5.1", input="Curitiba está com 18 graus. Preencha o JSON.", text={"format": {"type": "json_schema", "name": "clima", "schema": SCHEMA}})
    try:
        j = json.loads(r.output_text)
        check("json_schema", j.get("graus_c") == 18, r.output_text[:80])
    except Exception as e:
        check("json_schema", False, repr(e))
    r = c.responses.create(model="gpt-5.1", input="Diga apenas: ok", tools=[{"type": "web_search_preview"}])
    check("web_search omitted (no error)", r.output_text.strip().lower().startswith("ok"), repr(r.output_text)[:40])


# ----------------------------------------------------------------------------- messages


def test_messages(base: str) -> None:
    print("[messages]")
    c = Anthropic(base_url=base, api_key="gateway")
    r = c.messages.create(model="gpt-5.1", max_tokens=200, system="Responda em maiúsculas.", messages=[{"role": "user", "content": "Diga apenas: ok"}])
    check("text", r.content[0].text.strip().upper().startswith("OK") and r.stop_reason == "end_turn" and r.usage.input_tokens > 0, repr(r.content[0].text)[:40])
    parts = []
    with c.messages.stream(model="gpt-5.1", max_tokens=200, messages=[{"role": "user", "content": "Liste três frutas, uma por linha."}]) as s:
        for t in s.text_stream:
            parts.append(t)
        final = s.get_final_message()
    check("stream", len("".join(parts).strip().splitlines()) >= 3 and final.usage.output_tokens > 0, repr("".join(parts))[:60])
    msgs: list = [{"role": "user", "content": TASK}]
    calls_seen = []
    for _ in range(4):
        r = c.messages.create(model="gpt-5.1", max_tokens=500, messages=msgs, tools=TOOLS_ANT)
        uses = [b for b in r.content if b.type == "tool_use"]
        if not uses:
            break
        msgs.append({"role": "assistant", "content": [b.model_dump() for b in r.content]})
        msgs.append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": u.id, "content": fake_tool(u.name, u.input)} for u in uses]})
        calls_seen += [u.name for u in uses]
    text = "".join(b.text for b in r.content if b.type == "text")
    check("tool loop", calls_seen[:2] == ["get_weather", "convert"] and "64" in text and r.stop_reason == "end_turn", f"calls={calls_seen} → {text!r}"[:120])
    names = []
    with c.messages.stream(model="gpt-5.1", max_tokens=300, messages=[{"role": "user", "content": "Temperatura em Lisboa? Use get_weather."}], tools=TOOLS_ANT) as s:
        final = s.get_final_message()
    names = [b.name for b in final.content if b.type == "tool_use"]
    check("stream tool_use", names == ["get_weather"] and final.stop_reason == "tool_use" and isinstance(final.content[-1].input, dict), f"{names} stop={final.stop_reason}")
    r = c.messages.create(model="gpt-5.1", max_tokens=300, messages=[{"role": "user", "content": "Oi"}], tools=TOOLS_ANT, tool_choice={"type": "tool", "name": "convert"})
    check("tool_choice tool", any(b.type == "tool_use" and b.name == "convert" for b in r.content))
    n = c.messages.count_tokens(model="gpt-5.1", messages=[{"role": "user", "content": "x" * 4000}])
    check("count_tokens (estimate)", 700 < n.input_tokens < 1500, str(n.input_tokens))
    r = c.messages.create(model="gpt-5.1", max_tokens=300, stop_sequences=["FIM"], messages=[{"role": "user", "content": "Escreva: um FIM dois"}])
    check("stop_sequence", r.stop_reason == "stop_sequence" and "dois" not in r.content[0].text, f"{r.stop_reason} {r.content[0].text!r}")
    r = c.messages.create(model="gpt-5.1", max_tokens=50, messages=[{"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}, {"type": "text", "text": "Responda apenas: ok"}]}])
    check("image becomes a placeholder (no error)", r.content[0].text.strip().lower().startswith("ok"), repr(r.content[0].text)[:40])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:18880")
    ap.add_argument("--only", choices=["chat", "responses", "messages"])
    a = ap.parse_args()
    httpx.get(a.base + "/health", timeout=5).raise_for_status()
    t0 = time.perf_counter()
    for name, fn in (("chat", test_chat), ("responses", test_responses), ("messages", test_messages)):
        if a.only and a.only != name:
            continue
        try:
            fn(a.base)
        except Exception as e:
            check(f"{name}: exception", False, repr(e)[:200])
    fails = [r for r in results if not r[1]]
    print(f"\nSMOKE {'OK' if not fails else 'FAIL'}: {len(results) - len(fails)}/{len(results)} in {time.perf_counter() - t0:.0f}s")
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())
