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
        ("secret assignment", Regex::new(r#"(CLIENT_SECRET|API_KEY|TOKEN|PASSWORD)[ \t]*[=:][ \t]*['"]?[A-Za-z0-9_\-]{12,}"#).unwrap()),
    ];
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
                hits.push(format!("{rel}: {what}: {:?}", m.as_str().chars().take(40).collect::<String>()));
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
