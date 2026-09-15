# 2026-09-15 — Gemini backend, cursor-agent CLI backend, optional inbound auth

Fork of `fcakyon/claude-code-with-codex` at `FarisHijazi/claude-code-with-codex`.
Upstream base: `2c34184` (v0.3.1).

## The governing constraint

**No paid API keys.** Every backend must run on a login that already exists and
works — the Claude Code subscription, the Codex CLI login, the Cursor CLI
session, a signed-in Google account in Chrome. A backend that needs a
per-token-billed key is not interesting here, and that is what decides most of
the design choices below.

After this change every backend satisfies it:

| Backend | Session it reuses | Key needed |
| --- | --- | --- |
| `anthropic` | Claude Code's own subscription login | no |
| `codex` | Codex CLI `~/.codex/auth.json` | no |
| `cursor` | Cursor API login | no |
| `cursor-cli` | `cursor-agent login` | no |
| `gemini` | Google session inside gemini-web-api | no |
| `kimi`, `grok` | their own device logins | no |

## Context

The proxy exposes the Anthropic Messages API and routes per model name to a
backend (`anthropic` passthrough, `codex`, `kimi`, `grok`, `cursor`). Each
backend is a `Provider` (`src/provider.rs`) registered in `src/registry.rs`.

Three additions, all additive — no existing backend is refactored, because a
server instance is in production use on `:18765`.

## 1. `gemini` backend — gemini-web-api

`FarisHijazi/gemini-web-api` already serves an **OpenAI-compatible**
`/v1/chat/completions` with SSE streaming and emulated tool calling. So the
backend is a plain Anthropic ⇄ OpenAI-chat translation against a configurable
base URL — no new protocol work.

**LLM only.** `/v1/images/generations` and `/v1/videos/generations` are
deliberately not wired: the ask was the LLM part only, and the Anthropic
Messages surface has nowhere to put them.

**Why not reuse the kimi translator.** Kimi's `translate/` is 2,865 lines and
coupled to kimi specifics (`is_k3`, `KIMI_DEFAULT_MODEL`, thinking signatures).
Extracting a shared module would refactor a working, in-use backend for no
functional gain. Gemini needs a strict subset, so it gets a focused translator
built on the genuinely shared primitives in `providers/translate_shared.rs`
(`ContentBlock`, `normalize_content`, `flatten_system_text`,
`image_source_to_url`, `parallel_tool_calls`).

Because the base URL is configurable, this backend also works against any
OpenAI-compatible server (Ollama, LM Studio, OpenRouter). That is a side effect
of the shape, not a framework.

## 2. `cursor-cli` backend — the cursor-agent CLI

Distinct from the existing `cursor` backend, which speaks Cursor's private
connectrpc API (`providers/cursor/{proto,connect}.rs`) and has its own login.
This one drives the **locally installed, already-authenticated `cursor-agent`
binary**, so it reuses the Cursor CLI session with no separate credentials.

Verified `stream-json` event shape against the installed CLI:

| cursor-agent event | Anthropic SSE |
|---|---|
| `system/init` | `message_start` (carries `session_id`, resolved model) |
| `thinking/delta`, `thinking/completed` | `thinking` block + `thinking_delta` |
| `assistant` | `text_delta` — see the de-duplication note below |
| `tool_call/started`,`/completed` | surfaced as progress text, **not** `tool_use` |
| `result` | `message_delta` (`stop_reason`, real usage) + `message_stop` |

**Semantics — agent-as-model, not tool-bridge.** `cursor-agent` runs its own
tool loop inside its workspace; there is no way to make it emit a tool call and
pause for an external executor. So Claude Code's `tools` are not advertised and
no `tool_use` block is ever emitted: one Claude Code turn delegates the whole
turn to a Cursor agent, and what comes back is its final answer. Tool activity
is surfaced as visible progress so the turn is not opaque.

**Safety — read-only by default.** Because that agent can edit files, the
default mode is `ask` (read-only). Write access requires *both* an explicit
`cursor-cli-agent:` model prefix and `cursorCli.allowWrite` in config. The
`--trust` flag is required for headless runs and is only passed in that case.

Model ids are discovered from `cursor-agent --list-models` and cached; they
drift, so nothing is hardcoded beyond a fallback list.

## 3. Optional inbound auth

The proxy had **no inbound auth at all** — every `/v1/*` route was open on
whatever address it binds. Fine on loopback, not fine once `bindAddress` is
non-loopback.

