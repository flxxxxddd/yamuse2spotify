//! finding the spotify counterpart of a yandex track.
//!
//! the shape of the problem: no shared identifier exists. yandex does not send
//! an isrc, so nothing can be looked up directly and every answer is inferred
//! from text and duration. that makes two things load-bearing — a query cascade
//! that gives spotify several chances to find the record, and a scoring band
//! wide enough that doubtful answers reach a human instead of the library.

pub mod normalize;
pub mod score;

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, io};
use crate::model::SourceTrack;
use crate::spotify::Spotify;
use score::{Candidate, Score};

/// at or above this, a match is taken without asking.
pub const AUTO_THRESHOLD: f64 = 0.88;
/// below this, nothing plausible was found at all.
pub const REVIEW_THRESHOLD: f64 = 0.60;
/// how many hits to score per query.
const SEARCH_LIMIT: u32 = 10;
/// how many candidates to offer when asking.
const OFFER: usize = 5;

/// what the matcher concluded about one track.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// convincing enough to take unattended.
    Auto(Box<Candidate>, Score),
    /// plausible candidates, none convincing. ordered best first.
    Ambiguous(Vec<(Candidate, Score)>),
    /// spotify has nothing that resembles this track.
    NotFound,
}

/// thresholds, so a rerun can be stricter or looser without touching the code.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// auto-accept boundary.
    pub auto: f64,
    /// review boundary.
    pub review: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            auto: AUTO_THRESHOLD,
            review: REVIEW_THRESHOLD,
        }
    }
}

/// spotify search results, cached across runs.
///
/// keyed by the query rather than by the track, so that rescoring with new
/// thresholds — or after a change to [`normalize`] — is free. a full library is
/// thousands of requests; paying for them twice is the difference between a
/// two-minute rerun and another twenty-minute one.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchCache {
    queries: HashMap<String, Vec<Candidate>>,
    #[serde(skip)]
    dirty: bool,
}

impl SearchCache {
    /// load the cache, starting empty when there is no file or it is unreadable.
    ///
    /// a corrupt cache is a performance problem, never a correctness one, so it
    /// is discarded rather than reported.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(%e, "discarding an unreadable search cache");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// write the cache back, if anything was added since it was last written.
    ///
    /// the flag matters because this is called every hundred tracks: without it
    /// a resumed run would rewrite a multi-megabyte file for nothing.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }

        // `dirty` carries `#[serde(skip)]`, so the file holds only the queries.
        let text = serde_json::to_string(self)?;
        std::fs::write(path, text).map_err(io(path))?;
        self.dirty = false;

        Ok(())
    }

    /// how many queries are held.
    pub fn len(&self) -> usize {
        self.queries.len()
    }
}

/// the matcher: a spotify client, a cache, and the thresholds to judge by.
///
/// `find` takes `&self` and keeps the cache behind a mutex rather than taking
/// `&mut self`. that is what lets it be called from inside
/// [`crate::resilience::guarded`], whose retry closure cannot hand out a unique
/// borrow across an await.
pub struct Matcher<'a> {
    spotify: &'a Spotify,
    cache: std::sync::Mutex<SearchCache>,
    thresholds: Thresholds,
}

impl<'a> Matcher<'a> {
    /// build a matcher over an existing client and cache.
    pub fn new(spotify: &'a Spotify, cache: SearchCache, thresholds: Thresholds) -> Self {
        Self {
            spotify,
            cache: std::sync::Mutex::new(cache),
            thresholds,
        }
    }

    /// persist the cache mid-run, so an interrupted phase keeps its lookups.
    pub fn save_cache(&self, path: &Path) -> Result<()> {
        self.with_cache(|c| c.save(path))
    }

