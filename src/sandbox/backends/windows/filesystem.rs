use crate::sandbox::backends::windows::acl;
use crate::sandbox::backends::windows_plan::{
    WindowsFileSystemPlan, WindowsPathAccess, WindowsProtectedPath,
};
use crate::sandbox::config::FileAccess;
use crate::sandbox::matcher::{glob_matches, normalize_path_text};
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::env;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn apply_filesystem_plan(
    plan: &WindowsFileSystemPlan,
    allow_sids: &[*mut c_void],
    deny_sids: &[*mut c_void],
) -> Result<()> {
    let total_start = Instant::now();
    if allow_sids.is_empty() {
        bail!("Windows filesystem sandbox requires at least one allow SID");
    }
    if deny_sids.is_empty() {
        bail!("Windows filesystem sandbox requires at least one deny SID");
    }

    let read_roots = existing_paths(&plan.read_roots);
    let write_roots = existing_paths(&plan.write_roots);
    let grant_start = Instant::now();
    for root in &read_roots {
        for sid in allow_sids {
            unsafe {
                acl::ensure_allow_ace(root, *sid, acl::READ_MASK)
                    .with_context(|| format!("grant read ACL on {}", root.display()))?;
            }
        }
    }
    for root in &write_roots {
        for sid in allow_sids {
            unsafe {
                acl::ensure_allow_ace(root, *sid, acl::WRITE_MASK)
                    .with_context(|| format!("grant write ACL on {}", root.display()))?;
            }
        }
    }
    timing_log(
        "windows filesystem allow ACL grants",
        grant_start,
        format!(
            "read_roots={}, write_roots={}",
            read_roots.len(),
            write_roots.len()
        ),
    );

    let materialize_start = Instant::now();
    let protected_paths = materialize_protected_paths(plan)?;
    timing_log(
        "windows filesystem materialize protected paths",
        materialize_start,
        format!(
            "rules={}, protected_paths={}",
            plan.protected_paths.len(),
            protected_paths.len()
        ),
    );
    let deny_start = Instant::now();
    for protected in protected_paths {
        let mask = match (protected.deny_read, protected.deny_write) {
            (true, true) => acl::ALL_MASK,
            (true, false) => acl::READ_MASK,
            (false, true) => acl::WRITE_MASK,
            (false, false) => continue,
        };
        for sid in deny_sids {
            unsafe {
                acl::ensure_deny_ace(&protected.path, *sid, mask)
                    .with_context(|| format!("apply deny ACL on {}", protected.path.display()))?;
            }
        }
    }
    timing_log(
        "windows filesystem deny ACL grants",
        deny_start,
        String::new(),
    );
    timing_log("windows filesystem apply total", total_start, String::new());
    Ok(())
}

#[derive(Debug, Clone)]
struct ResolvedProtectedPath {
    path: PathBuf,
    deny_read: bool,
    deny_write: bool,
}

fn existing_paths(paths: &[WindowsPathAccess]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| !path.glob && path.path != "*")
        .map(|path| PathBuf::from(&path.path))
        .filter(|path| path.exists())
        .collect()
}

fn materialize_protected_paths(plan: &WindowsFileSystemPlan) -> Result<Vec<ResolvedProtectedPath>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let access_roots = access_roots(plan);
    let subtree_patterns = protected_subtree_patterns(plan);

    for rule in &plan.protected_paths {
        if let Some(pattern) = subtree_selector_pattern(&rule.path) {
            for root in &access_roots {
                if root.is_dir() {
                    materialize_protected_subtree_roots(&mut out, &mut seen, rule, root, &pattern)?;
                }
            }
        } else if rule.glob
            && materialize_non_recursive_glob(&mut out, &mut seen, rule, &access_roots)?
        {
            // Root-level and literal-parent globs like `*.pem` or `certs/*.pem`
            // should not recursively scan large workspaces such as target/.
        } else if rule.glob {
            for root in &access_roots {
                if !root.is_dir() {
                    continue;
                }
                let mut builder = protected_walk_builder(root);
                let subtree_patterns = subtree_patterns.clone();
                builder.filter_entry(move |entry| {
                    !entry_is_protected_subtree(entry.path(), &subtree_patterns)
                });
                for entry in builder.build() {
                    let entry = entry.with_context(|| format!("walk {}", root.display()))?;
                    let path = entry.path();
                    if path_matches_rule(&rule.path, path, root) {
                        push_protected(&mut out, &mut seen, rule, path.to_path_buf());
                    }
                }
            }
        } else {
            let path = PathBuf::from(&rule.path);
            if !path_is_under_any_root(&path, &access_roots) {
                continue;
            }
            if path.exists() {
                push_protected(&mut out, &mut seen, rule, path);
            } else if rule.deny_write {
                // Windows ACLs only attach to real filesystem objects. For write-deny
                // carveouts under writable parents, create a sentinel directory before
                // launch so the deny ACE exists before the process can create it.
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create protected path {}", path.display()))?;
                push_protected(&mut out, &mut seen, rule, path);
            }
        }
    }

    Ok(out)
}

