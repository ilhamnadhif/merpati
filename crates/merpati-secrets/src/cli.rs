//! Running helper programs (`op`, `az`, AWS `credential_process`).
//!
//! Desktop apps started from the Finder or a launcher get a minimal `PATH`,
//! so the usual install folders are searched too. Output is never logged.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::env::ProcessEnv;
use crate::error::{ErrorKind, SecretError, scrub, shorten};

/// A CLI the user may need to install.
pub(crate) struct Tool {
    pub program: &'static str,
    pub label: &'static str,
    pub install_hint: &'static str,
}

pub(crate) const OP: Tool = Tool {
    program: "op",
    label: "1Password CLI (op)",
    install_hint: "install it from https://developer.1password.com/docs/cli/get-started/",
};

pub(crate) const AZ: Tool = Tool {
    program: "az",
    label: "Azure CLI (az)",
    install_hint: "install it from https://learn.microsoft.com/cli/azure/install-azure-cli and run `az login`",
};

fn extra_dirs(env: &ProcessEnv) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env.var(var) {
                let base = PathBuf::from(base);
                dirs.push(base.join("1Password CLI"));
                dirs.push(base.join("Microsoft SDKs").join("Azure").join("CLI2").join("wbin"));
                dirs.push(base.join("Microsoft").join("WinGet").join("Links"));
            }
        }
    } else {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/snap/bin"].map(PathBuf::from));
        if let Some(home) = env.home() {
            dirs.push(home.join(".local").join("bin"));
            dirs.push(home.join("bin"));
        }
    }
    dirs
}

fn candidates(program: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{program}.exe"), format!("{program}.cmd"), format!("{program}.bat")]
    } else {
        vec![program.to_string()]
    }
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Finds `tool`: the configured path, else `PATH`, else the usual folders.
pub(crate) fn find(tool: &Tool, configured: &str, env: &ProcessEnv) -> Result<PathBuf, SecretError> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        if is_executable(&p) {
            return Ok(p);
        }
        return Err(SecretError::new(
            ErrorKind::CliMissing,
            format!(
                "{} was not found at {} (or is not executable); fix the path or {}.",
                tool.label,
                p.display(),
                tool.install_hint
            ),
        ));
    }
    let path_dirs: Vec<PathBuf> = env.path_var().map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for dir in path_dirs.into_iter().chain(extra_dirs(env)) {
        for name in candidates(tool.program) {
            let p = dir.join(&name);
            if is_executable(&p) {
                return Ok(p);
            }
        }
    }
    Err(SecretError::new(
        ErrorKind::CliMissing,
        format!(
            "{} was not found on PATH; {}, or set its path in the provider settings.",
            tool.label, tool.install_hint
        ),
    ))
}

pub(crate) struct CliOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub success: bool,
}

impl CliOutput {
    /// First stderr line, shortened and scrubbed.
    pub fn error_line(&self, secrets: &[&str]) -> String {
        let line = self
            .stderr
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .map(|l| {
                // `[ERROR] 2024/01/02 03:04:05 message` -> `message`.
                let l = l.strip_prefix("[ERROR]").map(str::trim_start).unwrap_or(l);
                let mut parts = l.splitn(3, ' ');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(d), Some(t), Some(rest)) if d.contains('/') && t.contains(':') => rest.to_string(),
                    _ => l.to_string(),
                }
            })
            .unwrap_or_else(|| "no error message".into());
        // Scrub first: shortening could cut a secret so that it no longer matches.
        shorten(&scrub(&line, secrets))
    }
}

/// Runs a program with a timeout; stdin is closed.
pub(crate) async fn run(
    program: &Path,
    args: &[OsString],
    envs: &[(&str, String)],
    timeout: Duration,
    label: &str,
) -> Result<CliOutput, SecretError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd.spawn().map_err(|e| {
        let kind = if e.kind() == std::io::ErrorKind::NotFound { ErrorKind::CliMissing } else { ErrorKind::Provider };
        SecretError::new(kind, format!("Could not start {label} ({}): {e}", program.display()))
    })?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(CliOutput {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }),
        Ok(Err(e)) => Err(SecretError::provider(format!("{label} failed: {e}"))),
        Err(_) => Err(SecretError::new(
            ErrorKind::Timeout,
            format!(
                "{label} did not finish within {} s (is it waiting for a sign-in or approval prompt?).",
                timeout.as_secs()
            ),
        )),
    }
}

/// Runs a shell command line (`sh -c` / `cmd /C`).
pub(crate) async fn run_shell(command: &str, timeout: Duration, label: &str) -> Result<CliOutput, SecretError> {
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    run(Path::new(shell), &[flag.into(), command.into()], &[], timeout, label).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_line_strips_op_prefix() {
        let out = CliOutput {
            stdout: Vec::new(),
            stderr: "\n[ERROR] 2024/05/06 07:08:09 could not read secret: item not found\nmore".into(),
            success: false,
        };
        assert_eq!(out.error_line(&[]), "could not read secret: item not found");
    }

    #[test]
    fn error_line_scrubs_a_secret_cut_by_shortening() {
        let out = CliOutput {
            stdout: Vec::new(),
            stderr: format!("{} ops_0123456789abcdef is invalid", "x".repeat(296)),
            success: false,
        };
        let line = out.error_line(&["ops_0123456789abcdef"]);
        assert!(!line.contains("ops"), "{line}");
    }

    #[cfg(unix)]
    #[test]
    fn find_uses_configured_path_and_path_var() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("op");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = ProcessEnv::fixed(&[("PATH", dir.path().to_str().unwrap()), ("HOME", "/nonexistent")]);
        assert_eq!(find(&OP, "", &env).unwrap(), p);
        assert_eq!(find(&OP, p.to_str().unwrap(), &env).unwrap(), p);
        let e = find(&OP, "/nonexistent/op", &env).unwrap_err();
        assert_eq!(e.kind, ErrorKind::CliMissing);
        let tool = Tool { program: "merpati-no-such-tool", label: "X", install_hint: "install X" };
        assert_eq!(find(&tool, "", &env).unwrap_err().kind, ErrorKind::CliMissing);
    }
}
