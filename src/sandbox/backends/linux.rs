use crate::sandbox::config::{FileAccess, FileSystemRule, NetworkMode, ResolvedSandbox};
use crate::sandbox::matcher::path_pattern_matches;
use crate::sandbox::path_vars::resolve_config_path;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;

const LINUX_PLATFORM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

static BWRAP_STARTUP_PREFLIGHT: OnceLock<Option<String>> = OnceLock::new();

pub fn run_status(
    sandbox: &ResolvedSandbox,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: BTreeMap<String, String>,
) -> Result<ExitStatus> {
    if is_full_access(sandbox) {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd).env_clear().envs(env);
        return command
            .status()
            .with_context(|| format!("failed to execute command `{program}`"));
    }

    let bwrap_args = build_bwrap_args(sandbox, program, args, cwd, env)?;
    if let Some(bwrap) = find_bwrap() {
        return Command::new(bwrap)
            .args(&bwrap_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to execute system bubblewrap sandbox");
    }

    if crate::sandbox::backends::vendored_bwrap::available() {
        let mut argv = vec!["bwrap".to_string()];
        argv.extend(bwrap_args);
        crate::sandbox::backends::vendored_bwrap::exec(argv);
    }

    anyhow::bail!(
        "Linux sandbox requires system bubblewrap or duckagent vendored bubblewrap; refusing to run unsandboxed"
    )
}

fn build_bwrap_args(
    sandbox: &ResolvedSandbox,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut out = vec![
        "--die-with-parent".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--unshare-cgroup-try".to_string(),
    ];

    if has_full_read_mount(sandbox) {
        out.extend([
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
        ]);
    } else {
        out.extend(["--tmpfs".to_string(), "/".to_string()]);
        append_platform_read_roots(&mut out)?;
        append_current_exe_read_root(&mut out)?;
        out.extend(["--dev".to_string(), "/dev".to_string()]);
    }

    out.extend([
        "--proc".to_string(),
        "/proc".to_string(),
        "--clearenv".to_string(),
    ]);

    if matches!(
        sandbox.preset.network.mode,
        NetworkMode::Deny | NetworkMode::Proxy
    ) {
        out.push("--unshare-net".to_string());
    }

    for mount in &sandbox.preset.filesystem.mounts {
        if mount.path == "*" {
            if mount.access == FileAccess::Rw {
                out.extend(["--bind".to_string(), "/".to_string(), "/".to_string()]);
            }
            continue;
        }
        let path = resolve_config_path(&mount.path, cwd);
        let path_text = path.to_string_lossy().to_string();
        append_mount_target_parent_dirs_unless_platform_parent(&mut out, &path);
        if mount.access.can_write() {
            out.extend(["--bind".to_string(), path_text.clone(), path_text]);
        } else if mount.access.can_read() {
            out.extend(["--ro-bind".to_string(), path_text.clone(), path_text]);
        }
    }

    apply_filesystem_rules(&mut out, &sandbox.preset.filesystem.rules, cwd)?;

    out.extend(["--chdir".to_string(), cwd.to_string_lossy().to_string()]);
    for (key, value) in env {
        out.extend(["--setenv".to_string(), key, value]);
    }
    out.push("--".to_string());
    if matches!(sandbox.preset.network.mode, NetworkMode::Proxy) {
        let route_spec =
            crate::sandbox::backends::linux_proxy_routing::prepare_host_proxy_route_spec_from_env(
                &collect_setenv_pairs(&out),
            )
            .context("failed to prepare Linux sandbox proxy routing")?;
        out.push(std::env::current_exe()?.to_string_lossy().to_string());
        out.push("__sandbox-linux-inner".to_string());
        out.push("--proxy-route-spec".to_string());
        out.push(route_spec);
        out.push("--".to_string());
    }
    out.push(program.to_string());
    out.extend(args.iter().cloned());
    Ok(out)
}

fn collect_setenv_pairs(bwrap_args: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let mut index = 0;
    while index + 2 < bwrap_args.len() {
        if bwrap_args[index] == "--setenv" {
            env.insert(bwrap_args[index + 1].clone(), bwrap_args[index + 2].clone());
            index += 3;
        } else {
            index += 1;
        }
    }
    env
}

