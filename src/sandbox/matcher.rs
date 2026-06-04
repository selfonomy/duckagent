use std::path::{Path, PathBuf};

pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = normalize_slashes(pattern.trim());
    let value = normalize_slashes(value.trim());
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == "**" {
        return true;
    }
    let Ok(regex) = regex::Regex::new(&format!("^{}$", glob_to_regex(&pattern))) else {
        return false;
    };
    regex.is_match(&value)
}

pub fn any_glob_matches(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| glob_matches(pattern, value))
}

pub fn path_pattern_matches(pattern: &str, path: &Path, workspace: &Path) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == "**" {
        return true;
    }

    let absolute_path = normalize_path_text(path);
    let workspace = normalize_path_text(workspace);
    if pattern == "." {
        return absolute_path == workspace || absolute_path.starts_with(&(workspace + "/"));
    }

    let expanded = crate::sandbox::path_vars::expand_path_vars(pattern, Path::new(&workspace));
    if crate::sandbox::path_vars::is_absolute_path_text(&expanded) {
        if !contains_glob(&expanded) {
            return path_root_matches(&expanded, &absolute_path);
        }
        return glob_matches(&expanded, &absolute_path);
    }

    let relative = path
        .strip_prefix(Path::new(&workspace))
        .ok()
        .map(normalize_path_text)
        .unwrap_or_else(|| absolute_path.clone());
    if !contains_glob(&expanded) {
        return path_root_matches(&expanded, &relative)
            || path_root_matches(&expanded, &absolute_path);
    }
    glob_matches(&expanded, &relative) || glob_matches(&expanded, &absolute_path)
}

pub fn normalize_path_text(path: &Path) -> String {
    normalize_slashes(&lexical_normalize(path).to_string_lossy())
}

pub fn normalize_slashes(value: &str) -> String {
    strip_windows_verbatim_prefix(&value.replace('\\', "/"))
}

fn strip_windows_verbatim_prefix(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("//?/UNC/") {
        return format!("//{rest}");
    }
    if let Some(rest) = value.strip_prefix("//?/") {
        return rest.to_string();
    }
    if let Some(rest) = value.strip_prefix("//./") {
        return rest.to_string();
    }
    value.to_string()
}

fn contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn path_root_matches(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn glob_to_regex(pattern: &str) -> String {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') && chars.get(i + 2) == Some(&'/') => {
                out.push_str("(?:.*/)?");
                i += 3;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                out.push_str(".*");
                i += 2;
            }
            '*' => {
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            ch => {
                out.push_str(&regex::escape(&ch.to_string()));
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_supports_double_star_prefix() {
        assert!(glob_matches("**/.env", ".env"));
        assert!(glob_matches("**/.env", "app/.env"));
        assert!(glob_matches("**/.git/**", ".git/config"));
        assert!(glob_matches("**/.git/**", "repo/.git/config"));
        assert!(glob_matches("context7_*", "context7_search"));
        assert!(!glob_matches("context7_*", "other_search"));
    }

    #[test]
    fn single_star_does_not_cross_path_separators() {
        assert!(glob_matches("*.pem", "root.pem"));
        assert!(!glob_matches("*.pem", "certs/root.pem"));
        assert!(glob_matches("certs/*.pem", "certs/root.pem"));
        assert!(!glob_matches("certs/*.pem", "certs/nested/root.pem"));
        assert!(glob_matches("**/*.pem", "certs/nested/root.pem"));
    }

    #[test]
    fn path_patterns_keep_single_star_non_recursive() {
        let workspace = Path::new("/tmp/work");
        assert!(path_pattern_matches(
            "*.pem",
            Path::new("/tmp/work/root.pem"),
            workspace
        ));
        assert!(!path_pattern_matches(
            "*.pem",
            Path::new("/tmp/work/certs/root.pem"),
            workspace
        ));
        assert!(path_pattern_matches(
            "certs/*.pem",
            Path::new("/tmp/work/certs/root.pem"),
            workspace
        ));
        assert!(!path_pattern_matches(
            "certs/*.pem",
            Path::new("/tmp/work/certs/nested/root.pem"),
            workspace
        ));
    }

    #[test]
    fn normalize_slashes_strips_windows_verbatim_prefixes() {
        assert_eq!(
            normalize_slashes(r"\\?\D:\repo\file.txt"),
            "D:/repo/file.txt"
        );
        assert_eq!(
            normalize_slashes(r"\\?\UNC\server\share\file.txt"),
            "//server/share/file.txt"
        );
        assert_eq!(
            normalize_slashes(r"\\.\D:\repo\file.txt"),
            "D:/repo/file.txt"
        );
    }

    #[test]
    fn dot_path_pattern_matches_workspace_descendants() {
        let workspace = Path::new("/tmp/work");
        assert!(path_pattern_matches(
            ".",
            Path::new("/tmp/work/src/main.rs"),
            workspace
        ));
        assert!(!path_pattern_matches(
            ".",
            Path::new("/tmp/other/main.rs"),
            workspace
        ));
    }

    #[test]
    fn literal_absolute_path_matches_descendants() {
        let workspace = Path::new("/tmp/work");
        assert!(path_pattern_matches(
            "/foo",
            Path::new("/foo/test/1.md"),
            workspace
        ));
        assert!(path_pattern_matches("/foo", Path::new("/foo"), workspace));
        assert!(!path_pattern_matches(
            "/foo",
            Path::new("/foobar/test.md"),
            workspace
        ));
    }

    #[test]
    fn literal_relative_path_matches_descendants_inside_workspace() {
        let workspace = Path::new("/tmp/work");
        assert!(path_pattern_matches(
            "docs",
            Path::new("/tmp/work/docs/guide.md"),
            workspace
        ));
        assert!(!path_pattern_matches(
            "docs",
            Path::new("/tmp/work/docs-old/guide.md"),
            workspace
        ));
    }

    #[test]
    fn path_patterns_expand_cwd_variable_before_matching() {
        let workspace = Path::new("/tmp/work");
        assert!(path_pattern_matches(
            "$CWD/generated/**",
            Path::new("/tmp/work/generated/out.txt"),
            workspace
        ));
        assert!(!path_pattern_matches(
            "$CWD/generated/**",
            Path::new("/tmp/other/generated/out.txt"),
            workspace
        ));
    }

    #[test]
    fn path_patterns_treat_expanded_windows_drive_as_absolute_text() {
        let workspace = Path::new("C:/repo");
        assert!(path_pattern_matches(
            "$CWD/generated/**",
            Path::new("C:/repo/generated/out.txt"),
            workspace
        ));
        assert!(!path_pattern_matches(
            "$CWD/generated/**",
            Path::new("C:/other/generated/out.txt"),
            workspace
        ));
    }
}
