//! everything that talks to spotify, with the rate limit and the batch sizes
//! handled once instead of at every call site.
//!
//! rspotify does not retry, does not pace, and does not chunk id lists to the
//! per-endpoint maxima. a full library migration is thousands of requests and
//! tens of thousands of ids, so all three have to live somewhere; they live
//! here, behind methods that take whole `Vec`s and hand back plain results.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::http::HttpError;
use rspotify::model::{
    AlbumId, ArtistId, FullTrack, Id, LibraryId, Market, PlayableId, PlaylistId, SearchResult,
    SearchType, TrackId, UserId,
};
use rspotify::{AuthCodePkceSpotify, ClientError};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::matcher::score::Candidate;

/// how many uris go into one `PUT /me/library`.
///
/// spotify's february 2026 consolidation replaced the per-type save endpoints
/// with one that takes uris in the query string, so the real limit is url
/// length rather than a documented count. a spotify uri is 36 bytes, and 50 of
/// them plus separators stays well inside every proxy's ceiling.
const LIBRARY_ADD_BATCH: usize = 50;
/// `POST /playlists/{id}/tracks` accepts 100 uris per call.
const PLAYLIST_ADD_BATCH: usize = 100;
/// the largest page the paginated endpoints will serve.
const PAGE_SIZE: u32 = 50;

/// no explicit market.
///
/// spotify's own documentation is clear that with a user access token "the
/// country associated with the user account will take priority over this
/// parameter", so sending one is redundant — and the legacy `from_token` value
/// is one more thing that can be rejected. omitting it is the documented path
/// to exactly the availability filtering this needs.
const MARKET: Option<Market> = None;

/// how many times a request is retried before the caller is asked what to do.
const MAX_ATTEMPTS: u32 = 4;
/// backoff for a server-side failure that carries no `Retry-After`.
const BACKOFF: Duration = Duration::from_millis(700);
/// the longest a `Retry-After` is honoured before giving up on the request.
///
/// spotify occasionally answers a rate limit with a delay measured in hours.
/// waiting that out inside a progress bar is indistinguishable from a hang.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// parse a slice of bare ids into rspotify's typed ids.
///
/// a macro rather than a generic function because `from_id` is an inherent
/// method each id type generates, not something a trait exposes.
macro_rules! parse_ids {
    ($ty:ident, $ids:expr) => {
        $ids.iter()
            .map(|id| $ty::from_id(id.clone()).map_err(Error::from))
            .collect::<Result<Vec<_>>>()
    };
}

// a spotify uri is 36 bytes plus a separator, checked against the 2 kb query
// string every proxy in the path is safe with. compile-time, because a batch
// size that overflows a url fails at runtime as an opaque 414.
const _: () = assert!(LIBRARY_ADD_BATCH * 37 < 2048);
const _: () = assert!(PLAYLIST_ADD_BATCH <= 100);

/// a spotify client that paces, retries and batches.
pub struct Spotify {
    client: AuthCodePkceSpotify,
    user_id: UserId<'static>,
    /// when the next request may go out. a plain mutex rather than a governor:
    /// the whole pipeline is one task deep, so contention never happens.
    next_slot: Mutex<Instant>,
    min_gap: Duration,
}

impl Spotify {
    /// wrap an authenticated client, resolving the current user once.
    ///
    /// `rps` is the self-imposed ceiling on requests per second. spotify does
    /// not document its limit and it moves; staying under it is cheaper than
    /// discovering it.
    pub async fn new(client: AuthCodePkceSpotify, rps: f64) -> Result<Self> {
        let user_id = client.me().await?.id;
        let min_gap = if rps > 0.0 {
            Duration::from_secs_f64(1.0 / rps)
        } else {
            Duration::ZERO
        };

        Ok(Self {
            client,
            user_id,
            next_slot: Mutex::new(Instant::now()),
            min_gap,
        })
    }

    /// the display name of the signed-in account, for the startup banner.
    pub async fn account_label(&self) -> Result<String> {
        let me = self.call("me", || self.client.me()).await?;
        Ok(me.display_name.unwrap_or_else(|| self.user_id.to_string()))
    }

    // ── searching ───────────────────────────────────────────────────────────

