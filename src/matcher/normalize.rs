//! string folding, so that "Nine Inch Nails" and "NINE INCH NAILS" and
//! "Nine Inch Nails (feat. …)" all compare as the same thing.
//!
//! nothing here is clever on its own. it exists because yandex and spotify
//! disagree about punctuation, capitalisation, where featured artists live and
//! whether a remaster year belongs in the title — and every one of those
//! disagreements costs a match if left alone.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// bracketed or dash-suffixed fragments that mean the same recording.
///
/// a spotify release says "Remastered 2011" where yandex says nothing; treating
/// that as part of the title loses the match outright.
const COSMETIC: &[&str] = &[
    "remaster",
    "remastered",
    "digitally remastered",
    "deluxe",
    "deluxe edition",
    "expanded",
    "expanded edition",
    "reissue",
    "anniversary",
    "bonus track",
    "bonus",
    "explicit",
    "clean",
    "album version",
    "single version",
    "original mix",
    "original version",
    "stereo",
    "mono",
    "audio",
    "official audio",
    "official video",
    "official music video",
    "lyric video",
    "hd",
    "hq",
];

/// fragments that mean a *different* recording, so they must survive folding.
///
/// dropping these would let a studio cut match a live one at full confidence.
const MEANINGFUL: &[&str] = &[
    "remix",
    "live",
    "acoustic",
    "unplugged",
    "instrumental",
    "demo",
    "radio edit",
    "extended",
    "edit",
    "mix",
    "version",
    "reprise",
    "part",
    "pt",
    "session",
];

/// markers of a recording by someone else entirely.
///
/// spotify is full of karaoke and tribute uploads that score well on title and
/// artist alike; only an explicit penalty keeps them out of a library.
const DERIVATIVE: &[&str] = &[
    "karaoke",
    "tribute",
    "made famous by",
    "made popular by",
    "in the style of",
    "cover version",
    "covered by",
    "8 bit",
    "8-bit",
    "lullaby",
    "nightcore",
    "sped up",
    "slowed",
    "reverb",
    "workout mix",
    "backing track",
];

/// separators that join several artists into one string.
///
/// deliberately excludes a bare `и`/`and`: "Simon and Garfunkel" is one act and
/// splitting it produces two artists that match nothing.
const ARTIST_SEPARATORS: &[&str] = &[
    " feat. ",
    " feat ",
    " featuring ",
    " ft. ",
    " ft ",
    " с участием ",
    " при участии ",
    " x ",
    " vs. ",
    " vs ",
    " & ",
    ", ",
    "; ",
    " / ",
];

/// fold a string into the form every comparison runs on.
///
/// lowercases, strips diacritics, and reduces punctuation to single spaces. the
/// diacritic pass is what makes `ё` and `е` compare equal for free: `ё`
/// decomposes to `е` plus a combining mark, and the mark is dropped.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;

    for c in s.nfd().filter(|c| !is_combining_mark(*c)) {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }

    out
}

/// pull `feat. …` out of a title, returning the cleaned title and the guests.
///
/// yandex keeps featured artists in the artist list; spotify frequently keeps
/// them in the title instead. moving them to one side makes both comparable.
pub fn split_featuring(title: &str) -> (String, Vec<String>) {
    const MARKERS: &[&str] = &[
        "feat.",
        "feat ",
        "featuring",
        "ft.",
        "ft ",
        "при участии",
        "с участием",
    ];

    let lower = title.to_lowercase();
    let Some((at, marker)) = MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|i| (i, *m)))
        .min_by_key(|(i, _)| *i)
    else {
        return (title.trim().to_string(), Vec::new());
    };

    let head_raw = &title[..at];
    // a bracket left open before the marker is the one the guest list closes.
    let open = head_raw
        .rfind(['(', '['])
        .filter(|o| !head_raw[*o..].contains([')', ']']));
    let close = title[at..].find([')', ']']).map(|i| i + at);

    // only cut the brackets away when they were opened for the guests alone;
    // "song (live feat. x)" must keep the "live" and its parentheses.
    let (cut_start, cut_end) = match (open, close) {
        (Some(o), Some(c)) if head_raw[o + 1..].trim().is_empty() => (o, c + 1),
        (Some(_), Some(c)) => (at, c),
        _ => (at, title.len()),
    };
    let cut_end = cut_end.min(title.len());

    let guests = title[at + marker.len()..cut_end].trim_end_matches([')', ']']);

    let mut head = title[..cut_start]
        .trim_end_matches(['(', '[', '-', ' ', ','])
        .to_string();
    head.push_str(&title[cut_end..]);

    (head.trim().to_string(), split_artists(guests))
}

