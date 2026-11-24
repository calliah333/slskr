//! Search model types: query, scope, state flags and search snapshot.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};

/// A search query: space-separated terms plus `-`-prefixed exclusions.
///
/// Mirrors Soulseek.NET `SearchQuery`; [`SearchQuery::matches`] reproduces the
/// matching slskd applies when answering incoming searches.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SearchQuery {
    /// Full search text: terms followed by `-`-prefixed exclusions.
    pub search_text: String,
    /// Search terms, in order (duplicates preserved).
    pub terms: Vec<String>,
    /// Excluded phrases, without their leading `-`.
    pub exclusions: Vec<String>,
}

impl SearchQuery {
    /// Creates a query from explicit terms and exclusions.
    pub fn new(terms: Vec<String>, exclusions: Vec<String>) -> Self {
        let search_text = build_search_text(&terms, &exclusions);
        Self {
            search_text,
            terms,
            exclusions,
        }
    }

    /// Creates a query from a space-separated term string and explicit exclusions.
    pub fn with_exclusions(query: &str, exclusions: Vec<String>) -> Self {
        Self::new(query.split(' ').map(str::to_owned).collect(), exclusions)
    }

    /// Parses search text; tokens starting with `-` (and longer than one
    /// character) become distinct exclusions, everything else is a term.
    pub fn parse(search_text: &str) -> Self {
        let tokens: Vec<&str> = search_text.split(' ').collect();
        let excluded: Vec<&str> = tokens
            .iter()
            .copied()
            .filter(|t| t.starts_with('-') && t.chars().count() > 1)
            .collect();

        let mut exclusions: Vec<String> = Vec::new();
        for t in &excluded {
            let e = t.trim_start_matches('-');
            if !exclusions.iter().any(|x| x == e) {
                exclusions.push(e.to_owned());
            }
        }

        let terms = tokens
            .iter()
            .filter(|t| !excluded.contains(t))
            .map(|t| (*t).to_owned())
            .collect();

        Self::new(terms, exclusions)
    }

    /// The terms joined by spaces, without exclusions.
    pub fn query(&self) -> String {
        self.terms.join(" ")
    }

    /// Whether `filename` (typically a full, masked path) satisfies the query.
    ///
    /// Every term must match (case- and diacritic-insensitive) as a phrase of
    /// consecutive alphanumeric words; a term starting with `*` matches words
    /// ending with it. A file is rejected when any exclusion occurs in it,
    /// either as a word phrase or as a case-insensitive substring. Terms without
    /// any word characters are ignored; a query without effective terms matches
    /// nothing.
    pub fn matches(&self, filename: &str) -> bool {
        self.matcher().matches(filename)
    }

    /// Prepares the query once and reuses token storage across file matches.
    pub fn matcher(&self) -> SearchMatcher {
        SearchMatcher {
            terms: self
                .terms
                .iter()
                .map(|term| (tokenize(term), term.starts_with('*')))
                .filter(|(phrase, _)| !phrase.is_empty())
                .collect(),
            exclusions: self
                .exclusions
                .iter()
                .filter(|exclusion| !exclusion.is_empty())
                .map(|exclusion| (exclusion.to_lowercase(), tokenize(exclusion)))
                .collect(),
            words: Vec::new(),
        }
    }
}

/// Prepared incoming search, with scratch space for one filename.
pub struct SearchMatcher {
    terms: Vec<(Vec<String>, bool)>,
    exclusions: Vec<(String, Vec<String>)>,
    words: Vec<String>,
}

impl SearchMatcher {
    /// Whether no file can satisfy the query because it has no effective terms.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Tests a filename against the prepared query.
    pub fn matches(&mut self, filename: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let words = tokenize_into(filename, &mut self.words);
        if !self
            .terms
            .iter()
            .all(|(phrase, wildcard)| contains_phrase(words, phrase, *wildcard))
        {
            return false;
        }
        if self.exclusions.is_empty() {
            return true;
        }
        let lower = filename.to_lowercase();
        !self.exclusions.iter().any(|(substring, phrase)| {
            lower.contains(substring)
                || (!phrase.is_empty() && contains_phrase(words, phrase, false))
        })
    }
}

impl fmt::Display for SearchQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.search_text)
    }
}

impl From<&str> for SearchQuery {
    fn from(search_text: &str) -> Self {
        Self::parse(search_text)
    }
}