    /// run one track search and convert the hits into scoreable candidates.
    pub async fn search_tracks(&self, query: &str, limit: u32) -> Result<Vec<Candidate>> {
        let result = self
            .call("search tracks", || {
                self.client
                    .search(query, SearchType::Track, MARKET, None, Some(limit), None)
            })
            .await?;

        let SearchResult::Tracks(page) = result else {
            // the endpoint echoes back the type it was asked for; anything else
            // is a shape change upstream, and an empty hit list degrades better
            // than a panic partway through a library.
            tracing::warn!(query, "search answered with a non-track payload");
            return Ok(Vec::new());
        };

        Ok(page.items.iter().filter_map(to_candidate).collect())
    }

    /// find the best album id for `query`, if spotify has one.
    pub async fn search_album(&self, query: &str) -> Result<Option<(String, String, Vec<String>)>> {
        let result = self
            .call("search albums", || {
                self.client
                    .search(query, SearchType::Album, MARKET, None, Some(5), None)
            })
            .await?;

        let SearchResult::Albums(page) = result else {
            return Ok(None);
        };

        Ok(page.items.into_iter().find_map(|a| {
            let id = a.id?;
            Some((
                id.id().to_string(),
                a.name,
                a.artists.into_iter().map(|x| x.name).collect(),
            ))
        }))
    }

    /// find the best artist id for `query`, if spotify has one.
    pub async fn search_artist(&self, query: &str) -> Result<Option<(String, String)>> {
        let result = self
            .call("search artists", || {
                self.client
                    .search(query, SearchType::Artist, MARKET, None, Some(5), None)
            })
            .await?;

        let SearchResult::Artists(page) = result else {
            return Ok(None);
        };

        Ok(page
            .items
            .into_iter()
            .next()
            .map(|a| (a.id.id().to_string(), a.name)))
    }

    // ── library ─────────────────────────────────────────────────────────────

    /// add tracks to "liked songs", in batches the endpoint accepts.
    pub async fn save_tracks(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(LIBRARY_ADD_BATCH) {
            let items: Vec<LibraryId<'static>> = parse_ids!(TrackId, chunk)?
                .into_iter()
                .map(LibraryId::Track)
                .collect();

            self.call("save tracks", || {
                self.client.library_add(items.iter().cloned())
            })
            .await?;
        }
        Ok(())
    }

    /// the ids already in "liked songs".
    ///
    /// pulled up front so a rerun does not re-add thousands of tracks; spotify
    /// tolerates that, but it reorders the library by date added, which is a
    /// destructive surprise for anyone who sorts by it.
    pub async fn saved_track_ids(&self) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        let mut offset = 0;

        loop {
            let page = self
                .call("list saved tracks", || {
                    self.client.current_user_saved_tracks_manual(
                        MARKET,
                        Some(PAGE_SIZE),
                        Some(offset),
                    )
                })
                .await?;

            let received = u32::try_from(page.items.len()).unwrap_or(0);
            out.extend(
                page.items
                    .iter()
                    .filter_map(|s| s.track.id.as_ref().map(|id| id.id().to_string())),
            );

            if page.next.is_none() || received == 0 {
                break;
            }
            offset += received;
        }

