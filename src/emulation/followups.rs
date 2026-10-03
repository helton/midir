//! Detectors for replies that need one automatic follow-up: announce and stop, redundant
//! confirmation, false incapacity; and how two calls are compared when deduplicating the invalid-JSON repair.
//! CAVEAT: heuristics, pt/en/es. SENT_RE (a lookbehind) is hand-written.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::canonical::{CanonicalRequest, CanonicalResponse, ToolCall, ToolChoice};
use crate::py::json as pyjson;
use crate::py::text;

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

static CODE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)```.*?(```|$)"));
static OFFER_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(se (quiser|preferir|precisar|desejar)|caso (queira|precise|deseje)|quer que eu|deseja que eu|posso (também |ainda )?(detalhar|ajudar|explicar|fazer|ajustar|mostrar|revisar|seguir)|é só (avisar|pedir|falar|me dizer)|me (avise|diga|fale)|fico à disposição|",
        r"let me know|if you('d)? (want|like|need|prefer)|would you like|do you want|feel free|happy to help|just ask)\b"
    ))
});
static WAIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(em segundo plano|in the background|assim que (ele|ela|eles|o \w+|a \w+|os \w+)?\s*(terminar|terminarem|concluir|finalizar)|quando (ele|o \w+) terminar|aguardando|vou aguardar|aguardo (o|a|os)|",
        r"when (it|they|the \w+) (finish|finishes|complete|completes)|waiting for|once (it|they|the \w+) (finish|finishes|complete|completes))\b"
    ))
});
pub static INTENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(vou|vamos|irei|iremos|farei|faremos|seguirei|prosseguirei|começarei|lerei|rodarei|executarei|criarei|editarei|ajustarei|corrigirei|verificarei|",
        r"deixa(-| )me|deixe(-| )me|preciso (primeiro )?(ler|ver|verificar|checar|analisar|rodar|executar|criar|editar|abrir|inspecionar|olhar|entender|conferir|revisar|ajustar|corrigir)|",
        r"segue (a )?(leitura|abaixo a execução)|começo (lendo|por|verificando|analisando)|começando (pela|pelo|por|com)|(em seguida|a seguir|depois disso|depois),? (eu )?[a-zà-ú]+o\b|primeiro,? (vou|preciso|leio|verifico)|agora,? (vou|leio|verifico|rodo|crio|edito)|em seguida,? (vou|leio|rodo|crio|edito)|a seguir,? (vou|leio|rodo)|",
        r"i'll|i will|i'm going to|i am going to|let me|let's|let us|i need to (first )?(read|check|look|inspect|run|open|see|review|verify|understand|examine|update|fix|create|edit|confirm)|",
        r"next,? i|now i('ll| will| need)|i('ll| will) (now|then|next|first|start|proceed)|proceeding (to|with)|i'll proceed|starting (with|by)|first,? i|going to (read|check|run|edit|create|update|fix|look|inspect)|",
        r"voy a|vamos a|déjame|procederé|necesito (leer|revisar|ver|verificar))\b"
    ))
});
static GERUND_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)^\W*(?:now |first |next |then |agora |primeiro |em seguida )?(running|reading|checking|creating|updating|editing|searching|looking|inspecting|listing|opening|fetching|writing|adding|fixing|installing|starting|examining|exploring|reviewing|scanning|verifying|testing|analyzing|investigating|gathering|",
        r"lendo|verificando|rodando|executando|criando|analisando|buscando|procurando|abrindo|editando|atualizando|checando|investigando|inspecionando|começando|iniciando|listando|conferindo|aplicando|implementando|corrigindo|ajustando)\b"
    ))
});
static PAST_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(implementei|criei|adicionei|ajustei|corrigi|atualizei|rodei|executei|li |verifiquei|fiz|concluí|finalizei|i('ve| have) (implemented|added|created|updated|fixed|run|completed|made)|implemented|added|created|updated|fixed|completed)\b",
    )
});
static DONE_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(conclu[íi]d[oa]s?|tudo pronto|pronto[!.]|feito[!.]|finalizad[oa]|implementad[oa]s?|com sucesso|todos os testes (passaram|passam|passando)|all tests pass(ed|ing)?|",
        r"tarefa conclu[íi]da|task (is )?(complete|completed|done)|completed successfully|resumo (do que foi feito|das (mudanças|alterações))|summary of (the )?changes|changes made|alterações (realizadas|concluídas)|files changed|arquivos? (alterados?|modificados?|criados?))\b"
    ))
});
static PLAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(o plano é|meu plano|plano( de ação)?:|passos:|etapas:|here'?s (the|my) plan|my plan|the plan is|plan:|steps:|next steps:|próximos passos:)",
    )
});
static STEP_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(\d+[.)]|[-*•])\s+"));
static CONFIRM_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(desej[ao]|quer|prefere|gostaria|posso|devo|sigo|confirma|autoriza|pode(mos)?|shall i|should i|do you want|would you like|want me to|prefer|can i|may i|ok to|go ahead)\b",
    )
});
static PENDING_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(o que falta|falta(m)?( apenas| só| somente)?|faltando|pendente|resta(m)?( apenas| só)?|pronto para( o)?|pronta para|",
        r"what'?s left|left to do|remaining( step)?s?:?|still (need|have) to|ready (to|for))\b"
    ))
});
static NOTHING_LEFT_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(nada (mais )?(pendente|falta|a fazer)|n[ãa]o (falta|resta|h[áa]) (nada|pend[êe]ncias?)|sem pend[êe]ncias|nothing (left|remaining|pending)|no (remaining|pending) (steps|work|items))\b",
    )
});
static INCAPACITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(não (tenho|possuo) acesso|não consigo (acessar|abrir|ler|navegar|buscar|consultar)|sem acesso (à|a) (internet|web)|(don't|do not|cannot|can't) (have )?(access|open|browse|fetch|read|reach)|no tengo acceso|no puedo acceder)",
    )
});
pub static TOOL_ABILITY_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(fetch|web|http|url|browse|search|page|read|file|shell|terminal|command|exec|bash)"));
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| re(r"\w"));
static ACTIONS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        ("commit", re(r"commit")),
        ("push", re(r"\bpush")),
        ("merge", re(r"\bmerge")),
        ("deploy", re(r"deploy")),
        ("tests", re(r"(rod(e|ar)|execut(e|ar)|run)\b.{0,30}(test|pytest|suíte|suite)")),
        ("install", re(r"instal")),
    ]
});
static ACTION_DONE: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        ("commit", re(r"(commitad[oa]s?|commit (feito|realizado|criado|conclu[íi]do)|committed|fiz o commit|made the commit)")),
        ("push", re(r"(push (feito|realizado)|pushed)")),
        ("tests", re(r"(testes? (passaram|passando|passam|rodad[oa]s?)|tests? (pass(ed)?|ran))")),
    ]
});
static NEVER: LazyLock<Regex> = LazyLock::new(|| re(r"$^"));

