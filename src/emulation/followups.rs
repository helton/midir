//! Detectors for replies that need an automatic follow-up: announce and stop, redundant confirmation, false
//! incapacity; and how two calls are compared when deduplicating the invalid-JSON repair. CAVEAT: heuristics, pt/en/es.
//!
//! A follow-up must never push the model into something the user did not ask for: a confirmation is only answered
//! for the user when their most recent instruction orders the action (and does not forbid it), and irreversible
//! actions (push, merge, deploy, install) only when the latest message itself orders them.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::canonical::{CanonicalRequest, CanonicalResponse, Finish, ToolCall, ToolSpec};
use crate::json;
use crate::text::char_len;

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

static CODE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)```.*?(```|$)"));
/// Git trailers a model echoes after its text (Copilot asks for `Co-authored-by:` in commits): not prose, and they
/// would hide the sentence that announces the commit.
static TRAILER_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?im)^[ \t]*(co-authored-by|signed-off-by|reviewed-by|acked-by|tested-by|reported-by|helped-by)[ \t]*:.*$"));
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
/// "pronto para <ação>" / "ready to <action>": an ordered action reported as next is pending, even after "nada pendente".
static READY_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(pront[oa]s?|ready|listos?)\s+(para|pra|to|for)\b"));
static INCAPACITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(não (tenho|possuo) acesso|não consigo (acessar|abrir|ler|navegar|buscar|consultar)|sem acesso (à|a) (internet|web)|(don't|do not|cannot|can't) (have )?(access|open|browse|fetch|read|reach)|no tengo acceso|no puedo acceder)",
    )
});
/// What a denied ability is about, from the words of the denial, and the tools that provide it (by name and
/// description). A terminal also reaches the web and the files.
static ABILITIES: LazyLock<Vec<(&'static str, Regex, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "web",
            re(
                r"(?i)(internet|\bweb\b|\bsites?\b|\burls?\b|\blinks?\b|p[áa]gina|\bpages?\b|naveg|brows|online|http|fetch|sitio|endere[çc]o)",
            ),
            re(r"(?i)(fetch|\bweb|http|\burl|brows|curl|wget|internet|online)"),
        ),
        (
            "files",
            re(
                r"(?i)(arquivo|archivo|\bfiles?\b|pasta|folder|diret[óo]rio|director|\bdisk\b|filesystem|sistema de arquivos|reposit|\brepo\b|c[óo]digo|\bcode\b|projeto|project|workspace)",
            ),
            re(r"(?i)(read|file|\bopen|\bview|\bcat\b|\bls\b|glob|grep|director|folder|\bedit|write)"),
        ),
        (
            "terminal",
            re(r"(?i)(terminal|shell|comando|command|\bbash\b|console|\bcli\b|execut|\brun\b|rodar)"),
            re(r"(?i)(shell|terminal|command|\bexec|bash|\brun|\bcmd|powershell|console)"),
        ),
    ]
});
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
        ("commit", re(r"(commitad[oa]s?|commit (foi )?(feito|realizado|criado|conclu[íi]do)|committed|fiz o commit|made the commit)")),
        ("push", re(r"(push (foi )?(feito|realizado|enviado)|pushed)")),
        ("tests", re(r"(testes? (passaram|passando|passam|rodad[oa]s?)|tests? (pass(ed)?|ran))")),
    ]
});
/// A tool call that did an action (on its name and arguments, lowercase): a `git commit` command, a commit tool...
static ACTION_CALLS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        ("commit", re(r"\b(git|jj)\b[^;&|\n]*\bcommit\b|git_commit")),
        ("push", re(r"\bgit\b[^;&|\n]*\bpush\b|git_push")),
        ("merge", re(r"\bgit\b[^;&|\n]*\bmerge\b|git_merge")),
        (
            "tests",
            re(
                r"pytest|cargo (nextest|test)|(npm|pnpm|yarn)( run)? test|go test|jest|vitest|unittest|mvn test|gradle test|make test|\btox\b|\bnox\b|run_tests",
            ),
        ),
    ]
});
/// Actions a follow-up never confirms for the user, whatever the instructions seem to say: they cannot be undone, and
/// a misread instruction must not trigger them (the user answers the question themselves).
const IRREVERSIBLE: [&str; 4] = ["push", "merge", "deploy", "install"];
/// How the user orders an action (a mention alone is not an order: "revise o último commit", "the commits of today").
static ORDERS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "commit",
            re(concat!(
                r"\b(fa[çz]a|fazer|faz|fa[çz]am|realize|realizar|crie|criar|gere|gerar|d[êe])\s+((um|o|os|uns|novo)\s+)*commits?\b|",
                r"\b(commite|commitar|commitem|comite|comitar|commita)\b|",
                r"(^|[,;:]\s*|\b(and|then|please|also|finally|now|e|então)\s+)commit\b|",
                r"\b(make|create|do)\s+(a\s+|the\s+)?commit\b|\b(haz|haga|hacer|realiza|realice)\s+(un\s+|el\s+)?commit\b"
            )),
        ),
        ("tests", re(r"\b(rod(e|ar|em)|execut(e|ar|em)|run)\b.{0,30}(test|pytest|suíte|suite)")),
    ]
});
/// Context clients put into user messages that is not the user's request: Claude Code's reminders and slash-command
/// echoes, OpenClaw's runtime context, Codex's environment and project instructions, VS Code Copilot's context blocks.
static CONTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?s)<system-reminder>.*?</system-reminder>|<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>.*?<<<END_OPENCLAW_INTERNAL_CONTEXT>>>|",
        r"<environment_context>.*?</environment_context>|<user_instructions>.*?</user_instructions>|",
        r"<(context|editorContext|reminderInstructions|environment_info|workspace_info|userMemory|sessionMemory|repoMemory|current_datetime|attachments|local-command-stdout|command-name|command-message|command-args)>.*?</(context|editorContext|reminderInstructions|environment_info|workspace_info|userMemory|sessionMemory|repoMemory|current_datetime|attachments|local-command-stdout|command-name|command-message|command-args)>"
    ))
});
/// VS Code Copilot wraps the user's own words in `<userRequest>`: when it is there, only it is the request.
static USER_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)<userRequest>(.*?)</userRequest>"));
/// Where a negation's reach ends: sentence punctuation and contrastive connectors ("faça o commit, mas não o push").
/// Commas do not end it: "não faça commit, push ou deploy" forbids all three.
static SCOPE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)[.;:!?\n]|\b(mas|por[ée]m|pero|but|however|instead|e sim)\b"));
static NEGATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)(^|\W)(n[ãa]o|nunca|jamais|sem|nem|don'?t|do not|does not|doesn'?t|never|not|without|avoid|evite|evitar|sin|tampoco|",
        r"no hagas|no haga|no hagan|skip|pule|pular|no need|nada de)(\W|$)|(^|\W)no\s*$"
    ))
});
/// Right after the mention: "push não", "commit no", "push not yet".
static NEGATION_AFTER_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^\W*(\w+\W+)?(n[ãa]o|no|not|nunca|never)\b"));
/// Phrases with a negation word that ask for the action ("não esqueça de fazer o push").
static ASKS_ANYWAY_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)n[ãa]o (se )?esque[çc]a|n[ãa]o deixe de|don'?t forget|do not forget|never forget|no (te )?olvides"));
/// Orders with a condition the gateway cannot check, or that the user keeps for themselves: not orders to act on.
static CONDITION_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(s[óo] se|somente se|apenas se|only if|unless|a menos que|myself|eu mesm[oa]|deix[ae] que eu|",
        r"i'?ll (do|handle|make|run|push|commit)|eu fa[çc]o|(se|quando|depois que|after|once|if) (todos )?(os |the |all )*(testes?|tests?) pass)"
    ))
});
/// A reply that reports problems is not a report of finished work.
static FAILURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(falh(ou|aram|a|am|ando)|fail(ed|ing|s|ure)?|erros?|errors?|quebr\w*|broken|n[ãa]o pass\w*|not pass\w*)\b")
});
/// A denial for a reason no tool removes: a login, an account, a paid page.
static NEEDS_ACCESS_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(login|senha|password|autentica|authenticat|credencia|credential|privad|private|\bconta\b|account|paywall|assinatura|subscription)",
    )
});