**Disabled by default**, so an existing deployment is unaffected.

**Why a dedicated `x-claude-codex-key` header and not `Authorization`.** The
anthropic passthrough forwards the client's `Authorization` verbatim to
`api.anthropic.com` — it carries Claude Code's subscription OAuth token.
Requiring a proxy token there would force `ANTHROPIC_AUTH_TOKEN`, which
upstream documents as breaking the Claude route with a 401. So:

- `x-claude-codex-key: <token>` — the Claude Code path.
- `Authorization: Bearer <token>` — *also accepted* for plain OpenAI SDK
  clients on `/v1/chat/completions`.
- Match rule: accept if **any** presented credential matches; reject only if
  none do. A non-matching `Authorization` is therefore not an error, which is
  what lets Claude Code send its Claude token and its proxy key at once.

Constant-time comparison. `/healthz` stays unauthenticated. A warning is logged
when binding to a non-loopback address with auth disabled.


## What end-to-end testing changed

Unit tests passed on all three features before any of this surfaced. Each of
these was found only by running the real binary against the real CLI.

### `--trust` is required in every mode, not just write mode

The first design passed `--trust` only for `cursor-cli-agent:`, reasoning that a
read-only run needs no trust. Headless `cursor-agent` then refused to run at all:

```
⚠ Workspace Trust Required … Pass --trust, --yolo, or -f
```

`--trust` answers the directory-trust prompt, which has no TTY to answer it
headless. It is not the write grant — `--force` is. So `--trust` now goes on
every mode and `--force` appears in exactly one arm, which is what the safety
test asserts.

Verified: asked an `ask`-mode agent to overwrite a file and create another. It
answered *"I'm in Ask mode, so I can't edit or create files"* and the workspace
was byte-identical afterwards.

### `timestamp_ms` does not distinguish a delta from the settled message

The probe showed timestamped per-token `assistant` events followed by an
untimestamped settled one, so the translator keyed on that field. A real
multi-step run then emitted the settled message **with** a timestamp, and the
reply doubled:

```
I'll read `note.txt` and report the number.I'll read `note.txt` and report the number.
```

The rule is now content-based and uses no undocumented field. The translator
tracks `segment`, the message being built; an `assistant` event equal to the
segment is the settled repeat and contributes nothing, one that extends it
contributes the tail, anything else is a fresh delta. `segment` resets at each
thinking or tool_call boundary, so two identical messages in one run are still
both kept.

An earlier attempt subtracted deltas by substring search, which silently dropped
any delta already seen — a lone `" "`, `"the"`, a newline. Regression tests
cover all three failure modes.

`result` became a pure fallback for the same reason: it repeats the final
answer, so it is emitted only when nothing else produced text.

### Two advertising bugs

- `print_models` had a hardcoded provider list, so neither new backend appeared
  in the startup banner. It is now derived from the registry.
- Advertising `cursor-cli:<model>` for every discovered model produced **446**
  ids and would have buried Claude Code's `/model` picker. Now three bare ids
  are advertised and any `cursor-cli:<model>` still routes, matching what the
  API-backed `cursor` backend does. This also removed a subprocess call from
  registry construction, so server startup no longer shells out.

## Verified end to end

Against a dev server on `:18999`/`:18997`/`:18996`/`:18995` — never the
instance in use on `:18765`.

- gemini streaming and non-streaming, against a mock OpenAI server: correct
  Anthropic event order, text, and usage passthrough (42/7).
- gemini tool call: OpenAI `tool_calls` fragments reassembled into one
  `tool_use` block with parsed input and `stop_reason: tool_use`; the upstream
  request body confirmed Anthropic `input_schema` became an OpenAI
  `function.parameters`.
- cursor-cli streaming and non-streaming against the real CLI, including a
  multi-step run where the agent used its own tools.
- cursor-cli write gate refused with an actionable message.
- All eight inbound-auth cases, including a Claude subscription token in
  `Authorization` alongside the proxy key.


## Prior art: musistudio/claude-code-router

Checked after the fact, and it is a fair challenge to the gemini backend.

CCR is a local gateway that routes Claude Code to arbitrary providers. A plain
OpenAI-compatible endpoint needs no transformer, because OpenAI shape is its
default. So the gemini use case really is about six lines of its config:

```json
{
  "name": "gemini-web",
  "api_base_url": "http://localhost:8100/v1/chat/completions",
  "api_key": "not-needed",
  "models": ["gemini-3-pro", "gemini-3-flash"]
}
```

