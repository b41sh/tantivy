use common::json_path_writer::JsonArrayPathEntry;

use crate::query::Scorer;

/// Trait for scorers that can expose JSON array path metadata for their current doc.
pub trait JsonPathScorer: Scorer {
    fn json_array_paths_dyn(&mut self) -> Option<&[Vec<JsonArrayPathEntry>]>;
}