        Ok(out)
    }

    /// add albums to the library, in batches.
    pub async fn save_albums(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(LIBRARY_ADD_BATCH) {
            let items: Vec<LibraryId<'static>> = parse_ids!(AlbumId, chunk)?
                .into_iter()
                .map(LibraryId::Album)
                .collect();

            self.call("save albums", || {
                self.client.library_add(items.iter().cloned())
            })
            .await?;
        }
        Ok(())
    }

    /// follow artists, in batches.
    pub async fn follow_artists(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(LIBRARY_ADD_BATCH) {
            let items: Vec<LibraryId<'static>> = parse_ids!(ArtistId, chunk)?
                .into_iter()
                .map(LibraryId::Artist)
                .collect();

            self.call("follow artists", || {
                self.client.library_add(items.iter().cloned())
            })
            .await?;
        }
        Ok(())
    }

    // ── playlists ───────────────────────────────────────────────────────────

    /// every playlist the account owns, as `name -> id`.
    ///
    /// playlists owned by someone else are skipped: they cannot be written to,
    /// and matching one by name would silently target a stranger's list.
    pub async fn own_playlists(&self) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        let mut offset = 0;

        loop {
            let page = self
                .call("list playlists", || {
                    self.client
                        .current_user_playlists_manual(Some(PAGE_SIZE), Some(offset))
                })
                .await?;

            let received = u32::try_from(page.items.len()).unwrap_or(0);
            for p in &page.items {
                if p.owner.id == self.user_id {
                    out.insert(p.name.clone(), p.id.id().to_string());
                }
            }

            if page.next.is_none() || received == 0 {
                break;
            }
            offset += received;
        }

        Ok(out)
    }

    /// create a playlist owned by the signed-in user.
    pub async fn create_playlist(&self, name: &str, description: Option<&str>) -> Result<String> {
        let playlist = self
            .call("create playlist", || {
                self.client.user_playlist_create(
                    self.user_id.as_ref(),
                    name,
                    // private by default: a migration should not publish
                    // somebody's listening history to their followers.
                    Some(false),
                    Some(false),
                    description,
                )
            })
            .await?;

        Ok(playlist.id.id().to_string())
    }

    /// the track ids already in a playlist, in order.
    pub async fn playlist_track_ids(&self, playlist_id: &str) -> Result<Vec<String>> {
        let id = PlaylistId::from_id(playlist_id.to_string())?;
        let mut out = Vec::new();
        let mut offset = 0;

        loop {
            let page = self
                .call("list playlist items", || {
                    self.client.playlist_items_manual(
                        id.as_ref(),
                        None,
                        MARKET,
                        Some(PAGE_SIZE),
                        Some(offset),
                    )
                })
                .await?;

            let received = u32::try_from(page.items.len()).unwrap_or(0);
            for item in &page.items {
                if let Some(rspotify::model::PlayableItem::Track(t)) = &item.item
                    && let Some(tid) = &t.id
                {
                    out.push(tid.id().to_string());
                }
            }

            if page.next.is_none() || received == 0 {
                break;
            }
            offset += received;
        }

        Ok(out)
    }

    /// append tracks to a playlist, in batches, preserving the given order.
    pub async fn playlist_add(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let playlist = PlaylistId::from_id(playlist_id.to_string())?;

        for chunk in track_ids.chunks(PLAYLIST_ADD_BATCH) {
            let parsed = parse_ids!(TrackId, chunk)?;
            self.call("add to playlist", || {
                let items: Vec<PlayableId<'_>> =
                    parsed.iter().cloned().map(PlayableId::Track).collect();
                self.client
                    .playlist_add_items(playlist.as_ref(), items, None)
            })
            .await?;
        }

        Ok(())
    }

    // ── plumbing ────────────────────────────────────────────────────────────

    /// run one request, pacing it and retrying what is worth retrying.
    ///
    /// `op` is a closure rather than a future because a retry needs a fresh one.
    async fn call<T, F, Fut>(&self, what: &str, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = rspotify::ClientResult<T>>,
    {
        let mut attempt = 0;

        loop {
            attempt += 1;
            self.pace().await;

            let error = match op().await {
                Ok(value) => return Ok(value),
                Err(e) => e,
            };

            let Some(delay) = retry_delay(&error, attempt) else {
                return Err(explain(error).await);
            };

            tracing::warn!(what, attempt, ?delay, %error, "retrying");
            tokio::time::sleep(delay).await;
        }
    }

    /// block until this request's turn in the self-imposed rate limit.
    async fn pace(&self) {
        if self.min_gap.is_zero() {
            return;
        }

        let mut slot = self.next_slot.lock().await;
        let now = Instant::now();
        let at = (*slot).max(now);
        *slot = at + self.min_gap;
        drop(slot);

        if at > now {
            tokio::time::sleep_until(at).await;
        }
    }
}

/// turn a final failure into an error that says what spotify actually objected to.
///
/// rspotify renders a rejected request as "status code 403" and stops there,
/// which is indistinguishable between a missing scope, an app the account is not
/// allowlisted on, and a parameter spotify no longer accepts. the body says
/// which, so it is read here — the response is consumed, which is why this only
/// runs once the retry loop has given up.
async fn explain(error: ClientError) -> Error {
    let ClientError::Http(boxed) = error else {
        return Error::Spotify(error);
    };

    let HttpError::StatusCode(response) = *boxed else {
        return Error::Spotify(ClientError::Http(boxed));
    };

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    Error::SpotifyStatus {
        status,
        message: message_from_body(&body),
    }
}

