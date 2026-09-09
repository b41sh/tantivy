//! The term dictionary main role is to associate the sorted [`Term`s](crate::Term) to
//! a [`TermInfo`] struct that contains some meta-information
//! about the term.
//!
//! Internally, the term dictionary relies on the `fst` crate to store a sorted mapping that
//! associates each term to its rank in lexicographical order, or on an SSTable when the
//! `quickwit` feature is enabled.

#[allow(dead_code, unused_imports)]
mod fst_termdict;
mod merger;
#[cfg(feature = "quickwit")]
mod sstable_termdict;
mod streamer;

#[cfg(test)]
mod tests;

/// Position of the term in the sorted list of terms.
pub type TermOrdinal = u64;

use std::io;

use common::file_slice::FileSlice;
use common::BinarySerializable;
use tantivy_fst::Automaton;

#[cfg(not(feature = "quickwit"))]
use self::fst_termdict::TermDictionaryBuilder as InnerTermDictBuilder;
pub use self::fst_termdict::TermInfoStore;
pub use self::merger::TermMerger;
#[cfg(feature = "quickwit")]
use self::sstable_termdict::TermDictionaryBuilder as InnerTermDictBuilder;
pub use self::streamer::{TermStreamer, TermStreamerBuilder};
use crate::postings::TermInfo;

#[derive(Debug, Eq, PartialEq)]
#[repr(u32)]
enum DictionaryType {
    Fst = 1,
    SSTable = 2,
}

impl TryFrom<u32> for DictionaryType {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(DictionaryType::Fst),
            2 => Ok(DictionaryType::SSTable),
            _ => Err("Invalid value for DictionaryType"),
        }
    }
}

#[cfg(not(feature = "quickwit"))]
const CURRENT_WRITE_TYPE: DictionaryType = DictionaryType::Fst;

#[cfg(feature = "quickwit")]
const CURRENT_WRITE_TYPE: DictionaryType = DictionaryType::SSTable;

#[derive(Clone)]
enum TermDictionaryInner {
    Fst(fst_termdict::TermDictionary),
    #[cfg(feature = "quickwit")]
    SSTable(sstable_termdict::TermDictionary),
}

/// A term dictionary backed by either the historical FST format or the SSTable format.
///
/// With the `quickwit` feature enabled, both formats can be read while newly built term
/// dictionaries use the SSTable format.
#[derive(Clone)]
pub struct TermDictionary(TermDictionaryInner);

impl TermDictionary {
    /// Opens a `TermDictionary` and selects its implementation from its on-disk type tag.
    pub fn open(file: FileSlice) -> io::Result<Self> {
        let (main_slice, dict_type_slice) = file.split_from_end(4);
        let mut dict_type_bytes = dict_type_slice.read_bytes()?;
        let dict_type_value = u32::deserialize(&mut dict_type_bytes)?;
        let dict_type = DictionaryType::try_from(dict_type_value).map_err(|_| {
            io::Error::other(format!(
                "Unsupported dictionary type, found {dict_type_value}"
            ))
        })?;

        match dict_type {
            DictionaryType::Fst => fst_termdict::TermDictionary::open(main_slice)
                .map(TermDictionaryInner::Fst)
                .map(TermDictionary),
            #[cfg(feature = "quickwit")]
            DictionaryType::SSTable => sstable_termdict::TermDictionary::open(main_slice)
                .map(TermDictionaryInner::SSTable)
                .map(TermDictionary),
            #[cfg(not(feature = "quickwit"))]
            DictionaryType::SSTable => Err(io::Error::other(
                "SSTable term dictionaries require the quickwit feature",
            )),
        }
    }

    /// Creates an empty term dictionary in the current write format.
    pub fn empty() -> Self {
        #[cfg(not(feature = "quickwit"))]
        {
            TermDictionary(TermDictionaryInner::Fst(
                fst_termdict::TermDictionary::empty(),
            ))
        }
        #[cfg(feature = "quickwit")]
        {
            TermDictionary(TermDictionaryInner::SSTable(
                sstable_termdict::TermDictionary::empty(),
            ))
        }
    }

