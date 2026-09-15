# claude-code-with-codex (FarisHijazi fork)

Local Rust proxy that speaks the Anthropic Messages API and routes each request
to a backend based on the model name. Fork of
[fcakyon/claude-code-with-codex](https://github.com/fcakyon/claude-code-with-codex);
`upstream` remote points there. Upstream conventions live in
[AGENTS.md](AGENTS.md) and still apply.

## The constraint that drives everything

**No paid API keys.** Every backend runs on a login that already exists: the
Claude Code subscription, the Codex CLI login, `cursor-agent login`, a
signed-in Google account. Before adding a backend, check it can work without a
per-token-billed key — if it cannot, it does not belong here.

## Architecture

One `Provider` trait (`src/provider.rs`) per backend; `src/registry.rs` maps a
model name to one. `src/server.rs` owns the routes. Translation helpers shared
by every backend live in `src/providers/translate_shared.rs` — prefer extending
that over duplicating per-backend logic.

| Backend | Talks to | Login |
| --- | --- | --- |
| `anthropic` | api.anthropic.com, byte-passthrough | Claude Code's own |
| `codex` | OpenAI Responses API | Codex CLI `~/.codex/auth.json` |
| `kimi`, `grok`, `cursor` | each vendor's API | own, per backend |
| `gemini` | gemini-web-api, OpenAI-compatible | none — lives in that server |
| `cursor-cli` | the `cursor-agent` binary | none — reuses the CLI's |

## What this fork adds

Design rationale, including the alternatives rejected and why, is in
`docs/devlog/claude_20260915-gemini-cursorcli-auth.md`. Read it before changing
any of the three.

1. **`gemini`** (`src/providers/gemini/`) — Anthropic ⇄ OpenAI chat-completions
   against a configurable base URL, so it also serves any other
   OpenAI-compatible server. Chat only, by design.
2. **`cursor-cli`** (`src/providers/cursor_cli/`) — spawns headless
   `cursor-agent` and maps its `stream-json` events to Anthropic SSE.
3. **Optional inbound auth** (`src/inbound_auth.rs`) — off unless a token is
   configured.

## Traps

1. **`cursor-agent` emits the full reply twice**, and `timestamp_ms` does not
   tell you which is which — a multi-step run was observed sending the settled
   message *with* a timestamp. `events.rs` compares against `segment` (the
   message being built) instead: equal means settled, extending means a tail,
   anything else is a delta, and `segment` resets at each thinking/tool_call
   boundary. Do not "simplify" this to a timestamp check or a substring
   search; both were tried and both corrupt output. Four regression tests
   guard it.
2. **`--trust` belongs on every cursor-agent mode.** It answers the
   directory-trust prompt, which headless runs cannot answer, so without it
   even `ask` mode refuses to start. The write grant is `--force`, and that is
   what must never appear in a read-only mode.
3. **`cursor-cli` never returns `tool_use`.** The agent already ran its tools.
   Advertising Claude Code's tools or emitting `tool_use` would make Claude Code
   re-run work that is already done.
4. **Write access is gated twice** — the `cursor-cli-agent:` prefix *and*
   `allowWrite`. Read-only modes must never pass `--force`/`--trust`; there is a
   test asserting exactly that.
5. **Inbound auth must not use `Authorization` alone.** That header carries
   Claude Code's subscription token to the Anthropic passthrough. The proxy
   credential goes in `x-claude-codex-key`; `Authorization` is only *also*
   accepted, and a non-matching one is not an error.
6. **Kimi's translator is not shared.** It is coupled to kimi specifics; the
   gemini backend has its own. `kimi::count_tokens` *is* generic and is reused
   by both new backends, as is `anthropic::accumulate_response` (SSE -> one
   Messages JSON), which every streaming backend needs for its non-streaming
   path.
7. **`gemini` overlaps with `claude-code-router`**, which routes to any
   OpenAI-compatible endpoint from config. It is kept to avoid running a second
   gateway alongside the Claude/Codex subscription logic here — see the devlog
   before extending it. `cursor-cli` has no equivalent anywhere.

## Working on it

```sh
cargo build && cargo test
cargo fmt --check && cargo clippy --all-targets
```

A server instance may already be running on `:18765` and be in active use —
check `lsof -nP -iTCP:18765` before starting one, and use a different port
(`PORT=18999 cargo run -- serve`) rather than stopping it.

Unit tests did not catch any of the traps above; all four came out of running
the binary against the real CLI. Exercise both new backends end to end before
calling a change to them done. The gemini path can be driven deterministically
by pointing `CCP_GEMINI_BASE_URL` at any OpenAI-compatible mock.
