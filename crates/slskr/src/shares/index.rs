//! Inverted word index over a share snapshot for answering incoming searches.
//!
//! Every shared file gets an id in search order (shares, then directories,
//! then files, as the snapshot's maps iterate), and every word of its full
//! remote path maps to the sorted ids of the files containing it. A query
//! intersects the postings of its required words and then confirms each
//! candidate with the full [`soulseek::SearchMatcher`], so results and their
//! order are exactly those of a linear scan.

use std::{borrow::Cow, collections::BTreeMap, collections::HashMap, fmt};

use soulseek::{RequiredToken, SearchMatcher, SearchTokenizer};

use super::ShareContents;

pub(super) struct SearchIndex {
    /// Non-empty directories in search order, with the id of their first file.
    directories: Vec<IndexedDirectory>,
    /// Word → ascending ids of the files whose remote path contains it.
    postings: HashMap<Box<str>, Vec<u32>>,
}

struct IndexedDirectory {
    first: u32,
    name: Box<str>,
}

/// Where a file id points: its directory's position and name, and the file's
/// position within the directory.
pub(super) struct Located<'a> {
    pub directory: usize,
    pub name: &'a str,
    pub file: usize,
}

impl SearchIndex {
    /// Indexes the snapshot; `None` when it holds more files than ids can name.
    pub(super) fn build(shares: &BTreeMap<String, ShareContents>) -> Option<Self> {
        let mut directories = Vec::new();
        let mut postings: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        let mut parent_words = SearchTokenizer::default();
        let mut file_words = SearchTokenizer::default();
        let mut next = 0u32;
        for (name, files) in shares.values().flat_map(|share| &share.directories) {
            if files.is_empty() {
                continue;
            }
            let first = next;
            for file in files {
                for word in file_words.tokenize(&file.name) {
                    add(&mut postings, word, next);
                }
                next = next.checked_add(1)?;
            }
            // Directory words cover the whole directory's id range. Drop any
            // ids its file names already added so the postings stay sorted
            // and unique.
            for word in parent_words.tokenize(name) {
                if !postings.contains_key(word.as_str()) {
                    postings.insert(word.as_str().into(), Vec::new());
                }
                let ids = postings.get_mut(word.as_str()).expect("inserted word");
                let kept = ids.partition_point(|&id| id < first);
                ids.truncate(kept);
                ids.extend(first..next);
            }
            directories.push(IndexedDirectory {
                first,
                name: name.as_str().into(),
            });
        }
        for ids in postings.values_mut() {
            ids.shrink_to_fit();
        }
        postings.shrink_to_fit();
        directories.shrink_to_fit();
        Some(Self {
            directories,
            postings,
        })
    }

    /// Sorted id lists whose intersection holds every file the matcher can
    /// accept, or `None` when the query has no word the index can use.
    ///
    /// Exact words are looked up directly. Only when a query has none are its
    /// `*` suffixes expanded through the vocabulary.
    pub(super) fn candidates(&self, matcher: &SearchMatcher) -> Option<Vec<Cow<'_, [u32]>>> {
        let mut lists = Vec::new();
        let mut suffixes = Vec::new();
        for token in matcher.required_tokens() {
            match token {
                RequiredToken::Exact(word) => match self.postings.get(word) {
                    Some(ids) => lists.push(Cow::Borrowed(ids.as_slice())),
                    None => return Some(vec![Cow::Borrowed(&[][..])]),
                },
                RequiredToken::Suffix(word) => suffixes.push(word),
            }
        }
        if lists.is_empty() {
            for suffix in suffixes {
                let mut ids: Vec<u32> = self
                    .postings
                    .iter()
                    .filter(|(word, _)| word.ends_with(suffix))
                    .flat_map(|(_, ids)| ids.iter().copied())
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                lists.push(Cow::Owned(ids));
            }
        }
        (!lists.is_empty()).then_some(lists)
    }

    /// Finds the directory and file an id names.
    pub(super) fn locate(&self, id: u32) -> Located<'_> {
        let directory = self.directories.partition_point(|dir| dir.first <= id) - 1;
        let entry = &self.directories[directory];
        Located {
            directory,
            name: &entry.name,
            file: (id - entry.first) as usize,
        }
    }

    /// Approximate heap bytes held by the index.
    #[cfg(test)]
    pub(super) fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let directories = self.directories.capacity() * size_of::<IndexedDirectory>()
            + self
                .directories
                .iter()
                .map(|dir| dir.name.len())
                .sum::<usize>();
        // hashbrown keeps one control byte per bucket next to the entries.
        let buckets = self.postings.capacity().next_power_of_two() * 8 / 7;
        let table = buckets * (size_of::<(Box<str>, Vec<u32>)>() + 1);
        let entries = self
            .postings
            .iter()
            .map(|(word, ids)| word.len() + ids.capacity() * size_of::<u32>())
            .sum::<usize>();
        directories + table + entries
    }

    #[cfg(test)]
    pub(super) fn words(&self) -> usize {
        self.postings.len()
    }

    #[cfg(test)]
    pub(super) fn postings_len(&self) -> usize {
        self.postings.values().map(Vec::len).sum()
    }
}

