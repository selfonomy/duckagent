use crate::sandbox::matcher::normalize_path_text;
use std::path::{Path, PathBuf};

pub fn expand_path_vars(template: &str, cwd: &Path) -> String {
    let input = expand_home_prefix(template);
    expand_env_vars(&input, cwd)
}

pub fn resolve_config_path(template: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path_vars(template, cwd);
    if is_absolute_path_text(&expanded) {
        return PathBuf::from(expanded);
    }
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub fn is_absolute_path_text(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("\\\\")
        || value.as_bytes().get(0..3).is_some_and(|bytes| {
            bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\')
        })
}

fn expand_home_prefix(template: &str) -> String {
    if template == "~" {
        return dirs::home_dir()
            .map(|home| normalize_path_text(&home))
            .unwrap_or_else(|| template.to_string());
    }
    if let Some(rest) = template.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| normalize_path_text(&home.join(rest)))
            .unwrap_or_else(|| template.to_string());
    }
    template.to_string()
}

fn expand_env_vars(input: &str, cwd: &Path) -> String {
    let chars = input.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] != '$' {
            out.push(chars[idx]);
            idx += 1;
            continue;
        }
        if chars.get(idx + 1) == Some(&'{') {
            if let Some(end) = chars[idx + 2..].iter().position(|ch| *ch == '}') {
                let name = chars[idx + 2..idx + 2 + end].iter().collect::<String>();
                if let Some(value) = env_value(&name, cwd) {
                    out.push_str(&value);
                } else {
                    out.push('$');
                    out.push('{');
                    out.push_str(&name);
                    out.push('}');
                }
                idx += end + 3;
                continue;
            }
        }
        let start = idx + 1;
        let mut end = start;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if end == start {
            out.push('$');
            idx += 1;
            continue;
        }
        let name = chars[start..end].iter().collect::<String>();
        if let Some(value) = env_value(&name, cwd) {
            out.push_str(&value);
        } else {
            out.push('$');
            out.push_str(&name);
        }
        idx = end;
    }
    out
}

fn env_value(name: &str, cwd: &Path) -> Option<String> {
    match name {
        "CWD" => Some(normalize_path_text(cwd)),
        "HOME" => dirs::home_dir().map(|home| normalize_path_text(&home)),
        "TMPDIR" | "TEMP" | "TMP" => Some(normalize_path_text(&std::env::temp_dir())),
        other => std::env::var(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_cwd_and_tmpdir() {
        let cwd = Path::new("/tmp/work");
        let expanded = expand_path_vars("$CWD/cache:$TMPDIR", cwd);
        assert!(expanded.starts_with("/tmp/work/cache:"));
        assert!(!expanded.contains("$TMPDIR"));
    }

    #[test]
    fn expands_braced_cwd_without_prefix_collisions() {
        let cwd = Path::new("/repo");
        assert_eq!(expand_path_vars("${CWD}/cache", cwd), "/repo/cache");
        assert_eq!(
            expand_path_vars("$CWD_CACHE/cache", cwd),
            "$CWD_CACHE/cache"
        );
    }

    #[test]
    fn tmp_aliases_use_platform_temp_dir() {
        let tmp = normalize_path_text(&std::env::temp_dir());
        for name in ["TMPDIR", "TEMP", "TMP"] {
            assert_eq!(
                expand_path_vars(&format!("${name}/duckagent"), Path::new("/work")),
                format!("{tmp}/duckagent")
            );
        }
    }

    #[test]
    fn leaves_unknown_vars_literal() {
        assert_eq!(
            expand_path_vars("$DUCKAGENT_TEST_UNKNOWN/foo", Path::new("/work")),
            "$DUCKAGENT_TEST_UNKNOWN/foo"
        );
    }

    #[test]
    fn resolves_expanded_windows_drive_paths_as_absolute_text() {
        assert_eq!(
            resolve_config_path("$CWD/cache", Path::new("C:/repo"))
                .to_string_lossy()
                .replace('\\', "/"),
            "C:/repo/cache"
        );
    }
}
