//! pulling a whole yandex library into one [`Library`] on disk.
//!
//! every later phase reads that file, not the api. it makes matching and
//! pushing reproducible, keeps a rerun off the yandex servers entirely, and
//! means a change in the private api can only ever break this one module.

use std::collections::{HashMap, HashSet};

use yamuse::Client;
use yamuse::models::like::Like;
use yamuse::models::track::TrackShort;

use crate::error::Result;
use crate::model::{Library, SourceAlbum, SourceArtist, SourcePlaylist, SourceTrack, canonical_id};
use crate::resilience::guarded;
use crate::ui::{Ui, advance};

/// how many ids go into one `POST /tracks`.
///
/// the endpoint takes them as repeated form fields, so the ceiling is the url
/// and body size rather than a documented count; 150 stays comfortably inside.
const HYDRATE_BATCH: usize = 150;

/// which parts of the library to pull.
#[derive(Debug, Clone, Copy)]
pub struct Scope {
    /// liked tracks.
    pub liked_tracks: bool,
    /// the user's own playlists, with their contents.
    pub playlists: bool,
    /// liked albums.
    pub albums: bool,
    /// liked artists.
    pub artists: bool,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            liked_tracks: true,
            playlists: true,
            albums: true,
            artists: true,
        }
    }
}

/// pull everything in `scope` and assemble it into a [`Library`].
pub async fn pull(client: &Client, scope: Scope, ui: &Ui) -> Result<Library> {
    let mut library = Library {
        pulled_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };

    // ids gathered from every source, hydrated in one pass at the end so a
    // track shared by the likes and four playlists costs one lookup, not five.
    let mut wanted: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut inlined: Vec<SourceTrack> = Vec::new();

    if scope.liked_tracks {
        let spinner = ui.spinner("любимые треки…");
        let liked = guarded(ui, "загрузка любимых треков", || {
            client.liked_tracks(None)
        })
        .await?;
        spinner.finish_and_clear();

        if let Some(liked) = liked {
            // paired with their timestamps so the order can be restored from
            // the data rather than assumed from however the api happened to
            // sort the list.
            let mut liked_at: Vec<(String, Option<String>)> = Vec::new();

            for short in &liked.tracks {
                if let Some(id) = short_id(short) {
                    liked_at.push((id.clone(), short.timestamp.clone()));
                    collect(short, id, &mut wanted, &mut seen, &mut inlined);
                }
            }

            library.liked_track_ids = oldest_first(liked_at);
            ui.note(&format!(
                "  любимых треков: {}",
                library.liked_track_ids.len()
            ));
        }
    }

    if scope.playlists {
        library.playlists =
            pull_playlists(client, ui, &mut wanted, &mut seen, &mut inlined).await?;
        ui.note(&format!("  плейлистов: {}", library.playlists.len()));
    }

    if scope.albums {
        let spinner = ui.spinner("любимые альбомы…");
        let likes = guarded(
            ui,
            "загрузка любимых альбомов",
            || client.liked_albums(None),
        )
        .await?;
        spinner.finish_and_clear();

        library.albums = collect_albums(client, ui, &likes.unwrap_or_default()).await?;
        ui.note(&format!("  любимых альбомов: {}", library.albums.len()));
    }

    if scope.artists {
        let spinner = ui.spinner("любимые исполнители…");
        let artists = guarded(
            ui,
            "загрузка любимых исполнителей",
            || client.liked_artists(None),
        )
        .await?;
        spinner.finish_and_clear();

        // `likes/artists` returns bare artist objects with no timestamp on
        // them, so unlike tracks and albums there is nothing to sort by. the
        // list is reversed on the same convention the other likes endpoints
        // follow — newest first — which is an inference, not a measurement.
        // following order is also far less visible than liked-songs order, so
        // being wrong here costs little.
        library.artists = artists
            .unwrap_or_default()
            .iter()
            .rev()
            .filter_map(|a| SourceArtist::try_from(a).ok())
            .collect();
        ui.note(&format!(
            "  любимых исполнителей: {}",
            library.artists.len()
        ));
    }

    library.tracks = hydrate(client, ui, &wanted, inlined).await?;
    Ok(library)
}

