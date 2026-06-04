use crate::sandbox::config::{FileAccess, ResolvedSandbox};
use crate::sandbox::matcher::normalize_path_text;
use crate::sandbox::path_vars::{expand_path_vars, resolve_config_path};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub fn run_status(
    sandbox: &ResolvedSandbox,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: BTreeMap<String, String>,
) -> Result<ExitStatus> {
    if sandbox.is_full_access() {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd).env_clear().envs(env);
        return command
            .status()
            .with_context(|| format!("failed to execute command `{program}`"));
    }

    let policy = build_policy(sandbox, cwd, &env)?;
    let mut command = Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(policy);
    command.arg(program).args(args);
    command.current_dir(cwd);
    command.env_clear().envs(env);
    command
        .status()
        .with_context(|| format!("failed to execute sandboxed command through {SANDBOX_EXEC}"))
}

fn build_policy(
    sandbox: &ResolvedSandbox,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut policy = String::new();
    policy.push_str(BASE_POLICY);
    policy.push('\n');

    match sandbox.preset.network.mode {
        crate::sandbox::config::NetworkMode::Allow => {
            policy.push_str("(allow network-outbound)\n(allow network-inbound)\n");
        }
        crate::sandbox::config::NetworkMode::Proxy => {
            let proxy_addr = crate::sandbox::network_proxy::managed_proxy_addr_from_env(env)?
                .context(
                    "network.mode=proxy requires a managed proxy env before macOS sandbox launch",
                )?;
            // Seatbelt accepts `localhost:<port>` but rejects numeric loopback
            // hosts such as `127.0.0.1:<port>` in this policy position.
            policy.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{}\"))\n",
                proxy_addr.port()
            ));
        }
        crate::sandbox::config::NetworkMode::Deny => {}
    }

    let mut read_roots = Vec::new();
    let mut write_roots = Vec::new();
    for mount in &sandbox.preset.filesystem.mounts {
        if mount.path == "*" {
            read_roots.push(PathBuf::from("/"));
            if mount.access.can_write() {
                write_roots.push(PathBuf::from("/"));
            }
            continue;
        }
        let path = resolve_config_path(&mount.path, cwd);
        if mount.access.can_read() {
            read_roots.push(path.clone());
        }
        if mount.access.can_write() {
            write_roots.push(path);
        }
    }

    append_path_policy(&mut policy, "file-read* file-test-existence", &read_roots);
    append_path_policy(&mut policy, "file-map-executable", &read_roots);
    append_path_policy(&mut policy, "file-write*", &write_roots);

    for rule in &sandbox.preset.filesystem.rules {
        append_rule_policy(&mut policy, rule, cwd);
    }

    Ok(policy)
}

fn append_rule_policy(
    policy: &mut String,
    rule: &crate::sandbox::config::FileSystemRule,
    cwd: &Path,
) {
    let action = match rule.access {
        FileAccess::None => "file-read* file-test-existence file-map-executable file-write*",
        FileAccess::Ro => "file-write*",
        FileAccess::Rw => return,
    };

    if !contains_glob(&rule.path) {
        let path = resolve_config_path(&rule.path, cwd);
        let path_text = normalize_path_text(&path).replace('"', "\\\"");
        if path.is_dir() {
            policy.push_str(&format!(
                "(deny {action} (literal \"{path_text}\") (subpath \"{path_text}\"))\n"
            ));
        } else {
            policy.push_str(&format!("(deny {action} (literal \"{path_text}\"))\n"));
        }
        return;
    }

    let regex = glob_pattern_to_absolute_regex(&rule.path, cwd).replace('"', "\\\"");
    policy.push_str(&format!("(deny {action} (regex #\"{regex}\"))\n"));
}

fn append_path_policy(policy: &mut String, action: &str, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    policy.push_str(&format!("(allow {action}\n"));
    for path in paths {
        let path = normalize_path_text(path).replace('"', "\\\"");
        policy.push_str(&format!("  (subpath \"{path}\")\n"));
    }
    policy.push_str(")\n");
}

fn glob_pattern_to_absolute_regex(pattern: &str, cwd: &Path) -> String {
    let pattern = expand_path_vars(pattern, cwd);
    let absolute_pattern = if pattern.starts_with('/') {
        pattern
    } else {
        let cwd = normalize_path_text(cwd);
        format!("{cwd}/{pattern}")
    };
    format!("^{}$", glob_to_regex(&absolute_pattern))
}