fn apply_filesystem_rules(
    out: &mut Vec<String>,
    rules: &[FileSystemRule],
    cwd: &Path,
) -> Result<()> {
    let mut entries = Vec::new();
    for (order, rule) in rules.iter().enumerate() {
        for path in existing_matches(&rule.path, cwd)? {
            entries.push((pattern_specificity(&rule.path), order, path, rule.access));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    for (_, _, path, access) in entries {
        let path_text = path.to_string_lossy().to_string();
        append_mount_target_parent_dirs_unless_platform_parent(out, &path);
        match access {
            FileAccess::Rw => out.extend(["--bind".to_string(), path_text.clone(), path_text]),
            FileAccess::Ro => out.extend(["--ro-bind".to_string(), path_text.clone(), path_text]),
            FileAccess::None if path.is_dir() => out.extend(["--tmpfs".to_string(), path_text]),
            FileAccess::None => {
                out.extend(["--bind".to_string(), "/dev/null".to_string(), path_text])
            }
        }
    }
    Ok(())
}

fn has_full_read_mount(sandbox: &ResolvedSandbox) -> bool {
    sandbox
        .preset
        .filesystem
        .mounts
        .iter()
        .any(|mount| mount.path == "*" && mount.access.can_read())
}

fn append_platform_read_roots(out: &mut Vec<String>) -> Result<()> {
    for root in LINUX_PLATFORM_READ_ROOTS {
        let path = Path::new(root);
        if path.exists() {
            append_ro_bind(out, path);
        }
    }
    Ok(())
}

fn append_current_exe_read_root(out: &mut Vec<String>) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve duck executable path")?;
    if exe.exists() && !path_is_inside_existing_platform_root(&exe) {
        append_ro_bind(out, &exe);
    }
    Ok(())
}

fn append_ro_bind(out: &mut Vec<String>, path: &Path) {
    append_mount_target_parent_dirs(out, path);
    let path_text = path.to_string_lossy().to_string();
    out.extend(["--ro-bind".to_string(), path_text.clone(), path_text]);
}

fn append_mount_target_parent_dirs(out: &mut Vec<String>, path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let mut current = PathBuf::new();
    let mut parents = Vec::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        if current.as_os_str() != "/" && !current.as_os_str().is_empty() {
            parents.push(current.clone());
        }
    }
    for parent in parents {
        out.extend(["--dir".to_string(), parent.to_string_lossy().to_string()]);
    }
}

fn append_mount_target_parent_dirs_unless_platform_parent(out: &mut Vec<String>, path: &Path) {
    if path_is_inside_existing_platform_root(path) {
        return;
    }
    append_mount_target_parent_dirs(out, path);
}

fn path_is_inside_existing_platform_root(path: &Path) -> bool {
    LINUX_PLATFORM_READ_ROOTS
        .iter()
        .map(Path::new)
        .any(|root| root.exists() && path.starts_with(root))
}

fn pattern_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .count()
}