/// turn album likes into [`SourceAlbum`]s, fetching the ones sent as bare ids.
///
/// `likes/albums` answers with references — `{"id": 38695104, "timestamp": …}` —
/// not with album objects, so reading `like.album` alone yields an empty
/// library. the inlined form is still handled because other endpoints do send
/// it and there is no way to tell them apart from the type.
async fn collect_albums(client: &Client, ui: &Ui, likes: &[Like]) -> Result<Vec<SourceAlbum>> {
    let mut out = Vec::new();
    let mut wanted = Vec::new();
    // the album endpoint knows nothing about when it was liked, so the like's
    // own timestamp is carried across by id.
    let mut liked_at: HashMap<i64, String> = HashMap::new();

    for like in likes {
        let id = like.id.as_ref().and_then(yamuse::models::Id::as_number);
        if let (Some(id), Some(at)) = (id, like.timestamp.clone()) {
            liked_at.insert(id, at);
        }

        match like.album.as_ref() {
            Some(album) => out.extend(SourceAlbum::try_from(album).ok()),
            None => wanted.extend(id),
        }
    }

    if wanted.is_empty() {
        stamp_and_sort(&mut out, &liked_at);
        return Ok(out);
    }

    let bar = ui.bar(wanted.len(), "детали альбомов");

    for chunk in wanted.chunks(HYDRATE_BATCH) {
        let fetched = guarded(ui, "загрузка деталей альбомов", || client.albums(chunk)).await?;

        if let Some(albums) = fetched {
            out.extend(albums.iter().filter_map(|a| SourceAlbum::try_from(a).ok()));
        }

        advance(&bar, chunk.len());
    }

    bar.finish_and_clear();
    stamp_and_sort(&mut out, &liked_at);
    Ok(out)
}

/// attach each album's like timestamp and put the oldest like first.
fn stamp_and_sort(albums: &mut [SourceAlbum], liked_at: &HashMap<i64, String>) {
    for album in albums.iter_mut() {
        album.liked_at = liked_at.get(&album.id).cloned();
    }

    albums.sort_by(|a, b| compare_liked_at(a.liked_at.as_deref(), b.liked_at.as_deref()));
}

/// order two like timestamps, oldest first.
///
/// parsed rather than compared as text. a lexical comparison is only correct
/// while every timestamp shares one layout and one utc offset — true of what
/// yandex sends today, and exactly the kind of thing a private api changes
/// without saying. an entry with no timestamp, or one that will not parse,
/// sorts last instead of landing somewhere arbitrary in the middle.
fn compare_liked_at(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a.and_then(parse_timestamp), b.and_then(parse_timestamp)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// parse one of the timestamp shapes yandex uses for a like.
fn parse_timestamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&chrono::Utc));
    }

    // some endpoints send a naive local time with no offset at all.
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// order ids by when they were liked, oldest first.
///
/// spotify has no way to backdate a save: it stamps every addition with the
/// current time. the order things go in is therefore the only control there is
/// over how the library sorts afterwards, and pushing yandex's newest-first
/// list unchanged would invert a decade of listening history.
fn oldest_first(mut liked: Vec<(String, Option<String>)>) -> Vec<String> {
    // a stable sort, so entries without a usable timestamp keep the order the
    // api gave them instead of being shuffled against each other.
    liked.sort_by(|a, b| compare_liked_at(a.1.as_deref(), b.1.as_deref()));
    liked.into_iter().map(|(id, _)| id).collect()
}

