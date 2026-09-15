//! Model-name parsing for the cursor-agent CLI backend.
//!
//! The model slot carries two things: which Cursor model to run, and which
//! execution mode to run it in. Mode rides on a prefix, matching the
//! `cursor:` / `cursor-plan:` / `cursor-ask:` convention the API-backed cursor
//! backend already uses.
//!
//! ```text
//! cursor-cli                      -> ask mode, default model
//! cursor-cli:gpt-5.3-codex        -> ask mode  (read-only, the default)
//! cursor-cli-ask:composer-2.5     -> ask mode  (read-only Q&A)
//! cursor-cli-plan:composer-2.5    -> plan mode (read-only analysis)
//! cursor-cli-agent:composer-2.5   -> full agent, CAN EDIT FILES (gated)
//! ```

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Used when no model is named. `auto` lets Cursor pick, which keeps working
/// as their lineup changes.
pub const CURSOR_CLI_DEFAULT_MODEL: &str = "auto";

pub const CURSOR_CLI_PREFIXES: &[&str] = &[
    "cursor-cli-agent:",
    "cursor-cli-plan:",
    "cursor-cli-ask:",
    "cursor-cli:",
];

/// Bare ids that select the backend without naming a model.
pub const CURSOR_CLI_BARE: &[&str] = &[
    "cursor-cli",
    "cursor-cli-ask",
    "cursor-cli-plan",
    "cursor-cli-agent",
];

/// How the spawned agent is allowed to act on the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// `--mode ask`: question answering, read-only.
    Ask,
    /// `--mode plan`: analysis and proposals, read-only.
    Plan,
    /// Full agent with write and shell access. Requires explicit opt-in.
    Agent,
}

impl AgentMode {
    pub fn is_write(self) -> bool {
        matches!(self, AgentMode::Agent)
    }

    /// Flags for a headless run.
    ///
    /// `--trust` goes on every mode: it answers the CLI's "do you trust this
    /// directory" prompt, which otherwise refuses to run at all with no TTY —
    /// including in read-only modes. It grants *directory* trust, not write
    /// access.
    ///
    /// Read-only is enforced by `--mode ask` / `--mode plan`. The write grant
    /// is `--force`, which is the flag that lets the agent run commands and
    /// edit files, and it appears in exactly one arm below.
    pub fn cli_flags(self) -> Vec<String> {
        match self {
            AgentMode::Ask => vec!["--mode".into(), "ask".into(), "--trust".into()],
            AgentMode::Plan => vec!["--mode".into(), "plan".into(), "--trust".into()],
            AgentMode::Agent => vec!["--force".into(), "--trust".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModel {
    pub model: String,
    pub mode: AgentMode,
}

/// True when this model name belongs to the cursor-agent CLI backend.
///
/// Checked before the API-backed `cursor:` prefixes so `cursor-cli:` is never
/// swallowed by them.
pub fn is_cursor_cli_model(model: &str) -> bool {
    CURSOR_CLI_BARE.contains(&model)
        || CURSOR_CLI_PREFIXES
            .iter()
            .any(|prefix| model.starts_with(prefix))
}

pub fn parse_model(raw: &str) -> ParsedModel {
    let raw = raw.trim();

    // Longest prefix first, so `cursor-cli-agent:` is not read as `cursor-cli`.
    for prefix in CURSOR_CLI_PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let mode = mode_for_prefix(prefix);
            let model = if rest.trim().is_empty() {
                default_model()
            } else {
                rest.trim().to_string()
            };
            return ParsedModel { model, mode };
        }
    }

    let mode = match raw {
        "cursor-cli-agent" => AgentMode::Agent,
        "cursor-cli-plan" => AgentMode::Plan,
        _ => AgentMode::Ask,
    };
    ParsedModel {
        model: default_model(),
        mode,
    }
}

fn mode_for_prefix(prefix: &str) -> AgentMode {
    match prefix {
        "cursor-cli-agent:" => AgentMode::Agent,
        "cursor-cli-plan:" => AgentMode::Plan,
        _ => AgentMode::Ask,
    }
}

fn default_model() -> String {
    crate::config::cursor_cli_default_model()
        .unwrap_or_else(|| CURSOR_CLI_DEFAULT_MODEL.to_string())
}

/// Fallback ids used when `cursor-agent --list-models` cannot be reached.
/// Cursor's lineup drifts, so discovery is authoritative and this only keeps
/// `/v1/models` from going empty.
const FALLBACK_MODELS: &[&str] = &[
    "auto",
    "composer-2.5",
    "gpt-5.3-codex",
    "cursor-grok-4.5-high",
];

struct ModelCache {
    models: Vec<String>,
    fetched_at: Instant,
}

static MODEL_CACHE: once_cell::sync::Lazy<Mutex<Option<ModelCache>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

const MODEL_CACHE_TTL: Duration = Duration::from_secs(600);

/// Ask the CLI which models exist.
///
/// Only reached from `claude-codex cursor-cli auth status`; the registry no
/// longer shells out at startup. Cached anyway, since the answer changes
/// rarely.
pub fn discover_models() -> Vec<String> {
    if let Ok(guard) = MODEL_CACHE.lock()
        && let Some(cache) = guard.as_ref()
        && cache.fetched_at.elapsed() < MODEL_CACHE_TTL
    {
        return cache.models.clone();
    }

    let models = fetch_models().unwrap_or_else(|| {
        FALLBACK_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    });

    if let Ok(mut guard) = MODEL_CACHE.lock() {
        *guard = Some(ModelCache {
            models: models.clone(),
            fetched_at: Instant::now(),
        });
    }
    models
}

/// Discovery runs during registry construction, which happens at server
/// startup, so it must not be able to block indefinitely. The call is made on a
/// detached thread and abandoned past the deadline; the fallback list covers a
/// missing or wedged CLI.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

fn fetch_models() -> Option<Vec<String>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(list_models_blocking());
    });
    receiver.recv_timeout(DISCOVERY_TIMEOUT).ok().flatten()
}

