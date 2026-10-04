//! Which build is running: a release, a development snapshot, a local image or a source build.
//!
//! Images get MIDIR_BUILD_CHANNEL, MIDIR_BUILD_COMMIT and MIDIR_BUILD_DATE baked in at build time (docker/Dockerfile,
//! from the workflows or `cargo xtask deploy`); a binary built in a git checkout carries the commit (build.rs). Only a
//! release reports the bare version; everything else carries semver build metadata, so a snapshot is never mistaken
//! for the release it came after:
//!
//! ```text
//! release   0.1.0
//! dev       0.1.0+dev.a817822              image built from main
//! local     0.1.0+local.a817822.dirty      image built on this machine (".dirty": uncommitted changes)
//! source    0.1.0+src.a817822              `cargo build` in a git checkout
//! unknown   0.1.0+unknown                  anything else
//! ```

use std::sync::OnceLock;

use serde_json::{Value, json};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: String,
    pub channel: String,
    pub commit: String,
    pub dirty: bool,
    pub date: String,
}

impl BuildInfo {
    pub fn full_version(&self) -> String {
        if self.channel == "release" {
            return self.version.clone();
        }
        let suffix = match self.channel.as_str() {
            "dev" => "dev",
            "local" => "local",
            "source" => "src",
            "unknown" => "unknown",
            other => other,
        };
        let mut parts = vec![suffix.to_string()];
        if !self.commit.is_empty() {
            parts.push(self.commit.clone());
        }
        if self.dirty {
            parts.push("dirty".into());
        }
        format!("{}+{}", self.version, parts.join("."))
    }

    /// Human description for the banner: empty for a release.
    pub fn label(&self) -> String {
        match self.channel.as_str() {
            "release" => String::new(),
            "dev" => "development build".into(),
            "local" => "local build".into(),
            "source" => "running from source".into(),
            "unknown" => "unknown build".into(),
            other => format!("{other} build"),
        }
    }

    pub fn as_json(&self) -> Value {
        json!({"version": self.full_version(), "channel": self.channel, "commit": if self.commit.is_empty() { Value::Null } else { json!(self.commit) },
               "dirty": self.dirty, "date": if self.date.is_empty() { Value::Null } else { json!(self.date) }})
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

pub fn detect() -> BuildInfo {
    let channel = env("MIDIR_BUILD_CHANNEL").to_lowercase();
    if !channel.is_empty() {
        let commit = env("MIDIR_BUILD_COMMIT");
        let dirty = commit.ends_with("-dirty");
        let commit: String = commit.strip_suffix("-dirty").unwrap_or(&commit).chars().take(7).collect();
        return BuildInfo { version: VERSION.into(), channel, commit, dirty, date: env("MIDIR_BUILD_DATE") };
    }
    let commit = env!("MIDIR_SOURCE_COMMIT");
    if !commit.is_empty() {
        let dirty = !env!("MIDIR_SOURCE_DIRTY").is_empty();
        return BuildInfo { version: VERSION.into(), channel: "source".into(), commit: commit.into(), dirty, date: String::new() };
    }
    BuildInfo { version: VERSION.into(), channel: "unknown".into(), commit: String::new(), dirty: false, date: String::new() }
}

static BUILD: OnceLock<BuildInfo> = OnceLock::new();

/// The build of this process, detected once (before .env is loaded, so .env cannot change it).
pub fn build() -> &'static BuildInfo {
    BUILD.get_or_init(detect)
}

pub fn full_version() -> String {
    build().full_version()
}
