# yamuse2spotify

> русский: [README_RU.md](README_RU.md)

moves a **yandex music** library into **spotify**, and downloads whatever
spotify does not carry. rust, on top of [`yamuse`](https://crates.io/crates/yamuse)
and [`librespot`](https://github.com/librespot-org/librespot).

```
liked tracks ─┐                     ┌─→ liked songs
playlists     ├─→ search + scoring ─┤─→ playlists of the same name
albums        │                     ├─→ saved albums
artists      ─┘                     ├─→ followed artists
                                    └─→ music/  ← what spotify lacks
```

## what to know before starting

**you cannot upload audio to spotify through any api.** "local files" is a
desktop-client feature: it indexes a folder on disk, and no api offers an
upload. so unmatched tracks are downloaded into `out/music/` as
`artist/album/nn - title.flac`, tagged and with cover art, next to an
`unmatched.m3u8`. you point the desktop client at that folder once, under
"local files".

**yandex does not return an isrc.** there is nothing to match on but title,
artist and duration — which is why doubtful matches go to a human instead of
into your library. that is the ceiling of the available data, not a shortcut.

**flac needs an active yandex plus subscription.** without one the decryption
key is refused and downloads fall back to mp3 320.

## install

rust 1.85+ (edition 2024). everything comes from crates.io:

```bash
git clone https://github.com/flxxxxddd/yamuse2spotify
cd ./yamuse2spotify && cargo build --release
```

### no spotify app to register

there is nothing to set up at developer.spotify.com, and no client id to paste
in. this is not a convenience — a dashboard application genuinely cannot do the
job. see [about rate limits](#about-rate-limits) below; briefly, the public web
api meters per application and the quota is gone before a run starts, so the
client speaks the internal api instead, and that one is a first-party surface
which accepts one specific client id.

**stated plainly:** that client id belongs to spotify, not to this project, so
using it this way is outside their developer terms. if you need a strictly
dashboard-sanctioned path, this tool is not it.

## usage

```bash
# once: authorise both services
yamuse2spotify auth

# the whole migration
yamuse2spotify run
```

yandex uses the device flow: a code appears in the terminal, you enter it at
`ya.ru/device`, and the token lands in `~/.config/yamuse2spotify/config.json`
(mode `0600`). spotify opens a browser once and renews itself after that — its
token lives in `out/spotify-token.json` alongside the rest of the run's state,
because it expires hourly.

### one phase at a time

every phase runs alone and re-runs safely — progress lives in the journal, so
nothing is done twice.

```bash
yamuse2spotify pull        # dump the yandex library to out/library.json
yamuse2spotify match       # find each track's counterpart in spotify
yamuse2spotify push        # upload what matched
yamuse2spotify download    # fetch what did not
yamuse2spotify report      # rebuild the reports from the journal
```

### flags worth knowing

| flag | what it does |
|---|---|
| `--out DIR` | where everything goes (default `./out`) |
| `--non-interactive` | never ask; anything doubtful goes to the report |
| `--ambiguous accept-best` | take the top candidate without asking |
| `--auto-threshold 0.92` | stricter auto-accept (default `0.88`) |
| `--review-threshold 0.5` | wider manual-review band (default `0.60`) |
| `--download all\|unmatched\|none` | what to fetch locally |
| `--format flac-then-mp3\|flac\|mp3` | at what quality |
| `--jobs 3` | parallel downloads |
| `--rps 3` | self-imposed request ceiling; `0` disables it |
| `--no-playlists`, `--no-albums`, `--no-artists`, `--no-tracks` | narrow the scope |

## the order things are added in

spotify will not backdate a "date added" — it is always now. so the order of
the upload is the only thing that decides how the library sorts afterwards.

| what | order | where it comes from |
|---|---|---|
| liked tracks | oldest like first | sorted by yandex's `timestamp` |
| liked albums | oldest like first | sorted by the like's `timestamp` |
| artists | presumably oldest first | yandex sends no `timestamp`; the list is simply reversed |
| playlist tracks | as in the source playlist | the playlist's own order |

so after the migration, sorting by "date added" in spotify reproduces your
yandex chronology instead of inverting it.

## how matching works

a cascade of queries, narrowest first, stopping at the first convincing hit so
that no request is spent needlessly:

1. `lead artist + title`
2. every credited artist + title — for splits, features and compilations

all free text: the api this client talks to has no field-qualifier grammar. it
reads `track:"Closer"` as literal words and answers with tracks actually
*named* "Track 10", so the qualified form is not merely unhelpful here, it is
wrong.

each candidate is scored 0…1:

| component | weight | note |
|---|---:|---|
| title | 0.45 | jaro–winkler after stripping cosmetics and `feat.` |
| artist | 0.35 | noise below 0.75 is zeroed; the lead artist weighs 0.7 |
| duration | 0.15 | ±2 s is perfect, ≥10 s is nothing |
| album | 0.05 | tiebreaker only |

penalties: −0.35 for a karaoke, tribute or nightcore version, −0.02 for an
explicit-flag mismatch.

then:

- **≥ 0.88** — accepted automatically
- **0.60 … 0.88** — collected and reviewed in one batch at the end of the run
- **< 0.60** — treated as not found

reviewing does not interrupt the run: the questions come as a single block once
all the searching is done.

## when things go wrong

the automatic handling comes first, and quietly:

| situation | what happens |
|---|---|
| refused for pace | waits 30 s, says so, and halves the rate |
| 5xx, dropped connection, timeout | up to 4 attempts with a growing pause |
| expired token | renewed from the refresh token, no browser |

only once all of that is exhausted does a question appear: **retry / skip /
skip all like this / retry all like this / stop and keep the progress**. with
`--non-interactive` the question becomes "skip" plus a line in `errors.csv`.

### about rate limits

this used to be the whole problem and now barely is — but the reason is not
obvious, so it is worth spelling out.

spotify's quota is counted **per application**, not per user. the built-in
client id is shared with every librespot-based tool in existence, and its
bucket on the public web api is drained around the clock: `/v1/me` answers
`429` on the first request of a run, and waiting does not help — measured at
200 seconds of complete silence, same answer.

so the client does not use the public api at all. it speaks the internal one,
the same api spotify's own clients use, where no such ceiling shows up: thirty
unpaced requests come back in ten seconds. the pacing (`--rps`, default 3) and
the slow-down-after-refusal logic are kept as a courtesy, not a necessity;
`--rps 0` turns them off.

if a run is interrupted anyway, nothing is lost: the journal and the search
cache are on disk, so the same command picks up where it stopped.

ctrl-c and "stop" are equally safe — the journal is written atomically
(temp file + `rename`) and flushed every 100 entries and on the way out.

## what you end up with

```
out/
├── library.json          the dumped yandex library
├── search-cache.json     cached spotify answers — a rerun is nearly free
├── state.json            the journal: matched, pushed, downloaded
├── spotify-token.json    the spotify token and the means to renew it
├── run.log               the log, kept out of the terminal so the bars survive
├── music/                downloads + unmatched.m3u8
└── reports/
    ├── matched.csv       what matched what, with links and scores
    ├── unmatched.csv     what spotify does not have
    ├── downloaded.csv    what was fetched
    ├── errors.csv        what failed, and why
    └── summary.md        the overview
```

## development

```bash
cargo test                       # ~100 unit tests, no network
cargo clippy --all-targets       # clean under pedantic
cargo fmt
```

the unit tests need neither network nor tokens: they cover string
normalisation, scoring, link parsing, path sanitising, journal atomicity and
state-format compatibility.

there is also one live test, ignored by default, that talks to the real api and
writes to a real account — it likes a track, checks it, unlikes it, creates a
playlist, fills it, and removes it again:

```bash
cargo test -- --ignored --nocapture live_round_trip
```

it exists because every endpoint this client uses was found by probing rather
than from documentation. nothing guarantees they stay put, and no unit test can
notice when one moves.
