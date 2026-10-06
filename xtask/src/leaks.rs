//! Scan every file git would publish for things that must never leave the machine: the real values of the secret and
//! account keys in .env, token-shaped strings, StackSpot agent ids, home-directory paths and e-mail addresses. Fails on
//! a hit. The file list is `git ls-files --cached --others --exclude-standard` (tracked + would-be added). Run it before
//! every push; CI runs it too.

use regex::Regex;

use crate::root;

pub fn run() -> Result<(), String> {
    let root = root();
    let patterns = [
        ("GitHub token", Regex::new(r"gh[opsu]_[A-Za-z0-9]{20,}").unwrap()),
        ("StackSpot agent id", Regex::new(r"\b01[A-Z0-9]{24}\b").unwrap()),
        ("home path", Regex::new(r"/home/[a-z][\w-]*/|/Users/[A-Za-z][\w-]*/|C:\\\\Users\\\\").unwrap()),
        // any case (a TOML `client_secret = "..."`), and values with the characters of base64 and JWTs
        (
            "secret assignment",
            Regex::new(r#"(?i)\b(client_secret|api_key|apikey|secret|token|password|passwd)[ \t]*[=:][ \t]*['"]?[A-Za-z0-9_\-+/=.]{16,}"#)
                .unwrap(),
        ),
        ("JWT", Regex::new(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}").unwrap()),
    ];
    // a quoted value in a configuration file that looks random (a pasted credential), whatever its key
    let quoted = Regex::new(r#"["']([A-Za-z0-9_\-+/=.]{24,})["']"#).unwrap();
    let config_file = |rel: &str| [".toml", ".yml", ".yaml", ".json", ".env", ".example"].iter().any(|ext| rel.ends_with(ext));
    // name@domain, but not crate@1.2.3 or action@v4 (pinned versions)
    let email = Regex::new(r"[\w.+-]+@([\w-]+\.[\w.-]+)").unwrap();
    let version_like = Regex::new(r"^v?[\d.]+$").unwrap();
    let allowed = Regex::new(r"@(anthropic\.com|users\.noreply\.github\.com|example\.(com|org)|midir\.local)$").unwrap();
    let sensitive_key = Regex::new(r"(?i)SECRET|TOKEN|KEY|PASSWORD|CLIENT_ID|AGENT_ID|REALM|GIST").unwrap();

    let mut secrets = vec![];
    if let Ok(text) = std::fs::read_to_string(root.join(".env")) {
        let line_re = Regex::new(r"^\s*(?:export\s+)?([A-Z0-9_]+)\s*=\s*(.*)$").unwrap();
        for line in text.lines() {
            if let Some(c) = line_re.captures(line) {
                let value = c[2].split(" #").next().unwrap().trim().trim_matches(|ch| ch == '"' || ch == '\'').to_string();
                if value.len() >= 8 && sensitive_key.is_match(&c[1]) {
                    secrets.push((c[1].to_string(), value)); // TZ, ports and the like are not secrets
                }
            }
        }
    }

    let out = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .current_dir(&root)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err("git ls-files failed (not a git checkout?)".into());
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
    files.sort();
    let (mut scanned, mut hits) = (0, vec![]);
    for rel in &files {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else { continue }; // binary or deleted
        scanned += 1;
        for (name, value) in &secrets {
            if text.contains(value.as_str()) {
                hits.push(format!("{rel}: contains the value of {name}"));
            }
        }
        for (what, re) in &patterns {
            if let Some(m) = re.find(&text) {
                hits.push(format!("{rel}: {what}: {:?}", m.as_str().chars().take(12).collect::<String>()));
            }
        }
        if config_file(rel) {
            for c in quoted.captures_iter(&text) {
                let v = &c[1];
                if looks_random(v) {
                    hits.push(format!("{rel}: a value that looks like a credential: {:?}...", v.chars().take(6).collect::<String>()));
                    break;
                }
            }
        }
        for c in email.captures_iter(&text) {
            let (m, domain) = (c.get(0).unwrap().as_str(), &c[1]);
            if !version_like.is_match(domain)
                && !allowed.is_match(m)
                && ![".rs", ".md", ".json", ".toml", ".yml"].iter().any(|ext| m.ends_with(ext))
            {
                hits.push(format!("{rel}: e-mail {m:?}"));
                break;
            }
        }
    }
    println!("scanned {scanned} files, {} finding(s)", hits.len());
    for h in &hits {
        println!("  {h}");
    }
    if hits.is_empty() { Ok(()) } else { Err("possible leaks found".into()) }
}

/// Letters and digits mixed, with the character spread of a random string (Shannon entropy above 4 bits per character):
/// a pasted key or token, not a word, a path, a version or a hash-free identifier.
fn looks_random(v: &str) -> bool {
    let has_digit = v.bytes().any(|b| b.is_ascii_digit());
    let has_upper = v.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = v.bytes().any(|b| b.is_ascii_lowercase());
    if !(has_digit && has_upper && has_lower) || v.contains("..") || v.starts_with('.') {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for c in v.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let n = v.chars().count() as f64;
    let entropy: f64 = counts.values().map(|&k| k as f64 / n).map(|p| -p * p.log2()).sum();
    entropy > 4.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_values_are_flagged_and_words_are_not() {
        assert!(looks_random("aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW"));
        assert!(!looks_random("deepseek-harness/cordis.patch.yml"));
        assert!(!looks_random("01M3NSAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(!looks_random("gen_ai_client_token_usage_total"));
    }
}