fn existing_matches(pattern: &str, cwd: &Path) -> Result<Vec<PathBuf>> {
    if !contains_glob(pattern) {
        let path = resolve_config_path(pattern, cwd);
        return Ok(path.exists().then_some(path).into_iter().collect());
    }

    if let Some(matches) = non_recursive_existing_matches(pattern, cwd)? {
        return Ok(matches);
    }

    if let Some(matches) = ripgrep_existing_matches(pattern, cwd)? {
        return Ok(matches);
    }

    let root = scan_root(pattern, cwd);
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .follow_links(false);

    let mut matches = Vec::new();
    for entry in builder.build() {
        let entry = entry.with_context(|| format!("failed to scan {}", root.display()))?;
        let path = entry.path();
        if path_pattern_matches(pattern, path, cwd) {
            matches.push(path.to_path_buf());
            if let Ok(canonical) = path.canonicalize() {
                matches.push(canonical);
            }
        }
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn non_recursive_existing_matches(pattern: &str, cwd: &Path) -> Result<Option<Vec<PathBuf>>> {
    let Some(parent_pattern) = non_recursive_glob_parent_pattern(pattern) else {
        return Ok(None);
    };
    let dir = resolve_glob_parent(cwd, &parent_pattern);
    if !dir.is_dir() {
        return Ok(Some(Vec::new()));
    }

    let mut matches = Vec::new();
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("failed to scan {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to scan {}", dir.display()))?;
        let path = entry.path();
        if path_pattern_matches(pattern, &path, cwd) {
            matches.push(path.clone());
            if let Ok(canonical) = path.canonicalize() {
                matches.push(canonical);
            }
        }
    }
    matches.sort();
    matches.dedup();
    Ok(Some(matches))
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

fn resolve_glob_parent(cwd: &Path, parent_pattern: &str) -> PathBuf {
    if parent_pattern.is_empty() {
        return cwd.to_path_buf();
    }
    let parent = PathBuf::from(parent_pattern);
    if parent.is_absolute() {
        parent
    } else {
        resolve_config_path(parent_pattern, cwd)
    }
}

fn ripgrep_existing_matches(pattern: &str, cwd: &Path) -> Result<Option<Vec<PathBuf>>> {
    let Some(rg) = find_executable("rg") else {
        return Ok(None);
    };
    let Some((root, glob)) = split_pattern_for_ripgrep(pattern, cwd) else {
        return Ok(None);
    };
    if !root.is_dir() {
        return Ok(Some(Vec::new()));
    }

    let output = Command::new(rg)
        .arg("--files")
        .arg("--hidden")
        .arg("--no-ignore")
        .arg("--glob")
        .arg(&glob)
        .arg(&root)
        .output()
        .with_context(|| format!("failed to run ripgrep for sandbox glob `{pattern}`"))?;

    if !output.status.success() && output.status.code() != Some(1) {
        anyhow::bail!(
            "ripgrep failed while expanding sandbox glob `{pattern}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let path = PathBuf::from(line);
        if path_pattern_matches(pattern, &path, cwd) {
            matches.push(path.clone());
            if let Ok(canonical) = path.canonicalize() {
                matches.push(canonical);
            }
        }
    }
    matches.sort();
    matches.dedup();
    Ok(Some(matches))
}

fn split_pattern_for_ripgrep(pattern: &str, cwd: &Path) -> Option<(PathBuf, String)> {
    let absolute = if pattern.starts_with('/') {
        pattern.to_string()
    } else {
        resolve_config_path(pattern, cwd)
            .to_string_lossy()
            .to_string()
    };
    let first_glob = absolute
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(index))?;
    if first_glob == 0 {
        return None;
    }
    let static_prefix = &absolute[..first_glob];
    let root_text = static_prefix
        .rsplit_once('/')
        .map(|(prefix, _)| if prefix.is_empty() { "/" } else { prefix })
        .unwrap_or("/");
    if root_text == "/" {
        return None;
    }
    let root = PathBuf::from(root_text);
    let glob = absolute
        .strip_prefix(&(root_text.trim_end_matches('/').to_string() + "/"))
        .unwrap_or(absolute.as_str())
        .to_string();
    Some((root, glob))
}

fn scan_root(pattern: &str, cwd: &Path) -> PathBuf {
    if !pattern.starts_with('/') {
        return cwd.to_path_buf();
    }

    let first_glob = pattern
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(idx))
        .unwrap_or(pattern.len());
    let static_prefix = &pattern[..first_glob];
    let root = static_prefix
        .rsplit_once('/')
        .map(|(prefix, _)| if prefix.is_empty() { "/" } else { prefix })
        .unwrap_or("/");
    PathBuf::from(root)
}