fn list_models_blocking() -> Option<Vec<String>> {
    let output = std::process::Command::new(crate::config::cursor_cli_binary())
        .arg("--list-models")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed = parse_model_list(&String::from_utf8_lossy(&output.stdout));
    (!parsed.is_empty()).then_some(parsed)
}

/// `--list-models` prints `<id> - <label>` lines under a heading.
pub fn parse_model_list(stdout: &str) -> Vec<String> {
    let mut models = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("Available models") {
            continue;
        }
        let id = line
            .split_once(" - ")
            .map(|(id, _)| id)
            .unwrap_or(line)
            .trim();
        // Ids are single tokens; anything with a space is prose, not a model.
        if id.is_empty() || id.contains(char::is_whitespace) {
            continue;
        }
        if !models.iter().any(|existing| existing == id) {
            models.push(id.to_string());
        }
    }
    models
}

/// Ids advertised over `/v1/models`.
///
/// Deliberately just the bare mode selectors. Cursor offers a few hundred
/// models, and advertising `cursor-cli:<model>` for each would bury Claude
/// Code's `/model` picker under hundreds of near-identical entries — the same
/// reason the API-backed `cursor` backend advertises only its short list.
/// Any `cursor-cli:<model>` still routes, because routing matches the prefix;
/// `cursor-agent --list-models` (or `claude-codex cursor-cli auth status`)
/// lists what can go after the colon.
///
/// The write-capable id appears only once write access is enabled, so an
/// unconfigured install cannot pick a file-editing agent from the menu by
/// accident.
pub fn supported_models() -> Vec<String> {
    let mut out = vec![
        "cursor-cli".to_string(),
        "cursor-cli-ask".to_string(),
        "cursor-cli-plan".to_string(),
    ];
    if crate::config::cursor_cli_allow_write() {
        out.push("cursor-cli-agent".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_id_defaults_to_read_only_ask() {
        let parsed = parse_model("cursor-cli");
        assert_eq!(parsed.mode, AgentMode::Ask);
        assert!(!parsed.mode.is_write());
    }

    #[test]
    fn prefixes_select_the_mode_and_keep_the_model() {
        assert_eq!(
            parse_model("cursor-cli:composer-2.5"),
            ParsedModel {
                model: "composer-2.5".into(),
                mode: AgentMode::Ask
            }
        );
        assert_eq!(
            parse_model("cursor-cli-plan:gpt-5.3-codex"),
            ParsedModel {
                model: "gpt-5.3-codex".into(),
                mode: AgentMode::Plan
            }
        );
        assert_eq!(
            parse_model("cursor-cli-agent:composer-2.5"),
            ParsedModel {
                model: "composer-2.5".into(),
                mode: AgentMode::Agent
            }
        );
    }

    /// `cursor-cli-agent:` must not be parsed as `cursor-cli` with a model
    /// named `-agent:...`, which would silently downgrade the mode.
    #[test]
    fn longest_prefix_wins() {
        let parsed = parse_model("cursor-cli-agent:gpt-5.3-codex");
        assert_eq!(parsed.mode, AgentMode::Agent);
        assert_eq!(parsed.model, "gpt-5.3-codex");
    }

    #[test]
    fn cursor_cli_ids_are_recognized_and_others_are_not() {
        for model in [
            "cursor-cli",
            "cursor-cli:auto",
            "cursor-cli-ask:auto",
            "cursor-cli-plan:auto",
            "cursor-cli-agent:auto",
        ] {
            assert!(is_cursor_cli_model(model), "{model} should be cursor-cli");
        }
        // These belong to the API-backed cursor backend.
        for model in [
            "cursor",
            "cursor:gpt-5.5",
            "cursor-plan:gpt-5.5",
            "composer-2.5",
        ] {
            assert!(
                !is_cursor_cli_model(model),
                "{model} should not be cursor-cli"
            );
        }
    }

    /// `--force` is the write grant and must never appear in a read-only mode.
    /// `--trust` must appear everywhere: without it the CLI refuses to run
    /// headless, which is a real failure observed end to end.
    #[test]
    fn read_only_modes_pass_trust_but_never_force() {
        for mode in [AgentMode::Ask, AgentMode::Plan] {
            let flags = mode.cli_flags();
            assert!(
                !flags.contains(&"--force".to_string()),
                "{mode:?}: {flags:?}"
            );
            assert!(
                !flags.contains(&"--yolo".to_string()),
                "{mode:?}: {flags:?}"
            );
            assert!(
                flags.contains(&"--trust".to_string()),
                "{mode:?}: {flags:?}"
            );
            assert!(flags.contains(&"--mode".to_string()), "{mode:?}: {flags:?}");
        }
        assert_eq!(AgentMode::Ask.cli_flags(), vec!["--mode", "ask", "--trust"]);
        assert_eq!(
            AgentMode::Plan.cli_flags(),
            vec!["--mode", "plan", "--trust"]
        );
    }

    #[test]
    fn agent_mode_passes_force_and_trust() {
        let flags = AgentMode::Agent.cli_flags();
        assert!(flags.contains(&"--force".to_string()));
        assert!(flags.contains(&"--trust".to_string()));
        assert!(AgentMode::Agent.is_write());
    }

    #[test]
    fn model_list_output_is_parsed() {
        let stdout = "Available models\n\nauto - Auto (default)\ngpt-5.3-codex - Codex 5.3\ncomposer-2.5 - Composer 2.5\n";
        assert_eq!(
            parse_model_list(stdout),
            vec!["auto", "gpt-5.3-codex", "composer-2.5"]
        );
    }

    #[test]
    fn model_list_ignores_prose_and_duplicates() {
        let stdout = "Available models\nSome descriptive sentence here\nauto - Auto\nauto - Auto\n";
        assert_eq!(parse_model_list(stdout), vec!["auto"]);
    }
}