const TARGET_KEYS: [&str; 13] =
    ["path", "file_path", "filePath", "filepath", "file", "filename", "target", "command", "cmd", "url", "uri", "query", "pattern"];

/// `re.split(r"(?<=[.!?:])\s+|\n+", s)`.
pub fn sent_split(s: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let byte = |i: usize| if i < chars.len() { chars[i].0 } else { s.len() };
    let mut out = vec![];
    let mut last = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let after_punct = i > 0 && matches!(chars[i - 1].1, '.' | '!' | '?' | ':');
        if after_punct && text::is_space(c) {
            let mut j = i;
            while j < chars.len() && text::is_space(chars[j].1) {
                j += 1;
            }
            out.push(&s[last..byte(i)]);
            last = byte(j);
            i = j;
            continue;
        }
        if c == '\n' {
            let mut j = i;
            while j < chars.len() && chars[j].1 == '\n' {
                j += 1;
            }
            out.push(&s[last..byte(i)]);
            last = byte(j);
            i = j;
            continue;
        }
        i += 1;
    }
    out.push(&s[last..]);
    out
}

/// What a call acts on (its path, command, url...), for duplicate detection; the whole arguments when none.
pub fn call_target(c: &ToolCall) -> String {
    if let Value::Object(args) = &c.arguments {
        for k in TARGET_KEYS {
            if let Some(Value::String(v)) = args.get(k) {
                return format!("{k}={v}");
            }
        }
    }
    pyjson::dumps(&c.arguments, pyjson::Style { ensure_ascii: false, compact: false, sort_keys: true })
}

pub fn call_key(c: &ToolCall) -> (String, String) {
    (c.name.clone(), call_target(c))
}