fn contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn find_bwrap() -> Option<PathBuf> {
    find_executable("bwrap").filter(|candidate| {
        let current_dir = std::env::current_dir().ok();
        current_dir
            .as_ref()
            .is_none_or(|cwd| !candidate.starts_with(cwd))
    })
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn has_bwrap_backend() -> bool {
    find_bwrap().is_some() || crate::sandbox::backends::vendored_bwrap::available()
}

pub fn startup_preflight_error() -> Option<String> {
    BWRAP_STARTUP_PREFLIGHT
        .get_or_init(run_startup_preflight)
        .clone()
}

fn is_full_access(sandbox: &ResolvedSandbox) -> bool {
    sandbox.is_full_access()
}

fn run_startup_preflight() -> Option<String> {
    let Some(bwrap) = find_bwrap() else {
        if crate::sandbox::backends::vendored_bwrap::available() {
            return None;
        }
        return Some(
            "Linux sandbox requires system bubblewrap or duckagent vendored bubblewrap".to_string(),
        );
    };
    let true_bin = find_executable("true").unwrap_or_else(|| PathBuf::from("/bin/true"));
    let output = Command::new(&bwrap)
        .args([
            "--die-with-parent",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--clearenv",
            "--",
        ])
        .arg(&true_bin)
        .output();
    match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("bubblewrap exited with {}", output.status)
            };
            Some(format!(
                "Linux sandbox backend is installed but cannot start in this environment: {detail}"
            ))
        }
        Err(error) => Some(format!(
            "Linux sandbox backend is installed but failed to run bubblewrap preflight: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;
    use crate::sandbox::matcher::normalize_path_text;

    #[test]
    fn linux_bwrap_args_include_ro_and_rw_mounts() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.network.mode = NetworkMode::Deny;
        sandbox.preset.filesystem.rules.clear();
        let workdir = tempfile::tempdir()?;
        let workspace = workdir.path();
        let workspace_text = normalize_path_text(workspace);
        let args = build_bwrap_args(&sandbox, "true", &[], workspace, BTreeMap::new())?;
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--bind".to_string(),
                    workspace_text.clone(),
                    workspace_text.clone(),
                ]
        }));
        assert!(args.windows(3).any(|window| {
            window == ["--ro-bind".to_string(), "/".to_string(), "/".to_string()]
        }));
        assert!(
            !args
                .windows(2)
                .any(|window| { window == ["--tmpfs".to_string(), "/".to_string()] })
        );
        assert!(args.contains(&"--unshare-net".to_string()));
        Ok(())
    }

    #[test]
    fn linux_danger_keeps_full_root_bind() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("danger"))?;
        let workdir = tempfile::tempdir()?;
        let args = build_bwrap_args(&sandbox, "true", &[], workdir.path(), BTreeMap::new())?;
        assert!(args.windows(3).any(|window| {
            window == ["--ro-bind".to_string(), "/".to_string(), "/".to_string()]
        }));
        Ok(())
    }

    #[test]
    fn scan_root_uses_static_absolute_prefix() {
        assert_eq!(
            scan_root("/tmp/work/**/.env", Path::new("/ignored")),
            PathBuf::from("/tmp/work")
        );
        assert_eq!(
            scan_root("**/.env", Path::new("/cwd")),
            PathBuf::from("/cwd")
        );
    }

    #[test]
    fn root_level_glob_existing_matches_do_not_recurse() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let workspace = tmp.path();
        std::fs::create_dir_all(workspace.join("target").join("debug"))?;
        std::fs::write(workspace.join("root.pem"), "")?;
        std::fs::write(
            workspace.join("target").join("debug").join("nested.pem"),
            "",
        )?;

        let matches = existing_matches("*.pem", workspace)?
            .into_iter()
            .map(|path| normalize_path_text(path.strip_prefix(workspace).unwrap_or(&path)))
            .collect::<Vec<_>>();

        assert_eq!(matches, vec!["root.pem"]);
        Ok(())
    }

    #[test]
    fn literal_parent_glob_existing_matches_do_not_recurse() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let workspace = tmp.path();
        std::fs::create_dir_all(workspace.join("certs").join("nested"))?;
        std::fs::write(workspace.join("certs").join("root.pem"), "")?;
        std::fs::write(workspace.join("certs").join("nested").join("deep.pem"), "")?;

        let matches = existing_matches("certs/*.pem", workspace)?
            .into_iter()
            .map(|path| normalize_path_text(path.strip_prefix(workspace).unwrap_or(&path)))
            .collect::<Vec<_>>();

        assert_eq!(matches, vec!["certs/root.pem"]);
        Ok(())
    }

    #[test]
    fn linux_bwrap_args_expand_config_path_variables() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.network.mode = NetworkMode::Deny;
        let workdir = tempfile::tempdir()?;
        let workspace = workdir.path();
        let workspace_text = normalize_path_text(workspace);
        sandbox.preset.filesystem.mounts = vec![crate::sandbox::config::FileSystemMount {
            path: "$CWD".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules.clear();

        let args = build_bwrap_args(&sandbox, "true", &[], workspace, BTreeMap::new())?;

        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--bind".to_string(),
                    workspace_text.clone(),
                    workspace_text.clone(),
                ]
        }));
        Ok(())
    }
}