impl fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchIndex")
            .field("directories", &self.directories.len())
            .field("words", &self.postings.len())
            .finish_non_exhaustive()
    }
}

fn add(postings: &mut HashMap<Box<str>, Vec<u32>>, word: &str, id: u32) {
    match postings.get_mut(word) {
        Some(ids) => {
            if ids.last() != Some(&id) {
                ids.push(id);
            }
        }
        None => {
            postings.insert(word.into(), vec![id]);
        }
    }
}

/// Ascending ids present in every list, read lazily so a caller that stops
/// early pays only for the ids it consumed.
pub(super) fn intersect<'a>(lists: &'a [Cow<'a, [u32]>]) -> impl Iterator<Item = u32> + 'a {
    let mut sorted: Vec<&[u32]> = lists.iter().map(AsRef::as_ref).collect();
    sorted.sort_unstable_by_key(|ids| ids.len());
    let (lead, mut rest) = match sorted.split_first() {
        Some((lead, rest)) => (*lead, rest.to_vec()),
        None => (&[][..], Vec::new()),
    };
    lead.iter().copied().filter(move |&id| {
        rest.iter_mut().all(|ids| {
            *ids = &ids[gallop(ids, id)..];
            ids.first() == Some(&id)
        })
    })
}

/// The number of leading ids below `target`, found by exponential search.
fn gallop(ids: &[u32], target: u32) -> usize {
    if ids.first().is_none_or(|&id| id >= target) {
        return 0;
    }
    let mut bound = 1;
    while bound < ids.len() && ids[bound] < target {
        bound *= 2;
    }
    let low = bound / 2;
    let high = bound.min(ids.len());
    low + ids[low..high].partition_point(|&id| id < target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersects_sorted_lists() {
        let a: Vec<u32> = (0..100).collect();
        let b: Vec<u32> = (0..100).step_by(3).collect();
        let c = [3, 4, 9, 50, 51, 99];
        let lists = [Cow::Owned(a), Cow::Owned(b), Cow::Borrowed(&c[..])];
        assert_eq!(intersect(&lists).collect::<Vec<_>>(), [3, 9, 51, 99]);
        let empty = [Cow::Borrowed(&[][..]), Cow::Borrowed(&c[..])];
        assert_eq!(intersect(&empty).count(), 0);
        assert_eq!(intersect(&[]).count(), 0);
        for (ids, target, below) in [
            (&[1, 2, 3][..], 0, 0),
            (&[1, 2, 3][..], 4, 3),
            (&[1, 2, 3, 5, 8, 13, 21][..], 13, 5),
            (&[1, 2, 3, 5, 8, 13, 21][..], 14, 6),
        ] {
            assert_eq!(gallop(ids, target), below);
        }
    }
}
