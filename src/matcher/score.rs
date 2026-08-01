//! how well one spotify candidate matches one yandex track.
//!
//! there is no isrc on the yandex side — the api simply does not send it — so
//! everything here is inference from title, artist and duration. the weights
//! are tuned so that no single dimension can carry a match on its own: a right
//! title with a wrong artist, or a right pair with a two-minute duration gap,
//! both land in the review band rather than being accepted.

use serde::{Deserialize, Serialize};

use crate::matcher::normalize;
use crate::model::SourceTrack;

/// weight of the title agreement in the total.
const W_TITLE: f64 = 0.45;
/// weight of the artist agreement.
const W_ARTIST: f64 = 0.35;
/// weight of the duration agreement.
const W_DURATION: f64 = 0.15;
/// weight of the album agreement — a tiebreaker, not evidence on its own.
const W_ALBUM: f64 = 0.05;

/// how far durations may differ before the agreement starts dropping.
const DURATION_FREE_MS: i64 = 2_000;
/// how far durations may differ before the agreement is zero.
const DURATION_DEAD_MS: i64 = 10_000;

/// penalty for a karaoke, tribute or nightcore upload of the right song.
const DERIVATIVE_PENALTY: f64 = 0.35;
/// penalty for an explicit/clean mismatch — real, but a different master of the
/// same recording is still the right answer.
const EXPLICIT_PENALTY: f64 = 0.02;

/// a spotify track reduced to what scoring needs.
///
/// cached to disk between runs, which is why it is `Serialize`: rescoring with
/// different thresholds must not cost another round of search requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// spotify track id.
    pub id: String,
    /// track title as spotify spells it.
    pub title: String,
    /// performing artists, in spotify's order.
    pub artists: Vec<String>,
    /// album title.
    pub album: String,
    /// duration in milliseconds.
    pub duration_ms: u32,
    /// whether spotify flags the track explicit.
    pub explicit: bool,
    /// open.spotify.com url, shown in prompts and reports.
    pub url: String,
}

impl Candidate {
    /// `"artist a, artist b — title"`, for prompts.
    pub fn display(&self) -> String {
        format!("{} — {}", self.artists.join(", "), self.title)
    }

