# CLAUDE.md

guidance for working in this repository.

## what this is

a cli that migrates a yandex music library into spotify and downloads whatever
spotify does not carry. rust 2024, edition 2024, rust-version 1.85.

## build, test, lint

```bash
cargo build
cargo test                    # ~100 unit tests, no network, no tokens
cargo clippy --all-targets    # must be clean: pedantic is on, unsafe forbidden
cargo fmt
```

there is one live test, ignored by default. it talks to the real spotify api
and writes to a real account, undoing everything it does:

```bash
cargo test -- --ignored --nocapture live_round_trip
```

run it after any change to `src/spotify/` — the unit tests cannot catch an
endpoint that moved, and every endpoint here was found by probing.

**`vergen` is pinned to 9.0.6 in `Cargo.lock`.** librespot-core 0.8.0's build
script does not compile against 9.1.0 (two incompatible `vergen-lib` versions
land in the graph). if a lockfile refresh breaks the build with a
`vergen_lib::entries::Add` trait error, re-pin with
`cargo update -p vergen --precise 9.0.6`.

## the one thing to understand first

**this does not use the spotify web api, and must not go back to it.**

spotify meters its public api per *application*. the client id here is
spotify's own desktop id (`config::CLIENT_ID`), shared with every
librespot-based tool in existence, and its web-api quota is permanently spent:
`/v1/me` returns 429 on the first request of a run, with `Retry-After` values
that do not decay while you wait. this was measured, not assumed — 200 seconds
of complete silence, same 429, and a freshly minted token behaves identically.

so `src/spotify/mod.rs` speaks the *internal* api, the one spotify's own
clients use, reached through a librespot `Session` which supplies both the
access token and the client token. there is no observable ceiling there: thirty
unpaced requests return in ten seconds.

consequences that are easy to get wrong:

- **`accept: application/json` is load-bearing.** without it these endpoints
  answer `200` with an empty body, which reads as "accepted, did nothing" and
  is indistinguishable from success. hours were lost to this.
- **`content-type` must be plain `application/json`**, no `;charset=UTF-8`. the
  collection endpoints reject the parameterised form with a bare 400.
- **error bodies are empty.** a 400 tells you nothing. when stuck, compare
  against a real client's traffic rather than guessing shapes.
- **no field-qualifier grammar.** `track:"Closer" artist:"…"` is matched as
  literal text and returns tracks named "Track 10". queries must be free text;
  see `matcher::queries`.

## the endpoint map

all json, all through `Spotify::get` / `Spotify::post`.

| operation | endpoint |
|---|---|
| account name | `GET /user-profile-view/v3/profile/{user}` |
| search | `GET /context-resolve/v1/spotify:search:{query}` → bare uris |
| track metadata | `librespot_metadata::Track::get` |
| like track / save album | `POST /collection/v2/write`, `set=collection` |
| follow artist | same, `set=artist` — `collection` refuses artists with a 400 |
| already saved? | `POST /collection/v2/contains?market=from_token` → `{"found":[…]}` |
| own playlists | `GET /playlist/v2/user/{user}/rootlist` |
| playlist contents | `GET /playlist/v2/playlist/{id}` |
| create playlist | `POST /playlist/v2/playlist`, then add to rootlist |
| add tracks | `POST /playlist/v2/playlist/{id}/changes` |

two shapes worth remembering:

- collection bodies are `{username, set, items:[{uri, addedAt}]}`; removal is
  `{uri, isRemoved: true}`.
- the rootlist puts names and owners in a **parallel `metaItems` array**,
  aligned by position with `contents.items`. the item itself carries only its
  uri and a timestamp.
- creating a playlist takes two requests: `POST /playlist/v2/playlist` makes it,
  and a rootlist `ADD` is what makes it visible in any client.

search returns uris only, so each hit costs one metadata lookup to become
scoreable. that is affordable solely because of the absent rate limit.

## layout

| file | what lives there |
|---|---|
| `src/main.rs` | phases, wiring, the `App` struct |
| `src/cli.rs` | clap definitions |
| `src/config.rs` | the client id, the redirect uri, `Config`, `Paths` |
| `src/auth/spotify.rs` | pkce oauth, token cache, librespot session |
| `src/auth/yandex.rs` | device flow |
| `src/spotify/mod.rs` | the whole spotify client: pacing, retries, batching |
| `src/matcher/` | query cascade, normalisation, scoring |
| `src/pipeline.rs` | what each phase does, in order |
| `src/state.rs` | the resume journal, written atomically |
| `src/source.rs`, `src/download/` | the yandex side |

`pipeline.rs` calls twelve methods on `Spotify`. keep those signatures stable
when changing the client; that boundary is what kept the rspotify → librespot
rewrite from touching the rest of the program.

## conventions

- comments explain *why*, not what. if a constant or a branch exists because of
  something measured or something the api does, say so — most of the non-obvious
  code here is non-obvious for a reason that is not visible from the code.
- test names are sentences describing the property being protected, e.g.
  `a_short_contains_reply_never_shifts_which_track_is_marked_saved`.
- user-facing strings are russian; code, comments and docs are english.
- doc comments on everything public: `missing_docs` warns.
- `unsafe_code` is forbidden.

## ordering matters

spotify will not backdate "date added". the push order is the only thing
deciding how the library sorts afterwards, so likes go oldest-first, from each
item's yandex timestamp. do not parallelise the push or reorder it for speed.

## state and resumption

`state.rs` holds the journal; every phase records what it did so a rerun skips
it. the search cache in `matcher` is keyed by query string, so rescoring after
a change to `normalize` or the thresholds costs nothing. both are written
atomically (temp file + `rename`) and flushed every 100 entries.

changing `matcher::queries` invalidates cached entries by changing their keys —
harmless, but the next run pays for the searches again.

## the licence footnote

using spotify's own client id this way is outside their developer terms. the
readme says so plainly, and that honesty should survive edits.
