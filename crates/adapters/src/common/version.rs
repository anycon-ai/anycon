//! 从 `--version` 的输出里抠 semver。

use semver::Version;

/// 在输出里找第一个形如 `x.y.z`（可带 `-pre`、前缀 `v`）的词。
/// `2.1.293 (Claude Code)`、`codex-cli 0.162.1`、`v1.2.3-beta.1` 都能解析；`x.y` 补成 `x.y.0`。
pub fn parse_semver(output: &str) -> Option<Version> {
    for raw in output.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
        let tok = raw.trim_start_matches('v').trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if tok.is_empty() || !tok.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(v) = Version::parse(tok) {
            return Some(v);
        }
        // `x.y` 补成 `x.y.0`
        let core = tok.split(['-', '+']).next().unwrap_or(tok);
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() == 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
            if let Ok(v) = Version::parse(&format!("{core}.0")) {
                return Some(v);
            }
        }
    }
    None
}

/// `have >= min`（`min` 写成 `"2.1.0"` 这样的字面量）。`min` 写错时返回 true（不挡路）。
pub fn version_at_least(have: &Version, min: &str) -> bool {
    match Version::parse(min) {
        Ok(m) => have >= &m,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_shapes() {
        assert_eq!(parse_semver("2.1.293 (Claude Code)").unwrap().to_string(), "2.1.293");
        assert_eq!(parse_semver("codex-cli 0.162.1\n").unwrap().to_string(), "0.162.1");
        assert_eq!(parse_semver("v1.2.3-beta.1").unwrap().to_string(), "1.2.3-beta.1");
        assert_eq!(parse_semver("tool 3.4").unwrap().to_string(), "3.4.0");
        assert!(parse_semver("no version here").is_none());
        assert!(parse_semver("").is_none());
    }

    #[test]
    fn min_version_gate() {
        let v = parse_semver("2.0.99").unwrap();
        assert!(!version_at_least(&v, "2.1.0"));
        assert!(version_at_least(&parse_semver("2.1.0").unwrap(), "2.1.0"));
        assert!(version_at_least(&parse_semver("3.0.0").unwrap(), "2.1.0"));
    }
}
