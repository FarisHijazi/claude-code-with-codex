//! Spawning and supervising a headless `cursor-agent` run.
//!
//! Headless `-p` mode never blocks on stdin — no approval prompt or clarifying
//! question can hang a run — so stdin is closed and the process is left to
//! finish on its own, bounded by a timeout.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};

use super::models::{AgentMode, ParsedModel};
use crate::anthropic::schema::MessagesRequest;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub binary: String,
    pub model: String,
    pub mode: AgentMode,
    pub workspace: Option<String>,
    pub timeout: Duration,
}

impl RunOptions {
    /// The configured workspace, or the one the request states — see
    /// [`super::workspace`] for why a request may pick it at all.
    pub fn from_request(parsed: &ParsedModel, req: &MessagesRequest) -> Self {
        Self {
            binary: crate::config::cursor_cli_binary(),
            model: parsed.model.clone(),
            mode: parsed.mode,
            workspace: super::workspace::resolve(req),
            timeout: Duration::from_secs(crate::config::cursor_cli_timeout_secs()),
        }
    }

    /// Arguments for a headless, machine-readable run.
    pub fn args(&self, prompt: &str) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            // Without this the CLI emits only one settled message at the end,
            // and nothing reaches the caller until the run finishes.
            "--stream-partial-output".to_string(),
        ];
        if self.model != "auto" {
            args.push("--model".to_string());
            args.push(self.model.clone());
        }
        args.extend(self.mode.cli_flags());
        if let Some(workspace) = self.workspace.as_deref() {
            args.push("--workspace".to_string());
            args.push(workspace.to_string());
        }
        // The prompt is a positional argument and must come last.
        args.push(prompt.to_string());
        args
    }
}

#[derive(Debug)]
pub enum SpawnError {
    /// The binary is missing or not executable.
    NotFound {
        binary: String,
        detail: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::NotFound { binary, detail } => write!(
                formatter,
                "cannot run `{binary}` ({detail}). Install the Cursor CLI \
                 (https://cursor.com/cli), make sure it is on PATH, and sign in with \
                 `cursor-agent login`; or set CCP_CURSOR_CLI_BINARY to its full path."
            ),
            SpawnError::Io(error) => write!(formatter, "failed to start cursor-agent: {error}"),
        }
    }
}

pub struct RunningAgent {
    child: Child,
    pub stdout: ChildStdout,
}

impl RunningAgent {
    /// Kill the process and reap it, so a timed-out run leaves nothing behind.
    ///
    /// Targets the child by its own handle — never by name or command-line
    /// pattern, which would risk signalling an unrelated cursor-agent the user
    /// is running themselves.
    pub async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    /// Drain stderr after the fact, for a diagnostic when a run produced no
    /// usable output.
    pub async fn stderr_tail(&mut self) -> String {
        let Some(mut stderr) = self.child.stderr.take() else {
            return String::new();
        };
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer).await;
        String::from_utf8_lossy(&buffer).trim().to_string()
    }

    pub async fn wait(mut self) -> Option<std::process::ExitStatus> {
        self.child.wait().await.ok()
    }
}

