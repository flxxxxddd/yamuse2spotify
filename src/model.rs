//! the normalized shape of a yandex library.
//!
//! everything downstream — matching, pushing, downloading — reads these types
//! rather than `yamuse::models`, so a change upstream lands in exactly one
//! conversion instead of across five modules.

use serde::{Deserialize, Serialize};
use yamuse::models::{Id, album::Album, artist::Artist, track::Track};

/// a whole yandex library, as pulled once and then reused by every later phase.
///
/// tracks live in a single deduplicated catalogue and playlists reference them
/// by id: a track that appears in the likes and in three playlists is matched
/// against spotify once, not four times.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Library {
    /// when the pull ran, as an rfc 3339 timestamp.
    pub pulled_at: String,
    /// every distinct track referenced anywhere in the library.
    pub tracks: Vec<SourceTrack>,
    /// ids of liked tracks, **oldest liked first**.
    ///
    /// yandex hands these back newest first; they are reordered at pull time so
    /// that pushing them in order reproduces the original chronology. spotify
    /// stamps every addition with the current time, so the order they go in is
    /// the only thing that decides how the library sorts afterwards.
    pub liked_track_ids: Vec<String>,
    /// the user's own playlists.
    pub playlists: Vec<SourcePlaylist>,
    /// liked albums.
    pub albums: Vec<SourceAlbum>,
    /// liked artists.
    pub artists: Vec<SourceArtist>,
}

impl Library {
    /// look a track up in the catalogue.
    pub fn track(&self, id: &str) -> Option<&SourceTrack> {
        self.tracks.iter().find(|t| t.id == id)
    }
}

/// one track, flattened out of [`yamuse::models::track::Track`].
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceTrack {
    /// yandex track id, as a string because the api sends both forms.
    pub id: String,
    /// album id the track was referenced from, needed for download links.
    pub album_id: Option<i64>,
    /// track title, without the version suffix.
    pub title: String,
    /// version suffix, for example `radio edit` or `feat. someone`.
    pub version: Option<String>,
    /// performing artists, in order.
    pub artists: Vec<String>,
    /// album title.
    pub album: Option<String>,
    /// album artist, which differs from the track artist on compilations.
    pub album_artist: Option<String>,
    /// release year.
    pub year: Option<i64>,
    /// genre, as yandex spells it.
    pub genre: Option<String>,
    /// position within the album.
    pub track_number: Option<i64>,
    /// disc number for multi-volume releases.
    pub disc_number: Option<i64>,
    /// duration in milliseconds, the strongest signal after the title.
    pub duration_ms: Option<i64>,
    /// whether yandex flags the track explicit.
    pub explicit: bool,
    /// cover uri template, `%%` standing in for the size.
    pub cover_uri: Option<String>,
    /// whether the track is playable on this account at all.
    pub available: bool,
}

impl SourceTrack {
    /// `"artist a, artist b — title"`, for logs and prompts.
    pub fn display(&self) -> String {
        let title = match &self.version {
            Some(v) => format!("{} ({v})", self.title),
            None => self.title.clone(),
        };
        format!("{} — {title}", self.artists.join(", "))
    }

    /// the title with its version folded back in, as spotify usually stores it.
    pub fn full_title(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {v}", self.title),
            None => self.title.clone(),
        }
    }

    /// duration rendered as `m:ss`, or `?:??` when yandex omitted it.
    pub fn duration_hms(&self) -> String {
        match self.duration_ms {
            Some(ms) if ms > 0 => {
                let secs = ms / 1000;
                format!("{}:{:02}", secs / 60, secs % 60)
            }
            _ => "?:??".into(),
        }
    }
}

impl From<&Track> for SourceTrack {
    fn from(t: &Track) -> Self {
        // `substituted` is what yandex actually plays when the original is gone
        // from the catalogue — matching the dead original would strand the track.
        let source = t.substituted.as_deref().unwrap_or(t);
        let album = source.album();

        Self {
            id: source.id.as_ref().map_or_else(String::new, canonical_id),
            album_id: album.and_then(|a| a.id),
            title: source.title.clone().unwrap_or_default(),
            version: source.version.clone().filter(|v| !v.is_empty()),
            artists: source
                .artists
                .iter()
                .filter_map(|a| a.name.clone())
                .filter(|n| !n.is_empty())
                .collect(),
            album: album.and_then(|a| a.title.clone()),
            album_artist: album.and_then(|a| a.artists.first().and_then(|x| x.name.clone())),
            year: album.and_then(|a| a.year),
            genre: album
                .and_then(|a| a.genre.clone())
                .or_else(|| source.meta_data.as_ref().and_then(|m| m.genre.clone())),
            track_number: source.meta_data.as_ref().and_then(|m| m.number),
            disc_number: source.meta_data.as_ref().and_then(|m| m.volume),
            duration_ms: source.duration_ms,
            explicit: source.explicit.unwrap_or(false)
                || source.content_warning.as_deref() == Some("explicit"),
            cover_uri: source
                .cover_uri
                .clone()
                .or_else(|| album.and_then(|a| a.cover_uri.clone())),
            // absent means available: a track that never carried the flag is the
            // common case, and treating it as unplayable would drop most of a
            // library.
            available: source.available.unwrap_or(true),
        }
    }
}

