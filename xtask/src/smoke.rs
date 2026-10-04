//! Live acceptance of a running Midir in the three protocols (25 checks, real backend requests; the prompts are in
//! Portuguese on purpose, as the gateway's users write). Per protocol: plain text, streaming, a two-round
//! tool-calling loop, forced tool_choice, structured JSON, and unsupported content that must not break the request.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TASK: &str =
    "Qual a temperatura em Curitiba agora, em Fahrenheit? Use as ferramentas: primeiro get_weather, depois convert. Responda em uma frase.";

type Section = fn(&mut Smoke) -> Result<(), String>;

struct Smoke {
    base: String,
    http: reqwest::blocking::Client,
    results: Vec<(String, bool)>,
}

fn tools_chat() -> Value {
    json!([
        {"type": "function", "function": {"name": "get_weather", "description": "Temperatura atual em uma cidade.", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}},
        {"type": "function", "function": {"name": "convert", "description": "Converte Celsius para Fahrenheit.", "parameters": {"type": "object", "properties": {"celsius": {"type": "number"}}, "required": ["celsius"]}}},
    ])
}

fn tools_responses() -> Value {
    Value::Array(tools_chat().as_array().unwrap().iter().map(|t| json!({"type": "function", "name": t["function"]["name"], "description": t["function"]["description"], "parameters": t["function"]["parameters"]})).collect())
}

fn tools_anthropic() -> Value {
    Value::Array(tools_chat().as_array().unwrap().iter().map(|t| json!({"name": t["function"]["name"], "description": t["function"]["description"], "input_schema": t["function"]["parameters"]})).collect())
}

fn schema() -> Value {
    json!({"type": "object", "properties": {"cidade": {"type": "string"}, "graus_c": {"type": "number"}}, "required": ["cidade", "graus_c"], "additionalProperties": false})
}

fn fake_tool(name: &str, args: &Value) -> String {
    match name {
        "get_weather" => json!({"city": args["city"], "celsius": 18}).to_string(),
        "convert" => json!({"fahrenheit": args["celsius"].as_f64().unwrap_or(0.0) * 9.0 / 5.0 + 32.0}).to_string(),
        _ => "unknown tool".into(),
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn args_of(v: &Value) -> Value {
    serde_json::from_str(s(v)).unwrap_or(Value::Null)
}

fn short(text: &str, n: usize) -> String {
    let t: String = text.chars().take(n).collect();
    format!("{t:?}")
}

impl Smoke {
    fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        let detail = detail.as_ref();
        println!("  {} {name}{}", if ok { "✓" } else { "✗" }, if detail.is_empty() { String::new() } else { format!("  {detail}") });
        self.results.push((name.into(), ok));
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let r = self.http.post(format!("{}{path}", self.base)).json(&body).send().map_err(|e| e.to_string())?;
        let status = r.status();
        let v: Value = r.json().map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{status}: {v}"));
        }
        Ok(v)
    }

    /// (event name, data) for every SSE event of a streamed POST.
    fn stream(&self, path: &str, body: Value) -> Result<Vec<(Option<String>, Value)>, String> {
        let r = self.http.post(format!("{}{path}", self.base)).json(&body).send().map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("{}", r.status()));
        }
        let text = r.text().map_err(|e| e.to_string())?;
        let mut out = vec![];
        for block in text.replace("\r\n", "\n").split("\n\n") {
            let (mut name, mut data) = (None, vec![]);
            for line in block.lines() {
                if let Some(n) = line.strip_prefix("event:") {
                    name = Some(n.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.trim().to_string());
                }
            }
            if !data.is_empty() {
                let raw = data.join("\n");
                out.push((name, serde_json::from_str(&raw).unwrap_or(Value::String(raw))));
            }
        }
        Ok(out)
    }

    fn section(&mut self, name: &str, f: Section) {
        println!("[{name}]");
        if let Err(e) = f(self) {
            self.check(&format!("{name}: error"), false, short(&e, 200));
        }
    }
}

// ----------------------------------------------------------------------------- chat completions

