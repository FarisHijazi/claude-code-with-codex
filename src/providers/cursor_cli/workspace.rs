//! Which directory the agent runs in.
//!
//! An Anthropic request carries no working directory of its own, so without
//! this the agent would always run wherever the proxy happens to live — useless
//! for coding, because none of the caller's files are there. Claude Code does
//! state its cwd, in the environment block it prepends to the conversation, so
//! reading it back out puts the agent in the project the caller has open.
//!
//! A pinned `cursorCli.workspace` still wins, because an operator who named a
//! directory meant that one. But note what the pin is and is not: `--workspace`
//! chooses where the agent *starts*, not what it may touch. Measured against a
//! pinned instance, an agent handed an absolute path in the prompt read it
//! anyway. Containment comes from the mode — `ask` and `plan` cannot write —
//! and from the permissions of the user the proxy runs as; never from this.

use crate::anthropic::schema::MessagesRequest;
use crate::providers::translate_shared::{ContentBlock, flatten_system_text, normalize_content};

/// Lines Claude Code uses to state its own working directory. The first is what
/// current versions emit in the environment block; the second is the older
/// `<env>` phrasing, kept so an older client still lands in the right place.
const MARKERS: &[&str] = &["Primary working directory:", "Working directory:"];

/// The configured workspace if there is one, otherwise whatever the request
/// says, otherwise `None` — meaning the proxy's own cwd.
pub fn resolve(req: &MessagesRequest) -> Option<String> {
    if let Some(configured) = crate::config::cursor_cli_workspace() {
        return Some(configured);
    }
    detect(req)
}

/// The freshest working directory stated anywhere in the conversation.
///
/// Scanned newest-first: the environment block is re-sent every turn, and the
/// last one is the only one still true.
fn detect(req: &MessagesRequest) -> Option<String> {
    for message in req.messages.iter().rev() {
        let blocks = normalize_content(&message.content, serde_json::json!({}));
        for block in blocks.iter().rev() {
            if let ContentBlock::Text { text } = block
                && let Some(dir) = scan(text)
            {
                return Some(dir);
            }
        }
    }
    flatten_system_text(req.extra.get("system")).and_then(|text| scan(&text))
}

/// The last directory a marker line points at, if it is one that exists.
///
/// Prose *about* the marker is common in a CLAUDE.md that documents this very
/// feature, so a match has to be a whole line and has to name a real absolute
/// directory before it is believed.
fn scan(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let line = line.trim().trim_start_matches(['-', '*', '#']).trim();
        let rest = MARKERS
            .iter()
            .find_map(|marker| line.strip_prefix(marker))?
            .trim();
        let candidate = rest.trim_matches(['`', '"', '\'']).trim_end_matches('/');
        let path = std::path::Path::new(candidate);
        (path.is_absolute() && path.is_dir()).then(|| candidate.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(messages: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "cursor-cli",
            "max_tokens": 1024,
            "messages": messages,
        }))
        .expect("request")
    }

    /// The exact shape a live Claude Code sends, captured off the wire.
    #[test]
    fn detects_the_env_block_claude_code_sends() {
        let env = format!(
            "# Environment\n - Primary working directory: {}\n - Is a git repository: true\n",
            env!("CARGO_MANIFEST_DIR")
        );
        let req = request(json!([
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [{"type": "text", "text": env}]},
        ]));
        assert_eq!(detect(&req).as_deref(), Some(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn accepts_the_older_env_phrasing() {
        let env = format!(
            "<env>\nWorking directory: {}\n</env>",
            env!("CARGO_MANIFEST_DIR")
        );
        let req = request(json!([{"role": "user", "content": env}]));
        assert_eq!(detect(&req).as_deref(), Some(env!("CARGO_MANIFEST_DIR")));
    }

    /// Documentation quoting the marker must not be mistaken for the real one.
    #[test]
    fn ignores_prose_and_paths_that_do_not_exist() {
        let req = request(json!([{
            "role": "user",
            "content": "Claude Code puts its Primary working directory: in the env block.\n\
                        Primary working directory: /nope/not/a/real/dir",
        }]));
        assert_eq!(detect(&req), None);
    }

    #[test]
    fn relative_paths_are_rejected() {
        let req = request(json!([{"role": "user", "content": "Working directory: src"}]));
        assert_eq!(detect(&req), None);
    }

    /// Every turn re-sends the block; only the newest one is still true.
    #[test]
    fn the_freshest_block_wins() {
        let stale = format!(
            "Primary working directory: {}/src",
            env!("CARGO_MANIFEST_DIR")
        );
        let fresh = format!("Primary working directory: {}", env!("CARGO_MANIFEST_DIR"));
        let req = request(json!([
            {"role": "user", "content": stale},
            {"role": "user", "content": fresh},
        ]));
        assert_eq!(detect(&req).as_deref(), Some(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn a_trailing_slash_is_normalised_away() {
        let env = format!("Primary working directory: {}/", env!("CARGO_MANIFEST_DIR"));
        let req = request(json!([{"role": "user", "content": env}]));
        assert_eq!(detect(&req).as_deref(), Some(env!("CARGO_MANIFEST_DIR")));
    }
}
