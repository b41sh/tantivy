use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::postings::TermInfo;
use crate::termdict::TermStreamer;

struct HeapItem<'a> {
    streamer: TermStreamer<'a>,
    segment_ord: usize,
}

impl PartialEq for HeapItem<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.segment_ord == other.segment_ord
    }
}

impl Eq for HeapItem<'_> {}

impl PartialOrd for HeapItem<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        (&other.streamer.key(), &other.segment_ord).cmp(&(&self.streamer.key(), &self.segment_ord))
    }
}

/// Merges sorted term streams from FST or SSTable dictionaries into sorted unique terms.
pub struct TermMerger<'a> {
    heap: BinaryHeap<HeapItem<'a>>,
    current_streamers: Vec<HeapItem<'a>>,
}

impl<'a> TermMerger<'a> {
    /// Creates a merger over streams ordered by their segment ordinal.
    pub fn new(streams: Vec<TermStreamer<'a>>) -> TermMerger<'a> {
        TermMerger {
            heap: BinaryHeap::new(),
            current_streamers: streams
                .into_iter()
                .enumerate()
                .map(|(segment_ord, streamer)| HeapItem {
                    streamer,
                    segment_ord,
                })
                .collect(),
        }
    }

    fn advance_segments(&mut self) {
        for mut heap_item in self.current_streamers.drain(..) {
            if heap_item.streamer.advance() {
                self.heap.push(heap_item);
            }
        }
    }

    /// Advances to the next unique term.
    pub fn advance(&mut self) -> bool {
        self.advance_segments();
        let Some(head) = self.heap.pop() else {
            return false;
        };
        self.current_streamers.push(head);
        while let Some(next_streamer) = self.heap.peek() {
            if self.current_streamers[0].streamer.key() != next_streamer.streamer.key() {
                break;
            }
            let next_streamer = self.heap.pop().expect("the heap was just peeked");
            self.current_streamers.push(next_streamer);
        }
        true
    }

    /// Returns the current term.
    pub fn key(&self) -> &[u8] {
        self.current_streamers[0].streamer.key()
    }

    /// Returns `(segment ordinal, TermInfo)` for every segment containing the current term.
    pub fn current_segment_ords_and_term_infos<'b: 'a>(
        &'b self,
    ) -> impl 'b + Iterator<Item = (usize, TermInfo)> {
        self.current_streamers
            .iter()
            .map(|heap_item| (heap_item.segment_ord, heap_item.streamer.value().clone()))
    }
}