    /// hand the cache back so the caller can persist it.
    pub fn into_cache(self) -> SearchCache {
        self.cache
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// find the spotify counterpart of `track`.
    pub async fn find(&self, track: &SourceTrack) -> Result<Verdict> {
        let mut best: Vec<(Candidate, Score)> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for query in queries(track) {
            let candidates = self.search(&query).await?;

            for candidate in candidates {
                if !seen.insert(candidate.id.clone()) {
                    continue;
                }
                let s = score::score(track, &candidate);
                best.push((candidate, s));
            }

            // a convincing hit from a strict query makes the looser ones
            // pointless — and every one skipped is a request not spent.
            if best.iter().any(|(_, s)| s.total >= self.thresholds.auto) {
                break;
            }
        }

        best.sort_by(|a, b| b.1.total.total_cmp(&a.1.total));

        match best.first() {
            Some((candidate, s)) if s.total >= self.thresholds.auto => {
                Ok(Verdict::Auto(Box::new(candidate.clone()), *s))
            }
            Some((_, s)) if s.total >= self.thresholds.review => {
                best.truncate(OFFER);
                Ok(Verdict::Ambiguous(best))
            }
            _ => Ok(Verdict::NotFound),
        }
    }

    /// run one query, through the cache.
    async fn search(&self, query: &str) -> Result<Vec<Candidate>> {
        // the guard is taken and dropped around the await, never held across
        // it: a lock spanning a network round trip would serialise nothing
        // useful and deadlock the moment this grows a second caller.
        if let Some(hit) = self.with_cache(|c| c.queries.get(query).cloned()) {
            return Ok(hit);
        }

        let found = self.spotify.search_tracks(query, SEARCH_LIMIT).await?;

        // an empty result is cached too: "spotify has nothing for this" is just
        // as expensive to learn the second time.
        self.with_cache(|c| {
            c.queries.insert(query.to_owned(), found.clone());
            c.dirty = true;
        });

        Ok(found)
    }

    /// run `f` against the cache, tolerating a poisoned lock.
    ///
    /// a panic elsewhere must not turn a warm cache into a hard failure — the
    /// worst case of using it anyway is a stale search result.
    fn with_cache<T>(&self, f: impl FnOnce(&mut SearchCache) -> T) -> T {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

/// the query cascade for one track, strictest first.
///
/// the field-qualified forms are what spotify indexes best, but they also fail
/// hard on any spelling difference — hence the free-text fallbacks, and the
/// lead-artist-only form for the case where yandex credits five artists and
/// spotify credits one.
pub fn queries(track: &SourceTrack) -> Vec<String> {
    let (title, _) = normalize::split_featuring(&track.full_title());
    let title = normalize::strip_cosmetics(&title);
    let title = sanitize(&title);
    let lead = track
        .artists
        .first()
        .map(|a| sanitize(a))
        .unwrap_or_default();

    let mut out = Vec::new();

    if !title.is_empty() && !lead.is_empty() {
        out.push(format!("track:\"{title}\" artist:\"{lead}\""));

        if let Some(album) = track.album.as_deref() {
            let album = sanitize(&normalize::strip_cosmetics(album));
            if !album.is_empty() {
                out.push(format!(
                    "track:\"{title}\" artist:\"{lead}\" album:\"{album}\""
                ));
            }
        }

        out.push(format!("{lead} {title}"));
    } else if !title.is_empty() {
        out.push(title.clone());
    }

    // every credited artist, for releases where the lead differs between the
    // two catalogues — common on splits, features and compilations.
    if track.artists.len() > 1 && !title.is_empty() {
        let all = track
            .artists
            .iter()
            .map(|a| sanitize(a))
            .collect::<Vec<_>>()
            .join(" ");
        out.push(format!("{all} {title}"));
    }

    out.dedup();
    out
}

/// strip what spotify's query grammar would choke on.
///
/// double quotes end a field term early and the reserved words `AND`/`OR`/`NOT`
/// change the meaning of the query, so a title containing one silently searches
/// for something else.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '"' | '\\' | ':' => ' ',
            _ => c,
        })
        .collect();

    cleaned
        .split_whitespace()
        .filter(|w| !matches!(*w, "AND" | "OR" | "NOT"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> SourceTrack {
        SourceTrack {
            id: "1".into(),
            title: "Closer".into(),
            artists: vec!["Nine Inch Nails".into()],
            album: Some("The Downward Spiral".into()),
            duration_ms: Some(372_000),
            ..Default::default()
        }
    }

    #[test]
    fn the_cascade_runs_from_the_strictest_query_to_the_loosest() {
        let q = queries(&track());
        assert_eq!(q[0], "track:\"Closer\" artist:\"Nine Inch Nails\"");
        assert_eq!(
            q[1],
            "track:\"Closer\" artist:\"Nine Inch Nails\" album:\"The Downward Spiral\""
        );
        assert_eq!(q[2], "Nine Inch Nails Closer");
    }

    #[test]
    fn a_quote_in_a_title_cannot_break_out_of_its_field_term() {
        let mut t = track();
        t.title = r#"He said "no""#.into();
        for q in queries(&t) {
            assert_eq!(q.matches('"').count() % 2, 0, "unbalanced quotes in {q}");
        }
    }

    #[test]
    fn a_reserved_word_in_a_title_does_not_become_a_query_operator() {
        assert_eq!(sanitize("Love AND War"), "Love War");
        assert_eq!(sanitize("Colon: Here"), "Colon Here");
    }

    #[test]
    fn a_track_with_no_artist_still_produces_a_usable_query() {
        let mut t = track();
        t.artists.clear();
        let q = queries(&t);
        assert_eq!(q, vec!["Closer".to_string()]);
    }

    #[test]
    fn several_credited_artists_add_a_combined_query_at_the_end() {
        let mut t = track();
        t.artists = vec!["A".into(), "B".into()];
        let q = queries(&t);
        assert!(q.last().unwrap().starts_with("A B Closer"));
    }

    #[test]
    fn the_review_band_sits_below_the_auto_band_and_both_are_probabilities() {
        let t = Thresholds::default();
        assert!(t.review < t.auto);
        assert!((0.0..=1.0).contains(&t.auto) && (0.0..=1.0).contains(&t.review));
    }

    #[test]
    fn a_cache_that_will_not_parse_is_discarded_rather_than_failing_the_run() {
        let dir = std::env::temp_dir().join("yamuse2spotify-cache-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert_eq!(SearchCache::load(&path).len(), 0);
        std::fs::remove_file(&path).ok();
    }
}
