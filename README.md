# claude-code-with-codex

[![crates.io](https://img.shields.io/crates/v/claude-codex.svg)](https://crates.io/crates/claude-codex)
[![CI](https://github.com/fcakyon/claude-code-with-codex/actions/workflows/ci.yml/badge.svg)](https://github.com/fcakyon/claude-code-with-codex/actions/workflows/ci.yml)

Use Claude Code with your **Claude subscription and your ChatGPT (Codex)
subscription at the same time**, and switch between them mid-conversation.

<img src="https://github.com/fcakyon/claude-code-with-codex/releases/download/v0.3.0/claude-codex-demo.gif" alt="Claude Code running through the proxy" />

It runs as a tiny local proxy. Claude Code already speaks the Anthropic API, so
the proxy sits in front of it and sends each request to the right place based on
the model name:

- Ask for a **Claude** model and it uses your **Claude subscription** (the login
  Claude Code already has). Nothing is translated and no API key is needed.
- Ask for a **`gpt-5.6-*`** model and it uses your **ChatGPT subscription**
  through the Codex login.

So you can keep Opus on your Claude plan for hard work and run the fast slot on
your ChatGPT plan, in the same session, and flip between them whenever you want.

[Quickstart](#quickstart) · [Switching models](#switching-models) ·
[How it works](#how-it-works) · [Configuration](#configuration) ·
[Other backends](#other-backends) · [Limitations](#limitations)

## What you need

- **Claude Code** installed and signed in with a **Claude Pro or Max** plan.
- A **ChatGPT Plus, Pro, or Team** plan and the **Codex CLI** signed in.
- **Rust** only if you install from crates.io or source. The prebuilt binary needs nothing.

## Quickstart

**1. Install `claude-codex`** using a prebuilt binary:

Prebuilt binary, no Rust needed (macOS and Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/fcakyon/claude-code-with-codex/main/scripts/install.sh | bash
```

Or install from crates.io if you have Rust:

```sh
cargo install claude-codex --locked
```

**2. Check your Codex CLI login:**

```sh
claude-codex codex auth status
```

Run `codex login` first if no valid account is found.

**3. Point Claude Code at the router** in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:18765"
  }
}
```

**4. Start the router** and leave it running:

```sh
claude-codex serve
```

**5. Restart Claude Code.**

Claude Code now discovers the models exposed by the router. Switch directly:

```text
/model gpt-5.6-sol[1m]
/model claude-opus-5
```

The `[1m]` suffix enables Claude Code's larger-context mode. The router removes
the suffix before sending the model name to Codex.

## Switching models

- **Inside Claude Code.** Run `/model gpt-5.6-sol[1m]` for Codex or
  `/model claude-opus-5` for Claude.
- **For one new session.** Set `ANTHROPIC_MODEL` when launching Claude Code.
- **List what is available.** `claude-codex models`.

Reasoning is carried across a switch. When you move a conversation from one plan
to the other, the earlier turn's thinking is kept and shown to the next model as
plain tagged text, so context is not lost.

## How it works

Claude Code sends normal Anthropic API requests to the proxy. The proxy reads
the model name and routes:

- **Claude models** are relayed straight to `api.anthropic.com`, untouched,
  reusing the subscription token Claude Code already sends. The request body is
  forwarded as-is so Anthropic's prompt caching keeps working. The proxy stores
  no Claude credentials.
- **Codex models** are translated to the OpenAI Responses API and sent with the
  ChatGPT login from the Codex CLI's `~/.codex/auth.json`. The proxy refreshes
  that token when needed and writes it back so the Codex CLI keeps working.

An unknown model name returns a clear 400 that lists the ids you can use.

## Configuration

Only `ANTHROPIC_BASE_URL` is required in Claude Code's user settings. Restart
Claude Code after changing it so `/model` discovers the router's model list.

| Variable                                   | What it does                                                     |
| ------------------------------------------ | ---------------------------------------------------------------- |
| `ANTHROPIC_BASE_URL`                       | Point Claude Code at the proxy, e.g. `http://localhost:18765`.   |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`             | Optionally remap the Opus alias.                                 |
| `ANTHROPIC_DEFAULT_SONNET_MODEL`           | Optionally remap the Sonnet alias.                               |
| `ANTHROPIC_MODEL`                          | Optionally force one model for the whole session.                |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Set to `1` to skip Claude Code's non-essential background calls. |

### Pointing one session somewhere else

`~/.claude/settings.json` wins over the process environment, so
`ANTHROPIC_BASE_URL=... claude` silently does nothing once that file sets it —
the session goes to the configured router anyway. To try a second router
without disturbing sessions already running against the first, override the
settings for that one session:

```sh
claude --settings '{"env":{"ANTHROPIC_BASE_URL":"http://localhost:18766"}}'
```

### The unrecognized-model warning

Claude Code only knows the context window of models in its own catalog, so a
router model it has never heard of prints a warning and is assumed to hold
200k tokens — which silently caps auto-compact below what the model can take.
The reply itself is unaffected. Two measured ways to settle it:

```sh
claude --model 'gemini-3-flash[1m]'                                 # 1M window
claude --settings '{"env":{"CLAUDE_CODE_MAX_CONTEXT_TOKENS":"1000000"}}'
```

The `[1m]` suffix is Claude Code's own and never reaches a backend — the router
strips it while resolving the model.

Added by this fork:

| Variable | What it does |
| --- | --- |
| `CCP_AUTH_TOKEN` | Require this token on inbound `/v1/*` requests. Unset = open (the default). |
| `CCP_AUTH_TOKEN_FILE` | Read that token from a file instead of the environment. |
| `CCP_GEMINI_BASE_URL` | gemini-web-api base URL. Default `http://localhost:8100/v1`. |
| `CCP_GEMINI_API_KEY` | Key for that server, if it is deployed behind its own auth. |
| `CCP_CURSOR_CLI_BINARY` | Path to `cursor-agent`. Default: found on `PATH`. |
| `CCP_CURSOR_CLI_ALLOW_WRITE` | `1` lets `cursor-cli-agent:` edit files. Default off. |
| `CCP_CURSOR_CLI_WORKSPACE` | Pin `cursor-agent` to one directory. Default: follow the caller's cwd. |
| `CCP_CURSOR_CLI_TIMEOUT_SECS` | Kill a run after this long. Default `900`. |
| `CCP_CURSOR_CLI_DEFAULT_MODEL` | Model for bare `cursor-cli`. Default `auto`. |

The same settings can live in `config.json` under `auth`, `gemini`, and
`cursorCli`.

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`. Either one overrides
the Claude subscription login and the Claude route returns 401.

The proxy listens on `127.0.0.1:18765` by default. Change it with
`PORT=11435 claude-codex serve`, and match `ANTHROPIC_BASE_URL`.

Alias remapping is optional. For example,
`ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` makes `/model sonnet` use Codex.

## Only what you can actually use is listed

Every backend is probed before its models are offered, so `/model` and the
unknown-model error show what this machine can serve rather than everything the
binary knows how to serve. The probe is cheap and local — a credential file, a
binary on `PATH`, a socket that accepts a connection — and runs only when models
are listed, never on the request path.

| backend | offered when |
| --- | --- |
| `anthropic` | always — Claude Code forwards its own subscription credentials |
| `codex` | the Codex CLI has written `~/.codex/auth.json` |
| `cursor` | signed in through this proxy, **or** `cursor-agent` is signed in |
| `cursor-cli` | the `cursor-agent` binary is on `PATH` |
| `gemini` | something is listening at `gemini.baseUrl` |
| `kimi`, `grok` | signed in through this proxy |

Routing stays permissive: an id that is hidden still routes if you ask for it by
name, so signing into a backend takes effect without restarting the proxy. The
unknown-model error names what is missing and how to get it, rather than letting
a signed-out backend look like one that was never built.

`CCP_SHOW_ALL_MODELS=1` lists everything regardless — useful for seeing what
exists before signing in.

### Cursor without a second login

The `cursor` backend prefers its own `claude-codex cursor login`, and otherwise
borrows the access token `cursor-agent` is already holding (macOS Keychain,
service `cursor-access-token`). Borrowed, never copied: the CLI refreshes that
token on its own schedule, so reading it fresh each time keeps the two in step,
and `cursor-agent logout` takes this backend with it.

That means one `cursor-agent login` lights up **both** Cursor backends — the API
one (`composer-2.5`, `cursor-agent`, …) and the CLI one (`cursor-cli`).

## Subagents on a router model

A subagent runs on whatever its definition names, and Claude Code passes that id
through to the router untouched — so any model the router serves can back one.
Put the id in the frontmatter of a `.claude/agents/*.md` file:

```md
---
name: gemini-reviewer
description: Second-opinion reviewer. Use for reviewing a diff.
model: gemini-3-pro
tools: Read, Grep, Bash
---

Review the diff and report only what is wrong.
```

Claude Code names the subagent in its own telemetry (`agent:custom:gemini-reviewer`
alongside `model: gemini-3-pro`), which is the quickest way to confirm a run went
where you meant. A `gemini-3-pro` subagent drives Claude Code's tools normally —
measured at two tool calls for a read-a-file-and-report task.

The `model:` field takes a full id, not just `sonnet`/`opus`/`haiku`/`inherit`.
The `Agent` tool's own `model` override is a fixed enum and cannot reach these
ids, so an ad-hoc *"spawn a gemini subagent"* with no definition file falls back
to a Claude model — the definition file is what makes it stick.

**`cursor-cli` does not work as a subagent.** It routes — Claude Code reports
`agent:custom:<name>` against `cursor-cli-ask`, and `cursor-agent --mode ask`
processes do spawn — but no run has ever handed a result back. Three attempts,
two agent shapes (one with `tools: Bash`, one with `tools: []`): each looped
through repeated `cursor-agent` invocations for 8+ minutes and had to be stopped.

The cause is structural, not a routing bug. `cursor-cli` never returns a
`tool_use` block — given a request carrying a `Bash` tool it ignored the tool,
ran the command with its own shell, and replied in prose. A subagent on it can
neither use the tool list it was handed nor be held to the parent's permission
mode, so the parent's agent loop has nothing to drive and never terminates.

Use `cursor-cli` as a model you select for a turn and delegate the whole task
to. Not as a subagent.

## Other backends

The same proxy can also route to **Kimi**, **Grok**, and **Cursor** models, each
with its own login. Run `claude-codex models` to see every id, and
`claude-codex <backend> auth status` to check a login. These backends keep
the behavior of the upstream project this is based on.

This fork adds two more, plus an optional lock on the proxy itself. Both new
backends keep the project's premise: **no API key**. Each one runs on a login
you already have — `cursor-agent login` for one, a signed-in Google account for
the other — the same way the Claude and Codex routes run on your subscriptions.

### Gemini (`gemini-web-api`)

Routes to [gemini-web-api], which exposes the `gemini.google.com` web app as an
OpenAI-compatible API using your logged-in Google account. No API key, no
billing. Chat only — image and video generation are out of scope here.

```sh
# 1. start the gemini server (needs Chrome signed in to gemini.google.com)
uvx --from git+https://github.com/FarisHijazi/gemini-web-api gemini-web-api

# 2. check the proxy can see it
claude-codex gemini auth status
```

Then `/model gemini-3-pro` inside Claude Code. Ids: `gemini-3-pro`,
`gemini-3-flash`, `gemini-3-flash-thinking`, each also in `-plus` and
`-advanced` tiers.

There is no separate login: the Google session lives in the gemini-web-api
server, so `auth status` reports whether that server is reachable.

That server sends no usage in its stream, so token counts for gemini turns are
estimated with the same estimator `/v1/messages/count_tokens` uses. A backend
that does report usage keeps its exact numbers.

Because the base URL is configurable, this backend also works against any other
OpenAI-compatible server (Ollama, LM Studio, OpenRouter):

```sh
CCP_GEMINI_BASE_URL=http://localhost:11434/v1 claude-codex serve
```

### Cursor CLI (`cursor-cli`)

Drives the **`cursor-agent` binary you already have signed in**, rather than
Cursor's API. Nothing to log into: run `cursor-agent login` once and the proxy
reuses that session.

```sh
claude-codex cursor-cli auth status
```

That command also lists the models you can use.

The model id carries the execution mode:

| Model id | Mode | Can edit files |
| --- | --- | --- |
| `cursor-cli` | ask, default model | no |
| `cursor-cli:<model>` | ask — Q&A | no |
| `cursor-cli-plan:<model>` | plan — analysis only | no |
| `cursor-cli-agent:<model>` | full agent | **yes**, and it is off by default |

`<model>` is any id `cursor-agent --list-models` prints, e.g.
`/model cursor-cli:composer-2.5`. Only the bare ids are advertised to Claude
Code — Cursor offers a few hundred models and listing every combination would
bury the `/model` picker — but any of them routes.

**It follows the project you have open.** An Anthropic request has no working
directory field, but Claude Code states its cwd in the environment block it
prepends to the conversation, and the proxy reads it back out — so one proxy
serves every project. Set `cursorCli.workspace` to pin every run to one
directory instead.

Note what that pin is: `--workspace` chooses where the agent *starts*, not what
it may touch. An agent given an absolute path in the prompt will read it even
from a pinned instance. What actually contains a run is the mode — `ask` and
`plan` cannot write — and the file permissions of the user the proxy runs as.

The mode holds against a shell redirect, not just the edit tools. An ask-mode
run told to execute `echo SIDE-EFFECT > created_by_shell.txt` answered *"Ask
mode only allows read-only actions"* and left the directory byte-identical. It
does run read-only commands itself, so treat `ask` as "may read anything the
proxy user can read", not as "runs nothing".

**This backend is an agent, not a model.** `cursor-agent` runs its own tool loop
in its own workspace, so Claude Code's tools are not advertised and no
`tool_use` block is returned. One Claude Code turn delegates the whole turn to a
Cursor agent and gets its final answer back; the tools it ran along the way show
up as progress lines.

Write access needs **both** the `cursor-cli-agent:` prefix and an explicit
opt-in, and the id stays out of `/model` until you give it:

```sh
CCP_CURSOR_CLI_ALLOW_WRITE=1 CCP_CURSOR_CLI_WORKSPACE=~/code/project claude-codex serve
```

### Locking the proxy (optional)

The proxy has no inbound auth by default, which is fine on loopback. Set a token
before binding it anywhere else:

```sh
CCP_AUTH_TOKEN=$(openssl rand -hex 32) CCP_BIND_ADDRESS=0.0.0.0 claude-codex serve
```

Claude Code then sends it as a header, alongside its own Claude login:

```json
{
  "env": { "ANTHROPIC_BASE_URL": "http://host:18765" },
  "headers": { "x-claude-codex-key": "<token>" }
}
```

`Authorization: Bearer <token>` is accepted too, for OpenAI SDK clients hitting
`/v1/chat/completions`. It is not *required* there because that header already
carries Claude Code's subscription token on the Claude route — putting the proxy
token in it would break the Claude passthrough. `/healthz` stays open.

[gemini-web-api]: https://github.com/FarisHijazi/gemini-web-api

## Limitations

- Switching plans in the middle of an active tool call (for example pressing Esc
  during a tool use, then switching and continuing) can fail, because the next
  model cannot verify reasoning that came from the other plan. Starting the next
  step fresh avoids it.

## Credits

Built on [`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy),
which provides the Codex, Kimi, Grok, and Cursor backends. This fork adds using
your Claude subscription as a backend alongside Codex, reasoning that survives a
mid-conversation switch, and reading the Codex login from the Codex CLI.