fn materialize_non_recursive_glob(
    out: &mut Vec<ResolvedProtectedPath>,
    seen: &mut BTreeSet<String>,
    rule: &WindowsProtectedPath,
    roots: &[PathBuf],
) -> Result<bool> {
    let Some(parent_pattern) = non_recursive_glob_parent_pattern(&rule.path) else {
        return Ok(false);
    };

    for root in roots {
        let dir = resolve_glob_parent(root, &parent_pattern);
        if !path_is_under_any_root(&dir, std::slice::from_ref(root)) || !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).with_context(|| format!("scan {}", dir.display()))? {
            let entry = entry.with_context(|| format!("scan {}", dir.display()))?;
            let path = entry.path();
            if path_matches_rule(&rule.path, &path, root) {
                push_protected(out, seen, rule, path);
            }
        }
    }
    Ok(true)
}

fn non_recursive_glob_parent_pattern(pattern: &str) -> Option<String> {
    let normalized = pattern.replace('\\', "/");
    if normalized.contains("**") {
        return None;
    }
    let parent = normalized.rsplit_once('/').map_or("", |(parent, _)| parent);
    if contains_glob(parent) {
        return None;
    }
    Some(parent.to_string())
}

fn resolve_glob_parent(root: &Path, parent_pattern: &str) -> PathBuf {
    if parent_pattern.is_empty() {
        return root.to_path_buf();
    }
    let parent = PathBuf::from(parent_pattern);
    if parent.is_absolute() {
        parent
    } else {
        root.join(parent)
    }
}

fn protected_walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false);
    builder
}