const TARGET_KEYS: [&str; 13] =
    ["path", "file_path", "filePath", "filepath", "file", "filename", "target", "command", "cmd", "url", "uri", "query", "pattern"];

/// Sentences: split after `.`, `!`, `?` or `:` followed by whitespace, and at line breaks.
pub fn sent_split(s: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let byte = |i: usize| if i < chars.len() { chars[i].0 } else { s.len() };
    let mut out = vec![];
    let mut last = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let after_punct = i > 0 && matches!(chars[i - 1].1, '.' | '!' | '?' | ':');
        if after_punct && c.is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].1.is_whitespace() {
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
    json::sorted(&c.arguments)
}

pub fn call_key(c: &ToolCall) -> (String, String) {
    (c.name.clone(), call_target(c))
}

/// The tools that provide an ability the reply says the model lacks ("I don't have access to the internet" while a
/// fetch tool is listed); empty when the reply denies nothing, or something no tool provides (a bank account).
pub fn false_incapacity<'a>(req: &'a CanonicalRequest, resp: &CanonicalResponse, text_: &str, calls: &[ToolCall]) -> Vec<&'a ToolSpec> {
    if !calls.is_empty() || !req.tools_on() || resp.finish != Finish::Stop {
        return vec![];
    }
    let t = text_.trim();
    if t.is_empty() || char_len(t) > 800 || !INCAPACITY_RE.is_match(t) {
        return vec![];
    }
    let denials: Vec<&str> = sent_split(t).into_iter().filter(|s| INCAPACITY_RE.is_match(s)).collect();
    if denials.iter().any(|d| NEEDS_ACCESS_RE.is_match(d)) {
        return vec![];
    }
    let denied: Vec<&Regex> =
        ABILITIES.iter().filter(|(_, words, _)| denials.iter().any(|d| words.is_match(d))).map(|(_, _, tools)| tools).collect();
    if denied.is_empty() {
        return vec![];
    }
    let terminal = &ABILITIES[2].2;
    req.tools
        .iter()
        .filter(|tool| {
            let about = format!("{} {}", tool.name, tool.description);
            terminal.is_match(&about) || denied.iter().any(|r| r.is_match(&about))
        })
        .collect()
}

