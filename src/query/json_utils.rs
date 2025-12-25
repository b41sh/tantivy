use std::sync::Arc;

use common::json_path_writer::JsonArrayPathEntry;

use crate::query::Scorer;

/// Trait for scorers that can expose JSON array path metadata for their current doc.
///
/// This is used by JSON queries to make sure multiple term/phrase matches
/// refer to the same array element within a JSON field.
pub trait JsonPathScorer: Scorer {
    fn json_array_paths_dyn(&mut self) -> Option<&[Arc<[JsonArrayPathEntry]>]> {
        None
    }
}