fn contains_glob(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn glob_to_regex(pattern: &str) -> String {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') && chars.get(i + 2) == Some(&'/') => {
                out.push_str("(.*/)?");
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

// Small but practical Seatbelt base policy for CLI tools. It intentionally
// keeps user data closed by default; configured mounts reopen project paths.
const BASE_POLICY: &str = r#"
(version 1)
(deny default)

(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

(allow sysctl-read)
(allow sysctl-write)
(allow mach-lookup)
(allow iokit-open)
(allow ipc-posix*)
(allow pseudo-tty)

(allow file-read* file-test-existence
  (literal "/")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/usr")
  (subpath "/System")
  (subpath "/Library/Apple")
  (subpath "/Library/Preferences")
  (subpath "/private/etc")
  (subpath "/etc")
  (subpath "/opt/homebrew/bin")
  (subpath "/opt/homebrew/lib")
  (subpath "/opt/homebrew/opt")
  (subpath "/opt/homebrew/Cellar")
  (regex #"^/opt/homebrew/etc/openssl(@[^/]+)?(/.*)?$")
  (subpath "/usr/local/bin")
  (subpath "/usr/local/lib")
  (subpath "/usr/local/opt")
  (subpath "/usr/local/Cellar")
  (regex #"^/usr/local/etc/openssl(@[^/]+)?(/.*)?$")
  (subpath "/Applications"))

; Homebrew installs many executables as symlinks from bin/opt into Cellar.
; OpenSSL also reads its Homebrew config before Node can finish startup.
; Allow mapping those runtime binaries and reading config without opening user data.
(allow file-map-executable
  (subpath "/opt/homebrew/bin")
  (subpath "/opt/homebrew/lib")
  (subpath "/opt/homebrew/opt")
  (subpath "/opt/homebrew/Cellar")
  (subpath "/usr/local/bin")
  (subpath "/usr/local/lib")
  (subpath "/usr/local/opt")
  (subpath "/usr/local/Cellar"))

(allow file-read* file-test-existence
  (literal "/dev")
  (subpath "/dev/fd")
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/random")
  (literal "/dev/urandom")
  (literal "/dev/tty")
  (literal "/dev/ptmx"))

(allow file-read* file-write* file-ioctl
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/ptmx")
  (subpath "/dev/fd")
  (regex #"^/dev/ttys[0-9]+$"))

(allow file-read-metadata file-test-existence
  (literal "/tmp")
  (literal "/var")
  (literal "/private")
  (literal "/private/tmp")
  (literal "/private/var"))
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::SandboxConfig;
    use std::process::Command;

    #[test]
    fn glob_regex_supports_workspace_secret_patterns() {
        let regex = glob_pattern_to_absolute_regex("**/.env", Path::new("/work"));
        let re = regex::Regex::new(&regex).unwrap();
        assert!(re.is_match("/work/.env"));
        assert!(re.is_match("/work/app/.env"));
        assert!(!re.is_match("/other/.env"));
    }

    #[test]
    fn glob_regex_keeps_single_star_non_recursive() {
        let regex = glob_pattern_to_absolute_regex("*.pem", Path::new("/work"));
        let re = regex::Regex::new(&regex).unwrap();
        assert!(re.is_match("/work/root.pem"));
        assert!(!re.is_match("/work/certs/root.pem"));

        let regex = glob_pattern_to_absolute_regex("certs/*.pem", Path::new("/work"));
        let re = regex::Regex::new(&regex).unwrap();
        assert!(re.is_match("/work/certs/root.pem"));
        assert!(!re.is_match("/work/certs/nested/root.pem"));
    }

    #[test]
    fn proxy_policy_uses_seatbelt_supported_loopback_patterns() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;

        assert!(policy.contains("(allow network-outbound (remote ip \"localhost:43128\"))"));
        assert!(!policy.contains("(allow network-outbound (remote ip \"localhost:*\"))"));
        assert!(!policy.contains("(allow network-bind"));
        assert!(!policy.contains("(allow network-inbound"));
        assert!(!policy.contains("127.0.0.1:*"));
        assert!(!policy.contains("[::1]:*"));

        Ok(())
    }

    #[test]
    fn workspace_policy_allows_full_disk_read_without_full_disk_write() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;

        assert!(
            policy.contains("(allow file-read* file-test-existence\n  (subpath \"/\")"),
            "workspace should allow Codex-style full disk reads"
        );
        assert!(
            policy.contains("(allow file-map-executable\n  (subpath \"/\")"),
            "workspace read roots should allow executable mappings for interpreters and native addons"
        );
        assert!(
            !policy.contains("(allow file-write*\n  (subpath \"/\")"),
            "workspace must not allow full disk writes"
        );
        Ok(())
    }

    #[test]
    fn none_rules_deny_executable_mapping_too() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.filesystem.rules = vec![crate::sandbox::config::FileSystemRule {
            path: "blocked.node".to_string(),
            access: FileAccess::None,
        }];

        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;

        assert!(
            policy.contains("(deny file-read* file-test-existence file-map-executable file-write*"),
            "read-deny rules must also deny executable mapping"
        );
        assert!(policy.contains("(literal \"/work/blocked.node\")"));
        Ok(())
    }

    #[test]
    fn base_policy_allows_homebrew_runtime_roots_read_only() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;

        for path in [
            "/opt/homebrew/bin",
            "/opt/homebrew/lib",
            "/opt/homebrew/opt",
            "/opt/homebrew/Cellar",
            "/usr/local/bin",
            "/usr/local/lib",
            "/usr/local/opt",
            "/usr/local/Cellar",
        ] {
            assert!(
                policy.contains(&format!("(subpath \"{path}\")")),
                "policy should allow read/map access to Homebrew runtime root {path}"
            );
        }
        for regex in [
            r#"^/opt/homebrew/etc/openssl(@[^/]+)?(/.*)?$"#,
            r#"^/usr/local/etc/openssl(@[^/]+)?(/.*)?$"#,
        ] {
            assert!(
                policy.contains(&format!("(regex #\"{regex}\")")),
                "policy should allow versioned Homebrew OpenSSL config regex {regex}"
            );
        }

        Ok(())
    }

    #[test]
    fn non_glob_directory_rules_apply_to_subtree() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git)?;
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.filesystem.rules = vec![crate::sandbox::config::FileSystemRule {
            path: ".git".to_string(),
            access: FileAccess::Ro,
        }];

        let policy = build_policy(&sandbox, tmp.path(), &managed_proxy_test_env())?;
        let git = normalize_path_text(&git);
        assert!(policy.contains(&format!("(literal \"{git}\")")));
        assert!(policy.contains(&format!("(subpath \"{git}\")")));
        Ok(())
    }

    #[test]
    fn policy_expands_config_path_variables() -> Result<()> {
        let mut sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        sandbox.preset.filesystem.mounts = vec![crate::sandbox::config::FileSystemMount {
            path: "$CWD".to_string(),
            access: FileAccess::Rw,
        }];
        sandbox.preset.filesystem.rules = vec![crate::sandbox::config::FileSystemRule {
            path: "$CWD/private/**".to_string(),
            access: FileAccess::None,
        }];

        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;

        assert!(policy.contains("(subpath \"/work\")"));
        assert!(policy.contains("^/work/private/.*$"));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn generated_workspace_policy_reaches_sandbox_apply_stage() -> Result<()> {
        let sandbox = SandboxConfig::default().resolve(Some("workspace"))?;
        let policy = build_policy(&sandbox, Path::new("/work"), &managed_proxy_test_env())?;
        let output = Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(policy)
            .arg("/bin/pwd")
            .output()
            .context("failed to run sandbox-exec parse smoke test")?;
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !stderr.contains("unsupported syntax"),
            "policy must parse before sandbox application, stderr: {stderr}"
        );
        assert!(
            !stderr.contains("host must be * or localhost"),
            "policy must not contain unsupported numeric loopback hosts, stderr: {stderr}"
        );

        Ok(())
    }

    fn managed_proxy_test_env() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                crate::sandbox::network_proxy::MANAGED_PROXY_ENV_KEY.to_string(),
                "1".to_string(),
            ),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43128".to_string(),
            ),
        ])
    }
}
