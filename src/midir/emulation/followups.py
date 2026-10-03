"""Detectors for replies that need one automatic follow-up. Measured on 188 labeled real replies (precision 1.00):
- announce and stop: the reply promises an action ("I'll read the files") and ends without a tool call;
- redundant confirmation: it asks permission for, or leaves pending, an action the request already ordered;
- false incapacity: it denies an ability (web, files, shell) that a listed tool provides.
Also how two calls are compared when deduplicating the invalid-JSON repair. CAVEAT: heuristics, pt/en/es."""
from __future__ import annotations

import json
import re

from midir.canonical import CanonicalRequest, CanonicalResponse, ToolCall


class FollowUps:
    # "Announce and stop" detector, measured on 188 labeled real replies (dev/bench/promise): the last sentences decide,
    # closing offers and code blocks are ignored, a report of finished work counts as final, waiting for background
    # work is legitimate. Precision 1.00, recall 0.86 alone; with the prevention rule in TOOL_PROTOCOL, 59/64 promises
    # are resolved with no false follow-up. CAVEAT: heuristic, pt/en/es.
    CODE_RE = re.compile(r"```.*?(```|$)", re.S)
    SENT_RE = re.compile(r"(?<=[.!?:])\s+|\n+")
    OFFER_RE = re.compile(r"\b(se (quiser|preferir|precisar|desejar)|caso (queira|precise|deseje)|quer que eu|deseja que eu|posso (também |ainda )?(detalhar|ajudar|explicar|fazer|ajustar|mostrar|revisar|seguir)|é só (avisar|pedir|falar|me dizer)|me (avise|diga|fale)|fico à disposição|"
                          r"let me know|if you('d)? (want|like|need|prefer)|would you like|do you want|feel free|happy to help|just ask)\b", re.I)
    WAIT_RE = re.compile(r"\b(em segundo plano|in the background|assim que (ele|ela|eles|o \w+|a \w+|os \w+)?\s*(terminar|terminarem|concluir|finalizar)|quando (ele|o \w+) terminar|aguardando|vou aguardar|aguardo (o|a|os)|"
                         r"when (it|they|the \w+) (finish|finishes|complete|completes)|waiting for|once (it|they|the \w+) (finish|finishes|complete|completes))\b", re.I)
    INTENT_RE = re.compile(r"\b(vou|vamos|irei|iremos|farei|faremos|seguirei|prosseguirei|começarei|lerei|rodarei|executarei|criarei|editarei|ajustarei|corrigirei|verificarei|"
                           r"deixa(-| )me|deixe(-| )me|preciso (primeiro )?(ler|ver|verificar|checar|analisar|rodar|executar|criar|editar|abrir|inspecionar|olhar|entender|conferir|revisar|ajustar|corrigir)|"
                           r"segue (a )?(leitura|abaixo a execução)|começo (lendo|por|verificando|analisando)|começando (pela|pelo|por|com)|(em seguida|a seguir|depois disso|depois),? (eu )?[a-zà-ú]+o\b|primeiro,? (vou|preciso|leio|verifico)|agora,? (vou|leio|verifico|rodo|crio|edito)|em seguida,? (vou|leio|rodo|crio|edito)|a seguir,? (vou|leio|rodo)|"
                           r"i'll|i will|i'm going to|i am going to|let me|let's|let us|i need to (first )?(read|check|look|inspect|run|open|see|review|verify|understand|examine|update|fix|create|edit|confirm)|"
                           r"next,? i|now i('ll| will| need)|i('ll| will) (now|then|next|first|start|proceed)|proceeding (to|with)|i'll proceed|starting (with|by)|first,? i|going to (read|check|run|edit|create|update|fix|look|inspect)|"
                           r"voy a|vamos a|déjame|procederé|necesito (leer|revisar|ver|verificar))\b", re.I)
    GERUND_OPEN_RE = re.compile(r"^\W*(?:now |first |next |then |agora |primeiro |em seguida )?(running|reading|checking|creating|updating|editing|searching|looking|inspecting|listing|opening|fetching|writing|adding|fixing|installing|starting|examining|exploring|reviewing|scanning|verifying|testing|analyzing|investigating|gathering|"
                                r"lendo|verificando|rodando|executando|criando|analisando|buscando|procurando|abrindo|editando|atualizando|checando|investigando|inspecionando|começando|iniciando|listando|conferindo|aplicando|implementando|corrigindo|ajustando)\b", re.I)
    PAST_RE = re.compile(r"\b(implementei|criei|adicionei|ajustei|corrigi|atualizei|rodei|executei|li |verifiquei|fiz|concluí|finalizei|i('ve| have) (implemented|added|created|updated|fixed|run|completed|made)|implemented|added|created|updated|fixed|completed)\b", re.I)
    DONE_RE = re.compile(r"\b(conclu[íi]d[oa]s?|tudo pronto|pronto[!.]|feito[!.]|finalizad[oa]|implementad[oa]s?|com sucesso|todos os testes (passaram|passam|passando)|all tests pass(ed|ing)?|"
                         r"tarefa conclu[íi]da|task (is )?(complete|completed|done)|completed successfully|resumo (do que foi feito|das (mudanças|alterações))|summary of (the )?changes|changes made|alterações (realizadas|concluídas)|files changed|arquivos? (alterados?|modificados?|criados?))\b", re.I)
    PLAN_RE = re.compile(r"(o plano é|meu plano|plano( de ação)?:|passos:|etapas:|here'?s (the|my) plan|my plan|the plan is|plan:|steps:|next steps:|próximos passos:)", re.I)
    STEP_RE = re.compile(r"^\s*(\d+[.)]|[-*•])\s+")
    # "Redundant confirmation": the reply ends asking permission for an action the user's request already orders
    # (GPT-4.1: "Pronto para commit! Deseja que eu faça o commit?" after "... e faça um commit ao final").
    CONFIRM_RE = re.compile(r"\b(desej[ao]|quer|prefere|gostaria|posso|devo|sigo|confirma|autoriza|pode(mos)?|shall i|should i|do you want|would you like|want me to|prefer|can i|may i|ok to|go ahead)\b", re.I)
    PENDING_RE = re.compile(r"\b(o que falta|falta(m)?( apenas| só| somente)?|faltando|pendente|resta(m)?( apenas| só)?|pronto para( o)?|pronta para|"
                            r"what'?s left|left to do|remaining( step)?s?:?|still (need|have) to|ready (to|for))\b", re.I)
    NOTHING_LEFT_RE = re.compile(r"\b(nada (mais )?(pendente|falta|a fazer)|n[ãa]o (falta|resta|h[áa]) (nada|pend[êe]ncias?)|sem pend[êe]ncias|nothing (left|remaining|pending)|no (remaining|pending) (steps|work|items))\b", re.I)
    ACTION_DONE = {"commit": r"(commitad[oa]s?|commit (feito|realizado|criado|conclu[íi]do)|committed|fiz o commit|made the commit)", "push": r"(push (feito|realizado)|pushed)",
                   "tests": r"(testes? (passaram|passando|passam|rodad[oa]s?)|tests? (pass(ed)?|ran))"}
    ACTIONS = {"commit": r"commit", "push": r"\bpush", "merge": r"\bmerge", "deploy": r"deploy", "tests": r"(rod(e|ar)|execut(e|ar)|run)\b.{0,30}(test|pytest|suíte|suite)", "install": r"instal"}
    # "false incapacity": the model denies an ability that a listed tool provides (CAVEAT: heuristic, pt/en/es)
    INCAPACITY_RE = re.compile(r"(não (tenho|possuo) acesso|não consigo (acessar|abrir|ler|navegar|buscar|consultar)|sem acesso (à|a) (internet|web)|(don't|do not|cannot|can't) (have )?(access|open|browse|fetch|read|reach)|no tengo acceso|no puedo acceder)", re.I)
    TOOL_ABILITY_RE = re.compile(r"(fetch|web|http|url|browse|search|page|read|file|shell|terminal|command|exec|bash)", re.I)

    TARGET_KEYS = ("path", "file_path", "filePath", "filepath", "file", "filename", "target", "command", "cmd", "url", "uri", "query", "pattern")

    @classmethod
    def call_target(cls, c: ToolCall) -> str:
        """What a call acts on (its path, command, url...), for duplicate detection; the whole arguments when none."""
        args = c.arguments if isinstance(c.arguments, dict) else {}
        for k in cls.TARGET_KEYS:
            if isinstance(args.get(k), str):
                return f"{k}={args[k]}"
        return json.dumps(c.arguments, sort_keys=True, ensure_ascii=False)

    @classmethod
    def call_key(cls, c: ToolCall) -> tuple[str, str]:
        return c.name, cls.call_target(c)

    @classmethod
    def false_incapacity(cls, req: CanonicalRequest, resp: CanonicalResponse, text: str, calls: list) -> bool:
        if calls or not req.tools or req.tool_choice == "none" or resp.finish != "stop":
            return False
        t = text.strip()
        if not t or len(t) > 800 or not cls.INCAPACITY_RE.search(t):
            return False
        return any(cls.TOOL_ABILITY_RE.search(tool.name + " " + tool.description) for tool in req.tools)

    @classmethod
    def promise_only(cls, req: CanonicalRequest, resp: CanonicalResponse, text: str, calls: list) -> bool:
        if calls or not req.tools or req.tool_choice == "none" or resp.finish != "stop":
            return False
        return cls.announces_without_acting(text) or bool(cls.redundant_confirmation(req, text))

    @classmethod
    def redundant_confirmation(cls, req: CanonicalRequest, text: str) -> str | None:
        """The action name when the reply's closing question asks permission for, or its closing lines report as still
        pending, something the first user request already orders (only a short list of unambiguous actions: commit,
        push, merge, deploy, tests, install)."""
        prose = cls.CODE_RE.sub(" ", text.strip())
        sents = [x.strip() for x in cls.SENT_RE.split(prose) if x and x.strip()]
        if not sents:
            return None
        asks = sents[-1].endswith("?") and cls.CONFIRM_RE.search(sents[-1])
        # or it reports the ordered action as still pending: "Pronto para commit. O que falta: apenas o commit."
        tail = " ".join(sents[-3:])
        pending = cls.PENDING_RE.search(tail) and not cls.NOTHING_LEFT_RE.search(tail)
        if not asks and not pending:
            return None
        ask = " ".join(sents[-3:] if pending else sents[-2:]).lower()  # the action is often named in the sentence before
        request = next((t.text for t in req.turns if t.role == "user" and t.text), "").lower()
        for name, pat in cls.ACTIONS.items():
            if re.search(pat, ask) and re.search(pat, request) and not (pending and not asks and re.search(cls.ACTION_DONE.get(name, r"$^"), ask)):
                return name
        return None

    @classmethod
    def announces_without_acting(cls, text: str) -> bool:
        t = text.strip()
        if not t:
            return False
        prose = cls.CODE_RE.sub(" ", t)
        sents = [x.strip() for x in cls.SENT_RE.split(prose) if x and x.strip() and re.search(r"\w", x)]
        sents = [x for x in sents if not cls.OFFER_RE.search(x)]
        if not sents or sents[-1].rstrip().endswith("?"):
            return False  # nothing left after closing offers, or it asks the user something
        if cls.WAIT_RE.search(" ".join(sents[-4:])):
            return False  # waiting for background work it already started
        if (cls.INTENT_RE.search(" ".join(sents[-2:])) or any(cls.GERUND_OPEN_RE.match(x) for x in sents[-2:])) and not cls.PAST_RE.search(sents[-1]):
            return True  # the last thing it says is an action it is about to do
        if prose.rstrip().endswith(":") and not t.endswith("```"):
            return True  # "Vou ler os arquivos:" and then nothing
        if cls.DONE_RE.search(prose):
            return False  # a report of completed work with no pending action at the end
        if cls.INTENT_RE.search(sents[0]) or cls.GERUND_OPEN_RE.match(sents[0]):
            return True  # opens with an announcement and never acts
        lines = [ln for ln in prose.splitlines() if ln.strip()]
        plan_at = next((k for k, ln in enumerate(lines) if cls.PLAN_RE.search(ln)), None)
        # a plan (opening it, or "Próximos passos:" further down) followed by at least two steps, with nothing done
        return plan_at is not None and sum(1 for ln in lines[plan_at + 1:] if cls.STEP_RE.match(ln)) >= 2