fn build_search_text(terms: &[String], exclusions: &[String]) -> String {
    let mut text = terms.join(" ");
    for e in exclusions {
        text.push_str(" -");
        text.push_str(e);
    }
    text
}

/// Splits into lowercase, diacritic-folded runs of alphanumeric characters
/// (equivalent to SQLite FTS5's default `unicode61` tokenizer used by slskd).
fn tokenize(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    tokenize_into(s, &mut words);
    words
}

fn tokenize_into<'a>(s: &str, words: &'a mut Vec<String>) -> &'a [String] {
    let mut count = 0;
    for token in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if count == words.len() {
            words.push(String::new());
        }
        let word = &mut words[count];
        word.clear();
        word.extend(
            token
                .chars()
                .flat_map(char::to_lowercase)
                .map(fold_diacritic),
        );
        count += 1;
    }
    &words[..count]
}

/// Whether `phrase` appears as consecutive words; with `wildcard` the first
/// phrase word only needs to be a suffix of the corresponding word.
fn contains_phrase(words: &[String], phrase: &[String], wildcard: bool) -> bool {
    if phrase.len() > words.len() {
        return false;
    }
    words.windows(phrase.len()).any(|window| {
        window.iter().zip(phrase).enumerate().all(|(i, (w, p))| {
            if i == 0 && wildcard {
                w.ends_with(p.as_str())
            } else {
                w == p
            }
        })
    })
}

/// Strips diacritics from lowercase Latin letters.
fn fold_diacritic(c: char) -> char {
    match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò'..='ö' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

/// The kind of a [`SearchScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum SearchScopeType {
    /// The entire network.
    #[default]
    Network = 0,
    /// One or more specific users.
    User = 1,
    /// A chat room.
    Room = 2,
    /// The server-side wishlist.
    Wishlist = 3,
}

impl SearchScopeType {
    /// The .NET enum name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "Network",
            Self::User => "User",
            Self::Room => "Room",
            Self::Wishlist => "Wishlist",
        }
    }
}

impl fmt::Display for SearchScopeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a search is sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SearchScope {
    pub kind: SearchScopeType,
    /// Room name (room scope) or usernames (user scope); empty otherwise.
    pub subjects: Vec<String>,
}

impl SearchScope {
    /// Creates a validated scope.
    pub fn new(kind: SearchScopeType, subjects: Vec<String>) -> Result<Self, String> {
        let scope = Self { kind, subjects };
        scope.validate()?;
        Ok(scope)
    }

    /// Network-wide search.
    pub fn network() -> Self {
        Self {
            kind: SearchScopeType::Network,
            subjects: Vec::new(),
        }
    }

    /// Wishlist search.
    pub fn wishlist() -> Self {
        Self {
            kind: SearchScopeType::Wishlist,
            subjects: Vec::new(),
        }
    }

    /// Search of a single room (unvalidated; see [`SearchScope::validate`]).
    pub fn room(name: impl Into<String>) -> Self {
        Self {
            kind: SearchScopeType::Room,
            subjects: vec![name.into()],
        }
    }

    /// Search of specific users (unvalidated; see [`SearchScope::validate`]).
    pub fn user<I, S>(usernames: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: SearchScopeType::User,
            subjects: usernames.into_iter().map(Into::into).collect(),
        }
    }

    /// Checks the subject constraints of the scope kind, as Soulseek.NET does.
    pub fn validate(&self) -> Result<(), String> {
        match self.kind {
            SearchScopeType::Network | SearchScopeType::Wishlist if !self.subjects.is_empty() => {
                Err(format!(
                    "The {} search scope can not be used with subjects",
                    self.kind
                ))
            }
            SearchScopeType::Room if self.subjects.len() != 1 || self.subjects[0].is_empty() => {
                Err(
                    "The Room search scope requires a single, non null and non empty subject"
                        .into(),
                )
            }
            SearchScopeType::User if self.subjects.is_empty() => {
                Err("The User search scope requires at least one subject".into())
            }
            SearchScopeType::User if self.subjects.iter().any(String::is_empty) => {
                Err("One or more of the supplied User scope subjects is null or empty".into())
            }
            _ => Ok(()),
        }
    }
}

/// Search state flags (Soulseek.NET `SearchStates`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SearchState(pub u32);

