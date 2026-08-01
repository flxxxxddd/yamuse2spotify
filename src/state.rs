//! what a run has already done, so the next one does not do it again.
//!
//! a full migration is long enough that it will be interrupted — by a rate
//! limit, a dropped connection, or someone closing the laptop. every decision
//! is therefore written the moment it is made, and every phase consults this
//! before acting. rerunning is always safe and always cheap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, io};

/// everything known about one accepted match.
///
/// the spotify id alone would be enough to push, but not enough to rebuild the
/// report afterwards — and a report you can only produce during the run that
/// made it is not much of a record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchRecord {
    /// the spotify track id.
    pub spotify_id: String,
    /// how the spotify track reads.
    pub target: String,
    /// the spotify url.
    pub url: String,
    /// the score it was accepted at.
    pub score: f64,
    /// whether a human confirmed it.
    pub reviewed: bool,
}

/// why a track was given up on, kept for the same reason as [`MatchRecord`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UnmatchedRecord {
    /// the best score seen, if anything plausible turned up.
    pub best_score: Option<f64>,
    /// what that best candidate was.
    pub best_candidate: String,
}

/// the durable record of a migration in progress.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// yandex track id → what it was matched to.
    pub matched: BTreeMap<String, MatchRecord>,
    /// yandex track id → why it has no counterpart.
    pub unmatched: BTreeMap<String, UnmatchedRecord>,
    /// spotify track ids already added to "liked songs".
    pub saved_tracks: BTreeSet<String>,
    /// yandex playlist kind → spotify playlist id.
    pub playlists: BTreeMap<String, String>,
    /// spotify album ids already saved.
    pub saved_albums: BTreeSet<String>,
    /// spotify artist ids already followed.
    pub followed_artists: BTreeSet<String>,
    /// yandex track ids the user explicitly asked to download during review,
    /// regardless of what the download policy would otherwise do.
    pub download_forced: BTreeSet<String>,
    /// yandex track id → path of the downloaded file, relative to the music dir.
    pub downloaded: BTreeMap<String, String>,
    /// yandex track ids whose download failed and was given up on.
    pub download_failed: BTreeSet<String>,
}

impl State {
    /// whether a track has already been decided about, either way.
    pub fn is_decided(&self, yandex_id: &str) -> bool {
        self.matched.contains_key(yandex_id) || self.unmatched.contains_key(yandex_id)
    }

    /// the spotify id a track was matched to, if any.
    pub fn spotify_id(&self, yandex_id: &str) -> Option<&str> {
        self.matched.get(yandex_id).map(|m| m.spotify_id.as_str())
    }

    /// how many tracks found a counterpart.
    pub fn matched_count(&self) -> usize {
        self.matched.len()
    }

    /// how many of those a human confirmed.
    pub fn reviewed_count(&self) -> usize {
        self.matched.values().filter(|m| m.reviewed).count()
    }
}

/// a [`State`] bound to its file, saved after every change worth keeping.
pub struct Journal {
    /// the state itself.
    pub state: State,
    path: PathBuf,
}

impl Journal {
    /// load the journal, starting fresh when there is nothing to resume.
    ///
    /// an unparsable journal is fatal rather than ignored: silently starting
    /// over would re-add a library that is already half migrated.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(io(&path)(e)),
        };

        Ok(Self { state, path })
    }

    /// write the journal out atomically.
    ///
    /// through a temporary file and a rename, because the alternative — a
    /// truncated json file after an interrupt — loses the whole record of what
    /// was already pushed.
    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.state)?;
        let temp = self.path.with_extension("json.tmp");

        std::fs::write(&temp, text).map_err(io(&temp))?;
        std::fs::rename(&temp, &self.path).map_err(io(&self.path))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("yamuse2spotify-state-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_missing_journal_starts_empty_instead_of_failing() {
        let journal = Journal::load(temp_path("absent.json")).unwrap();
        assert!(journal.state.matched.is_empty());
    }

    #[test]
    fn a_saved_journal_reloads_with_every_decision_intact() {
        let path = temp_path("roundtrip.json");
        std::fs::remove_file(&path).ok();

        let mut journal = Journal::load(&path).unwrap();
        {
            let s = &mut journal.state;
            s.matched.insert(
                "42".into(),
                MatchRecord {
                    spotify_id: "spotify-id".into(),
                    target: "Artist — Song".into(),
                    url: "https://open.spotify.com/track/spotify-id".into(),
                    score: 0.97,
                    reviewed: true,
                },
            );
            s.unmatched.insert("43".into(), UnmatchedRecord::default());
        }
        journal.save().unwrap();

        let reloaded = Journal::load(&path).unwrap();
        assert_eq!(reloaded.state.spotify_id("42"), Some("spotify-id"));
        assert_eq!(reloaded.state.reviewed_count(), 1);
        assert!(reloaded.state.unmatched.contains_key("43"));
        assert!(reloaded.state.is_decided("42"));
        assert!(reloaded.state.is_decided("43"));
        assert!(!reloaded.state.is_decided("44"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let path = temp_path("atomic.json");
        std::fs::remove_file(&path).ok();

        let journal = Journal::load(&path).unwrap();
        journal.save().unwrap();

        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_journal_written_by_an_older_version_still_loads() {
        // `#[serde(default)]` is what makes adding a field later a non-event.
        let path = temp_path("partial.json");
        std::fs::write(
            &path,
            r#"{"matched":{"1":{"spotify_id":"a","target":"t","url":"u","score":1.0,"reviewed":false}}}"#,
        )
        .unwrap();

        let journal = Journal::load(&path).unwrap();
        assert_eq!(journal.state.matched_count(), 1);
        assert!(journal.state.downloaded.is_empty());

        std::fs::remove_file(&path).ok();
    }
}
