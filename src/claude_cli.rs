use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const UPDATE_TIMEOUT: Duration = Duration::from_secs(60);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateResult {
    pub before_version: Option<String>,
    pub after_version: Option<String>,
    pub version_changed: bool,
}

pub(crate) fn run_update() -> Result<UpdateResult, String> {
    if updates_disabled() {
        return Err("disabled_by_environment".to_string());
    }
    let executable = find_executable().ok_or_else(|| "cli_not_found".to_string())?;
    let before_version = read_version(&executable);

    let status = run_with_timeout(
        Command::new(&executable)
            .arg("update")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        UPDATE_TIMEOUT,
    )
    .ok_or_else(|| "update_start_or_timeout".to_string())?;
    if !status.success() {
        return Err(format!("update_exit_{}", status.code().unwrap_or(-1)));
    }

    let after_version = read_version(&executable);
    let version_changed = versions_differ(before_version.as_deref(), after_version.as_deref());
    Ok(UpdateResult {
        before_version,
        after_version,
        version_changed,
    })
}

fn versions_differ(before: Option<&str>, after: Option<&str>) -> bool {
    matches!((before, after), (Some(before), Some(after)) if before != after)
}

fn updates_disabled() -> bool {
    std::env::var_os("DISABLE_UPDATES").is_some_and(|value| !value.is_empty())
}

pub(crate) fn find_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_CLI_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }

    let names = ["claude.exe", "claude.cmd", "claude.bat"];
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    let mut candidates = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        candidates.push(PathBuf::from(profile).join(".local/bin/claude.exe"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let npm = PathBuf::from(appdata).join("npm");
        candidates.push(npm.join("claude.exe"));
        candidates.push(npm.join("claude.cmd"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn read_version(executable: &Path) -> Option<String> {
    let output = run_output_with_timeout(
        Command::new(executable)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        VERSION_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    first_nonempty_line(&output.stdout).or_else(|| first_nonempty_line(&output.stderr))
}

fn first_nonempty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Option<std::process::ExitStatus> {
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

fn run_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_nonempty_line_ignores_blank_lines() {
        assert_eq!(
            first_nonempty_line(b"\r\n  2.1.85 (Claude Code) \r\n"),
            Some("2.1.85 (Claude Code)".to_string())
        );
    }

    #[test]
    fn notification_requires_two_known_different_versions() {
        assert!(versions_differ(Some("2.1.84"), Some("2.1.85")));
        assert!(!versions_differ(Some("2.1.85"), Some("2.1.85")));
        assert!(!versions_differ(None, Some("2.1.85")));
        assert!(!versions_differ(Some("2.1.85"), None));
    }
}