    /// duration rendered as `m:ss`.
    pub fn duration_hms(&self) -> String {
        let secs = self.duration_ms / 1000;
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// the breakdown of one comparison.
///
/// the components are kept rather than collapsed because the reports print them:
/// "0.71" tells you nothing, "title 0.98 / artist 0.42" tells you the album was
/// credited to the wrong artist.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// the weighted total after penalties, clamped to `0.0..=1.0`.
    pub total: f64,
    /// title agreement.
    pub title: f64,
    /// artist agreement.
    pub artist: f64,
    /// duration agreement.
    pub duration: f64,
    /// album agreement.
    pub album: f64,
    /// how much was subtracted for derivative or explicit mismatches.
    pub penalty: f64,
}

/// score a candidate against the track it is meant to replace.
pub fn score(source: &SourceTrack, candidate: &Candidate) -> Score {
    let (source_title, source_guests) = normalize::split_featuring(&source.full_title());
    let (cand_title, cand_guests) = normalize::split_featuring(&candidate.title);

    let title = strsim::jaro_winkler(
        &normalize::fold(&normalize::strip_cosmetics(&source_title)),
        &normalize::fold(&normalize::strip_cosmetics(&cand_title)),
    );

    let artist = artist_similarity(
        &folded_artists(&source.artists, &source_guests),
        &folded_artists(&candidate.artists, &cand_guests),
    );

    let duration = duration_similarity(source.duration_ms, candidate.duration_ms);

    let album = match source.album.as_deref() {
        // no album on the yandex side is not evidence either way, so it must not
        // drag the total down: hand back the neutral middle.
        None => 0.5,
        Some(a) => strsim::jaro_winkler(
            &normalize::fold(&normalize::strip_cosmetics(a)),
            &normalize::fold(&normalize::strip_cosmetics(&candidate.album)),
        ),
    };

    let mut penalty = 0.0;
    if normalize::derivative_marker(&candidate.title).is_some()
        && normalize::derivative_marker(&source.full_title()).is_none()
    {
        penalty += DERIVATIVE_PENALTY;
    }
    if candidate.explicit != source.explicit {
        penalty += EXPLICIT_PENALTY;
    }

    let total = (W_TITLE * title + W_ARTIST * artist + W_DURATION * duration + W_ALBUM * album
        - penalty)
        .clamp(0.0, 1.0);

    Score {
        total,
        title,
        artist,
        duration,
        album,
        penalty,
    }
}

/// how similar two artist names must read before they count as the same act.
///
/// jaro-winkler is generous by construction: "nine inch nails" and "johnny
/// cash" score above 0.6 on shared letters alone. left raw, that noise floor
/// would push a wrong-artist match into the auto band on title strength.
const ARTIST_NOISE_FLOOR: f64 = 0.75;

/// the folded artist names to compare, guests from the title included.
fn folded_artists(credited: &[String], guests: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for name in credited
        .iter()
        .flat_map(|a| normalize::split_artists(a))
        .chain(guests.iter().cloned())
    {
        let folded = normalize::fold(&name);
        // "the beatles" and "beatles" are the same act, and the two catalogues
        // disagree about the article often enough to matter.
        let folded = folded.strip_prefix("the ").unwrap_or(&folded).to_owned();

        // order is load-bearing — the first entry is the lead artist — so this
        // deduplicates in place rather than sorting.
        if !folded.is_empty() && !out.contains(&folded) {
            out.push(folded);
        }
    }

    out
}

/// how well the source's artists are covered by the candidate's.
///
/// deliberately one-directional: spotify routinely credits guests that yandex
/// leaves in the title, and punishing those extras would reject correct matches.
fn artist_similarity(source: &[String], candidate: &[String]) -> f64 {
    if source.is_empty() || candidate.is_empty() {
        return 0.0;
    }

    let best = |name: &String| {
        candidate
            .iter()
            .map(|c| sharpen(strsim::jaro_winkler(name, c)))
            .fold(0.0_f64, f64::max)
    };

    // the lead artist dominates: a compilation may credit ten names, but a wrong
    // first one means this is somebody else's recording of the same song.
    let lead = best(&source[0]);
    let rest = if source.len() > 1 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "an artist list is a handful of names; the count is exactly representable"
        )]
        let n = (source.len() - 1) as f64;
        source[1..].iter().map(best).sum::<f64>() / n
    } else {
        lead
    };

    0.7 * lead + 0.3 * rest
}

/// rescale a raw name similarity so the noise floor lands at zero.
///
/// unrelated names cluster around 0.5–0.65 under jaro-winkler; without this,
/// "no artist agreement at all" would contribute a third of the artist weight.
fn sharpen(similarity: f64) -> f64 {
    ((similarity - ARTIST_NOISE_FLOOR) / (1.0 - ARTIST_NOISE_FLOOR)).clamp(0.0, 1.0)
}

/// how well two durations agree, on a `0.0..=1.0` scale.
fn duration_similarity(source_ms: Option<i64>, candidate_ms: u32) -> f64 {
    // yandex omits the duration on some catalogue entries. unknown is not
    // disagreement, so answer the neutral middle rather than zero.
    let Some(source_ms) = source_ms.filter(|ms| *ms > 0) else {
        return 0.5;
    };

    let diff = (source_ms - i64::from(candidate_ms)).abs();
    if diff <= DURATION_FREE_MS {
        return 1.0;
    }
    if diff >= DURATION_DEAD_MS {
        return 0.0;
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "diff is bounded by DURATION_DEAD_MS, far inside f64's exact integer range"
    )]
    let ratio = (diff - DURATION_FREE_MS) as f64 / (DURATION_DEAD_MS - DURATION_FREE_MS) as f64;
    1.0 - ratio
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "these compare against values the arithmetic produces exactly — \
              0.0, 1.0 and the clamp boundaries — where an epsilon would only \
              hide a regression"
)]
mod tests {
    use super::*;

    fn source(title: &str, artists: &[&str], ms: i64) -> SourceTrack {
        SourceTrack {
            id: "1".into(),
            title: title.into(),
            artists: artists.iter().map(|s| (*s).to_string()).collect(),
            album: Some("An Album".into()),
            duration_ms: Some(ms),
            ..Default::default()
        }
    }

    fn candidate(title: &str, artists: &[&str], ms: u32) -> Candidate {
        Candidate {
            id: "s1".into(),
            title: title.into(),
            artists: artists.iter().map(|s| (*s).to_string()).collect(),
            album: "An Album".into(),
            duration_ms: ms,
            explicit: false,
            url: String::new(),
        }
    }