**So the `gemini` backend duplicates something that already exists.** What it
buys is staying inside one proxy: this repo already holds the Claude-subscription
passthrough and the Codex-CLI login, and CCR would be a second gateway in front
of or behind it rather than a replacement.

What CCR does **not** do:

- **Drive a local CLI agent as a backend.** It treats Claude Code, Codex CLI,
  Cursor CLI and friends as *clients* pointed at it. `cursor-agent` is not
  mentioned as a backend at all. The `cursor-cli` backend here has no
  equivalent, and it is the piece that turns an existing `cursor-agent login`
  into a model slot without an API key.
- **Confirmed Claude + ChatGPT subscription reuse.** Its provider model is
  `api_key`-shaped; this fork's whole premise is subscription logins.

Its Gemini support is the *official* Gemini protocol, which wants a Google AI
Studio key — the thing the constraint above rules out. Reaching Gemini without
a key still means gemini-web-api either way; the only question is which proxy
fronts it.

`accumulate_response` was moved from the gemini module to `src/anthropic/` as
part of this review: it folds an Anthropic SSE stream into a Messages JSON and
is not gemini-specific. `cursor-cli` was reaching across into the gemini module
for it, which was wrong regardless, and it also means the gemini backend can be
removed cleanly if that is the call.

## Follow-up: cursor-cli now follows the caller

The first cut left `cursor-agent` in a fixed directory, which made it close to
useless for coding — the caller's files were never there. The fix needed one
fact I did not have: what Claude Code actually puts in its prompt.

Rather than guess, I captured it. A throwaway HTTP server that dumps the request
body and returns a minimal Messages reply, then a one-shot `claude -p` pointed at
it. The 191 KB body settled two things:

1. The environment block is **not** in the `system` field. It arrives as a
   message with `role: "system"`, and the line reads
   `- Primary working directory: /abs/path`.
2. `ANTHROPIC_BASE_URL=... claude` **does not work** when
   `~/.claude/settings.json` sets it — the first attempt went to the configured
   router on `:18765` and never reached the dump server. The settings file wins
   over the process environment. `claude --settings '{"env":{...}}'` does
   override, which is how a second router gets exercised without editing a file
   every other running session reads.

`src/providers/cursor_cli/workspace.rs` scans the conversation newest-first for
that marker (and the older `<env>`/`Working directory:` phrasing), requiring a
whole-line match on an absolute path that exists — documentation quoting the
marker, this devlog included, must not be mistaken for the real thing. A
configured `cursorCli.workspace` still takes precedence.

### `--workspace` is not a sandbox

Worth stating plainly, because the first version of the module doc claimed
otherwise. A second instance was started on a spare port with
`CCP_CURSOR_CLI_WORKSPACE` pinned to an unrelated directory and sent the same
request. The agent started in the pinned directory — precedence works — and then
read the absolute path out of the prompt anyway:

```
> read .../ws-probe/secret_marker.txt
> read .../pinned-elsewhere/secret_marker.txt
ZUCCHINI-4471
```

So the pin chooses where a run *starts*, nothing more. Containment is the mode
(`ask` and `plan` cannot write, proven earlier in this log) and the file
permissions of the user the proxy runs as. The doc comment was corrected to say
so.

End-to-end, against the live fork on `:18766`: a request whose env block named a
scratch project got back `ZUCCHINI-4471`, read from a file that exists only
there. Six unit tests cover the marker forms, the freshest-block rule, and the
two rejections (relative paths, prose).

## Connected, end to end

Real `claude -p` sessions against the fork on `:18766`, not curl:

| model | via | result |
| --- | --- | --- |
| `gemini-3-flash` | Google web session, no API key | `HANDOVER-OK` |
| `cursor-cli-ask` | `cursor-agent login` | `CURSOR-OK` |
| `gpt-5.6-sol` | ChatGPT subscription | `PONG` |

Each also probed streaming and non-streaming directly; all six emit the full
Anthropic event vocabulary and a non-zero usage block.

Claude Code prints an `unrecognized_model` warning for any id outside its own
catalog and then assumes a 200k window, which caps auto-compact below what the
model actually takes. A guessed `modelPicker`/`behavesAs` row did **not** settle
it. Two things that did, both measured: the `[1m]` suffix on the model name, and
`CLAUDE_CODE_MAX_CONTEXT_TOKENS`. The suffix never reaches a backend —
`normalize_incoming_model` already strips it, so `gemini-3-flash[1m]` routes to
gemini unchanged. Both are in the README now.