fn materialize_protected_subtree_roots(
    out: &mut Vec<ResolvedProtectedPath>,
    seen: &mut BTreeSet<String>,
    rule: &WindowsProtectedPath,
    root: &Path,
    subtree_pattern: &str,
) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir != root && path_matches_rule(subtree_pattern, &dir, root) {
            push_protected(out, seen, rule, dir);
            continue;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                return Err(error).with_context(|| format!("walk {}", dir.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("walk {}", dir.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("read file type for {}", entry.path().display()))?;
            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

fn protected_subtree_patterns(plan: &WindowsFileSystemPlan) -> Vec<String> {
    plan.protected_paths
        .iter()
        .filter_map(|rule| subtree_selector_pattern(&rule.path))
        .collect()
}

fn subtree_selector_pattern(pattern: &str) -> Option<String> {
    let pattern = pattern.replace('\\', "/");
    let subtree_pattern = pattern.strip_suffix("/**")?;
    let leaf = subtree_pattern.rsplit('/').next()?;
    if leaf.is_empty() || contains_glob(leaf) {
        return None;
    }
    Some(subtree_pattern.to_string())
}

fn contains_glob(value: &str) -> bool {
    value.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn timing_log(label: &str, start: Instant, details: String) {
    if !sandbox_timing_enabled() {
        return;
    }
    if details.is_empty() {
        eprintln!(
            "[duckagent][sandbox][timing] {label}: {} ms",
            start.elapsed().as_millis()
        );
    } else {
        eprintln!(
            "[duckagent][sandbox][timing] {label}: {} ms ({details})",
            start.elapsed().as_millis()
        );
    }
}

fn sandbox_timing_enabled() -> bool {
    env::var("DUCKAGENT_SANDBOX_TIMING")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn entry_is_protected_subtree(path: &Path, subtree_patterns: &[String]) -> bool {
    if subtree_patterns.is_empty() || !path.is_dir() {
        return false;
    }
    let path_text = comparable_path(path);
    subtree_patterns.iter().any(|pattern| {
        let pattern = comparable_pattern(pattern);
        glob_matches(&pattern, &path_text)
    })
}

fn comparable_pattern(pattern: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        pattern.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        pattern.to_string()
    }
}

fn access_roots(plan: &WindowsFileSystemPlan) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for mount in plan.read_roots.iter().chain(plan.write_roots.iter()) {
        if mount.path == "*" || mount.glob {
            continue;
        }
        let path = PathBuf::from(&mount.path);
        if path.is_dir() {
            roots.insert(path);
        }
    }
    roots.into_iter().collect()
}

fn path_is_under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let path = comparable_path(path);
    roots.iter().any(|root| {
        let root = comparable_path(root);
        path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
    })
}

fn comparable_path(path: &Path) -> String {
    let value = normalize_path_text(path).trim_end_matches('/').to_string();
    #[cfg(target_os = "windows")]
    {
        value.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        value
    }
}

fn path_matches_rule(pattern: &str, path: &Path, root: &Path) -> bool {
    let absolute = normalize_path_text(path);
    if glob_matches(pattern, &absolute) {
        return true;
    }
    if let Ok(relative) = path.strip_prefix(root) {
        return glob_matches(pattern, &normalize_path_text(relative));
    }
    false
}

fn push_protected(
    out: &mut Vec<ResolvedProtectedPath>,
    seen: &mut BTreeSet<String>,
    rule: &WindowsProtectedPath,
    path: PathBuf,
) {
    let key = normalize_path_text(&path);
    if !seen.insert(key) {
        return;
    }
    out.push(ResolvedProtectedPath {
        path,
        deny_read: rule.deny_read,
        deny_write: rule.deny_write || matches!(rule.requested_access, FileAccess::Ro),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_text(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    fn plan_with_roots(
        read_roots: Vec<PathBuf>,
        write_roots: Vec<PathBuf>,
        protected_paths: Vec<WindowsProtectedPath>,
    ) -> WindowsFileSystemPlan {
        WindowsFileSystemPlan {
            mounts: Vec::new(),
            rules: Vec::new(),
            read_roots: read_roots
                .into_iter()
                .map(|path| WindowsPathAccess {
                    path: path_text(&path),
                    access: FileAccess::Ro,
                    glob: false,
                })
                .collect(),
            write_roots: write_roots
                .into_iter()
                .map(|path| WindowsPathAccess {
                    path: path_text(&path),
                    access: FileAccess::Rw,
                    glob: false,
                })
                .collect(),
            protected_paths,
        }
    }

    fn protected_path(path: &Path, access: FileAccess) -> WindowsProtectedPath {
        WindowsProtectedPath {
            path: path_text(path),
            requested_access: access,
            glob: false,
            deny_read: !access.can_read(),
            deny_write: !access.can_write(),
        }
    }

    fn protected_glob(path: &str, access: FileAccess) -> WindowsProtectedPath {
        WindowsProtectedPath {
            path: path.to_string(),
            requested_access: access,
            glob: true,
            deny_read: !access.can_read(),
            deny_write: !access.can_write(),
        }
    }

    #[test]
    fn direct_protected_paths_outside_granted_roots_are_not_materialized() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let control = tmp.path().join(".duckagent").join("config.json");
        std::fs::create_dir_all(&repo)?;
        std::fs::create_dir_all(control.parent().expect("control file parent"))?;
        std::fs::write(&control, "{}")?;

        let plan = plan_with_roots(
            vec![repo],
            Vec::new(),
            vec![protected_path(&control, FileAccess::None)],
        );

        assert!(materialize_protected_paths(&plan)?.is_empty());
        Ok(())
    }

    #[test]
    fn missing_write_deny_outside_granted_roots_does_not_create_sentinel() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join(".duckagent");
        std::fs::create_dir_all(&repo)?;

        let plan = plan_with_roots(
            Vec::new(),
            vec![repo],
            vec![protected_path(&outside, FileAccess::None)],
        );

        assert!(materialize_protected_paths(&plan)?.is_empty());
        assert!(!outside.exists());
        Ok(())
    }

    #[test]
    fn missing_write_deny_inside_granted_roots_creates_sentinel() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let protected = repo.join(".duckagent");
        std::fs::create_dir_all(&repo)?;

        let plan = plan_with_roots(
            Vec::new(),
            vec![repo],
            vec![protected_path(&protected, FileAccess::None)],
        );

        let materialized = materialize_protected_paths(&plan)?;
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].path, protected);
        assert!(protected.exists());
        Ok(())
    }

    #[test]
    fn glob_protected_paths_only_walk_granted_roots() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo)?;
        std::fs::create_dir_all(&outside)?;
        std::fs::write(repo.join(".env"), "in")?;
        std::fs::write(outside.join(".env"), "out")?;

        let plan = plan_with_roots(
            vec![repo.clone()],
            Vec::new(),
            vec![WindowsProtectedPath {
                path: "**/.env".to_string(),
                requested_access: FileAccess::None,
                glob: true,
                deny_read: true,
                deny_write: true,
            }],
        );

        let materialized = materialize_protected_paths(&plan)?;
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].path, repo.join(".env"));
        Ok(())
    }

    #[test]
    fn wildcard_read_root_is_not_used_as_glob_scan_root() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo)?;
        std::fs::create_dir_all(&outside)?;
        std::fs::write(repo.join(".env"), "in")?;
        std::fs::write(outside.join(".env"), "out")?;

        let mut plan = plan_with_roots(
            Vec::new(),
            vec![repo.clone()],
            vec![protected_glob("**/.env", FileAccess::None)],
        );
        plan.read_roots.push(WindowsPathAccess {
            path: "*".to_string(),
            access: FileAccess::Ro,
            glob: false,
        });

        let materialized = materialize_protected_paths(&plan)?;
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].path, repo.join(".env"));
        Ok(())
    }

    #[test]
    fn subtree_glob_materializes_directory_root_not_children() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(git.join("objects").join("aa"))?;
        std::fs::write(git.join("objects").join("aa").join("object"), "blob")?;

        let plan = plan_with_roots(
            vec![repo.clone()],
            Vec::new(),
            vec![protected_glob("**/.git/**", FileAccess::Ro)],
        );

        let materialized = materialize_protected_paths(&plan)?;
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].path, git);
        assert!(materialized[0].deny_write);
        assert!(!materialized[0].deny_read);
        Ok(())
    }

    #[test]
    fn file_globs_skip_already_protected_subtrees() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git)?;
        std::fs::write(git.join("id_rsa"), "git secret")?;
        std::fs::write(repo.join("id_rsa"), "workspace secret")?;

        let plan = plan_with_roots(
            vec![repo.clone()],
            Vec::new(),
            vec![
                protected_glob("**/.git/**", FileAccess::Ro),
                protected_glob("**/id_rsa", FileAccess::None),
            ],
        );

        let materialized = materialize_protected_paths(&plan)?;
        assert!(materialized.iter().any(|path| path.path == git));
        assert!(
            materialized
                .iter()
                .any(|path| path.path == repo.join("id_rsa"))
        );
        assert!(
            !materialized
                .iter()
                .any(|path| path.path == git.join("id_rsa"))
        );
        Ok(())
    }

    #[test]
    fn root_level_globs_only_scan_mount_root_entries() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("target").join("debug"))?;
        std::fs::write(repo.join("root.pem"), "")?;
        std::fs::write(repo.join("target").join("debug").join("nested.pem"), "")?;

        let plan = plan_with_roots(
            vec![repo.clone()],
            Vec::new(),
            vec![protected_glob("*.pem", FileAccess::None)],
        );

        let protected_paths = materialize_protected_paths(&plan)?
            .into_iter()
            .map(|path| path.path)
            .collect::<Vec<_>>();

        assert_eq!(protected_paths, vec![repo.join("root.pem")]);
        Ok(())
    }

    #[test]
    fn literal_parent_globs_only_scan_that_parent_directory() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("certs").join("nested"))?;
        std::fs::write(repo.join("certs").join("root.pem"), "")?;
        std::fs::write(repo.join("certs").join("nested").join("deep.pem"), "")?;

        let plan = plan_with_roots(
            vec![repo.clone()],
            Vec::new(),
            vec![protected_glob("certs/*.pem", FileAccess::None)],
        );

        let protected_paths = materialize_protected_paths(&plan)?
            .into_iter()
            .map(|path| path.path)
            .collect::<Vec<_>>();

        assert_eq!(protected_paths, vec![repo.join("certs").join("root.pem")]);
        Ok(())
    }
}
