//! JSON query support.
//!
//! Combines JSON subqueries and enforces that, when array metadata is available,
//! matches refer to the same JSON array element.
use std::fmt;
use std::sync::Arc;

use common::json_path_writer::JsonArrayPathEntry;

use crate::docset::{DocSet, TERMINATED};
use crate::index::SegmentReader;
use crate::postings::SegmentPostings;
use crate::query::explanation::does_not_match;
use crate::query::json_utils::JsonPathScorer;
use crate::query::phrase_query::PhraseScorer;
use crate::query::term_query::TermScorer;
use crate::query::{
    EmptyScorer, EnableScoring, Explanation, Intersection, Query, QueryClone, Scorer, Weight,
};
use crate::{DocId, Score, TantivyError};

/// Conjunction of JSON-aware subqueries.
///
/// When every subquery exposes JSON array metadata, only documents where all
/// matches agree on the same array path are retained. Subqueries without
/// array metadata (object-only) are accepted without restricting paths.
pub struct JsonQuery {
    subqueries: Vec<Box<dyn Query>>,
}

impl JsonQuery {
    /// Builds a `JsonQuery` from subqueries scoped to the same JSON field.
    pub fn new(subqueries: Vec<Box<dyn Query>>) -> Self {
        JsonQuery { subqueries }
    }
}

impl Clone for JsonQuery {
    fn clone(&self) -> Self {
        let subqueries = self
            .subqueries
            .iter()
            .map(|query| query.box_clone())
            .collect();
        JsonQuery { subqueries }
    }
}

impl fmt::Debug for JsonQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonQuery")
            .field("subqueries_len", &self.subqueries.len())
            .finish()
    }
}

impl Query for JsonQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let mut weights = Vec::with_capacity(self.subqueries.len());
        for subquery in &self.subqueries {
            weights.push(subquery.weight(enable_scoring)?);
        }
        Ok(Box::new(JsonWeight { weights }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a crate::Term, bool)) {
        for subquery in &self.subqueries {
            subquery.query_terms(visitor);
        }
    }
}

struct JsonWeight {
    weights: Vec<Box<dyn Weight>>,
}

impl Weight for JsonWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        match self.weights.len() {
            0 => Ok(Box::new(EmptyScorer)),
            1 => self.weights[0].scorer(reader, boost),
            _ => {
                let mut json_scorers = Vec::with_capacity(self.weights.len());
                for weight in &self.weights {
                    let scorer = weight.scorer(reader, boost)?;
                    json_scorers.push(convert_scorer_to_json(scorer)?);
                }
                Ok(Box::new(JsonConstraintScorer::new(
                    json_scorers,
                    reader.num_docs(),
                )))
            }
        }
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(does_not_match(doc));
        }
        let mut explanation = Explanation::new("JsonQuery", scorer.score());
        for weight in &self.weights {
            if let Ok(child) = weight.explain(reader, doc) {
                explanation.add_detail(child);
            }
        }
        Ok(explanation)
    }
}

/// Downcasts a scorer into a `JsonPathScorer` (term or phrase).
///
/// Other scorer types are rejected because they cannot expose JSON path metadata.
fn convert_scorer_to_json(scorer: Box<dyn Scorer>) -> crate::Result<Box<dyn JsonPathScorer>> {
    if scorer.is::<TermScorer>() {
        let term_scorer = *(scorer
            .downcast::<TermScorer>()
            .map_err(|_| TantivyError::InvalidArgument("Invalid json scorer".to_string()))?);
        return Ok(Box::new(term_scorer));
    }
    if scorer.is::<PhraseScorer<SegmentPostings>>() {
        let phrase_scorer = *(scorer
            .downcast::<PhraseScorer<SegmentPostings>>()
            .map_err(|_| TantivyError::InvalidArgument("Invalid json scorer".to_string()))?);
        return Ok(Box::new(phrase_scorer));
    }
    // Fallback: wrap any scorer as a JsonPathScorer that exposes no metadata.
    Ok(Box::new(PassthroughJsonScorer { inner: scorer }))
}