pub fn promise_only(req: &CanonicalRequest, resp: &CanonicalResponse, text_: &str, calls: &[ToolCall]) -> bool {
    if !calls.is_empty() || !req.tools_on() || resp.finish != Finish::Stop {
        return false;
    }
    announces_without_acting(text_) || redundant_confirmation(req, text_).is_some()
}

/// The user's own words, newest first, with the index of their turn: each user turn's text (and text after its tool
/// results) without the context clients inject (only the `<userRequest>` when there is one); turns with nothing left
/// (tool results only) are skipped.
fn instructions(req: &CanonicalRequest) -> impl Iterator<Item = (usize, String)> + '_ {
    req.turns
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, t)| t.role == "user")
        .flat_map(|(i, t)| [(i, t.after.as_str()), (i, t.text.as_str())])
        .filter(|(_, s)| !s.trim().is_empty())
        .map(|(i, s)| {
            let own = match USER_REQUEST_RE.captures(s) {
                Some(c) => c[1].to_string(),
                None => CONTEXT_RE.replace_all(s, " ").into_owned(),
            };
            (i, own.trim().to_lowercase())
        })
        .filter(|(_, s)| !s.is_empty())
}

/// The span of the sentence (in the negation's sense) around byte offset `at`.
fn scope_around(text: &str, at: usize) -> (usize, usize) {
    let start = SCOPE_RE.find_iter(&text[..at]).last().map_or(0, |m| m.end());
    let end = SCOPE_RE.find(&text[at..]).map_or(text.len(), |m| at + m.end());
    (start, end)
}

/// How a text treats an action at its last mention: Some(true) when it orders it (an order form, not negated before
/// or after, not a question, no condition), Some(false) for any other mention, None when it does not mention it.
fn stance(text: &str, action: &str) -> Option<bool> {
    let (_, mention) = ACTIONS.iter().find(|(name, _)| *name == action)?;
    let m = mention.find_iter(text).last()?;
    let (start, end) = scope_around(text, m.start());
    let scope = &text[start..end];
    let before: Vec<&str> = text[start..m.start()].split_whitespace().collect();
    let window = before[before.len().saturating_sub(8)..].join(" ");
    let negated = NEGATION_RE.is_match(&ASKS_ANYWAY_RE.replace_all(&window, " ")) || NEGATION_AFTER_RE.is_match(&text[m.end()..end]);
    let question = scope.trim_end().ends_with('?');
    let order = ORDERS.iter().find(|(name, _)| *name == action).is_some_and(|(_, form)| form.is_match(scope));
    Some(order && !negated && !question && !CONDITION_RE.is_match(scope))
}