## Upstream fixes sent to gemini-web-api

https://github.com/FarisHijazi/gemini-web-api/pull/2 — neither bug is reachable
on Linux, which is presumably why they survived:

- `CHROME_DIR` was hardcoded to `~/.config/google-chrome`, so cookie discovery
  globbed a path that does not exist on macOS and reported "No Gemini
  credentials" — on a machine whose five Chrome profiles all had a valid
  `__Secure-1PSID`.
- `gemini-webapi` was `>=2.0.0` with `2.0.0` pinned only in `uv.lock`. `uvx`
  ignores the lockfile, so the README's one-liner resolved 2.1.1, which renamed
  the `Model.*_THINKING` tiers `config.py` maps. Measured with `uv pip compile`:
  `>=2.0.0` resolves 2.1.1, `>=2.0.0,<2.1` resolves 2.0.0.

The local checkout stays on that branch until the PR merges, because the server
on `:8100` needs the fix on disk to restart.

## Subagents on a router model

Asked whether these backends can back a Claude Code subagent. They can, and
nothing had to be built for it: Claude Code sends a subagent's request with
whatever `model:` its definition names, so the router routes it like any other.

Measured, in a scratch project with `.claude/agents/*.md`:

| agent | `model:` | result |
| --- | --- | --- |
| `gemini-probe` | `gemini-3-flash` | returned its token; 3 requests landed on gemini-web-api during the run |
| `gemini-tool-probe` | `gemini-3-pro` | ran a real Claude Code tool loop (2 tool calls), returned the file contents |
| `cursor-probe` | `cursor-cli-ask` | routed, never handed back — see below |

The proof that routing is real, rather than a silent fallback to a Claude model,
is Claude Code's own telemetry line: `{"model":"gemini-3-flash",
"query_source":"agent:custom:gemini-probe"}`, plus the request count moving on
the gemini-web-api side.

Two limits worth stating:

- `model:` in an agent file takes a full id. The `Agent` tool's own `model`
  override is a fixed enum (`sonnet`/`opus`/`haiku`/`fable`), so *"spawn a
  gemini subagent"* with no definition file cannot reach these ids — it quietly
  gets a Claude model. The definition file is the mechanism.
- The ids are the Gemini **3** family. `gemini-2.5-pro` is not one of them and
  returns a clean 400 listing what is; gemini-web-api itself advertises nine
  ids, none of them 2.5.

### Why `cursor-cli` makes a bad subagent

Not a routing failure — a shape mismatch. Sent a request carrying a `Bash` tool
definition, `cursor-cli-ask` ignored it, ran the command with its own shell, and
answered in text:

```
stop_reason: end_turn
block types: ['thinking', 'text', 'thinking', 'text']
TEXT: Running the command now.
TEXT: `HELLO`
```

No `tool_use` block, so a subagent on it cannot drive the tool list it was given
and cannot be held to the parent's permission mode. It is a model you delegate a
whole task to, not a subagent worker. Documented in the README as such.

### Ask mode holds against a shell redirect

Worth recording because "read-only" could mean two different things. An ask-mode
run *did* execute `echo HELLO` and report the output — so it runs commands. Told
to run `echo SIDE-EFFECT > created_by_shell.txt` it refused:

> I can't run that command — **Ask mode** only allows read-only actions

and the directory was byte-identical afterwards. So `ask` is "may read anything
the proxy user can read", not "runs nothing", and the write block is enforced on
shell redirects, not only on the edit tools.

Three cursor-cli subagent runs were attempted across two agent shapes — one with
`tools: Bash`, one with `tools: []` to test a pure-delegation framing. All three
behaved identically: Claude Code reported `agent:custom:<name>` against
`cursor-cli-ask`, `cursor-agent --mode ask --trust -p ... stream-json` processes
were observed running, and the parent looped through repeated invocations for
8+ minutes without a handback. The first self-reported *"the failure looks like
the cursor-cli backend not producing a handback"*; the other two were stopped to
stop burning Cursor quota.

The direct path is unaffected — `claude -p --model cursor-cli-ask` returns
normally — so this is the subagent loop specifically, and it follows from the
missing `tool_use`: the parent has no tool calls to drive and no terminating
condition it recognises. Not worth working around; the backend is a delegate,
not a worker.