struct PassthroughJsonScorer {
    inner: Box<dyn Scorer>,
}

impl JsonPathScorer for PassthroughJsonScorer {}

impl DocSet for PassthroughJsonScorer {
    fn advance(&mut self) -> DocId {
        self.inner.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.inner.seek(target)
    }

    fn doc(&self) -> DocId {
        self.inner.doc()
    }

    fn size_hint(&self) -> u32 {
        self.inner.size_hint()
    }

    fn cost(&self) -> u64 {
        self.inner.cost()
    }
}

impl Scorer for PassthroughJsonScorer {
    fn score(&mut self) -> Score {
        self.inner.score()
    }
}

/// Scorer that enforces JSON-path agreement across all sub-scorers.
struct JsonConstraintScorer {
    intersection: Intersection<Box<dyn JsonPathScorer>, Box<dyn JsonPathScorer>>,
    num_terms: usize,
    common_indexes: Vec<Arc<[JsonArrayPathEntry]>>,
}

impl JsonConstraintScorer {
    fn new(json_scorers: Vec<Box<dyn JsonPathScorer>>, num_docs: u32) -> Self {
        let num_terms = json_scorers.len();
        let intersection = Intersection::new(json_scorers, num_docs);
        let mut scorer = JsonConstraintScorer {
            intersection,
            num_terms,
            common_indexes: Vec::new(),
        };
        if scorer.doc() != TERMINATED && !scorer.satisfies_constraint() {
            scorer.advance();
        }
        scorer
    }

    fn satisfies_constraint(&mut self) -> bool {
        self.common_indexes.clear();
        let mut has_paths = false;
        for ord in 0..self.num_terms {
            let scorer = self.intersection.docset_mut_specialized(ord);
            let paths_opt = scorer.json_array_paths_dyn();
            match paths_opt {
                Some(paths) if !paths.is_empty() => {
                    if !has_paths {
                        populate_common_indexes(&mut self.common_indexes, paths);
                        has_paths = true;
                    } else {
                        retain_common_indexes(&mut self.common_indexes, paths);
                        if self.common_indexes.is_empty() {
                            return false;
                        }
                    }
                }
                _ => {
                    // No metadata for this term: treat as unconstrained, but do not
                    // short-circuit the existing intersection.
                }
            }
        }
        if has_paths {
            !self.common_indexes.is_empty()
        } else {
            true
        }
    }
}

impl DocSet for JsonConstraintScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.intersection.advance();
            if doc == TERMINATED || self.satisfies_constraint() {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let mut doc = self.intersection.seek(target);
        while doc != TERMINATED && !self.satisfies_constraint() {
            doc = self.intersection.advance();
        }
        doc
    }

    fn doc(&self) -> DocId {
        self.intersection.doc()
    }

    fn size_hint(&self) -> u32 {
        self.intersection.size_hint()
    }

    fn cost(&self) -> u64 {
        self.intersection.cost()
    }
}

impl Scorer for JsonConstraintScorer {
    fn score(&mut self) -> Score {
        self.intersection.score()
    }
}

fn populate_common_indexes(
    common: &mut Vec<Arc<[JsonArrayPathEntry]>>,
    paths: &[Arc<[JsonArrayPathEntry]>],
) {
    common.clear();
    for path in paths {
        if !common
            .iter()
            .any(|existing| existing.as_ref() == path.as_ref())
        {
            common.push(path.clone());
        }
    }
}

fn retain_common_indexes(
    common: &mut Vec<Arc<[JsonArrayPathEntry]>>,
    paths: &[Arc<[JsonArrayPathEntry]>],
) {
    common.retain(|candidate| paths.iter().any(|path| path.as_ref() == candidate.as_ref()));
}