pub fn spawn(options: &RunOptions, prompt: &str) -> Result<RunningAgent, SpawnError> {
    let mut command = Command::new(&options.binary);
    command
        .args(options.args(prompt))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Leaving the group attached would let a Ctrl-C in the proxy's terminal
        // reach the agent mid-edit.
        .kill_on_drop(true);

    if let Some(workspace) = options.workspace.as_deref() {
        command.current_dir(workspace);
    }

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SpawnError::NotFound {
                binary: options.binary.clone(),
                detail: error.to_string(),
            }
        } else {
            SpawnError::Io(error)
        }
    })?;

    let stdout = child.stdout.take().expect("stdout was piped");
    Ok(RunningAgent { child, stdout })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(mode: AgentMode, model: &str) -> RunOptions {
        RunOptions {
            binary: "cursor-agent".into(),
            model: model.into(),
            mode,
            workspace: None,
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn headless_flags_are_always_present() {
        let args = options(AgentMode::Ask, "auto").args("do a thing");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"--stream-partial-output".to_string()));
    }

    #[test]
    fn prompt_is_the_final_positional_argument() {
        let args = options(AgentMode::Ask, "auto").args("the prompt");
        assert_eq!(args.last().map(String::as_str), Some("the prompt"));
    }

    /// `auto` is the CLI's own default; passing it explicitly is unnecessary
    /// and would break if the id is ever renamed.
    #[test]
    fn auto_model_is_not_passed_explicitly() {
        let args = options(AgentMode::Ask, "auto").args("x");
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn explicit_model_is_passed() {
        let args = options(AgentMode::Ask, "composer-2.5").args("x");
        let index = args
            .iter()
            .position(|arg| arg == "--model")
            .expect("--model");
        assert_eq!(args[index + 1], "composer-2.5");
    }

    /// The safety-critical assertion: `--force` is what lets the agent write
    /// and run commands, so a read-only run must never carry it. `--trust`
    /// only answers the directory-trust prompt and is required headless.
    #[test]
    fn read_only_modes_never_pass_the_write_grant() {
        for mode in [AgentMode::Ask, AgentMode::Plan] {
            let args = options(mode, "auto").args("x");
            assert!(!args.contains(&"--force".to_string()), "{mode:?}: {args:?}");
            assert!(!args.contains(&"--yolo".to_string()), "{mode:?}: {args:?}");
            assert!(args.contains(&"--mode".to_string()), "{mode:?}: {args:?}");
        }
    }

    #[test]
    fn agent_mode_passes_force_and_trust() {
        let args = options(AgentMode::Agent, "auto").args("x");
        assert!(args.contains(&"--force".to_string()));
        assert!(args.contains(&"--trust".to_string()));
    }

    #[test]
    fn workspace_is_forwarded_when_configured() {
        let mut opts = options(AgentMode::Ask, "auto");
        opts.workspace = Some("/tmp/project".into());
        let args = opts.args("x");
        let index = args
            .iter()
            .position(|arg| arg == "--workspace")
            .expect("--workspace");
        assert_eq!(args[index + 1], "/tmp/project");
    }

    #[test]
    fn missing_binary_produces_actionable_guidance() {
        let mut opts = options(AgentMode::Ask, "auto");
        opts.binary = "definitely-not-a-real-binary-xyz".into();
        let Err(error) = spawn(&opts, "hi") else {
            panic!("spawning a missing binary must fail");
        };
        let message = error.to_string();
        assert!(
            message.contains("definitely-not-a-real-binary-xyz"),
            "{message}"
        );
        assert!(message.contains("cursor-agent login"), "{message}");
        assert!(message.contains("CCP_CURSOR_CLI_BINARY"), "{message}");
    }
}

/// Whether `binary` can actually be spawned: an explicit path that exists and is
/// executable, or a bare name resolvable on `PATH`.
///
/// Deliberately does not run it — this answers "is the CLI installed", which is
/// all a model listing needs, without paying for a subprocess.
pub fn binary_is_runnable(binary: &str) -> bool {
    let path = std::path::Path::new(binary);
    if path.is_absolute() || binary.contains('/') {
        return is_executable_file(path);
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| is_executable_file(&dir.join(binary)))
}

fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod availability_tests {
    use super::binary_is_runnable;

    #[test]
    fn a_bare_name_resolves_through_path() {
        assert!(binary_is_runnable("sh"));
        assert!(!binary_is_runnable("definitely-not-a-real-binary-xyzzy"));
    }

    #[test]
    fn an_explicit_path_must_exist_and_be_executable() {
        assert!(binary_is_runnable("/bin/sh"));
        assert!(!binary_is_runnable("/bin/sh/nope"));
        assert!(!binary_is_runnable("/etc/hosts"));
    }
}