/// render a track id in the one form the catalogue keys on.
///
/// the api sends both `42` and the compound `"42:1001"` for the same track;
/// collapsing to the numeric part keeps the catalogue deduplicated and is also
/// the only form the download endpoints accept.
pub fn canonical_id(id: &yamuse::models::Id) -> String {
    id.as_number()
        .map_or_else(|| id.to_string(), |n| n.to_string())
}

/// one liked album.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceAlbum {
    /// yandex album id.
    pub id: i64,
    /// album title.
    pub title: String,
    /// album artists.
    pub artists: Vec<String>,
    /// release year.
    pub year: Option<i64>,
    /// when the album was liked, as yandex timestamps it.
    ///
    /// kept only to restore the order likes were made in — the album endpoint
    /// does not carry it, so it comes from the like that referenced the album.
    pub liked_at: Option<String>,
}

impl SourceAlbum {
    /// `"artist — title"`, for logs and prompts.
    pub fn display(&self) -> String {
        format!("{} — {}", self.artists.join(", "), self.title)
    }
}

impl TryFrom<&Album> for SourceAlbum {
    type Error = ();

    fn try_from(a: &Album) -> Result<Self, Self::Error> {
        let (Some(id), Some(title)) = (a.id, a.title.clone()) else {
            return Err(());
        };
        Ok(Self {
            id,
            title,
            artists: a.artists.iter().filter_map(|x| x.name.clone()).collect(),
            year: a.year,
            liked_at: None,
        })
    }
}

/// one liked artist.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceArtist {
    /// yandex artist id.
    pub id: i64,
    /// artist name.
    pub name: String,
}

impl TryFrom<&Artist> for SourceArtist {
    type Error = ();

    fn try_from(a: &Artist) -> Result<Self, Self::Error> {
        // `likes/artists` sends the id as a string where the rest of the api
        // sends a number, so it arrives as an `Id` and is normalised here.
        let (Some(id), Some(name)) = (a.id.as_ref().and_then(Id::as_number), a.name.clone()) else {
            return Err(());
        };
        Ok(Self { id, name })
    }
}

/// one of the user's own playlists.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourcePlaylist {
    /// playlist kind — its id within the owner's playlists.
    pub kind: i64,
    /// playlist title.
    pub title: String,
    /// playlist description, carried over to spotify.
    pub description: Option<String>,
    /// track ids in playlist order, referencing [`Library::tracks`].
    pub track_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_substituted_track_is_read_instead_of_the_dead_original() {
        let raw = serde_json::json!({
            "id": 1,
            "title": "gone",
            "available": false,
            "substituted": { "id": 2, "title": "here", "available": true }
        });
        let track: Track = serde_json::from_value(raw).unwrap();
        let source = SourceTrack::from(&track);

        assert_eq!(source.id, "2");
        assert_eq!(source.title, "here");
        assert!(source.available);
    }

    #[test]
    fn a_track_missing_the_available_flag_counts_as_available() {
        let track: Track = serde_json::from_str(r#"{"id":1,"title":"x"}"#).unwrap();
        assert!(SourceTrack::from(&track).available);
    }

    #[test]
    fn a_content_warning_marks_a_track_explicit_without_the_explicit_flag() {
        let track: Track =
            serde_json::from_str(r#"{"id":1,"title":"x","contentWarning":"explicit"}"#).unwrap();
        assert!(SourceTrack::from(&track).explicit);
    }

    #[test]
    fn the_version_suffix_shows_in_the_display_form_and_the_full_title() {
        let track = SourceTrack {
            title: "song".into(),
            version: Some("radio edit".into()),
            artists: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert_eq!(track.display(), "a, b — song (radio edit)");
        assert_eq!(track.full_title(), "song radio edit");
    }

    #[test]
    fn a_compound_id_collapses_to_the_same_key_as_the_bare_numeric_one() {
        let compound: Track = serde_json::from_str(r#"{"id":"42:1001","title":"x"}"#).unwrap();
        let bare: Track = serde_json::from_str(r#"{"id":42,"title":"x"}"#).unwrap();

        assert_eq!(SourceTrack::from(&compound).id, "42");
        assert_eq!(SourceTrack::from(&bare).id, "42");
    }

    #[test]
    fn a_missing_duration_renders_as_unknown_rather_than_zero() {
        assert_eq!(SourceTrack::default().duration_hms(), "?:??");
    }
}