fn chat(t: &mut Smoke) -> Result<(), String> {
    let p = "/v1/chat/completions";
    let models =
        t.http.get(format!("{}/v1/models", t.base)).send().map_err(|e| e.to_string())?.json::<Value>().map_err(|e| e.to_string())?;
    t.check("models", models["data"].as_array().map(|d| d.iter().any(|m| m["id"] == "gpt-5.1")).unwrap_or(false), "");

    let t0 = Instant::now();
    let r = t.post(p, json!({"model": "gpt-5.1", "temperature": 0, "messages": [{"role": "system", "content": "Responda em maiúsculas."}, {"role": "user", "content": "Diga apenas: ok"}]}))?;
    let text = s(&r["choices"][0]["message"]["content"]).trim().to_uppercase();
    t.check(
        "text",
        text.starts_with("OK") && r["usage"]["prompt_tokens"].as_i64().unwrap_or(0) > 0,
        format!("{:.1}s {}", t0.elapsed().as_secs_f64(), short(&text, 30)),
    );

    let evs = t.stream(p, json!({"model": "gpt-5.1", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "Liste três frutas, uma por linha, sem numeração."}]}))?;
    let streamed: String = evs.iter().filter_map(|(_, d)| d["choices"][0]["delta"]["content"].as_str()).collect();
    let usage = evs.iter().any(|(_, d)| d["usage"].is_object());
    t.check("stream", streamed.trim().lines().count() >= 3 && usage, short(&streamed, 60));

    let mut msgs = vec![json!({"role": "user", "content": TASK})];
    let mut seen = vec![];
    let mut last = Value::Null;
    for _ in 0..4 {
        last = t.post(p, json!({"model": "gpt-5.1", "messages": msgs, "tools": tools_chat()}))?;
        let m = last["choices"][0]["message"].clone();
        let Some(calls) = m["tool_calls"].as_array().filter(|c| !c.is_empty()) else { break };
        msgs.push(json!({"role": "assistant", "content": m["content"], "tool_calls": calls}));
        for c in calls {
            let name = s(&c["function"]["name"]).to_string();
            msgs.push(json!({"role": "tool", "tool_call_id": c["id"], "content": fake_tool(&name, &args_of(&c["function"]["arguments"]))}));
            seen.push(name);
        }
    }
    let final_text = s(&last["choices"][0]["message"]["content"]).to_string();
    t.check(
        "tool loop",
        seen.starts_with(&["get_weather".into(), "convert".into()])
            && final_text.contains("64")
            && last["choices"][0]["finish_reason"] == "stop",
        short(&format!("calls={seen:?} → {final_text}"), 120),
    );

    let evs = t.stream(p, json!({"model": "gpt-5.1", "stream": true, "messages": [{"role": "user", "content": "Qual a temperatura em Lisboa? Use get_weather."}], "tools": tools_chat()}))?;
    let names: Vec<String> = evs
        .iter()
        .flat_map(|(_, d)| d["choices"][0]["delta"]["tool_calls"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["function"]["name"].as_str().map(String::from))
        .collect();
    let finish: Vec<String> = evs.iter().filter_map(|(_, d)| d["choices"][0]["finish_reason"].as_str().map(String::from)).collect();
    t.check("stream tool call", names == ["get_weather"] && finish == ["tool_calls"], format!("{names:?} finish={finish:?}"));

    let r = t.post(p, json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": "Oi, tudo bem?"}], "tools": tools_chat(), "tool_choice": {"type": "function", "function": {"name": "get_weather"}}}))?;
    t.check("tool_choice named", r["choices"][0]["message"]["tool_calls"][0]["function"]["name"] == "get_weather", "");
    let r = t.post(p, json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": "Qual a temperatura em Lisboa? Use get_weather."}], "tools": tools_chat(), "tool_choice": "none"}))?;
    t.check("tool_choice none", r["choices"][0]["message"]["tool_calls"].as_array().map(|c| c.is_empty()).unwrap_or(true), "");

    let r = t.post(p, json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": "Curitiba está com 18 graus. Preencha o JSON."}], "response_format": {"type": "json_schema", "json_schema": {"name": "clima", "schema": schema()}}}))?;
    let content = s(&r["choices"][0]["message"]["content"]).to_string();
    let j = args_of(&json!(content));
    t.check(
        "json_schema",
        s(&j["cidade"]).to_lowercase().starts_with("curitiba") && j["graus_c"].as_f64() == Some(18.0),
        short(&content, 80),
    );

    let r = t.post(
        p,
        json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": "Escreva 20 linhas numeradas."}], "max_tokens": 20}),
    )?;
    t.check(
        "max_tokens → length",
        r["choices"][0]["finish_reason"] == "length",
        format!("{} chars", s(&r["choices"][0]["message"]["content"]).len()),
    );

    let r = t.post(p, json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": [{"type": "text", "text": "Se houver uma imagem nesta mensagem, responda apenas: SEM IMAGEM. Caso contrário responda: OK."}, {"type": "image_url", "image_url": {"url": "http://x/y.png"}}]}]}))?;
    let text = s(&r["choices"][0]["message"]["content"]).to_uppercase();
    t.check("image becomes a placeholder (no error)", text.contains("IMAGEM") || text.contains("OK"), short(&text, 60));
    Ok(())
}

// ----------------------------------------------------------------------------- responses

fn output_text(r: &Value) -> String {
    r["output"]
        .as_array()
        .map(|o| {
            o.iter()
                .filter(|i| i["type"] == "message")
                .flat_map(|i| i["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|c| c["text"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn function_calls(r: &Value) -> Vec<Value> {
    r["output"].as_array().map(|o| o.iter().filter(|i| i["type"] == "function_call").cloned().collect()).unwrap_or_default()
}

fn responses(t: &mut Smoke) -> Result<(), String> {
    let p = "/v1/responses";
    let r = t.post(p, json!({"model": "gpt-5.1", "instructions": "Responda em maiúsculas.", "input": "Diga apenas: ok"}))?;
    let text = output_text(&r);
    t.check("text", text.trim().to_uppercase().starts_with("OK") && r["usage"]["input_tokens"].as_i64().unwrap_or(0) > 0, short(&text, 40));

    let evs = t.stream(p, json!({"model": "gpt-5.1", "stream": true, "input": "Liste três frutas, uma por linha."}))?;
    let deltas: String =
        evs.iter().filter(|(_, d)| d["type"] == "response.output_text.delta").map(|(_, d)| s(&d["delta"]).to_string()).collect();
    let completed = evs.iter().find(|(_, d)| d["type"] == "response.completed").map(|(_, d)| d["response"].clone());
    t.check(
        "stream",
        deltas.trim().lines().count() >= 3 && completed.map(|c| c["usage"]["output_tokens"].as_i64().unwrap_or(0) > 0).unwrap_or(false),
        short(&deltas, 60),
    );

    let mut r = t.post(p, json!({"model": "gpt-5.1", "input": TASK, "tools": tools_responses()}))?;
    let mut seen = vec![];
    for _ in 0..4 {
        let calls = function_calls(&r);
        if calls.is_empty() {
            break;
        }
        let outs: Vec<Value> = calls.iter().map(|c| {
            seen.push(s(&c["name"]).to_string());
            json!({"type": "function_call_output", "call_id": c["call_id"], "output": fake_tool(s(&c["name"]), &args_of(&c["arguments"]))})
        }).collect();
        r = t.post(p, json!({"model": "gpt-5.1", "previous_response_id": r["id"], "input": outs, "tools": tools_responses()}))?;
    }
    let text = output_text(&r);
    t.check(
        "tool loop + previous_response_id",
        seen.starts_with(&["get_weather".into(), "convert".into()]) && text.contains("64"),
        short(&format!("calls={seen:?} → {text}"), 120),
    );

    let mut input = vec![json!({"role": "user", "content": TASK})];
    let mut r = t.post(p, json!({"model": "gpt-5.1", "input": input, "tools": tools_responses(), "store": false}))?;
    for _ in 0..4 {
        let calls = function_calls(&r);
        if calls.is_empty() {
            break;
        }
        for c in calls {
            input.push(json!({"type": "function_call", "call_id": c["call_id"], "name": c["name"], "arguments": c["arguments"]}));
            input.push(json!({"type": "function_call_output", "call_id": c["call_id"], "output": fake_tool(s(&c["name"]), &args_of(&c["arguments"]))}));
        }
        r = t.post(p, json!({"model": "gpt-5.1", "input": input, "tools": tools_responses(), "store": false}))?;
    }
    let text = output_text(&r);
    t.check("tool loop stateless", text.contains("64"), short(&text, 80));

    let evs = t.stream(
        p,
        json!({"model": "gpt-5.1", "stream": true, "input": "Temperatura em Lisboa? Use get_weather.", "tools": tools_responses()}),
    )?;
    let names: Vec<String> = evs
        .iter()
        .filter(|(_, d)| d["type"] == "response.output_item.done" && d["item"]["type"] == "function_call")
        .map(|(_, d)| s(&d["item"]["name"]).to_string())
        .collect();
    t.check("stream tool call", names == ["get_weather"], format!("{names:?}"));

    let r = t.post(p, json!({"model": "gpt-5.1", "input": "Curitiba está com 18 graus. Preencha o JSON.", "text": {"format": {"type": "json_schema", "name": "clima", "schema": schema()}}}))?;
    let text = output_text(&r);
    t.check("json_schema", args_of(&json!(text))["graus_c"].as_f64() == Some(18.0), short(&text, 80));

    let r = t.post(p, json!({"model": "gpt-5.1", "input": "Diga apenas: ok", "tools": [{"type": "web_search_preview"}]}))?;
    let text = output_text(&r);
    t.check("web_search omitted (no error)", text.trim().to_lowercase().starts_with("ok"), short(&text, 40));
    Ok(())
}

// ----------------------------------------------------------------------------- messages

fn blocks_text(r: &Value) -> String {
    r["content"]
        .as_array()
        .map(|c| c.iter().filter(|b| b["type"] == "text").map(|b| s(&b["text"]).to_string()).collect())
        .unwrap_or_default()
}

fn messages(t: &mut Smoke) -> Result<(), String> {
    let p = "/v1/messages";
    let r = t.post(p, json!({"model": "gpt-5.1", "max_tokens": 200, "system": "Responda em maiúsculas.", "messages": [{"role": "user", "content": "Diga apenas: ok"}]}))?;
    let text = blocks_text(&r);
    t.check(
        "text",
        text.trim().to_uppercase().starts_with("OK")
            && r["stop_reason"] == "end_turn"
            && r["usage"]["input_tokens"].as_i64().unwrap_or(0) > 0,
        short(&text, 40),
    );

    let evs = t.stream(p, json!({"model": "gpt-5.1", "stream": true, "max_tokens": 200, "messages": [{"role": "user", "content": "Liste três frutas, uma por linha."}]}))?;
    let streamed: String =
        evs.iter().filter(|(_, d)| d["type"] == "content_block_delta").filter_map(|(_, d)| d["delta"]["text"].as_str()).collect();
    let out_tokens =
        evs.iter().find(|(_, d)| d["type"] == "message_delta").map(|(_, d)| d["usage"]["output_tokens"].as_i64().unwrap_or(0)).unwrap_or(0);
    t.check("stream", streamed.trim().lines().count() >= 3 && out_tokens > 0, short(&streamed, 60));

    let mut msgs = vec![json!({"role": "user", "content": TASK})];
    let mut seen = vec![];
    let mut r = Value::Null;
    for _ in 0..4 {
        r = t.post(p, json!({"model": "gpt-5.1", "max_tokens": 500, "messages": msgs, "tools": tools_anthropic()}))?;
        let uses: Vec<Value> =
            r["content"].as_array().cloned().unwrap_or_default().into_iter().filter(|b| b["type"] == "tool_use").collect();
        if uses.is_empty() {
            break;
        }
        msgs.push(json!({"role": "assistant", "content": r["content"]}));
        msgs.push(json!({"role": "user", "content": uses.iter().map(|u| json!({"type": "tool_result", "tool_use_id": u["id"], "content": fake_tool(s(&u["name"]), &u["input"])})).collect::<Vec<_>>()}));
        seen.extend(uses.iter().map(|u| s(&u["name"]).to_string()));
    }
    let text = blocks_text(&r);
    t.check(
        "tool loop",
        seen.starts_with(&["get_weather".into(), "convert".into()]) && text.contains("64") && r["stop_reason"] == "end_turn",
        short(&format!("calls={seen:?} → {text}"), 120),
    );

    let evs = t.stream(p, json!({"model": "gpt-5.1", "stream": true, "max_tokens": 300, "messages": [{"role": "user", "content": "Temperatura em Lisboa? Use get_weather."}], "tools": tools_anthropic()}))?;
    let names: Vec<String> = evs
        .iter()
        .filter(|(_, d)| d["type"] == "content_block_start" && d["content_block"]["type"] == "tool_use")
        .map(|(_, d)| s(&d["content_block"]["name"]).to_string())
        .collect();
    let input: String = evs
        .iter()
        .filter(|(_, d)| d["delta"]["type"] == "input_json_delta")
        .map(|(_, d)| s(&d["delta"]["partial_json"]).to_string())
        .collect();
    let stop =
        evs.iter().find(|(_, d)| d["type"] == "message_delta").map(|(_, d)| s(&d["delta"]["stop_reason"]).to_string()).unwrap_or_default();
    t.check(
        "stream tool_use",
        names == ["get_weather"] && stop == "tool_use" && serde_json::from_str::<Value>(&input).map(|v| v.is_object()).unwrap_or(false),
        format!("{names:?} stop={stop}"),
    );

    let r = t.post(p, json!({"model": "gpt-5.1", "max_tokens": 300, "messages": [{"role": "user", "content": "Oi"}], "tools": tools_anthropic(), "tool_choice": {"type": "tool", "name": "convert"}}))?;
    t.check(
        "tool_choice tool",
        r["content"].as_array().map(|c| c.iter().any(|b| b["type"] == "tool_use" && b["name"] == "convert")).unwrap_or(false),
        "",
    );

    let r =
        t.post("/v1/messages/count_tokens", json!({"model": "gpt-5.1", "messages": [{"role": "user", "content": "x".repeat(4000)}]}))?;
    let n = r["input_tokens"].as_i64().unwrap_or(0);
    t.check("count_tokens (estimate)", (700..1500).contains(&n), n.to_string());

    let r = t.post(p, json!({"model": "gpt-5.1", "max_tokens": 300, "stop_sequences": ["FIM"], "messages": [{"role": "user", "content": "Escreva: um FIM dois"}]}))?;
    let text = blocks_text(&r);
    t.check(
        "stop_sequence",
        r["stop_reason"] == "stop_sequence" && !text.contains("dois"),
        format!("{} {}", s(&r["stop_reason"]), short(&text, 40)),
    );

    let r = t.post(p, json!({"model": "gpt-5.1", "max_tokens": 50, "messages": [{"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}, {"type": "text", "text": "Responda apenas: ok"}]}]}))?;
    let text = blocks_text(&r);
    t.check("image becomes a placeholder (no error)", text.trim().to_lowercase().starts_with("ok"), short(&text, 40));
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut base = "http://127.0.0.1:18880".to_string();
    let mut only = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => base = it.next().ok_or("--base needs a URL")?.trim_end_matches('/').to_string(),
            "--only" => only = Some(it.next().ok_or("--only needs chat, responses or messages")?.clone()),
            other => return Err(format!("smoke: unknown argument {other:?}")),
        }
    }
    let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(300)).build().map_err(|e| e.to_string())?;
    http.get(format!("{base}/health")).send().and_then(|r| r.error_for_status()).map_err(|e| format!("{base}/health: {e}"))?;
    let mut t = Smoke { base, http, results: vec![] };
    let t0 = Instant::now();
    let sections: [(&str, Section); 3] = [("chat", chat), ("responses", responses), ("messages", messages)];
    for (name, f) in sections {
        if only.as_deref().map(|o| o == name).unwrap_or(true) {
            t.section(name, f);
        }
    }
    let fails = t.results.iter().filter(|(_, ok)| !ok).count();
    println!(
        "\nSMOKE {}: {}/{} in {:.0}s",
        if fails == 0 { "OK" } else { "FAIL" },
        t.results.len() - fails,
        t.results.len(),
        t0.elapsed().as_secs_f64()
    );
    if fails == 0 { Ok(()) } else { Err(format!("{fails} check(s) failed")) }
}