fn tools_on(req: &CanonicalRequest) -> bool {
    !req.tools.is_empty() && req.tool_choice != ToolChoice::None
}

pub fn false_incapacity(req: &CanonicalRequest, resp: &CanonicalResponse, text_: &str, calls: &[ToolCall]) -> bool {
    if !calls.is_empty() || !tools_on(req) || resp.finish != "stop" {
        return false;
    }
    let t = text::strip(text_);
    if t.is_empty() || text::len(t) > 800 || !INCAPACITY_RE.is_match(t) {
        return false;
    }
    req.tools.iter().any(|tool| TOOL_ABILITY_RE.is_match(&format!("{} {}", tool.name, tool.description)))
}

pub fn promise_only(req: &CanonicalRequest, resp: &CanonicalResponse, text_: &str, calls: &[ToolCall]) -> bool {
    if !calls.is_empty() || !tools_on(req) || resp.finish != "stop" {
        return false;
    }
    announces_without_acting(text_) || redundant_confirmation(req, text_).is_some()
}

/// The action name when the reply's closing question asks permission for, or its closing lines report as still
/// pending, something the first user request already orders.
pub fn redundant_confirmation(req: &CanonicalRequest, text_: &str) -> Option<&'static str> {
    let prose = CODE_RE.replace_all(text::strip(text_), " ").into_owned();
    let sents: Vec<&str> = sent_split(&prose).into_iter().filter(|x| !x.is_empty() && !text::is_blank(x)).map(text::strip).collect();
    let last = *sents.last()?;
    let asks = last.ends_with('?') && CONFIRM_RE.is_match(last);
    let tail = sents[sents.len().saturating_sub(3)..].join(" ");
    let pending = PENDING_RE.is_match(&tail) && !NOTHING_LEFT_RE.is_match(&tail);
    if !asks && !pending {
        return None;
    }
    let n = if pending { 3 } else { 2 };
    let ask = sents[sents.len().saturating_sub(n)..].join(" ").to_lowercase();
    let request = req.turns.iter().find(|t| t.role == "user" && !t.text.is_empty()).map(|t| t.text.to_lowercase()).unwrap_or_default();
    for (name, pat) in ACTIONS.iter() {
        let done_re = ACTION_DONE.iter().find(|(n, _)| n == name).map(|(_, r)| r).unwrap_or(&NEVER);
        if pat.is_match(&ask) && pat.is_match(&request) && !(pending && !asks && done_re.is_match(&ask)) {
            return Some(name);
        }
    }
    None
}

pub fn announces_without_acting(text_: &str) -> bool {
    let t = text::strip(text_);
    if t.is_empty() {
        return false;
    }
    let prose = CODE_RE.replace_all(t, " ").into_owned();
    let sents: Vec<&str> = sent_split(&prose)
        .into_iter()
        .filter(|x| !x.is_empty() && !text::is_blank(x) && WORD_RE.is_match(x))
        .map(text::strip)
        .filter(|x| !OFFER_RE.is_match(x))
        .collect();
    let Some(last) = sents.last() else { return false };
    if text::rstrip(last).ends_with('?') {
        return false; // it asks the user something
    }
    let last4 = sents[sents.len().saturating_sub(4)..].join(" ");
    if WAIT_RE.is_match(&last4) {
        return false; // waiting for background work it already started
    }
    let last2 = &sents[sents.len().saturating_sub(2)..];
    if (INTENT_RE.is_match(&last2.join(" ")) || last2.iter().any(|x| GERUND_OPEN_RE.is_match(x))) && !PAST_RE.is_match(last) {
        return true; // the last thing it says is an action it is about to do
    }
    if text::rstrip(&prose).ends_with(':') && !t.ends_with("```") {
        return true;
    }
    if DONE_RE.is_match(&prose) {
        return false;
    }
    if INTENT_RE.is_match(sents[0]) || GERUND_OPEN_RE.is_match(sents[0]) {
        return true; // opens with an announcement and never acts
    }
    let lines: Vec<&str> = text::splitlines(&prose).into_iter().filter(|l| !text::is_blank(l)).collect();
    let Some(plan_at) = lines.iter().position(|l| PLAN_RE.is_match(l)) else { return false };
    lines[plan_at + 1..].iter().filter(|l| STEP_RE.is_match(l)).count() >= 2
}