/// pull the user's own playlists and their track references.
async fn pull_playlists(
    client: &Client,
    ui: &Ui,
    wanted: &mut Vec<String>,
    seen: &mut HashSet<String>,
    inlined: &mut Vec<SourceTrack>,
) -> Result<Vec<SourcePlaylist>> {
    let spinner = ui.spinner("список плейлистов…");
    let kinds = guarded(
        ui,
        "загрузка списка плейлистов",
        || client.user_playlist_kinds(None),
    )
    .await?;
    spinner.finish_and_clear();

    let Some(kinds) = kinds.filter(|k| !k.is_empty()) else {
        return Ok(Vec::new());
    };

    let bar = ui.bar(kinds.len(), "плейлисты");
    let mut out = Vec::new();

    // the batch endpoint answers with the full track list per playlist, so one
    // request covers several playlists at once.
    for chunk in kinds.chunks(20) {
        let playlists = guarded(ui, "загрузка плейлистов", || {
            client.user_playlists(chunk.iter().copied(), None)
        })
        .await?;

        for playlist in playlists.unwrap_or_default() {
            let Some(kind) = playlist.kind else { continue };
            let mut track_ids = Vec::new();

            for short in &playlist.tracks {
                if let Some(id) = short_id(short) {
                    track_ids.push(id.clone());
                    collect(short, id, wanted, seen, inlined);
                }
            }

            out.push(SourcePlaylist {
                kind,
                title: playlist.title.unwrap_or_else(|| format!("playlist {kind}")),
                description: playlist.description.filter(|d| !d.is_empty()),
                track_ids,
            });
        }

        advance(&bar, chunk.len());
    }

    bar.finish_and_clear();
    Ok(out)
}

/// fetch the full track objects for everything referenced, in batches.
async fn hydrate(
    client: &Client,
    ui: &Ui,
    wanted: &[String],
    inlined: Vec<SourceTrack>,
) -> Result<Vec<SourceTrack>> {
    let mut out = inlined;

    if wanted.is_empty() {
        return Ok(out);
    }

    let bar = ui.bar(wanted.len(), "детали треков");

    for chunk in wanted.chunks(HYDRATE_BATCH) {
        let fetched = guarded(ui, "загрузка деталей треков", || {
            client.tracks(chunk)
        })
        .await?;

        // a skipped batch loses those tracks rather than the whole run; they
        // land in the report as unresolved and the rest of the library survives.
        if let Some(tracks) = fetched {
            out.extend(
                tracks
                    .iter()
                    .map(SourceTrack::from)
                    .filter(|t| !t.id.is_empty()),
            );
        }

        advance(&bar, chunk.len());
    }

    bar.finish_and_clear();
    Ok(out)
}

/// the canonical id of a track reference, if it carries one.
fn short_id(short: &TrackShort) -> Option<String> {
    short
        .id
        .as_ref()
        .map(canonical_id)
        .filter(|id| !id.is_empty())
}