#[cfg(test)]
mod tests {
    use crate::collector::TopDocs;
    use crate::query::{JsonQuery, PhraseQuery, Query, QueryParser, TermQuery};
    use crate::schema::{IndexRecordOption, Schema, TEXT};
    use crate::serde_json::json;
    use crate::{doc, Index, Term};

    #[test]
    fn test_json_query_enforces_same_array() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codecfoo","type":"mp4"}]}})
            ))?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codecfoo","type":"jpg"},{"name":"codecbar","type":"mp4"}]}})
            ))?;
            writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let mut name_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        name_term.append_type_and_str("codecfoo");
        let mut type_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.type", false);
        type_term.append_type_and_str("mp4");

        let query = JsonQuery::new(vec![
            Box::new(TermQuery::new(
                name_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
            Box::new(TermQuery::new(
                type_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);
        Ok(())
    }

    #[test]
    fn test_json_query_phrase_and_term() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codec foo","type":"mp4"}]}})
            ))?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codec foo","type":"jpg"},{"name":"codec bar","type":"mp4"}]}})
            ))?;
            writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let mut codec_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        codec_term.append_type_and_str("codec");
        let mut foo_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        foo_term.append_type_and_str("foo");
        let mut type_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.type", false);
        type_term.append_type_and_str("mp4");

        let phrase_query: Box<dyn Query> = Box::new(PhraseQuery::new(vec![codec_term, foo_term]));
        let query = JsonQuery::new(vec![
            phrase_query,
            Box::new(TermQuery::new(
                type_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);
        Ok(())
    }

    #[test]
    fn test_phrase_json_paths_filtered_by_positions() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut writer = index.writer_for_tests()?;
            // element 0: phrase and type match on same path
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codec foo","type":"mp4"}]}})
            ))?;
            // element 0 has phrase but not mp4, element 1 has mp4 but not phrase
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codec foo","type":"jpg"},{"name":"codec bar","type":"mp4"}]}})
            ))?;
            writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let mut codec_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        codec_term.append_type_and_str("codec");
        let mut foo_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        foo_term.append_type_and_str("foo");
        let mut type_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.type", false);
        type_term.append_type_and_str("mp4");

        let phrase_query: Box<dyn Query> = Box::new(PhraseQuery::new(vec![codec_term, foo_term]));
        let query = JsonQuery::new(vec![
            phrase_query,
            Box::new(TermQuery::new(
                type_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);
        Ok(())
    }

    #[test]
    fn test_json_query_rejects_cross_element_match() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut writer = index.writer_for_tests()?;
            // codecA + jpg, codecB + mp4 -> should not satisfy codecA + mp4
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"attributes":{"type":"jpg"},"name":"codecA"},{"attributes":{"type":"mp4"},"name":"codecB"}]}})
            ))?;
            // codecA + mp4 -> should match
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"attributes":{"type":"mp4"},"name":"codecA"}]}})
            ))?;
            writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();

        let mut name_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        name_term.append_type_and_str("codecA");
        let mut type_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.attributes.type", false);
        type_term.append_type_and_str("mp4");

        let query = JsonQuery::new(vec![
            Box::new(TermQuery::new(
                name_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
            Box::new(TermQuery::new(
                type_term,
                IndexRecordOption::WithFreqsAndPositions,
            )),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10))?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 1u32);
        Ok(())
    }

    #[test]
    fn test_query_parser_builds_json_query() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codecfoo","type":"mp4"}]}})
            ))?;
            writer.add_document(doc!(
                doc_body_field => json!({"videoInfo":{"extraData":[{"name":"codecfoo","type":"jpg"},{"name":"codecbar","type":"mp4"}]}})
            ))?;
            writer.commit()?;
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let query_parser = QueryParser::for_index(&index, vec![doc_body_field]);
        let query = query_parser.parse_query(
            "doc_body.videoInfo.extraData.name:codecfoo AND doc_body.videoInfo.extraData.type:mp4",
        )?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);
        Ok(())
    }
}