impl SearchState {
    pub const NONE: Self = Self(0);
    pub const REQUESTED: Self = Self(1);
    pub const IN_PROGRESS: Self = Self(2);
    pub const COMPLETED: Self = Self(4);
    pub const CANCELLED: Self = Self(8);
    pub const TIMED_OUT: Self = Self(16);
    pub const RESPONSE_LIMIT_REACHED: Self = Self(32);
    pub const FILE_LIMIT_REACHED: Self = Self(64);
    pub const ERRORED: Self = Self(128);
    pub const QUEUED: Self = Self(256);

    /// Named flags in ascending value order.
    const NAMES: [(Self, &'static str); 9] = [
        (Self::REQUESTED, "Requested"),
        (Self::IN_PROGRESS, "InProgress"),
        (Self::COMPLETED, "Completed"),
        (Self::CANCELLED, "Cancelled"),
        (Self::TIMED_OUT, "TimedOut"),
        (Self::RESPONSE_LIMIT_REACHED, "ResponseLimitReached"),
        (Self::FILE_LIMIT_REACHED, "FileLimitReached"),
        (Self::ERRORED, "Errored"),
        (Self::QUEUED, "Queued"),
    ];

    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all bits of `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit of `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// .NET `[Flags]` `ToString()`: `"None"`, or set flag names in ascending
    /// order joined by `", "` (e.g. `"Completed, TimedOut"`); the number when
    /// undefined bits are set.
    pub fn to_slskd_string(&self) -> String {
        if self.0 == 0 {
            return "None".to_owned();
        }
        let known = Self::NAMES.iter().fold(0, |acc, (f, _)| acc | f.0);
        if self.0 & !known != 0 {
            return self.0.to_string();
        }
        Self::NAMES
            .iter()
            .filter(|(f, _)| self.contains(*f))
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parses the output of [`SearchState::to_slskd_string`] (names are
    /// case-insensitive; a plain number is accepted too).
    pub fn from_slskd_str(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Ok(n) = s.parse::<u32>() {
            return Some(Self(n));
        }
        let mut state = Self::NONE;
        for part in s.split(',') {
            let part = part.trim();
            if part.eq_ignore_ascii_case("None") {
                continue;
            }
            let (flag, _) = Self::NAMES
                .iter()
                .find(|(_, n)| n.eq_ignore_ascii_case(part))?;
            state |= *flag;
        }
        Some(state)
    }
}

impl fmt::Display for SearchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_slskd_string())
    }
}