/// record a track reference for hydration, unless it is already accounted for.
fn collect(
    short: &TrackShort,
    id: String,
    wanted: &mut Vec<String>,
    seen: &mut HashSet<String>,
    inlined: &mut Vec<SourceTrack>,
) {
    if !seen.insert(id.clone()) {
        return;
    }

    // some endpoints inline the whole track; taking it saves a lookup.
    match &short.track {
        Some(track) => inlined.push(SourceTrack::from(track)),
        None => wanted.push(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(json: &str) -> TrackShort {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_track_referenced_twice_is_only_queued_for_hydration_once() {
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        let mut inlined = Vec::new();

        let reference = short(r#"{"id":42}"#);
        collect(
            &reference,
            "42".into(),
            &mut wanted,
            &mut seen,
            &mut inlined,
        );
        collect(
            &reference,
            "42".into(),
            &mut wanted,
            &mut seen,
            &mut inlined,
        );

        assert_eq!(wanted, vec!["42".to_string()]);
    }

    #[test]
    fn an_inlined_track_is_taken_as_is_instead_of_being_looked_up_again() {
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        let mut inlined = Vec::new();

        let reference = short(r#"{"id":42,"track":{"id":42,"title":"here"}}"#);
        collect(
            &reference,
            "42".into(),
            &mut wanted,
            &mut seen,
            &mut inlined,
        );

        assert!(wanted.is_empty());
        assert_eq!(inlined.len(), 1);
        assert_eq!(inlined[0].title, "here");
    }

    #[test]
    fn likes_are_ordered_oldest_first_regardless_of_how_the_api_sorted_them() {
        // spotify cannot backdate a save, so the order things are pushed in is
        // the only control over how the library sorts afterwards.
        let newest_first = vec![
            (
                "c".to_string(),
                Some("2024-03-01T00:00:00+00:00".to_string()),
            ),
            (
                "b".to_string(),
                Some("2022-01-01T00:00:00+00:00".to_string()),
            ),
            (
                "a".to_string(),
                Some("2019-07-04T00:00:00+00:00".to_string()),
            ),
        ];

        assert_eq!(oldest_first(newest_first), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_like_without_a_timestamp_goes_last_and_keeps_its_relative_place() {
        let mixed = vec![
            ("no-stamp-1".to_string(), None),
            (
                "dated".to_string(),
                Some("2020-01-01T00:00:00+00:00".to_string()),
            ),
            ("no-stamp-2".to_string(), None),
        ];

        assert_eq!(
            oldest_first(mixed),
            vec!["dated", "no-stamp-1", "no-stamp-2"]
        );
    }

    #[test]
    fn timestamps_are_compared_as_instants_not_as_text() {
        // 02:00+03:00 is 23:00 the previous day in utc, so the moscow stamp is
        // the earlier *instant* while sorting later as *text*. comparing the
        // strings would put these two the wrong way round.
        const MOSCOW: &str = "2024-01-01T02:00:00+03:00";
        const UTC: &str = "2024-01-01T00:30:00+00:00";

        assert_eq!(
            compare_liked_at(Some(MOSCOW), Some(UTC)),
            std::cmp::Ordering::Less
        );
        assert!(
            MOSCOW > UTC,
            "text order really is the opposite of the instant order"
        );
    }

    #[test]
    fn a_timestamp_that_will_not_parse_sorts_last_rather_than_arbitrarily() {
        let good = Some("2020-01-01T00:00:00+00:00");
        assert_eq!(
            compare_liked_at(good, Some("nonsense")),
            std::cmp::Ordering::Less
        );
        assert_eq!(compare_liked_at(None, good), std::cmp::Ordering::Greater);
        assert_eq!(compare_liked_at(None, None), std::cmp::Ordering::Equal);
    }

    #[test]
    fn a_naive_timestamp_without_an_offset_still_parses() {
        assert!(parse_timestamp("2023-06-01T12:00:00").is_some());
        assert!(parse_timestamp("2023-06-01T12:00:00+00:00").is_some());
        assert!(parse_timestamp("").is_none());
    }

    #[test]
    fn albums_take_their_order_from_the_like_that_referenced_them() {
        let mut albums = vec![
            SourceAlbum {
                id: 2,
                title: "newer".into(),
                ..Default::default()
            },
            SourceAlbum {
                id: 1,
                title: "older".into(),
                ..Default::default()
            },
        ];

        let mut liked_at = HashMap::new();
        liked_at.insert(1, "2019-01-01T00:00:00+00:00".to_string());
        liked_at.insert(2, "2024-01-01T00:00:00+00:00".to_string());

        stamp_and_sort(&mut albums, &liked_at);

        assert_eq!(albums[0].title, "older");
        assert_eq!(albums[1].title, "newer");
        assert_eq!(
            albums[0].liked_at.as_deref(),
            Some("2019-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn a_reference_without_an_id_is_dropped_rather_than_queued_as_empty() {
        assert!(short_id(&short(r#"{"timestamp":"x"}"#)).is_none());
        assert_eq!(
            short_id(&short(r#"{"id":"42:1001"}"#)).as_deref(),
            Some("42")
        );
    }
}
