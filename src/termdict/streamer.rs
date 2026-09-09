use std::io;

use tantivy_fst::automaton::AlwaysMatch;
use tantivy_fst::Automaton;

#[cfg(feature = "quickwit")]
use super::sstable_termdict;
use super::{fst_termdict, TermOrdinal};
use crate::postings::TermInfo;

enum TermStreamerBuilderInner<'a, A>
where
    A: Automaton,
    A::State: Clone,
{
    Fst(fst_termdict::TermStreamerBuilder<'a, A>),
    #[cfg(feature = "quickwit")]
    SSTable(sstable_termdict::TermStreamerBuilder<'a, A>),
}

/// A builder for a term stream backed by either an FST or an SSTable dictionary.
pub struct TermStreamerBuilder<'a, A = AlwaysMatch>
where
    A: Automaton,
    A::State: Clone,
{
    inner: TermStreamerBuilderInner<'a, A>,
}

impl<'a, A> TermStreamerBuilder<'a, A>
where
    A: Automaton,
    A::State: Clone,
{
    pub(super) fn from_fst(builder: fst_termdict::TermStreamerBuilder<'a, A>) -> Self {
        Self {
            inner: TermStreamerBuilderInner::Fst(builder),
        }
    }

    #[cfg(feature = "quickwit")]
    pub(super) fn from_sstable(builder: sstable_termdict::TermStreamerBuilder<'a, A>) -> Self {
        Self {
            inner: TermStreamerBuilderInner::SSTable(builder),
        }
    }

    /// Limits the range to terms greater than or equal to `bound`.
    pub fn ge<T: AsRef<[u8]>>(self, bound: T) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                TermStreamerBuilderInner::Fst(builder.ge(bound))
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                TermStreamerBuilderInner::SSTable(builder.ge(bound))
            }
        };
        Self { inner }
    }

    /// Limits the range to terms strictly greater than `bound`.
    pub fn gt<T: AsRef<[u8]>>(self, bound: T) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                TermStreamerBuilderInner::Fst(builder.gt(bound))
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                TermStreamerBuilderInner::SSTable(builder.gt(bound))
            }
        };
        Self { inner }
    }

    /// Limits the range to terms less than or equal to `bound`.
    pub fn le<T: AsRef<[u8]>>(self, bound: T) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                TermStreamerBuilderInner::Fst(builder.le(bound))
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                TermStreamerBuilderInner::SSTable(builder.le(bound))
            }
        };
        Self { inner }
    }

    /// Limits the range to terms strictly less than `bound`.
    pub fn lt<T: AsRef<[u8]>>(self, bound: T) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                TermStreamerBuilderInner::Fst(builder.lt(bound))
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                TermStreamerBuilderInner::SSTable(builder.lt(bound))
            }
        };
        Self { inner }
    }

    /// Limits how many dictionary entries are loaded as an optimization.
    ///
    /// FST dictionaries are fully loaded when opened, so this is a no-op for FST-backed streams.
    pub fn limit(self, limit: u64) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                let _ = limit;
                TermStreamerBuilderInner::Fst(builder)
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                TermStreamerBuilderInner::SSTable(builder.limit(limit))
            }
        };
        Self { inner }
    }

    /// Iterates over the range backwards.
    #[cfg(not(feature = "quickwit"))]
    pub fn backward(self) -> Self {
        let inner = match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                TermStreamerBuilderInner::Fst(builder.backward())
            }
        };
        Self { inner }
    }

    /// Creates the stream corresponding to this builder.
    pub fn into_stream(self) -> io::Result<TermStreamer<'a, A>> {
        match self.inner {
            TermStreamerBuilderInner::Fst(builder) => {
                builder.into_stream().map(TermStreamer::from_fst)
            }
            #[cfg(feature = "quickwit")]
            TermStreamerBuilderInner::SSTable(builder) => {
                builder.into_stream().map(TermStreamer::from_sstable)
            }
        }
    }

    #[cfg(feature = "quickwit")]
    /// Creates the stream asynchronously.
    pub async fn into_stream_async(self) -> io::Result<TermStreamer<'a, A>> {
        self.into_stream_async_merging_holes(0).await
    }

    #[cfg(feature = "quickwit")]
    /// Creates the stream asynchronously, merging nearby SSTable reads when possible.
    pub async fn into_stream_async_merging_holes(
        self,
        merge_holes_under_bytes: usize,
    ) -> io::Result<TermStreamer<'a, A>> {
        match self.inner {
            // FST dictionaries are fully loaded when opened.
            TermStreamerBuilderInner::Fst(builder) => {
                builder.into_stream().map(TermStreamer::from_fst)
            }
            TermStreamerBuilderInner::SSTable(builder) => builder
                .into_stream_async_merging_holes(merge_holes_under_bytes)
                .await
                .map(TermStreamer::from_sstable),
        }
    }
}

enum TermStreamerInner<'a, A>
where
    A: Automaton,
    A::State: Clone,
{
    Fst(fst_termdict::TermStreamer<'a, A>),
    #[cfg(feature = "quickwit")]
    SSTable(sstable_termdict::TermStreamer<'a, A>),
}

/// A cursor over the sorted terms of either an FST or an SSTable dictionary.
pub struct TermStreamer<'a, A = AlwaysMatch>
where
    A: Automaton,
    A::State: Clone,
{
    inner: TermStreamerInner<'a, A>,
}

impl<'a, A> TermStreamer<'a, A>
where
    A: Automaton,
    A::State: Clone,
{
    fn from_fst(streamer: fst_termdict::TermStreamer<'a, A>) -> Self {
        Self {
            inner: TermStreamerInner::Fst(streamer),
        }
    }

    #[cfg(feature = "quickwit")]
    fn from_sstable(streamer: sstable_termdict::TermStreamer<'a, A>) -> Self {
        Self {
            inner: TermStreamerInner::SSTable(streamer),
        }
    }

    /// Advances to the next term.
    pub fn advance(&mut self) -> bool {
        match &mut self.inner {
            TermStreamerInner::Fst(streamer) => streamer.advance(),
            #[cfg(feature = "quickwit")]
            TermStreamerInner::SSTable(streamer) => streamer.advance(),
        }
    }

    /// Returns the ordinal of the current term.
    pub fn term_ord(&self) -> TermOrdinal {
        match &self.inner {
            TermStreamerInner::Fst(streamer) => streamer.term_ord(),
            #[cfg(feature = "quickwit")]
            TermStreamerInner::SSTable(streamer) => streamer.term_ord(),
        }
    }

    /// Returns the current term.
    pub fn key(&self) -> &[u8] {
        match &self.inner {
            TermStreamerInner::Fst(streamer) => streamer.key(),
            #[cfg(feature = "quickwit")]
            TermStreamerInner::SSTable(streamer) => streamer.key(),
        }
    }

    /// Returns the [`TermInfo`] associated with the current term.
    pub fn value(&self) -> &TermInfo {
        match &self.inner {
            TermStreamerInner::Fst(streamer) => streamer.value(),
            #[cfg(feature = "quickwit")]
            TermStreamerInner::SSTable(streamer) => streamer.value(),
        }
    }

    /// Returns the next `(term, TermInfo)` pair.
    #[expect(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<(&[u8], &TermInfo)> {
        if self.advance() {
            Some((self.key(), self.value()))
        } else {
            None
        }
    }
}
