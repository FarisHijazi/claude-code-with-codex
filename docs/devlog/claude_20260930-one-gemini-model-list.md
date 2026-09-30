# One gemini model list, plus a routing fallback

Replaces the design in `claude_20260916-advertised-vs-accepted-models.md`.

That change hid the `-thinking` gemini ids by adding `Provider::advertised_models()`,
a second `advertised` map on `Registry`, `advertised_from()`, and a gemini
override — because deleting the ids from `GEMINI_MODELS` broke routing, which
needed an exact match.

The exact match was the real inconsistency: `gemini::models::resolve_model` and
`assert_allowed_model` already forward any `gemini-*` id "so a server running
ahead of this list still works", but routing could never reach them. Now
`provider_for_model` falls back to gemini for any `gemini-` id after the
exact-match pass (after, so an exact id owned by another backend still wins).

Result: the thinking ids are simply deleted from `GEMINI_MODELS`, and the trait
method, the map, its builder and the gemini override are gone. A model the
server adds routes with no proxy change.

Verified: 867 + integration tests pass; live through `:18766`, `/v1/models`
lists 6 gemini ids with no `thinking`, and both `gemini-3-flash` and the
unlisted `gemini-3-flash-thinking` answer.