/// drop bracketed or dash-suffixed fragments that carry no musical meaning.
///
/// a fragment survives whenever it names something that changes the recording —
/// a remix, a live take — even if it also mentions a remaster.
pub fn strip_cosmetics(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut rest = title;

    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(close) = rest[open..].find(close_char).map(|i| i + open) else {
            break;
        };

        out.push_str(&rest[..open]);
        let inner = &rest[open + 1..close];
        if !is_cosmetic(inner) {
            out.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);

    // spotify also writes the same thing as a dash suffix: "song - Remastered".
    if let Some(dash) = out.rfind(" - ")
        && is_cosmetic(&out[dash + 3..])
    {
        out.truncate(dash);
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// whether a fragment is cosmetic — mentions a cosmetic term and nothing that
/// would change the recording.
fn is_cosmetic(fragment: &str) -> bool {
    let folded = fold(fragment);
    if folded.is_empty() {
        return true;
    }

    // an exact phrase beats the keyword guard below: "album version" is cosmetic
    // even though "version" on its own means the recording differs.
    if COSMETIC.iter().any(|c| fold(c) == folded) {
        return true;
    }

    // a bare year, as in "(2011)", is always cosmetic.
    if folded.len() == 4 && folded.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }

    if MEANINGFUL.iter().any(|m| contains_word(&folded, m)) {
        return false;
    }

    COSMETIC.iter().any(|c| contains_word(&folded, &fold(c)))
}

/// the derivative marker a title carries, if any.
pub fn derivative_marker(title: &str) -> Option<&'static str> {
    let folded = fold(title);
    DERIVATIVE
        .iter()
        .find(|d| contains_word(&folded, &fold(d)))
        .copied()
}

