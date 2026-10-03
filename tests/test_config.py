"""Configuration: midir.toml + environment precedence, ${VAR} expansion, backends and models and their errors, the
pre-0.0.1 layout, environment-only shortcuts, per-model knobs."""
from __future__ import annotations

from pathlib import Path

import pytest

from conftest import MIDIR_TOML
from midir.backends import StackSpotBackend, create_backend
from midir.canonical import CanonicalRequest, ToolSpec
from midir.config import Config, ConfigError
from midir.emulation.prompt import render_prompt
from midir.gateway import Gateway

LEGACY_TOML = """
default = "gpt-4.1"

[stackspot]
realm = "acme"
client_id = "cid"
client_secret = "secret"

[limits]
requests_per_minute = 33

[[agents]]
name = "gpt-5.1"
agent_id = "A51"

[[agents]]
name = "gpt-4.1"
agent_id = "${MY_41}"
aliases = ["claude-haiku-4-5"]
"""


def test_file_values_and_env_override(make_cfg):
    cfg = make_cfg(MIDIR_TOML, env={"STACKSPOT_REALM": "from-env", "MIDIR_REQUESTS_PER_MINUTE": "42", "MIDIR_PORT": "19999"})
    b = cfg.backends["stackspot"]
    assert b.type == "stackspot" and b.limits.requests_per_minute == 42 and b.limits.max_concurrent == 4
    assert cfg.server.port == 19999
    ss = StackSpotBackend(b, cfg.env)
    assert ss.realm == "from-env" and ss.client_id == "cid"
    assert ss.idm_url == "http://idm.mock/from-env/oidc/oauth/token"
    assert ss.agent_url("A") == "http://agent.mock/v1/agent/A/chat"


def test_env_references_and_whitespace(make_cfg):
    cfg = make_cfg(MIDIR_TOML.replace('target = "AGENT41"', 'target = "${MY_41}"'), env={"MY_41": "  ID41 \n"})
    assert cfg.resolve("gpt-4.1").target == "ID41"


def test_unset_reference_is_a_clear_error(make_cfg):
    with pytest.raises(ConfigError, match="'target' is empty"):
        make_cfg(MIDIR_TOML.replace('target = "AGENT41"', 'target = "${NOPE}"'))


@pytest.mark.parametrize("bad,msg", [('name = "flex"\ntarget = "X"\n', "unique"), ('name = "z"\ntarget = "X"\nmatch = "("\n', "invalid 'match'"),
                                     ('name = "z"\ntarget = "X"\nbackend = "nope"\n', "'backend' must be one of")])
def test_model_errors(make_cfg, bad, msg):
    with pytest.raises(ConfigError, match=msg):
        make_cfg(MIDIR_TOML + "\n[[models]]\n" + bad)


def test_default_must_exist(make_cfg):
    with pytest.raises(ConfigError, match="not one of the configured models"):
        make_cfg(MIDIR_TOML.replace('default_model = "gpt-5.1"', 'default_model = "nope"'))


def test_default_is_the_first_model_when_unset(make_cfg):
    assert make_cfg(MIDIR_TOML.replace('default_model = "gpt-5.1"\n', "")).default.name == "gpt-5.1"


def test_invalid_toml(make_cfg):
    with pytest.raises(ConfigError, match="cannot read"):
        make_cfg("this is = = not toml")


def test_unknown_backend_type(make_cfg):
    cfg = make_cfg(MIDIR_TOML.replace('type = "stackspot"', 'type = "nope"'))
    with pytest.raises(ConfigError, match="unknown type 'nope'"):
        create_backend(cfg.backends["stackspot"])


def test_legacy_layout_still_works(make_cfg, caplog):
    cfg = make_cfg(LEGACY_TOML, env={"MY_41": "A41"})
    assert "pre-0.0.1 layout" in caplog.text
    assert cfg.default.name == "gpt-4.1" and cfg.resolve("claude-haiku-4-5").target == "A41"
    assert cfg.backends["stackspot"].limits.requests_per_minute == 33
    assert StackSpotBackend(cfg.backends["stackspot"], cfg.env).client_secret == "secret"
    assert {m.backend for m in cfg.models} == {"stackspot"}


def test_env_only_shortcuts(tmp_path):
    cfg = Config(env={"STACKSPOT_DEFAULT_AGENT_ID": " D ", "STACKSPOT_GPT_5_1_AGENT_ID": "A", "STACKSPOT_GPT_4_1_MINI_AGENT_ID": "B", "STACKSPOT_O3_MINI_AGENT_ID": "C"},
                 config_file=tmp_path / "missing.toml")
    assert cfg.default.target == "D" and cfg.source == "env"
    assert sorted(m.name for m in cfg.models) == ["gpt-4.1-mini", "gpt-5.1", "o3-mini"]
    assert cfg.resolve("unknown").target == "D"


def test_no_models_at_all_is_a_clear_error(tmp_path):
    with pytest.raises(ConfigError, match="no models configured"):
        Config(env={}, config_file=tmp_path / "missing.toml")


def test_per_model_knobs_reach_the_prompt(make_cfg):
    cfg = make_cfg(MIDIR_TOML.replace('aliases = ["claude-haiku-4-5"]', 'aliases = ["claude-haiku-4-5"]\ntail_reminder = false\ntool_desc_max = 5'))
    _, tail, desc = cfg.knobs(cfg.resolve("gpt-4.1"))
    req = CanonicalRequest(tools=[ToolSpec("t", "a very long description")])
    req.add("user", "x")
    prompt, _ = render_prompt(req, 10**6, tail_reminder=tail, tool_desc_max=desc)
    assert "<reminder>" not in prompt and '"description":"a ver…"' in prompt
    assert cfg.knobs(cfg.resolve("gpt-5.1"))[1:] == (True, 0)


def test_missing_credentials_message(tmp_path):
    cfg = Config(env={"STACKSPOT_DEFAULT_AGENT_ID": "x"}, config_file=Path("/nonexistent"), root=tmp_path)
    with pytest.raises(ConfigError, match="STACKSPOT_REALM"):
        Gateway(cfg).validate()