    /// Returns the number of terms in the dictionary.
    /// Term ordinals range from 0 to `num_terms() - 1`.
    pub fn num_terms(&self) -> usize {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => dictionary.num_terms(),
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => dictionary.num_terms(),
        }
    }

    /// Returns the ordinal associated with a given term.
    pub fn term_ord<K: AsRef<[u8]>>(&self, key: K) -> io::Result<Option<TermOrdinal>> {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => dictionary.term_ord(key),
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => dictionary.term_ord(key),
        }
    }

    /// Stores the term associated with a given term ordinal in `bytes`.
    pub fn ord_to_term(&self, ord: TermOrdinal, bytes: &mut Vec<u8>) -> io::Result<bool> {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => dictionary.ord_to_term(ord, bytes),
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => dictionary.ord_to_term(ord, bytes),
        }
    }

    /// Looks up the value corresponding to the key.
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> io::Result<Option<TermInfo>> {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => dictionary.get(key),
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => dictionary.get(key),
        }
    }

    /// Returns a range builder used to stream terms within an interval.
    pub fn range(&self) -> TermStreamerBuilder<'_> {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => {
                TermStreamerBuilder::from_fst(dictionary.range())
            }
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => {
                TermStreamerBuilder::from_sstable(dictionary.range())
            }
        }
    }

    /// Returns a stream of all sorted terms.
    pub fn stream(&self) -> io::Result<TermStreamer<'_>> {
        self.range().into_stream()
    }

    /// Returns a search builder used to stream terms accepted by an automaton.
    pub fn search<'a, A: Automaton + 'a>(&'a self, automaton: A) -> TermStreamerBuilder<'a, A>
    where A::State: Clone {
        match &self.0 {
            TermDictionaryInner::Fst(dictionary) => {
                TermStreamerBuilder::from_fst(dictionary.search(automaton))
            }
            #[cfg(feature = "quickwit")]
            TermDictionaryInner::SSTable(dictionary) => {
                TermStreamerBuilder::from_sstable(dictionary.search(automaton))
            }
        }
    }

    #[cfg(feature = "quickwit")]
    /// Looks up the value corresponding to the key asynchronously.
    pub async fn get_async<K: AsRef<[u8]>>(&self, key: K) -> io::Result<Option<TermInfo>> {
        match &self.0 {
            // The FST and its TermInfoStore are fully loaded when the dictionary is opened.
            TermDictionaryInner::Fst(dictionary) => dictionary.get(key),
            TermDictionaryInner::SSTable(dictionary) => dictionary.get_async(key).await,
        }
    }

    #[cfg(feature = "quickwit")]
    #[doc(hidden)]
    pub async fn warm_up_dictionary(&self) -> io::Result<()> {
        match &self.0 {
            // Opening an FST dictionary already loads the complete dictionary.
            TermDictionaryInner::Fst(_) => Ok(()),
            TermDictionaryInner::SSTable(dictionary) => dictionary.warm_up_dictionary().await,
        }
    }

    #[cfg(feature = "quickwit")]
    /// Returns a file slice covering SSTable blocks that include the requested key range.
    ///
    /// FST dictionaries are fully loaded at open time and therefore return an empty slice.
    pub fn file_slice_for_range(
        &self,
        key_range: impl std::ops::RangeBounds<[u8]>,
        limit: Option<u64>,
    ) -> FileSlice {
        match &self.0 {
            TermDictionaryInner::Fst(_) => FileSlice::empty(),
            TermDictionaryInner::SSTable(dictionary) => {
                dictionary.file_slice_for_range(key_range, limit)
            }
        }
    }
}

/// A term dictionary builder using the current write format.
///
/// When the `quickwit` feature is enabled this always writes SSTable dictionaries. Otherwise it
/// writes FST dictionaries. Reading is selected independently by [`TermDictionary::open`].
pub struct TermDictionaryBuilder<W: io::Write>(InnerTermDictBuilder<W>);

impl<W: io::Write> TermDictionaryBuilder<W> {
    /// Creates a new `TermDictionaryBuilder`.
    pub fn create(w: W) -> io::Result<Self> {
        InnerTermDictBuilder::create(w).map(TermDictionaryBuilder)
    }

    /// Inserts a `(key, value)` pair in the term dictionary.
    ///
    /// Keys have to be inserted in order.
    pub fn insert<K: AsRef<[u8]>>(&mut self, key_ref: K, value: &TermInfo) -> io::Result<()> {
        self.0.insert(key_ref, value)
    }

    /// Inserts a key. Calls to this method and [`Self::insert_value`] must alternate.
    pub fn insert_key(&mut self, key: &[u8]) -> io::Result<()> {
        self.0.insert_key(key)
    }

    /// Inserts a value. Calls to [`Self::insert_key`] and this method must alternate.
    pub fn insert_value(&mut self, term_info: &TermInfo) -> io::Result<()> {
        self.0.insert_value(term_info)
    }

    /// Finalizes the dictionary and appends its on-disk type tag.
    pub fn finish(self) -> io::Result<W> {
        let mut writer = self.0.finish()?;
        (CURRENT_WRITE_TYPE as u32).serialize(&mut writer)?;
        Ok(writer)
    }
}
