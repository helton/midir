//! The version lives in Cargo.toml (the binary embeds it at build time); a bump also updates Cargo.lock and the default
//! image tag in docker/compose.yml.

use regex::Regex;

use crate::root;

fn cargo_re() -> Regex {
    Regex::new(r#"(?s)(\[package\][^\[]*?\nversion\s*=\s*")([^"]+)(")"#).unwrap()
}

fn lock_re() -> Regex {
    Regex::new(r#"(\[\[package\]\]\nname = "midir"\nversion = ")([^"]+)(")"#).unwrap()
}

fn image_re() -> Regex {
    Regex::new(r"(image:\s*\$\{MIDIR_IMAGE:-[^}]*?midir:)([^}\s]+)(\})").unwrap()
}

pub fn current() -> Result<String, String> {
    let text = std::fs::read_to_string(root().join("Cargo.toml")).map_err(|e| e.to_string())?;
    cargo_re().captures(&text).map(|c| c[2].to_string()).ok_or_else(|| "Cargo.toml: no version in [package]".into())
}

pub fn show() -> Result<(), String> {
    println!("{}", current()?);
    Ok(())
}

fn bumped(old: &str, part: &str) -> Result<String, String> {
    if Regex::new(r"^\d+\.\d+\.\d+([.-]?[0-9A-Za-z.]+)?$").unwrap().is_match(part) {
        return Ok(part.to_string());
    }
    let nums: Vec<u64> = Regex::new(r"^(\d+)\.(\d+)\.(\d+)")
        .unwrap()
        .captures(old)
        .ok_or("current version is not X.Y.Z")?
        .iter()
        .skip(1)
        .map(|m| m.unwrap().as_str().parse().unwrap())
        .collect();
    let (major, minor, patch) = (nums[0], nums[1], nums[2]);
    Ok(match part {
        "major" => format!("{}.0.0", major + 1),
        "minor" => format!("{major}.{}.0", minor + 1),
        "patch" => format!("{major}.{minor}.{}", patch + 1),
        other => return Err(format!("usage: cargo xtask bump [patch|minor|major|X.Y.Z] (got {other:?})")),
    })
}

fn replace(file: &str, re: &Regex, new: &str) -> Result<(), String> {
    let path = root().join(file);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
    if !re.is_match(&text) {
        return Err(format!("{file}: version not found"));
    }
    let out = re.replacen(&text, 1, |c: &regex::Captures| format!("{}{new}{}", &c[1], &c[3]));
    std::fs::write(&path, out.as_ref()).map_err(|e| format!("{file}: {e}"))
}

pub fn bump(part: &str) -> Result<(), String> {
    let old = current()?;
    let new = bumped(&old, part)?;
    replace("Cargo.toml", &cargo_re(), &new)?;
    replace("Cargo.lock", &lock_re(), &new)?;
    replace("docker/compose.yml", &image_re(), &new)?;
    println!("{old} -> {new}: Cargo.toml, Cargo.lock, docker/compose.yml");
    Ok(())
}
