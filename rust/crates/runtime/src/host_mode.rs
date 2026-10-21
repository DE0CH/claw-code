//! Host mode (DE0CH fork): switches for hosts that run `claw` as a per-turn agent loop on a server
//! (the Jarvis 3 chat runtime). Each one is a no-op unless its environment variable is set:
//!
//! | env | effect |
//! |---|---|
//! | `CLAW_SYSTEM_PROMPT_FILE` | the system prompt is that file's text plus an `# Environment` section |
//! | `CLAW_BASH_COMMAND` | the `bash` tool runs that executable instead of a local shell |
//! | `CLAW_CONFIG_DIR` | the only config dir read; nothing is read from the cwd or the home dir |
//! | `CLAW_SESSIONS_DIR` | where managed sessions live; resuming skips the workspace-root check |
//! | `CLAW_PROMPT_CACHE=1` | Anthropic requests carry `cache_control` breakpoints (in the `api` crate) |
//! | `CLAW_IMAGES=1` | `read_file` shows images and PDF pages to the model; `CLAW_PROMPT_IMAGES` attaches images to the prompt (`images.rs`) |
//!
//! See `docs/event-stream.md`.
use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use crate::bash::{BashCommandInput, BashCommandOutput};

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `CLAW_SYSTEM_PROMPT_FILE`, when set.
#[must_use]
pub fn system_prompt_file() -> Option<PathBuf> {
    env_path("CLAW_SYSTEM_PROMPT_FILE")
}

/// `CLAW_BASH_COMMAND`, when set.
#[must_use]
pub fn bash_command() -> Option<PathBuf> {
    env_path("CLAW_BASH_COMMAND")
}

/// `CLAW_CONFIG_DIR`, when set.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    env_path("CLAW_CONFIG_DIR")
}

/// `CLAW_SESSIONS_DIR`, when set.
#[must_use]
pub fn sessions_dir() -> Option<PathBuf> {
    env_path("CLAW_SESSIONS_DIR")
}

/// Today's date (UTC) as `YYYY-MM-DD`, from the system clock.
#[must_use]
pub fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (year, month, day) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a Gregorian date.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

/// The whole system prompt in host mode: the host's text, then an `# Environment` section. No coding
/// sections, git context, instruction files or config dump.
///
/// # Errors
/// The prompt file can't be read.
pub fn host_system_prompt(path: &Path, cwd: &Path, date: &str) -> io::Result<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("CLAW_SYSTEM_PROMPT_FILE {}: {error}", path.display()),
        )
    })?;
    Ok(vec![
        text.trim_end().to_string(),
        format!(
            "# Environment\n - Today's date (UTC): {date}\n - Working directory: {}",
            cwd.display()
        ),
    ])
}

#[derive(Debug, Deserialize)]
struct RemoteBashReply {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    exit_code: Option<i64>,
    #[serde(default)]
    interrupted: bool,
    background_task_id: Option<String>,
}