/// split a joined artist string into its parts.
pub fn split_artists(s: &str) -> Vec<String> {
    let mut parts = vec![s.to_string()];

    for sep in ARTIST_SEPARATORS {
        parts = parts
            .iter()
            .flat_map(|p| split_case_insensitive(p, sep))
            .collect();
    }

    parts
        .into_iter()
        .map(|p| {
            p.trim_matches(['(', ')', '[', ']', ' ', ',', '.'])
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// split on `sep` regardless of case, keeping the original casing of the parts.
fn split_case_insensitive(haystack: &str, sep: &str) -> Vec<String> {
    let lower = haystack.to_lowercase();
    let sep_lower = sep.to_lowercase();
    let mut out = Vec::new();
    let mut start = 0;

    while let Some(at) = lower[start..].find(&sep_lower).map(|i| i + start) {
        out.push(haystack[start..at].to_string());
        start = at + sep.len();
    }
    out.push(haystack[start..].to_string());
    out
}

/// whether `folded` contains `needle` on word boundaries.
///
/// substring matching would flag "livery" as "live" and "premastered" as
/// "remaster", both of which happen in real catalogues.
fn contains_word(folded: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(at) = folded[from..].find(needle).map(|i| i + from) {
        let before_ok = at == 0 || !folded[..at].ends_with(|c: char| c.is_alphanumeric());
        let after = at + needle.len();
        let after_ok =
            after == folded.len() || !folded[after..].starts_with(|c: char| c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_erases_case_punctuation_and_the_diaeresis_on_yo() {
        assert_eq!(fold("Ёлка!"), "елка");
        assert_eq!(fold("Елка"), "елка");
        assert_eq!(fold("  A.  B—C  "), "a b c");
        assert_eq!(fold("Björk"), "bjork");
    }

    #[test]
    fn folding_leaves_nothing_but_the_separator_for_a_punctuation_only_string() {
        assert_eq!(fold("!!! ??? ---"), "");
    }

    #[test]
    fn a_remaster_suffix_is_cosmetic_but_a_live_take_is_not() {
        assert_eq!(strip_cosmetics("Song (Remastered 2011)"), "Song");
        assert_eq!(strip_cosmetics("Song - Remastered"), "Song");
        assert_eq!(strip_cosmetics("Song (2011)"), "Song");
        assert_eq!(
            strip_cosmetics("Song (Live at Wembley)"),
            "Song (Live at Wembley)"
        );
        assert_eq!(
            strip_cosmetics("Song (Remastered Live Version)"),
            "Song (Remastered Live Version)"
        );
    }

    #[test]
    fn a_cosmetic_bracket_in_the_middle_does_not_swallow_what_follows() {
        assert_eq!(strip_cosmetics("Song (Explicit) Reprise"), "Song Reprise");
    }

    #[test]
    fn an_unclosed_bracket_is_left_alone_rather_than_truncating_the_title() {
        assert_eq!(strip_cosmetics("Song (Remastered"), "Song (Remastered");
    }

    #[test]
    fn featured_artists_move_from_the_title_to_the_artist_list() {
        let (title, guests) = split_featuring("Song (feat. A & B)");
        assert_eq!(title, "Song");
        assert_eq!(guests, vec!["A", "B"]);

        let (title, guests) = split_featuring("Song ft. C");
        assert_eq!(title, "Song");
        assert_eq!(guests, vec!["C"]);
    }

    #[test]
    fn removing_a_guest_list_keeps_whatever_followed_it() {
        let (title, guests) = split_featuring("Song (feat. X) - Remastered 2011");
        assert_eq!(title, "Song - Remastered 2011");
        assert_eq!(guests, vec!["X"]);
    }

    #[test]
    fn a_guest_list_sharing_a_bracket_leaves_the_rest_of_the_bracket_alone() {
        let (title, guests) = split_featuring("Song (Live feat. X)");
        assert_eq!(title, "Song (Live)");
        assert_eq!(guests, vec!["X"]);
    }

    #[test]
    fn an_album_version_tag_is_cosmetic_even_though_version_alone_is_not() {
        assert_eq!(strip_cosmetics("Song (Album Version)"), "Song");
        assert_eq!(strip_cosmetics("Song (Radio Edit)"), "Song (Radio Edit)");
    }

    #[test]
    fn a_title_without_guests_survives_the_split_untouched() {
        let (title, guests) = split_featuring("Song");
        assert_eq!(title, "Song");
        assert!(guests.is_empty());
    }

    #[test]
    fn artists_split_on_joiners_but_not_on_a_bare_and() {
        assert_eq!(split_artists("A, B & C"), vec!["A", "B", "C"]);
        assert_eq!(
            split_artists("Simon and Garfunkel"),
            vec!["Simon and Garfunkel"]
        );
        assert_eq!(split_artists("A vs. B"), vec!["A", "B"]);
    }

    #[test]
    fn karaoke_and_tribute_uploads_are_flagged_as_derivative() {
        assert!(derivative_marker("Song (Karaoke Version)").is_some());
        assert!(derivative_marker("Song - In the Style of Someone").is_some());
        assert!(derivative_marker("Song (Live)").is_none());
    }

    #[test]
    fn word_matching_does_not_fire_on_a_substring() {
        assert!(!contains_word("livery stable", "live"));
        assert!(contains_word("live at wembley", "live"));
        assert!(!contains_word("premastered", "remaster"));
    }
}