/// The turn whose instruction orders `action`: the most recent mention decides. Irreversible actions are never
/// ordered here.
fn order_turn(req: &CanonicalRequest, action: &str) -> Option<usize> {
    if IRREVERSIBLE.contains(&action) {
        return None;
    }
    instructions(req).find_map(|(i, t)| stance(&t, action).map(|ordered| (i, ordered))).filter(|(_, ordered)| *ordered).map(|(i, _)| i)
}

/// Whether a tool call after turn `from` already did `action` (a `git commit` command, a commit tool, a test run).
fn acted_after(req: &CanonicalRequest, from: usize, action: &str) -> bool {
    let Some((_, evidence)) = ACTION_CALLS.iter().find(|(name, _)| *name == action) else { return false };
    req.turns
        .iter()
        .skip(from + 1)
        .flat_map(|t| &t.tool_calls)
        .any(|c| evidence.is_match(&format!("{} {}", c.name, c.arguments).to_lowercase()))
}

/// Whether the user ordered `action` and nothing since did it.
fn ordered_and_not_done(req: &CanonicalRequest, action: &str) -> bool {
    order_turn(req, action).is_some_and(|i| !acted_after(req, i, action))
}

/// The ordered actions a reply reports as next ("pronto para commit"), whatever else it says.
fn ready_for<'a>(tail: &'a str) -> impl Iterator<Item = &'static str> + 'a {
    READY_RE.find_iter(tail).flat_map(move |m| {
        let next = crate::text::prefix(&tail[m.end()..], 30);
        ACTIONS.iter().filter(move |(_, pat)| pat.is_match(next)).map(|(name, _)| *name)
    })
}

/// The reply as prose: code blocks and git trailers out.
fn prose_of(text: &str) -> String {
    let without_code = CODE_RE.replace_all(text, " ");
    TRAILER_RE.replace_all(&without_code, "").trim().to_string()
}