/// Runs one `bash` tool call through the host's executable: the tool input as JSON on stdin, the
/// result as JSON on stdout.
///
/// # Errors
/// The helper can't be started, exits non-zero (the error carries its stderr) or prints no valid JSON.
pub fn run_remote_bash(
    helper: &OsStr,
    cwd: &Path,
    input: &BashCommandInput,
) -> io::Result<BashCommandOutput> {
    let request = json!({
        "command": input.command,
        "timeout": input.timeout,
        "description": input.description,
        "run_in_background": input.run_in_background.unwrap_or(false),
    });
    let mut child = Command::new(helper)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("CLAW_BASH_COMMAND {}: {error}", Path::new(helper).display()),
            )
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request.to_string().as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(io::Error::other(if stderr.is_empty() {
            format!("the bash helper failed ({})", output.status)
        } else {
            stderr
        }));
    }
    let reply: RemoteBashReply = serde_json::from_slice(&output.stdout).map_err(|error| {
        io::Error::other(format!(
            "the bash helper's reply is not valid JSON: {error}"
        ))
    })?;
    let return_code_interpretation = match (reply.exit_code, reply.interrupted) {
        (Some(code), _) if code != 0 => Some(format!("exit_code:{code}")),
        (_, true) => Some("timeout".to_string()),
        _ => None,
    };
    let background = reply.background_task_id.is_some();
    Ok(BashCommandOutput {
        no_output_expected: Some(reply.stdout.trim().is_empty() && reply.stderr.trim().is_empty()),
        stdout: reply.stdout,
        stderr: reply.stderr,
        raw_output_path: None,
        interrupted: reply.interrupted,
        is_image: None,
        background_task_id: reply.background_task_id,
        backgrounded_by_user: background.then_some(false),
        assistant_auto_backgrounded: background.then_some(false),
        dangerously_disable_sandbox: None,
        return_code_interpretation,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn input(command: &str) -> BashCommandInput {
        BashCommandInput {
            command: command.to_string(),
            timeout: Some(5_000),
            description: Some("d".to_string()),
            run_in_background: None,
            dangerously_disable_sandbox: None,
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        }
    }

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("helper.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn dates_from_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_746), (2026, 10, 20));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(today_utc().len(), 10);
    }

    #[test]
    fn host_prompt_is_the_file_plus_environment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.md");
        std::fs::write(&file, "You are YYC AI.\n\n").unwrap();
        let sections = host_system_prompt(&file, Path::new("/w/c1"), "2026-10-20").unwrap();
        assert_eq!(sections[0], "You are YYC AI.");
        assert!(sections[1].contains("2026-10-20"));
        assert!(sections[1].contains("/w/c1"));
        assert_eq!(sections.len(), 2);
        assert!(host_system_prompt(&dir.path().join("missing"), Path::new("/"), "d").is_err());
    }

    #[test]
    fn remote_bash_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // echoes back what it got, from the cwd it was given
        let helper = script(
            dir.path(),
            r#"in=$(cat); printf '{"stdout":"%s|%s","stderr":"","exit_code":3,"interrupted":false}' "$(pwd)" "$(echo "$in" | tr -d '"{}' | cut -c1-20)""#,
        );
        let out = run_remote_bash(helper.as_os_str(), dir.path(), &input("ls")).unwrap();
        assert!(out
            .stdout
            .starts_with(&dir.path().canonicalize().unwrap().display().to_string()));
        assert!(out.stdout.contains("command:ls"));
        assert_eq!(
            out.return_code_interpretation.as_deref(),
            Some("exit_code:3")
        );
    }

    #[test]
    fn remote_bash_background_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bg = script(
            dir.path(),
            r#"cat >/dev/null; echo '{"background_task_id":"job-1"}'"#,
        );
        let out = run_remote_bash(bg.as_os_str(), dir.path(), &input("sleep 9")).unwrap();
        assert_eq!(out.background_task_id.as_deref(), Some("job-1"));
        assert_eq!(out.return_code_interpretation, None);

        let fail = script(
            dir.path(),
            "cat >/dev/null; echo 'sandbox pool is full' >&2; exit 1",
        );
        let error = run_remote_bash(fail.as_os_str(), dir.path(), &input("x")).unwrap_err();
        assert_eq!(error.to_string(), "sandbox pool is full");

        let junk = script(dir.path(), "cat >/dev/null; echo nope");
        assert!(run_remote_bash(junk.as_os_str(), dir.path(), &input("x")).is_err());

        let timeout = script(
            dir.path(),
            r#"cat >/dev/null; echo '{"interrupted":true,"stderr":"timed out"}'"#,
        );
        let out = run_remote_bash(timeout.as_os_str(), dir.path(), &input("x")).unwrap();
        assert!(out.interrupted);
        assert_eq!(out.return_code_interpretation.as_deref(), Some("timeout"));
    }
}
