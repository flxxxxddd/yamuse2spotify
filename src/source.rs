//! pulling a whole yandex library into one [`Library`] on disk.
//!
//! every later phase reads that file, not the api. it makes matching and
//! pushing reproducible, keeps a rerun off the yandex servers entirely, and
//! means a change in the private api can only ever break this one module.

use std::collections::HashSet;

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
            for short in &liked.tracks {
                if let Some(id) = short_id(short) {
                    library.liked_track_ids.push(id.clone());
                    collect(short, id, &mut wanted, &mut seen, &mut inlined);
                }
            }
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

        library.artists = artists
            .unwrap_or_default()
            .iter()
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

    for like in likes {
        match like.album.as_ref() {
            Some(album) => out.extend(SourceAlbum::try_from(album).ok()),
            None => wanted.extend(like.id.as_ref().and_then(yamuse::models::Id::as_number)),
        }
    }

    if wanted.is_empty() {
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
    Ok(out)
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
    fn a_reference_without_an_id_is_dropped_rather_than_queued_as_empty() {
        assert!(short_id(&short(r#"{"timestamp":"x"}"#)).is_none());
        assert_eq!(
            short_id(&short(r#"{"id":"42:1001"}"#)).as_deref(),
            Some("42")
        );
    }
}
