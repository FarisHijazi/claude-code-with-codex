# 2026-09-16 — a model can be reachable without being offered

Follow-on from the gemini-webapi 2.1 work. That upgrade removed the `-thinking`
tier upstream, so `gemini-web-api` now serves `gemini-3-flash-thinking` as plain
flash and stopped advertising it. This proxy still offered it, which meant
`/model` listed a model that was silently something else.

## The wrong fix, and the test that caught it

First attempt: delete the three `-thinking` ids from `GEMINI_MODELS`.

That broke **routing**, not just advertising. `Registry::models` is built from
`Provider::supported_models()` and is what `provider_for_model()` searches, so an
id removed from the list stops resolving to any backend — and an unlisted
`gemini-`shaped id has no prefix fallback in the registry (only the provider's
own `resolve_model` forwards unknown ids, which is reached *after* routing).
`tests/fork_backends.rs::gemini_and_cursor_cli_resolve_to_their_own_providers`
failed immediately. The change was reverted rather than half-landed.

## The real shape of the problem

One list was doing two jobs:

| Job | Read by | Wants |
|---|---|---|
| what the backend **accepts** | `provider_for_model` | everything reachable |
| what we **offer** | `/v1/models`, `models` banner, `/model` picker | only what we can honour |

These are the same set for every backend except one whose upstream dropped a
model we still want to accept.

## The split

`Provider` gains `advertised_models()`, defaulting to `supported_models()`, so
every other backend is unchanged:

```rust
fn advertised_models(&self) -> Vec<String> {
    self.supported_models()
}
```

`Registry` keeps `models` (accepted, drives routing) and gains `advertised`
(offered, drives listings), both derived from the handlers themselves. The three
listing entry points — `all_supported_models`, `grouped_models`,
`grouped_models_all` — now read the advertised map through
`advertised_models_for()`; `provider_for_model` still reads `models`.

The gemini provider overrides `advertised_models()` to filter `-thinking`.

### The invariant worth pinning

`registry::tests::everything_advertised_can_actually_be_routed` walks every
offered id of every backend and asserts it routes. That is the direction that
matters: offering something unroutable is a broken menu item, whereas accepting
something unoffered is just a hidden alias.

## Verified

- `cargo test --release`: 940 tests, and the one stale expectation
  (`gemini_models_are_advertised`, which asserted `-thinking` was listed) updated
  to assert the opposite plus the no-substitution rule.
- Live against `:18766` with the rebuilt binary:
  - offered: `gemini-3-flash`, `-flash-advanced`, `-flash-plus`, `gemini-3-pro`,
    `-pro-advanced`, `-pro-plus` — no `thinking`.
  - `gemini-3-flash-thinking` still answers: `THINKING-ROUTES-OK`, `stop=end_turn`.

## Two launcher bugs found on the way

1. **`claude-codex-fork` reported "failed to start" for a server that started
   fine.** Not the probe URL — `/healthz` exists and answers `{"ok":true}`, and a
   cold start reaches it in **1 second**. It was a double start: the port guard
   used `lsof`, which is not always on PATH, and a "command not found" exits
   non-zero, so the guard failed **open**. A second instance launched, could not
   bind, and died, while the first served happily. The guard now probes
   `/healthz` with curl — no external tool — and fires on both a live port and a
   stale pidfile.
2. **A remembered PID is worthless across a reboot.** The machine restarted at
   02:27; pid 1178, tracked all of the previous session as the `:18765` server,
   had become `AirPlayUIAgent`. The real server was pid 1184. `claude-codex-fork`
   already guards against this (it greps the command line of the pidfile's pid
   before trusting it) — worth keeping in any launcher that stores a pid.
