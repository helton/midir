//! Build metadata for source builds: when the crate is built in a git checkout, the commit (and whether tracked files
//! had uncommitted changes) is embedded, so `cargo run` reports `X.Y.Z+src.<commit>[.dirty]`. Images do not need it:
//! they bake MIDIR_BUILD_CHANNEL / _COMMIT / _DATE into the environment (docker/Dockerfile), which takes precedence.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    let dirty = !commit.is_empty() && git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=MIDIR_SOURCE_COMMIT={commit}");
    println!("cargo:rustc-env=MIDIR_SOURCE_DIRTY={}", if dirty { "1" } else { "" });
    // a new commit, a staged change or an edited source file refreshes the metadata
    for path in [".git/HEAD", ".git/refs/heads", ".git/index", "src", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={path}");
    }
}