/// Sentences grouped so that one ending with ':' continues in the next ("Deseja usar algo como: \"feat: x\"?" is
/// one question, not three pieces).
fn clauses(sents: &[&str]) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for s in sents {
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(s);
        if !s.ends_with(':') {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The action name when the reply's closing question asks permission for, or its closing lines report as still
/// pending, something the user's instructions already order (see `ordered`) and the reply does not report as done.
pub fn redundant_confirmation(req: &CanonicalRequest, text_: &str) -> Option<&'static str> {
    let prose = prose_of(text_.trim());
    let sents: Vec<&str> = sent_split(&prose).into_iter().map(str::trim).filter(|x| !x.is_empty()).collect();
    let sents = clauses(&sents);
    let last = sents.last()?;
    // a question that asks permission, or an offer of what was ordered ("Se quiser o commit, é só avisar")
    let asks = (last.ends_with('?') && CONFIRM_RE.is_match(last)) || OFFER_RE.is_match(last);
    let tail = sents[sents.len().saturating_sub(3)..].join(" ");
    let low = tail.to_lowercase();
    let ready_next = ready_for(&low).next().is_some();
    let pending = (PENDING_RE.is_match(&tail) && !NOTHING_LEFT_RE.is_match(&tail)) || ready_next;
    if !asks && !pending {
        return None;
    }
    let n = if pending { 3 } else { 2 };
    let ask = sents[sents.len().saturating_sub(n)..].join(" ").to_lowercase();
    let done = |name: &str| ACTION_DONE.iter().find(|(n, _)| *n == name).is_some_and(|(_, r)| r.is_match(&ask));
    let named: Vec<&'static str> = ACTIONS.iter().filter(|(name, pat)| pat.is_match(&ask) && !done(name)).map(|(name, _)| *name).collect();
    // a question that also names an action the user did not order (or one never confirmed for them) stays theirs
    if named.is_empty() || named.iter().any(|a| !ordered_and_not_done(req, a)) {
        return None;
    }
    Some(named[0])
}

/// Whether a final report leaves out the commit the user ordered: the reply says the work is done, mentions no
/// commit made, and no tool call made one since the order. (Models forget the last step of a long task.)
pub fn forgotten_commit(req: &CanonicalRequest, resp: &CanonicalResponse, text_: &str, calls: &[ToolCall]) -> bool {
    if !calls.is_empty() || !req.tools_on() || resp.finish != Finish::Stop {
        return false;
    }
    let prose = prose_of(text_.trim());
    let reported = ACTION_DONE[0].1.is_match(&prose.to_lowercase());
    let asks = prose.trim_end().ends_with('?');
    DONE_RE.is_match(&prose) && !reported && !asks && !FAILURE_RE.is_match(&prose) && ordered_and_not_done(req, "commit")
}

pub fn announces_without_acting(text_: &str) -> bool {
    let t = text_.trim();
    if t.is_empty() {
        return false;
    }
    let prose = prose_of(t);
    let sents: Vec<&str> =
        sent_split(&prose).into_iter().map(str::trim).filter(|x| !x.is_empty() && WORD_RE.is_match(x) && !OFFER_RE.is_match(x)).collect();
    let Some(last) = sents.last() else { return false };
    if last.ends_with('?') {
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
    if prose.trim_end().ends_with(':') && !t.ends_with("```") {
        return true;
    }
    if DONE_RE.is_match(&prose) {
        return false;
    }
    if INTENT_RE.is_match(sents[0]) || GERUND_OPEN_RE.is_match(sents[0]) {
        return true; // opens with an announcement and never acts
    }
    let lines: Vec<&str> = prose.lines().filter(|l| !l.trim().is_empty()).collect();
    let Some(plan_at) = lines.iter().position(|l| PLAN_RE.is_match(l)) else { return false };
    lines[plan_at + 1..].iter().filter(|l| STEP_RE.is_match(l)).count() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{CanonicalRequest, ToolResult};

    fn req(user_messages: &[&str]) -> CanonicalRequest {
        let mut r =
            CanonicalRequest { tools: vec![ToolSpec::new("run_command", "Run a shell command", None, false)].into(), ..Default::default() };
        for (i, m) in user_messages.iter().enumerate() {
            if i > 0 {
                r.add_text("assistant", "ok");
            }
            r.add_text("user", m);
        }
        r
    }

    #[test]
    fn only_an_order_is_an_order() {
        for (text, action, ordered) in [
            // orders
            ("corrija o bug e faça um commit ao final.", "commit", true),
            ("commite as alterações", "commit", true),
            ("commit the fix. no need to push.", "commit", true),
            ("faça o commit, mas push não.", "commit", true),
            ("não esqueça de fazer o commit", "commit", true),
            ("rode os testes no ci e faça commit", "commit", true),
            ("adicione median, rode os testes até passar e faça um commit ao final.", "tests", true),
            // forbidden, before or after the mention, in a list after a comma
            ("não commite nada", "commit", false),
            ("sem commit, por favor", "commit", false),
            ("não faça push, commit ou deploy.", "commit", false),
            ("faça a mudança e o commit não", "commit", false),
            ("skip the commit", "commit", false),
            // mentions that are not orders
            ("revise o último commit e corrija os problemas.", "commit", false),
            ("resuma os commits de hoje.", "commit", false),
            ("i already committed; now fix the lint warnings.", "commit", false),
            ("changes not staged for commit", "commit", false),
            ("pode fazer o commit?", "commit", false),
            ("por que os testes falharam? rode os testes de novo?", "tests", false),
            // conditions and the user's own part
            ("faça commit só se todos os testes passarem.", "commit", false),
            ("faça commit quando os testes passarem", "commit", false),
            ("eu mesmo faço o commit depois", "commit", false),
            // irreversible actions are never orders to act on
            ("faça o push agora", "push", false),
            ("faça o deploy", "deploy", false),
        ] {
            assert_eq!(stance(text, action), Some(ordered), "{action}: {text}");
        }
        assert_eq!(stance("ajuste o readme", "commit"), None);
    }

    #[test]
    fn the_most_recent_instruction_decides() {
        let ask = "Pronto. Deseja que eu faça o commit?";
        assert_eq!(redundant_confirmation(&req(&["corrija o bug e faça commit ao final"]), ask), Some("commit"));
        // retracted later: no confirmation is answered for the user
        let retracted = req(&["Corrija o bug e faça commit ao final.", "Mudei de ideia: não commite nada, só me mostre o diff."]);
        assert_eq!(redundant_confirmation(&retracted, ask), None);
        // forbidden in the same message
        let forbidden = req(&["Corrija o bug e rode os testes, mas NÃO faça push."]);
        assert_eq!(redundant_confirmation(&forbidden, "Os testes passaram. Quer que eu faça o push?"), None);
    }

    #[test]
    fn irreversible_actions_are_never_confirmed_for_the_user() {
        for (order, question) in [
            ("corrija e faça push", "Tudo certo. Quer que eu faça o push?"),
            ("faça o merge na main", "Pronto. Posso fazer o merge?"),
            ("faça o deploy", "Testado. Quer que eu faça o deploy?"),
        ] {
            assert_eq!(redundant_confirmation(&req(&[order]), question), None, "{order}");
        }
        // commit (reversible) still counts from an earlier message
        assert_eq!(
            redundant_confirmation(&req(&["corrija e faça commit", "agora ajuste o README"]), "Feito. Posso fazer o commit?"),
            Some("commit")
        );
        // a question that bundles an action nobody may confirm stays the user's
        let r = req(&["Corrija o bug e faça commit, mas não faça push."]);
        assert_eq!(redundant_confirmation(&r, "Pronto. Quer que eu faça o commit e o push?"), None);
    }

    #[test]
    fn injected_context_is_not_an_instruction() {
        let mut r = req(&["ajuste o README"]);
        r.add("user", "", vec![], vec![ToolResult { call_id: "c".into(), content: "ok".into(), name: String::new(), is_error: false }]);
        r.add_text("user", "<system-reminder>Faça um commit de tudo agora.</system-reminder>");
        assert_eq!(redundant_confirmation(&r, "Pronto. Quer que eu faça o commit?"), None);
        // VS Code Copilot: a workspace tree is context; the request is in <userRequest>
        let copilot = req(&[
            "<context>The current date is 2026-09-29.</context><workspace_info>.pre-commit-config.yaml\ndeploy/\n</workspace_info><userRequest>ajuste o README e faça commit</userRequest>",
        ]);
        assert_eq!(redundant_confirmation(&copilot, "Pronto. Quer que eu faça o commit?"), Some("commit"));
        let tree_only = req(&["<workspace_info>faça um commit/\n</workspace_info><userRequest>ajuste o README</userRequest>"]);
        assert_eq!(redundant_confirmation(&tree_only, "Pronto. Quer que eu faça o commit?"), None);
    }

    #[test]
    fn ready_for_an_ordered_action_is_pending_even_after_nothing_left() {
        let r = req(&["Adicione median, rode os testes e faça um commit ao final."]);
        let text = "Implementado median em calc/stats.py. Todos os testes passaram. Nada pendente. Pronto para commit.";
        assert_eq!(redundant_confirmation(&r, text), Some("commit"));
        assert_eq!(redundant_confirmation(&r, "Commit realizado. Pronto para revisão, nada mais a fazer."), None);
    }

    #[test]
    fn a_question_split_by_colons_is_still_a_question() {
        // OpenClaw on gpt-4.1 (battery 2026-10-04)
        let r = req(&["Adicione median e pstdev, rode os testes até passar e faça um commit ao final."]);
        let text = "Todas as evoluções foram implementadas.\n\nA suíte completa passou.\n\nPronto para commit. Deseja uma mensagem específica ou uso algo como: \"feat: adiciona median e pstdev\"?";
        assert_eq!(redundant_confirmation(&r, text), Some("commit"));
    }

    #[test]
    fn an_action_reported_as_done_is_not_asked_again() {
        let r = req(&["corrija, rode os testes e faça commit"]);
        assert_eq!(redundant_confirmation(&r, "Commit realizado. Quer que eu rode os testes de novo?"), Some("tests"));
        assert_eq!(redundant_confirmation(&r, "Fiz o commit e os testes passaram. Posso ajudar em mais algo?"), None);
    }

    #[test]
    fn an_offer_of_an_ordered_action_counts_as_asking() {
        // OpenClaw on flex (battery 2026-10-04)
        let r = req(&["Adicione median, rode os testes e faça um commit ao final."]);
        let text = "A evolução foi concluída com sucesso. Tudo está funcionando conforme solicitado! Se quiser o commit ou mais alguma melhoria, só avisar.";
        assert_eq!(redundant_confirmation(&r, text), Some("commit"));
        assert_eq!(redundant_confirmation(&req(&["ajuste o README"]), text), None);
    }

    fn with_call(mut r: CanonicalRequest, command: &str) -> CanonicalRequest {
        let call = ToolCall { id: "c1".into(), name: "Bash".into(), arguments: serde_json::json!({"command": command}) };
        r.add("assistant", "", vec![call], vec![]);
        r.add("user", "", vec![], vec![ToolResult { call_id: "c1".into(), content: "ok".into(), name: String::new(), is_error: false }]);
        r
    }

    #[test]
    fn what_a_tool_call_already_did_is_not_asked_for() {
        let r = with_call(req(&["corrija e faça commit"]), "git add -A && git commit -m fix");
        assert_eq!(redundant_confirmation(&r, "Pronto. Quer que eu faça o commit?"), None);
        let r = with_call(req(&["corrija e faça commit"]), "uv run pytest -q");
        assert_eq!(redundant_confirmation(&r, "Pronto. Quer que eu faça o commit?"), Some("commit"));
    }

    #[test]
    fn a_final_report_without_the_ordered_commit() {
        // Claude Code on flex (battery 2026-10-04): the report says everything is done and never commits
        let resp = CanonicalResponse::default();
        let report = "Tudo pronto! As evoluções foram implementadas com sucesso. Toda a suíte passou: 14 passed. Se quiser revisar algum trecho, é só avisar!";
        let ordered = with_call(req(&["Adicione median, rode os testes até passar e faça um commit ao final."]), "uv run pytest -q");
        assert!(forgotten_commit(&ordered, &resp, report, &[]));
        let committed = with_call(ordered.clone(), "git commit -am median");
        assert!(!forgotten_commit(&committed, &resp, report, &[]));
        assert!(!forgotten_commit(&ordered, &resp, "Tudo pronto e commitado.", &[]));
        assert!(!forgotten_commit(&ordered, &resp, "Vou rodar os testes agora.", &[])); // not a final report
        assert!(!forgotten_commit(&req(&["Adicione median, mas não faça commit."]), &resp, report, &[]));
        // problems reported, a question asked, a condition the gateway cannot check
        assert!(!forgotten_commit(&ordered, &resp, "Implementado. Porém 2 testes falharam.", &[]));
        assert!(!forgotten_commit(&ordered, &resp, "Tudo pronto! Quer que eu revise algo?", &[]));
        let conditional = with_call(req(&["Faça commit só se todos os testes passarem."]), "uv run pytest -q");
        assert!(!forgotten_commit(&conditional, &resp, "Tudo pronto, mas 1 teste ainda falha.", &[]));
        // nor a summary of commits the user asked for
        assert!(!forgotten_commit(&req(&["Resuma os commits de hoje."]), &resp, "Resumo das mudanças: tudo pronto.", &[]));
        // ordered for an earlier task that was committed then: a later question does not bring it back
        let mut later = committed.clone();
        later.add_text("assistant", "Feito.");
        later.add_text("user", "Agora explique o que pstdev calcula.");
        assert!(!forgotten_commit(&later, &resp, "Pronto: pstdev é o desvio padrão populacional. Tudo concluído.", &[]));
    }

    #[test]
    fn git_trailers_do_not_hide_an_announcement() {
        // Copilot CLI on flex (battery 2026-10-04): the commit was announced, then the trailer Copilot asks for
        let text = "Tudo pronto! As funções foram implementadas e todos os testes passaram. Vou realizar o commit das alterações.\n\nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>";
        assert!(announces_without_acting(text));
        assert!(!announces_without_acting(
            "Commit feito: feat: median.\n\nCo-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
        ));
    }

    #[test]
    fn a_denied_ability_needs_a_tool_of_its_kind() {
        let resp = CanonicalResponse::default();
        let mut r = CanonicalRequest { tools: vec![ToolSpec::new("read_file", "Read a file", None, false)].into(), ..Default::default() };
        r.add_text("user", "x");
        assert!(false_incapacity(&r, &resp, "Não tenho acesso à sua conta do banco.", &[]).is_empty());
        assert!(false_incapacity(&r, &resp, "I don't have access to the internet.", &[]).is_empty()); // no web tool
        assert_eq!(false_incapacity(&r, &resp, "Não consigo ler arquivos do seu projeto.", &[]).len(), 1);
        r.tools = vec![ToolSpec::new("bash", "Run a command", None, false)].into();
        assert_eq!(false_incapacity(&r, &resp, "I can't browse the web.", &[]).len(), 1); // a terminal reaches the web
        assert!(false_incapacity(&r, &resp, "Não consigo acessar o site do banco porque ele exige login.", &[]).is_empty());
    }
}
