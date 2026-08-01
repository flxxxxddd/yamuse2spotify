//! everything that talks to spotify, over the api its own clients use.
//!
//! not the public web api: that one meters per application, and the client id
//! this tool authorises with is spotify's own, shared with every librespot-based
//! program in existence. its quota is permanently spent — the first request of
//! a run comes back 429 and waiting changes nothing.
//!
//! the internal api behind `spclient` has no such ceiling in practice (thirty
//! unpaced requests return in ten seconds), and it is reached through the
//! librespot session, which supplies the access token and the client token.
//! everything here speaks json; `accept: application/json` is not optional,
//! because without it the endpoints answer 200 with an empty body.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use librespot_core::{Session, spotify_uri::SpotifyUri};
use librespot_metadata::{Metadata, Track};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::matcher::score::Candidate;

/// how many uris go into one collection write.
///
/// the endpoint takes a list and documents no maximum; fifty keeps a failed
/// batch small enough to be worth retrying whole.
const COLLECTION_BATCH: usize = 50;
/// how many uris go into one playlist change.
const PLAYLIST_ADD_BATCH: usize = 100;
/// the largest page the rootlist will serve at once.
const PAGE_SIZE: usize = 50;

/// how many times a request is retried before the caller is asked what to do.
const MAX_ATTEMPTS: u32 = 4;
/// backoff for a server-side failure that carries no `Retry-After`.
const BACKOFF: Duration = Duration::from_millis(700);
/// how long to wait for a 429 that carries no `Retry-After` header.
const NO_RETRY_AFTER: Duration = Duration::from_secs(30);
/// the longest a wait is honoured before giving up on the request.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(900);
/// how much slower to go after spotify says the pace is too fast.
const THROTTLE_FACTOR: u32 = 2;
/// the slowest the adaptive throttle will go before giving up on pacing.
const MIN_RATE_GAP: Duration = Duration::from_secs(4);

/// how many search hits to score.
///
/// higher than the web api needed. the internal search resolves a *playback
/// context* rather than ranking a query, so the right track is reliably in the
/// list but not reliably at the top — the scorer sorts that out, provided it is
/// given enough to sort.
const SEARCH_WINDOW: usize = 20;

const _: () = assert!(PLAYLIST_ADD_BATCH <= 100);

/// a spotify client that paces, retries and batches.
pub struct Spotify {
    session: Session,
    user_id: String,
    /// how many metadata lookups may be in flight at once.
    jobs: usize,
    /// when the next request may go out, and how far apart they are spaced.
    pace: Mutex<Pace>,
    /// told how long a rate-limit wait will be, so it is visible rather than
    /// looking like a hung program.
    on_wait: Box<dyn Fn(Duration) + Send + Sync>,
}

/// the self-imposed rate limit, as it stands right now.
struct Pace {
    next_slot: Instant,
    gap: Duration,
}

impl Spotify {
    /// wrap a connected session.
    ///
    /// `rps` is the self-imposed ceiling on requests per second. the internal
    /// api does not appear to need one, but a migration is thousands of
    /// requests against somebody else's servers, so the knob stays.
    pub fn new(
        session: Session,
        rps: f64,
        jobs: usize,
        on_wait: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        // free, unlike the web api's `/v1/me`: the session already knows who
        // authenticated, so identifying the user costs no request at all.
        let user_id = session.username();

        let gap = if rps > 0.0 {
            Duration::from_secs_f64(1.0 / rps)
        } else {
            Duration::ZERO
        };

        Self {
            session,
            user_id,
            jobs: jobs.max(1),
            pace: Mutex::new(Pace {
                next_slot: Instant::now(),
                gap,
            }),
            on_wait: Box::new(on_wait),
        }
    }

