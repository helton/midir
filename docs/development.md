# Development

Everything is Rust: rustup installs the toolchain pinned in `rust-toolchain.toml` (Rust 1.99); Docker builds the
images. Repository tasks are a workspace member, `xtask`, run through a Cargo alias. How the code is organized:
[architecture.md](architecture.md).

```bash
cargo run --release                         # build and run from source (reads .env and config/midir.toml)
cargo test --profile ci                     # unit, property and black-box tests, as CI runs them (~1 min)
cargo clippy --all-targets -- -D warnings   # lints, as CI runs them (also: cargo fmt --check, cargo deny check)
cargo xtask smoke                           # live acceptance against a running Midir (25 checks, real backend requests)
cargo xtask check-leaks                     # scan what git would publish for secrets, agent ids, home paths, e-mails
cargo xtask deploy standalone|full          # build the image here and (re)start the compose stack
cargo xtask                                 # all tasks
```

## Tests

The regression suite is black-box: each test starts the binary Cargo built as a process, in front of a scripted
StackSpot (nothing leaves the machine), and checks what clients see over HTTP; unit and property tests cover the
parsers of untrusted input.

Opt-in tests, for material that stays out of the repository: `--test replay -- --ignored` (real client requests,
`MIDIR_CAPTURES=<dir>`), `--test followups_corpus -- --ignored` (labeled model replies, `MIDIR_PROMISE_CORPUS=<dir>`)
and `--test mitm -- --ignored` (the observability proxy streams SSE; needs mitmproxy 12: `mitmdump` on PATH or
`MIDIR_MITMDUMP=<path>`). The one file in another language is `docker/mitm/sse_stream.py`: a mitmproxy addon for the
observability stack (mitmproxy only loads Python addons).

## Repository layout

| Path | Contents |
|---|---|
| `src/` | the gateway: `protocols/`, `emulation/`, `backends/`, `gateway.rs`, `app.rs`, `config.rs`, `store.rs`, `telemetry.rs`, `main.rs` ([architecture](architecture.md)) |
| `tests/` | black-box regression suite: the binary as a process, a scripted backend (`tests/common/`) |
| `config/` | `midir.example.toml` (your `midir.toml` is not versioned) |
| `docker/` | `Dockerfile`, compose files, Grafana provisioning, `data/` (runtime, not versioned) |
| `examples/clients/` | client configurations |
| `docs/` | guides, architecture, backend notes, agent brief, changelog |
| `xtask/` | repository tasks (`cargo xtask`): version, bump, check-leaks, smoke, deploy |
| `.github/workflows/` | CI: tests, images and releases to ghcr.io from `main` |

## Branches and releases

Trunk-based: everything lands on `main`, directly or through a pull request from a feature branch.
[ci.yml](../.github/workflows/ci.yml) runs the tests on every pull request and push; then:

| Push to `main` | Image on `ghcr.io/helton/midir` (amd64 + arm64) |
|---|---|
| any | `dev` (the latest `main` build) and `sha-<commit>` |
| whose `Cargo.toml` version has no `vX.Y.Z` tag yet | the same build also as `X.Y.Z`, `X.Y` and `latest`, then the tag `vX.Y.Z` and a GitHub release with that version's section of the CHANGELOG |

Tags are never created by hand. Changes go under `## Unreleased` in [CHANGELOG.md](CHANGELOG.md); to release:

```bash
cargo xtask bump [patch|minor|major|X.Y.Z]  # Cargo.toml, Cargo.lock and the default image tag in docker/compose.yml
# rename "## Unreleased" in docs/CHANGELOG.md to "## X.Y.Z (date)", then commit
git push                                     # CI tests, publishes X.Y.Z and latest, tags vX.Y.Z
```

A version that is not released and has no CHANGELOG section fails CI, on `main` and in pull requests. The version
lives only in `Cargo.toml`.