    #[test]
    fn an_exact_match_scores_at_the_top_of_the_scale() {
        let s = score(
            &source("Closer", &["Nine Inch Nails"], 372_000),
            &candidate("Closer", &["Nine Inch Nails"], 372_000),
        );
        assert!(s.total > 0.99, "{s:?}");
    }

    #[test]
    fn a_remaster_tag_and_a_case_difference_do_not_cost_the_match() {
        let s = score(
            &source("Closer", &["Nine Inch Nails"], 372_000),
            &candidate("CLOSER - Remastered 2011", &["nine inch nails"], 372_400),
        );
        assert!(s.total > 0.95, "{s:?}");
    }

    #[test]
    fn a_karaoke_upload_of_the_right_song_falls_out_of_the_auto_band() {
        let s = score(
            &source("Closer", &["Nine Inch Nails"], 372_000),
            &candidate("Closer (Karaoke Version)", &["Karaoke Band"], 372_000),
        );
        assert!(s.penalty >= DERIVATIVE_PENALTY);
        assert!(s.total < 0.6, "{s:?}");
    }

    #[test]
    fn the_right_title_by_the_wrong_artist_never_reaches_the_auto_band() {
        let s = score(
            &source("Hurt", &["Nine Inch Nails"], 373_000),
            &candidate("Hurt", &["Johnny Cash"], 216_000),
        );
        assert!(s.title > 0.99);
        // an unrelated name must contribute nothing, not the ~0.6 that raw
        // jaro-winkler hands out for shared letters.
        assert_eq!(s.artist, 0.0, "{s:?}");
        assert!(s.total < 0.6, "{s:?}");
    }

    #[test]
    fn a_definite_article_does_not_make_two_spellings_of_one_band_disagree() {
        let s = score(
            &source("Song", &["The Beatles"], 200_000),
            &candidate("Song", &["Beatles"], 200_000),
        );
        assert!((s.artist - 1.0).abs() < f64::EPSILON, "{s:?}");
    }

    #[test]
    fn sharpening_zeroes_the_noise_floor_and_leaves_an_exact_match_alone() {
        assert_eq!(sharpen(0.6), 0.0);
        assert_eq!(sharpen(ARTIST_NOISE_FLOOR), 0.0);
        assert_eq!(sharpen(1.0), 1.0);
        assert!(sharpen(0.9) > 0.5);
    }

    #[test]
    fn a_two_minute_duration_gap_is_treated_as_total_disagreement() {
        assert_eq!(duration_similarity(Some(200_000), 320_000), 0.0);
    }

    #[test]
    fn a_duration_the_api_omitted_is_neutral_rather_than_a_mismatch() {
        assert_eq!(duration_similarity(None, 320_000), 0.5);
        assert_eq!(duration_similarity(Some(0), 320_000), 0.5);
    }

    #[test]
    fn duration_agreement_decays_smoothly_between_the_two_thresholds() {
        let close = duration_similarity(Some(200_000), 203_000);
        let far = duration_similarity(Some(200_000), 208_000);
        assert!(close > far);
        assert!((0.0..=1.0).contains(&close) && (0.0..=1.0).contains(&far));
    }

    #[test]
    fn a_guest_credited_in_the_title_matches_one_credited_as_an_artist() {
        let s = score(
            &source("Song", &["A", "B"], 200_000),
            &candidate("Song (feat. B)", &["A"], 200_000),
        );
        assert!(s.artist > 0.95, "{s:?}");
        assert!(s.total > 0.95, "{s:?}");
    }

    #[test]
    fn a_missing_album_on_the_yandex_side_does_not_drag_the_total_down() {
        let mut track = source("Song", &["A"], 200_000);
        track.album = None;
        let s = score(&track, &candidate("Song", &["A"], 200_000));
        assert_eq!(s.album, 0.5);
        assert!(s.total > 0.95, "{s:?}");
    }

    #[test]
    fn an_empty_artist_list_scores_zero_instead_of_dividing_by_nothing() {
        assert_eq!(artist_similarity(&[], &["a".into()]), 0.0);
        assert_eq!(artist_similarity(&["a".into()], &[]), 0.0);
    }
}
