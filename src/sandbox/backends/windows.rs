mod acl;
mod dpapi;
mod filesystem;
mod firewall;
mod process;
pub mod setup;
mod sid;
mod users;
mod wfp;
mod winutil;

use crate::sandbox::backends::windows_plan::{WindowsSandboxPlan, is_full_access};
use crate::sandbox::config::ResolvedSandbox;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Instant;

pub fn run_status(
    sandbox: &ResolvedSandbox,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: BTreeMap<String, String>,
) -> Result<ExitStatus> {
    let total_start = Instant::now();
    if is_full_access(sandbox) {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd).env_clear().envs(env);
        let status = command
            .status()
            .with_context(|| format!("failed to execute command `{program}`"))?;
        timing_log("windows danger process total", total_start);
        return Ok(status);
    }

    let plan_start = Instant::now();
    let plan = WindowsSandboxPlan::from_sandbox(sandbox, cwd);
    timing_log("windows plan build", plan_start);
    if !plan.unsupported_reasons.is_empty() {
        bail!("{}", plan.unsupported_command_error(program));
    }

    let credential_start = Instant::now();
    let duckagent_home = dirs::home_dir()
        .context("failed to resolve home directory for Windows sandbox")?
        .join(".duckagent");
    let credentials =
        users::credentials_for_network_mode(&duckagent_home, sandbox.preset.network.mode.clone())?;
    timing_log("windows credentials lookup", credential_start);
    let sid_start = Instant::now();
    let group_sid = users::sandbox_group_sid_string()?;
    let group = sid::local_sid(&group_sid)?;
    let deny_sid_strings = users::sandbox_user_sid_strings()?;
    let deny_sids = deny_sid_strings
        .iter()
        .map(|sid| sid::local_sid(sid))
        .collect::<Result<Vec<_>>>()?;
    let deny_sid_ptrs = deny_sids.iter().map(|sid| sid.as_ptr()).collect::<Vec<_>>();
    timing_log("windows SID lookup", sid_start);
    let fs_start = Instant::now();
    filesystem::apply_filesystem_plan(&plan.filesystem, &[group.as_ptr()], &deny_sid_ptrs)?;
    timing_log("windows filesystem apply", fs_start);
    let process_start = Instant::now();
    let status = process::run_with_logon(&credentials, program, args, cwd, &env)?;
    timing_log("windows CreateProcessWithLogonW run", process_start);
    timing_log("windows sandbox process total", total_start);
    Ok(status)
}

fn timing_log(label: &str, start: Instant) {
    if !sandbox_timing_enabled() {
        return;
    }
    eprintln!(
        "[duckagent][sandbox][timing] {label}: {} ms",
        start.elapsed().as_millis()
    );
}

fn sandbox_timing_enabled() -> bool {
    std::env::var("DUCKAGENT_SANDBOX_TIMING")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}