impl BitOr for SearchState {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for SearchState {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for SearchState {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// Snapshot of a search (Soulseek.NET `Search`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Search {
    pub query: SearchQuery,
    pub scope: SearchScope,
    pub token: u32,
    pub state: SearchState,
    pub response_count: u32,
    pub file_count: u32,
    pub locked_file_count: u32,
}

impl Search {
    /// The query's full search text.
    pub fn search_text(&self) -> &str {
        &self.query.search_text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn splits_terms_and_exclusions() {
        let q = SearchQuery::parse("foo bar -baz -qux");
        assert_eq!(q.terms, strings(&["foo", "bar"]));
        assert_eq!(q.exclusions, strings(&["baz", "qux"]));
        assert_eq!(q.query(), "foo bar");
        assert_eq!(q.search_text, "foo bar -baz -qux");
    }

    #[test]
    fn empty_search_text() {
        let q = SearchQuery::parse("");
        assert_eq!(q.terms, strings(&[""]));
        assert!(q.exclusions.is_empty());
        assert_eq!(q.query(), "");
        assert_eq!(q.search_text, "");
        assert!(!q.matches("anything"));

        let q = SearchQuery::new(Vec::new(), Vec::new());
        assert!(q.terms.is_empty());
        assert_eq!(q.search_text, "");
    }

    #[test]
    fn constructs_expected_search_text() {
        assert_eq!(
            SearchQuery::with_exclusions("foo", strings(&["bar", "baz"])).search_text,
            "foo -bar -baz"
        );
        assert_eq!(
            SearchQuery::with_exclusions("foo", strings(&["bar"])).search_text,
            "foo -bar"
        );
        assert_eq!(
            SearchQuery::with_exclusions("foo", Vec::new()).search_text,
            "foo"
        );
        assert_eq!(SearchQuery::parse("foo").to_string(), "foo");
    }

    #[test]
    fn parses_single_character_tokens_and_punctuation() {
        let q = SearchQuery::parse(
            "a ! b @ c # d % e ^ f & g * h ( i ) j - k _ l + m = n -big_old_exclusion ~ o ` p [ q { r ] s } t | u \\ v ; w : x ' y \" z , a < b . c > d / e ?",
        );
        assert_eq!(
            q.query(),
            "a ! b @ c # d % e ^ f & g * h ( i ) j - k _ l + m = n ~ o ` p [ q { r ] s } t | u \\ v ; w : x ' y \" z , a < b . c > d / e ?"
        );
        assert_eq!(
            q.search_text,
            "a ! b @ c # d % e ^ f & g * h ( i ) j - k _ l + m = n ~ o ` p [ q { r ] s } t | u \\ v ; w : x ' y \" z , a < b . c > d / e ? -big_old_exclusion"
        );
        assert_eq!(q.exclusions, strings(&["big_old_exclusion"]));
    }

    #[test]
    fn parses_exclusions_out_of_order_and_singly() {
        let q = SearchQuery::parse("-bar foo -baz");
        assert_eq!(q.query(), "foo");
        assert_eq!(q.search_text, "foo -bar -baz");

        let q = SearchQuery::parse("-bar foo -baz -baz -bar");
        assert_eq!(q.search_text, "foo -bar -baz");
        assert_eq!(q.exclusions, strings(&["bar", "baz"]));

        let q = SearchQuery::parse("foo --bar");
        assert_eq!(q.exclusions, strings(&["bar"]));
    }

    #[test]
    fn preserves_duplicate_terms_and_quotes() {
        let q = SearchQuery::parse("foo bar foo foo");
        assert_eq!(q.search_text, "foo bar foo foo");
        assert!(q.exclusions.is_empty());

        // quotes have no special meaning
        let q = SearchQuery::parse("\"foo bar\"");
        assert_eq!(q.terms, strings(&["\"foo", "bar\""]));
    }

    #[test]
    fn round_trips_search_text() {
        for text in ["foo", "foo bar -baz", "a  b", "x - y"] {
            assert_eq!(SearchQuery::parse(text).search_text, text);
        }
    }

    #[test]
    fn matches_terms_case_insensitively() {
        let q = SearchQuery::parse("Pink FLOYD");
        assert!(q.matches("@@abcde\\Music\\Pink Floyd\\Animals\\01 - Dogs.flac"));
        assert!(q.matches("music/floyd/pink.mp3"));
        assert!(!q.matches("music/pink/floydian.mp3"));
        assert!(!q.matches("music/pink/other.mp3"));
    }

    #[test]
    fn matches_whole_words_and_phrases() {
        assert!(!SearchQuery::parse("dog").matches("Dogs.flac"));
        assert!(SearchQuery::parse("dogs").matches("Dogs.flac"));
        // punctuation inside a term makes it a phrase of consecutive words
        let q = SearchQuery::parse("ac/dc");
        assert!(q.matches("AC-DC - Back in Black.mp3"));
        assert!(!q.matches("AC - Back - DC.mp3"));
        // terms without word characters are ignored
        assert!(SearchQuery::parse("artist - title").matches("Artist - Title.mp3"));
        assert!(!SearchQuery::parse("- !").matches("Artist - Title.mp3"));
    }

    #[test]
    fn matches_diacritics_insensitively() {
        assert!(SearchQuery::parse("beyonce").matches("Beyoncé - Halo.mp3"));
        assert!(SearchQuery::parse("BEYONCÉ").matches("beyonce - halo.mp3"));
    }

    #[test]
    fn matches_wildcard_suffix() {
        let q = SearchQuery::parse("*ello");
        assert!(q.matches("hello world.mp3"));
        assert!(q.matches("Jello.mp3"));
        assert!(q.matches("ello.mp3"));
        assert!(!q.matches("hellos.mp3"));
    }

    #[test]
    fn applies_exclusions() {
        let q = SearchQuery::parse("foo -bar");
        assert!(q.matches("foo baz.mp3"));
        assert!(!q.matches("foo bar baz.mp3"));
        // substrings are excluded too
        assert!(!q.matches("foo xbarx baz.mp3"));
        assert!(!q.matches("FOO BAR.mp3"));
        // diacritic-folded word exclusion
        assert!(!SearchQuery::parse("halo -beyonce").matches("Beyoncé - Halo.mp3"));
        // exclusions only
        assert!(!SearchQuery::parse("-bar").matches("foo.mp3"));
    }

    #[test]
    fn reuses_matcher_with_paths_of_different_lengths() {
        let mut matcher = SearchQuery::parse("*ello ac/dc -live").matcher();
        assert!(matcher.matches("Music/Hello/AC-DC - Track.flac"));
        assert!(!matcher.matches("Hello.mp3"));
        assert!(!matcher.matches("Jello/AC-DC - lIvE.flac"));
        assert!(matcher.matches("Jello/AC-DC.flac"));
        assert!(SearchQuery::parse("! -live").matcher().is_empty());
    }

    #[test]
    fn scope_constructors_and_validation() {
        assert_eq!(SearchScope::network().kind, SearchScopeType::Network);
        assert_eq!(SearchScope::wishlist().kind, SearchScopeType::Wishlist);
        let room = SearchScope::room("lobby");
        assert_eq!(room.kind, SearchScopeType::Room);
        assert_eq!(room.subjects, strings(&["lobby"]));
        let users = SearchScope::user(["a", "b"]);
        assert_eq!(users.subjects, strings(&["a", "b"]));
        for s in [SearchScope::network(), SearchScope::wishlist(), room, users] {
            assert!(s.validate().is_ok());
        }

        assert!(SearchScope::new(SearchScopeType::Network, strings(&["x"])).is_err());
        assert!(SearchScope::new(SearchScopeType::Wishlist, strings(&["x"])).is_err());
        assert!(SearchScope::new(SearchScopeType::Room, Vec::new()).is_err());
        assert!(SearchScope::new(SearchScopeType::Room, strings(&[""])).is_err());
        assert!(SearchScope::new(SearchScopeType::Room, strings(&["a", "b"])).is_err());
        assert!(SearchScope::new(SearchScopeType::User, Vec::new()).is_err());
        assert!(SearchScope::new(SearchScopeType::User, strings(&["a", ""])).is_err());
        assert!(SearchScope::new(SearchScopeType::User, strings(&["a"])).is_ok());
    }

    #[test]
    fn state_flags() {
        let s = SearchState::COMPLETED | SearchState::TIMED_OUT;
        assert_eq!(s.bits(), 20);
        assert!(s.contains(SearchState::COMPLETED));
        assert!(s.contains(SearchState::TIMED_OUT));
        assert!(!s.contains(SearchState::ERRORED));
        assert!(s.contains(SearchState::NONE));
        assert_eq!(s & SearchState::COMPLETED, SearchState::COMPLETED);

        let mut m = SearchState::REQUESTED;
        m |= SearchState::IN_PROGRESS;
        m.remove(SearchState::REQUESTED);
        assert_eq!(m, SearchState::IN_PROGRESS);
    }

    #[test]
    fn state_strings() {
        assert_eq!(SearchState::NONE.to_slskd_string(), "None");
        assert_eq!(
            (SearchState::TIMED_OUT | SearchState::COMPLETED).to_slskd_string(),
            "Completed, TimedOut"
        );
        assert_eq!(
            (SearchState::COMPLETED | SearchState::RESPONSE_LIMIT_REACHED).to_string(),
            "Completed, ResponseLimitReached"
        );
        assert_eq!(SearchState::IN_PROGRESS.to_slskd_string(), "InProgress");
        assert_eq!(SearchState::QUEUED.to_slskd_string(), "Queued");
        assert_eq!(SearchState(512 | 4).to_slskd_string(), "516");

        for bits in [0, 1, 2, 20, 36, 68, 132, 140, 256] {
            let s = SearchState(bits);
            assert_eq!(SearchState::from_slskd_str(&s.to_slskd_string()), Some(s));
        }
        assert_eq!(
            SearchState::from_slskd_str("completed,timedout"),
            Some(SearchState::COMPLETED | SearchState::TIMED_OUT)
        );
        assert_eq!(SearchState::from_slskd_str("Bogus"), None);
    }

    #[test]
    fn search_text_accessor() {
        let s = Search {
            query: SearchQuery::parse("foo -bar"),
            scope: SearchScope::network(),
            token: 1,
            state: SearchState::REQUESTED,
            ..Default::default()
        };
        assert_eq!(s.search_text(), "foo -bar");
    }
}