/// pull the human-readable reason out of an error body.
///
/// the documented shape is `{"error": {"status": …, "message": …}}`, but an edge
/// or a proxy in front of the api can answer with html or with nothing at all,
/// and "403 and no further information" is precisely the outcome this exists to
/// avoid — so the raw body is the fallback.
fn message_from_body(body: &str) -> String {
    if let Some(message) = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .filter(|m| !m.trim().is_empty())
    {
        return message;
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(пустой ответ)".to_owned();
    }

    // bounded, because an html error page is thousands of lines of nothing.
    trimmed.chars().take(300).collect()
}

/// how long to wait before retrying, or `None` when the error is final.
fn retry_delay(error: &ClientError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }

    match error {
        // a dropped connection or a timeout: worth another go.
        ClientError::Http(e) if matches!(e.as_ref(), HttpError::Client(_)) => {
            Some(BACKOFF * attempt)
        }
        ClientError::Http(e) => {
            let HttpError::StatusCode(response) = e.as_ref() else {
                return None;
            };
            let status = response.status();

            if status.as_u16() == 429 {
                // honour the header, but not past the point where waiting is
                // indistinguishable from a hung program.
                let after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(BACKOFF, Duration::from_secs);

                return (after <= MAX_RETRY_AFTER).then_some(after);
            }

            status.is_server_error().then(|| BACKOFF * attempt)
        }
        _ => None,
    }
}

/// convert a page of search hits into scoreable candidates.
///
/// tracks without an id are the user's own local files surfacing in search
/// results; nothing can be done with them, so they are dropped rather than
/// carried as a candidate that fails at push time.
fn to_candidate(track: &FullTrack) -> Option<Candidate> {
    let id = track.id.as_ref()?;

    Some(Candidate {
        id: id.id().to_string(),
        title: track.name.clone(),
        artists: track.artists.iter().map(|a| a.name.clone()).collect(),
        album: track.album.name.clone(),
        duration_ms: u32::try_from(track.duration.num_milliseconds()).unwrap_or(0),
        explicit: track.explicit,
        url: track
            .external_urls
            .get("spotify")
            .cloned()
            .unwrap_or_else(|| format!("https://open.spotify.com/track/{}", id.id())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_body_is_reduced_to_the_reason_spotify_gave() {
        assert_eq!(
            message_from_body(r#"{"error":{"status":403,"message":"Invalid market"}}"#),
            "Invalid market"
        );
    }

    #[test]
    fn a_body_that_is_not_the_documented_shape_still_says_something() {
        // the case this exists for: a 403 with nothing to go on is what sent
        // three debugging rounds chasing the wrong cause.
        assert_eq!(message_from_body(""), "(пустой ответ)");
        assert_eq!(message_from_body("   "), "(пустой ответ)");
        assert_eq!(
            message_from_body("<html>gateway</html>"),
            "<html>gateway</html>"
        );
        assert_eq!(
            message_from_body(r#"{"error":{"status":403}}"#),
            r#"{"error":{"status":403}}"#
        );
    }

    #[test]
    fn an_html_error_page_is_truncated_rather_than_dumped_into_the_terminal() {
        let huge = "x".repeat(10_000);
        assert_eq!(message_from_body(&huge).chars().count(), 300);
    }

    #[test]
    fn a_local_file_in_the_search_results_is_dropped_rather_than_carried() {
        let raw = serde_json::json!({
            "album": { "artists": [], "external_urls": {}, "images": [], "name": "x" },
            "artists": [],
            "disc_number": 1,
            "duration_ms": 1000,
            "explicit": false,
            "external_ids": {},
            "external_urls": {},
            "id": null,
            "is_local": true,
            "name": "local",
            "popularity": 0,
            "preview_url": null,
            "track_number": 1,
            "type": "track"
        });
        let track: FullTrack = serde_json::from_value(raw).unwrap();
        assert!(to_candidate(&track).is_none());
    }
}
