//! Repository tasks: `cargo xtask <task>` (the alias lives in .cargo/config.toml).
//!
//!     cargo xtask version                       show the version (Cargo.toml)
//!     cargo xtask bump [patch|minor|major|X.Y.Z] bump it in Cargo.toml, Cargo.lock and docker/compose.yml
//!     cargo xtask check-leaks                   scan what git would publish for secrets, agent ids, home paths, e-mails
//!     cargo xtask smoke [--base URL] [--only chat|responses|messages]
//!                                               live acceptance against a running Midir (real backend requests)
//!     cargo xtask deploy [standalone|full]      build the image here and (re)start the compose stack

mod leaks;
mod smoke;
mod version;

use std::path::PathBuf;
use std::process::ExitCode;

/// The repository root (the parent of this crate).
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn usage() -> ExitCode {
    eprintln!(
        "{}",
        include_str!("main.rs")
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n")
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = &args.get(1..).unwrap_or_default().to_vec();
    let result = match args.first().map(String::as_str) {
        Some("version") => version::show(),
        Some("bump") => version::bump(rest.first().map(String::as_str).unwrap_or("patch")),
        Some("check-leaks") => leaks::run(),
        Some("smoke") => smoke::run(rest),
        Some("deploy") => deploy(rest.first().map(String::as_str).unwrap_or("standalone")),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn sh(cmd: &str, args: &[&str], env: &[(&str, String)]) -> Result<String, String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .current_dir(root())
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_inherit(cmd: &str, args: &[&str], env: &[(&str, String)]) -> Result<(), String> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .current_dir(root())
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .status()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd} {} failed ({status})", args.join(" "))) }
}

/// Recreate the stack from this checkout: the image is built locally (docker/compose.build.yml) and tagged with the
/// commit (`-dirty` with uncommitted changes) so `/health` says which build runs. Bind-mounted data in docker/data
/// survives; its folders are created first so they belong to you and not to root.
fn deploy(kind: &str) -> Result<(), String> {
    let full = match kind {
        "standalone" => false,
        "full" => true,
        other => return Err(format!("deploy: unknown stack {other:?} (standalone or full)")),
    };
    let commit = sh("git", &["rev-parse", "--short=7", "HEAD"], &[])?;
    let dirty = !sh("git", &["status", "--porcelain", "--untracked-files=no"], &[])?.is_empty();
    let date = sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"], &[])?;
    let env = [("MIDIR_BUILD_COMMIT", format!("{commit}{}", if dirty { "-dirty" } else { "" })), ("MIDIR_BUILD_DATE", date)];
    let mut dirs = vec!["docker/data/gateway"];
    if full {
        dirs.extend(["docker/data/mitm", "docker/data/lgtm"]);
    }
    for d in dirs {
        std::fs::create_dir_all(root().join(d)).map_err(|e| format!("{d}: {e}"))?;
    }
    run_inherit(
        "docker",
        &["compose", "-f", "docker/compose.yml", "-f", "docker/compose.observability.yml", "down", "--remove-orphans"],
        &env,
    )?;
    let mut up = vec!["compose", "-f", "docker/compose.yml", "-f", "docker/compose.build.yml"];
    if full {
        up.extend(["-f", "docker/compose.observability.yml"]);
    }
    up.extend(["up", "--build", "-d", "--remove-orphans"]);
    run_inherit("docker", &up, &env)
}