    /// the display name of the signed-in account, for the startup banner.
    pub async fn account_label(&self) -> Result<String> {
        let profile = self
            .get(
                "профиль",
                &format!("/user-profile-view/v3/profile/{}", self.user_id),
            )
            .await?;

        Ok(profile["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.user_id)
            .to_owned())
    }

    // ── searching ───────────────────────────────────────────────────────────

    /// run one track search and convert the hits into scoreable candidates.
    ///
    /// search answers with bare uris, so each hit costs a metadata lookup to
    /// become scoreable — ten of them per query. done one after another that is
    /// where a match phase spends nearly all of its time: a lookup is about
    /// half a second of latency and almost no work, so the wire sits idle.
    /// running them together is worth roughly nine times the throughput,
    /// measured, and it is also what spotify's own web client does.
    pub async fn search_tracks(&self, query: &str, limit: u32) -> Result<Vec<Candidate>> {
        let uris = self.search_uris(query).await?;
        let wanted = (limit as usize).min(uris.len());

        // ordering is restored by score afterwards, so `buffer_unordered` costs
        // nothing here.
        let candidates: Vec<Option<Candidate>> =
            futures::stream::iter(uris.into_iter().take(wanted))
                .map(|uri| async move { self.hydrate(&uri).await })
                .buffer_unordered(self.jobs)
                .collect::<Vec<Result<Option<Candidate>>>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;

        Ok(candidates.into_iter().flatten().collect())
    }

    /// find the best album id for `query`, if spotify has one.
    ///
    /// derived from a track search rather than an album search: the internal
    /// search resolves playback contexts and only ever answers with tracks. an
    /// album is reached through the track that is on it, which costs one extra
    /// lookup and finds the same album the caller was describing.
    pub async fn search_album(&self, query: &str) -> Result<Option<(String, String, Vec<String>)>> {
        let uris = self.search_uris(query).await?;

        for uri in uris.into_iter().take(5) {
            let Some(track) = self.track(&uri).await? else {
                continue;
            };

            let Ok(id) = track.album.id.to_id() else {
                continue;
            };

            return Ok(Some((
                id,
                track.album.name.clone(),
                track.artists.0.iter().map(|a| a.name.clone()).collect(),
            )));
        }

        Ok(None)
    }

    /// find the best artist id for `query`, if spotify has one.
    ///
    /// reached the same way as an album, and for the same reason.
    pub async fn search_artist(&self, query: &str) -> Result<Option<(String, String)>> {
        let uris = self.search_uris(query).await?;

        for uri in uris.into_iter().take(5) {
            let Some(track) = self.track(&uri).await? else {
                continue;
            };
            let Some(artist) = track.artists.0.first() else {
                continue;
            };
            let Ok(id) = artist.id.to_id() else {
                continue;
            };

            return Ok(Some((id, artist.name.clone())));
        }

        Ok(None)
    }

    // ── library ─────────────────────────────────────────────────────────────

    /// add tracks to "liked songs", in batches the endpoint accepts.
    pub async fn save_tracks(&self, ids: &[String]) -> Result<()> {
        self.collection_add("track", "collection", ids).await
    }

    /// which of `ids` are already in "liked songs".
    ///
    /// asked in batches rather than by paging the whole library, so the cost
    /// scales with what is being migrated rather than with what the account
    /// already holds — a second run pays the same price as the first.
    ///
    /// worth asking at all because re-adding a saved track is not a no-op: it
    /// restamps the date added, which silently reorders the library for anyone
    /// who sorts by it.
    pub async fn already_saved(&self, ids: &[String]) -> Result<HashSet<String>> {
        let mut out = HashSet::new();

        for chunk in ids.chunks(COLLECTION_BATCH) {
            let body = json!({
                "username": self.user_id,
                "set": "collection",
                "items": chunk.iter().map(|id| json!({ "uri": uri_of("track", id) }))
                    .collect::<Vec<_>>(),
            });

            let answer = self
                .post(
                    "проверка сохранённого",
                    "/collection/v2/contains?market=from_token",
                    &body,
                )
                .await?;

            let found = answer["found"].as_array().cloned().unwrap_or_default();

            // the answer is positional; a short reply would silently shift the
            // mapping, so anything past its end is treated as "not saved" and
            // simply gets added again.
            out.extend(
                chunk
                    .iter()
                    .zip(found)
                    .filter(|(_, saved)| saved.as_bool().unwrap_or(false))
                    .map(|(id, _)| id.clone()),
            );
        }

        Ok(out)
    }

    /// take tracks back out of "liked songs".
    ///
    /// nothing in a migration removes anything, so this exists for the live
    /// test to undo what it does — which is the only way to check that a write
    /// landed without leaving it behind. test-only for exactly that reason: an
    /// unused deletion method on a migration tool is a trap, not a feature.
    #[cfg(test)]
    pub async fn remove_tracks(&self, ids: &[String]) -> Result<()> {
        self.collection_write("track", "collection", ids, true)
            .await
    }

    /// add albums to the library, in batches.
    pub async fn save_albums(&self, ids: &[String]) -> Result<()> {
        self.collection_add("album", "collection", ids).await
    }

    /// follow artists, in batches.
    ///
    /// artists live in their own set; sending them to `collection` alongside
    /// tracks and albums is refused outright.
    pub async fn follow_artists(&self, ids: &[String]) -> Result<()> {
        self.collection_add("artist", "artist", ids).await
    }

    // ── playlists ───────────────────────────────────────────────────────────

    /// every playlist the account owns, as `name -> id`.
    ///
    /// playlists owned by someone else are skipped: they cannot be written to,
    /// and matching one by name would silently target a stranger's list.
    pub async fn own_playlists(&self) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        let mut from = 0usize;

        loop {
            let page = self
                .get(
                    "список плейлистов",
                    &format!(
                        "/playlist/v2/user/{}/rootlist\
                         ?decorate=revision,length,attributes,timestamp,owner,capabilities\
                         &from={from}&length={PAGE_SIZE}",
                        self.user_id
                    ),
                )
                .await?;

            let items = page["contents"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let received = items.len();

            // the name and the owner are not on the item: they arrive in a
            // parallel `metaItems` array, aligned by position. the item itself
            // carries only its uri and when it was added.
            let meta = page["contents"]["metaItems"]
                .as_array()
                .cloned()
                .unwrap_or_default();

            for (index, item) in items.iter().enumerate() {
                let Some(uri) = item["uri"].as_str() else {
                    continue;
                };
                // folders and other people's lists both appear here; only an
                // owned playlist can be written to.
                if !uri.starts_with("spotify:playlist:") {
                    continue;
                }

                let Some(meta) = meta.get(index) else {
                    // no decoration for this row means no name to key on, and
                    // guessing one would risk writing into the wrong list.
                    continue;
                };
                let owner = meta["ownerUsername"].as_str().unwrap_or(&self.user_id);
                if owner != self.user_id {
                    continue;
                }

                if let (Some(name), Some(id)) =
                    (meta["attributes"]["name"].as_str(), spotify_id(uri))
                {
                    out.insert(name.to_owned(), id);
                }
            }

            if received < PAGE_SIZE {
                break;
            }
            from += received;
        }

        Ok(out)
    }

    /// create a playlist owned by the signed-in user.
    ///
    /// two requests, not one: creating a playlist and putting it in the user's
    /// library are separate acts here, and a playlist that is never added to
    /// the rootlist exists but is invisible in every client.
    pub async fn create_playlist(&self, name: &str, description: Option<&str>) -> Result<String> {
        let mut values = json!({ "name": name });
        if let Some(description) = description.filter(|d| !d.trim().is_empty()) {
            values["description"] = json!(description);
        }

        let made = self
            .post(
                "создание плейлиста",
                "/playlist/v2/playlist",
                &json!({
                    "ops": [{
                        "kind": "UPDATE_LIST_ATTRIBUTES",
                        "updateListAttributes": { "newAttributes": { "values": values } }
                    }]
                }),
            )
            .await?;

        let uri = made["uri"]
            .as_str()
            .ok_or_else(|| Error::Config("spotify не вернул uri созданного плейлиста".into()))?
            .to_owned();

        self.post(
            "плейлист в библиотеку",
            &format!("/playlist/v2/user/{}/rootlist/changes", self.user_id),
            &json!({
                "deltas": [{
                    "ops": [{
                        "kind": "ADD",
                        "add": {
                            "items": [{ "uri": uri, "attributes": { "timestamp": "0" } }],
                            "addFirst": true
                        }
                    }]
                }]
            }),
        )
        .await?;

        spotify_id(&uri)
            .ok_or_else(|| Error::Config(format!("spotify вернул непонятный uri: {uri}")))
    }

    /// drop a playlist from the library.
    ///
    /// spotify has no notion of deleting one — a playlist is "removed" by
    /// taking it out of the rootlist, after which no client shows it. nothing
    /// in a migration does this; it exists so the live test cleans up after
    /// itself instead of leaving debris in a real account, and is test-only so
    /// that it cannot be reached by accident.
    #[cfg(test)]
    pub async fn remove_playlist(&self, playlist_id: &str) -> Result<()> {
        self.post(
            "удаление плейлиста",
            &format!("/playlist/v2/user/{}/rootlist/changes", self.user_id),
            &json!({
                "deltas": [{
                    "ops": [{
                        "kind": "REM",
                        "rem": {
                            "items": [{ "uri": uri_of("playlist", playlist_id) }],
                            "itemsAsKey": true
                        }
                    }]
                }]
            }),
        )
        .await?;

        Ok(())
    }

    /// the track ids already in a playlist, in order.
    pub async fn playlist_track_ids(&self, playlist_id: &str) -> Result<Vec<String>> {
        let page = self
            .get(
                "содержимое плейлиста",
                &format!("/playlist/v2/playlist/{playlist_id}"),
            )
            .await?;

        Ok(page["contents"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i["uri"].as_str())
                    .filter(|u| u.starts_with("spotify:track:"))
                    .filter_map(spotify_id)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// append tracks to a playlist, in batches, preserving the given order.
    pub async fn playlist_add(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        for chunk in track_ids.chunks(PLAYLIST_ADD_BATCH) {
            let items: Vec<Value> = chunk
                .iter()
                .map(|id| json!({ "uri": uri_of("track", id) }))
                .collect();

            self.post(
                "добавление в плейлист",
                &format!("/playlist/v2/playlist/{playlist_id}/changes"),
                &json!({
                    "deltas": [{
                        "ops": [{ "kind": "ADD", "add": { "items": items, "addLast": true } }]
                    }]
                }),
            )
            .await?;
        }

        Ok(())
    }

    // ── plumbing ────────────────────────────────────────────────────────────

    /// add a batch of uris to one of the collection sets.
    async fn collection_add(&self, kind: &str, set: &str, ids: &[String]) -> Result<()> {
        self.collection_write(kind, set, ids, false).await
    }

    /// add or remove a batch of uris in one of the collection sets.
    async fn collection_write(
        &self,
        kind: &str,
        set: &str,
        ids: &[String],
        removing: bool,
    ) -> Result<()> {
        let added_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for chunk in ids.chunks(COLLECTION_BATCH) {
            let items: Vec<Value> = chunk
                .iter()
                .map(|id| {
                    let uri = uri_of(kind, id);
                    if removing {
                        json!({ "uri": uri, "isRemoved": true })
                    } else {
                        json!({ "uri": uri, "addedAt": added_at })
                    }
                })
                .collect();

            self.post(
                if removing {
                    "удаление из библиотеки"
                } else {
                    "сохранение в библиотеку"
                },
                "/collection/v2/write",
                &json!({ "username": self.user_id, "set": set, "items": items }),
            )
            .await?;
        }

        Ok(())
    }

    /// the uris a search answers with, best first.
    async fn search_uris(&self, query: &str) -> Result<Vec<String>> {
        // a title can hold anything a person can type — quotes, percent signs,
        // cyrillic, emoji — and all of it goes into a uri path here. encoding
        // the lot is the only way that does not end in `invalid uri character`
        // partway through somebody's library.
        let escaped = percent_encode(query.trim());
        if escaped.is_empty() {
            return Ok(Vec::new());
        }

        let Some(context) = self
            .get_optional(
                "поиск",
                &format!("/context-resolve/v1/spotify:search:{escaped}"),
            )
            .await?
        else {
            // the resolver answers 404 for a query it can make nothing of.
            // that is "spotify has nothing", not a failure worth stopping a
            // migration to ask about.
            return Ok(Vec::new());
        };

        Ok(context["pages"]
            .as_array()
            .and_then(|p| p.first())
            .and_then(|p| p["tracks"].as_array())
            .map(|tracks| {
                tracks
                    .iter()
                    .filter_map(|t| t["uri"].as_str().map(str::to_owned))
                    .take(SEARCH_WINDOW)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// full metadata for one track uri.
    async fn track(&self, uri: &str) -> Result<Option<Track>> {
        let Ok(parsed) = SpotifyUri::from_uri(uri) else {
            return Ok(None);
        };

        self.pace().await;
        match Track::get(&self.session, &parsed).await {
            Ok(track) => Ok(Some(track)),
            Err(e) => {
                // one unreadable hit is not worth aborting a library over; it
                // simply does not become a candidate.
                tracing::warn!(uri, %e, "не удалось получить метаданные трека");
                Ok(None)
            }
        }
    }

    /// turn a search hit into something the scorer can rank.
    async fn hydrate(&self, uri: &str) -> Result<Option<Candidate>> {
        let Some(track) = self.track(uri).await? else {
            return Ok(None);
        };
        let Some(id) = spotify_id(uri) else {
            return Ok(None);
        };

        Ok(Some(Candidate {
            title: track.name,
            artists: track.artists.0.iter().map(|a| a.name.clone()).collect(),
            album: track.album.name,
            duration_ms: u32::try_from(track.duration).unwrap_or(0),
            explicit: track.is_explicit,
            url: format!("https://open.spotify.com/track/{id}"),
            id,
        }))
    }

    /// one GET, paced and retried.
    async fn get(&self, what: &str, path: &str) -> Result<Value> {
        self.call(what, &Method::GET, path, None).await
    }

    /// one GET where a 404 means "there is none", not "something went wrong".
    async fn get_optional(&self, what: &str, path: &str) -> Result<Option<Value>> {
        match self.call(what, &Method::GET, path, None).await {
            Ok(value) => Ok(Some(value)),
            Err(Error::SpotifyStatus { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// one POST with a json body, paced and retried.
    async fn post(&self, what: &str, path: &str, body: &Value) -> Result<Value> {
        let encoded = serde_json::to_vec(body)?;
        self.call(what, &Method::POST, path, Some(&encoded)).await
    }

    /// run one request, pacing it and retrying what is worth retrying.
    async fn call(
        &self,
        what: &str,
        method: &Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Value> {
        let mut attempt = 0;

        loop {
            attempt += 1;
            self.pace().await;

            let error = match self
                .session
                .spclient()
                .request(method, path, Some(json_headers()), body)
                .await
            {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        // several endpoints answer 200 with nothing when they
                        // have nothing to say; that is success, not a shape to
                        // parse.
                        return Ok(Value::Null);
                    }
                    return serde_json::from_slice(&bytes).map_err(Error::from);
                }
                Err(e) => e,
            };

            let status = status_of(&error);

            let Some(delay) = retry_delay(status, attempt) else {
                return Err(explain(what, status, &error));
            };

            // a rate limit is not a transient blip: going back at the same pace
            // just spends the next window the same way. slow down and stay slow.
            if status == Some(429) {
                self.throttle(delay).await;
            }

            if delay >= Duration::from_secs(5) {
                (self.on_wait)(delay);
            }

            tracing::warn!(what, attempt, ?delay, %error, "повтор запроса");
            tokio::time::sleep(delay).await;
        }
    }

    /// block until this request's turn in the self-imposed rate limit.
    async fn pace(&self) {
        let mut pace = self.pace.lock().await;
        if pace.gap.is_zero() {
            return;
        }

        let now = Instant::now();
        let at = pace.next_slot.max(now);
        pace.next_slot = at + pace.gap;
        drop(pace);

        if at > now {
            tokio::time::sleep_until(at).await;
        }
    }

    /// halve the request rate after spotify said it was too fast, and hold every
    /// request back for the length of the wait.
    async fn throttle(&self, wait: Duration) {
        let mut pace = self.pace.lock().await;

        let widened = if pace.gap.is_zero() {
            Duration::from_millis(500)
        } else {
            pace.gap * THROTTLE_FACTOR
        };

        pace.gap = widened.min(MIN_RATE_GAP);
        pace.next_slot = pace.next_slot.max(Instant::now() + wait);

        tracing::warn!(gap = ?pace.gap, ?wait, "сбавляю темп после отказа");
    }
}

/// the headers every request carries.
///
/// the `accept` is load-bearing rather than polite: without it these endpoints
/// answer 200 with an empty body, which reads as "accepted, did nothing" and is
/// indistinguishable from success.
fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    // plain, with no `;charset=UTF-8`: the collection endpoints reject the
    // parameterised form with a bare 400, and the playlist ones accept both.
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers
}

/// percent-encode everything that is not unreserved.
///
/// hand-rolled rather than pulled from a crate: the inputs are query strings,
/// a client id, a loopback uri and base64url text, and four lines cover all of
/// them. non-ascii is encoded byte by byte, which is what utf-8 in a uri means.
pub(crate) fn percent_encode(value: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// `spotify:track:abc` → `abc`.
fn spotify_id(uri: &str) -> Option<String> {
    let id = uri.rsplit(':').next()?;
    (!id.is_empty() && id != uri).then(|| id.to_owned())
}

/// `abc` → `spotify:track:abc`, leaving a uri that already looks like one alone.
fn uri_of(kind: &str, id: &str) -> String {
    if id.starts_with("spotify:") {
        id.to_owned()
    } else {
        format!("spotify:{kind}:{id}")
    }
}

/// the http status behind a librespot error, when there is one.
fn status_of(error: &librespot_core::Error) -> Option<u16> {
    // librespot renders the status into its display text and does not expose it
    // structurally; the alternative is matching on an opaque error kind that
    // collapses every 4xx together.
    let text = error.to_string();
    let after = text.split("StatusCode(").nth(1)?;
    after
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// turn a final failure into an error that says what spotify objected to.
fn explain(what: &str, status: Option<u16>, error: &librespot_core::Error) -> Error {
    match status {
        Some(status) => Error::SpotifyStatus {
            status,
            message: format!("{what}: {error}").chars().take(300).collect(),
            retry_after: None,
        },
        None => Error::Spotify(format!("{what}: {error}")),
    }
}

/// how long to wait before retrying, or `None` when the error is final.
fn retry_delay(status: Option<u16>, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }

    match status {
        // the internal api does not send `Retry-After`, so a rate limit gets
        // the conservative default rather than a number it never supplied.
        Some(429) => (NO_RETRY_AFTER <= MAX_RETRY_AFTER).then_some(NO_RETRY_AFTER),
        Some(status) if (500..600).contains(&status) => Some(BACKOFF * attempt),
        // no status at all is a dropped connection or a timeout: worth another go.
        None => Some(BACKOFF * attempt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_contains_reply_never_shifts_which_track_is_marked_saved() {
        // the answer is positional, so zipping is what keeps id and flag
        // together — and a truncated reply must leave the tail unmarked rather
        // than mislabelling it.
        let ids = ["a".to_string(), "b".to_string(), "c".to_string()];
        let found = vec![json!(true), json!(false)];

        let saved: Vec<&String> = ids
            .iter()
            .zip(found)
            .filter(|(_, f)| f.as_bool().unwrap_or(false))
            .map(|(id, _)| id)
            .collect();

        assert_eq!(saved, vec!["a"]);
    }

    /// end-to-end against the real account, on demand.
    ///
    /// ignored by default: it needs a token cache and it writes to a live
    /// library. run it with
    /// `cargo test -- --ignored --nocapture live_round_trip`
    /// after an `auth`, and it will leave the account exactly as it found it.
    ///
    /// this exists because every endpoint here was found by probing rather than
    /// from documentation — nothing about them is guaranteed to stay put, and a
    /// unit test cannot notice when one moves.
    #[tokio::test]
    #[ignore = "talks to the live spotify api and writes to the account"]
    async fn live_round_trip() {
        use crate::config::Paths;

        let paths = Paths::new("./out");
        let session = crate::auth::spotify::connect(&paths.spotify_token)
            .await
            .expect("authorise first: `yamuse2spotify auth`");

        let spotify = Spotify::new(session, 0.0, 8, |_| {});

        // ── reads ───────────────────────────────────────────────────────────
        let label = spotify.account_label().await.unwrap();
        assert!(!label.is_empty());
        println!("account: {label}");

        let playlists = spotify.own_playlists().await.unwrap();
        println!("playlists: {}", playlists.len());

        let started = std::time::Instant::now();
        let hits = spotify
            .search_tracks("nirvana come as you are", 10)
            .await
            .unwrap();
        println!("search + 10 lookups: {:?}", started.elapsed());
        assert!(!hits.is_empty(), "search returned nothing");
        println!(
            "search: {} candidates, first {:?}",
            hits.len(),
            hits[0].title
        );
        // the hits have to be scoreable, not merely present.
        assert!(
            hits.iter()
                .all(|c| !c.title.is_empty() && c.duration_ms > 0)
        );

        // ── a write, and its undo ───────────────────────────────────────────
        let track = "18v5GSlymMWt3OZt0WBUlf".to_string();
        let ids = [track.clone()];

        let before = spotify.already_saved(&ids).await.unwrap();
        assert!(
            !before.contains(&track),
            "test track is already saved; unlike it first"
        );

        spotify.save_tracks(&ids).await.unwrap();
        let during = spotify.already_saved(&ids).await.unwrap();
        assert!(during.contains(&track), "save did not land");

        spotify.remove_tracks(&ids).await.unwrap();
        let after = spotify.already_saved(&ids).await.unwrap();
        assert!(!after.contains(&track), "undo did not land");

        println!("collection round trip: ok");

        // ── a playlist, created and filled ──────────────────────────────────
        let name = format!("yamuse2spotify smoke {}", std::process::id());
        let id = spotify
            .create_playlist(&name, Some("временный"))
            .await
            .unwrap();

        let listed = spotify.own_playlists().await.unwrap();
        assert_eq!(
            listed.get(&name),
            Some(&id),
            "a just-created playlist has to be findable by name"
        );

        spotify.playlist_add(&id, &ids).await.unwrap();
        assert_eq!(
            spotify.playlist_track_ids(&id).await.unwrap(),
            vec![track],
            "the track did not arrive in the playlist"
        );

        spotify.remove_playlist(&id).await.unwrap();
        assert!(
            !spotify.own_playlists().await.unwrap().contains_key(&name),
            "the smoke playlist outlived the test"
        );

        println!("playlist round trip: ok");
    }

    #[test]
    fn a_query_is_encoded_whole_rather_than_patched_up() {
        // the bug this replaces: spaces were turned into `%20` and *then* a
        // second pass put raw spaces back for `#?&`, while quotes, percent
        // signs and every non-ascii title went into the uri untouched. the
        // result was `invalid uri character` partway through a library.
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("rock & roll"), "rock%20%26%20roll");
        assert_eq!(percent_encode(r#"say "no""#), "say%20%22no%22");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("a#b?c"), "a%23b%3Fc");
        // unreserved characters are the only ones left alone.
        assert_eq!(percent_encode("azAZ09-._~"), "azAZ09-._~");
    }

    #[test]
    fn a_cyrillic_title_survives_the_trip_into_a_uri() {
        // utf-8, byte by byte — a raw cyrillic character in a uri path is what
        // the api rejected outright.
        let encoded = percent_encode("Кино");
        assert!(encoded.is_ascii(), "{encoded}");
        assert!(encoded.starts_with('%'));
        assert!(!encoded.contains('К'));
    }

    #[test]
    fn nothing_is_asked_of_the_api_for_an_empty_query() {
        // `spotify:search:` with nothing after it is a 404 waiting to happen.
        assert!(percent_encode("").is_empty());
        assert!(percent_encode("   ".trim()).is_empty());
    }

    #[test]
    fn an_id_is_recovered_from_a_uri_and_a_bare_id_is_left_alone() {
        assert_eq!(spotify_id("spotify:track:abc").as_deref(), Some("abc"));
        assert_eq!(spotify_id("spotify:playlist:xyz").as_deref(), Some("xyz"));
        assert!(spotify_id("abc").is_none());
    }

    #[test]
    fn a_bare_id_becomes_a_uri_and_a_uri_is_not_doubled() {
        assert_eq!(uri_of("track", "abc"), "spotify:track:abc");
        assert_eq!(uri_of("track", "spotify:track:abc"), "spotify:track:abc");
        assert_eq!(uri_of("artist", "xyz"), "spotify:artist:xyz");
    }

    #[test]
    fn artists_do_not_go_into_the_same_collection_set_as_tracks() {
        // sending an artist uri to `collection` is refused with a 400; the
        // separate set is not a stylistic choice.
        assert_ne!("collection", "artist");
    }

    #[test]
    fn a_rate_limit_waits_a_long_time_not_a_short_one() {
        // the internal api sends no `Retry-After`, so coming back in 700ms
        // after being told to slow down is how a limit becomes a permanent one.
        assert!(NO_RETRY_AFTER >= Duration::from_secs(10));
        assert!(NO_RETRY_AFTER > BACKOFF * MAX_ATTEMPTS);
        assert!(NO_RETRY_AFTER <= MAX_RETRY_AFTER);
    }

    #[test]
    fn the_throttle_cannot_widen_past_the_point_where_pacing_stops_helping() {
        let mut gap = Duration::from_secs_f64(1.0 / 3.0);
        for _ in 0..20 {
            gap = (gap * THROTTLE_FACTOR).min(MIN_RATE_GAP);
        }
        assert_eq!(gap, MIN_RATE_GAP);
    }

    #[test]
    fn a_dropped_connection_is_retried_but_a_rejection_is_not() {
        assert!(retry_delay(None, 1).is_some());
        assert!(retry_delay(Some(503), 1).is_some());
        assert!(retry_delay(Some(429), 1).is_some());
        // a 400 or a 404 will say the same thing however many times it is asked.
        assert!(retry_delay(Some(400), 1).is_none());
        assert!(retry_delay(Some(404), 1).is_none());
        // and nothing is retried forever.
        assert!(retry_delay(Some(503), MAX_ATTEMPTS).is_none());
    }

    #[test]
    fn the_search_window_is_wide_enough_for_a_context_resolver() {
        // the internal search ranks for playback, not for a query: in the
        // measured case the wanted track came back third behind two unrelated
        // ones. a narrow window would drop it before it could be scored.
        const { assert!(SEARCH_WINDOW >= 10) };
    }
}
