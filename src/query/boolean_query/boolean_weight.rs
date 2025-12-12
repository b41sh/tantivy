use std::collections::HashMap;

use rustc_hash::FxHashMap;

use crate::docset::{DocSet, COLLECT_BLOCK_BUFFER_LEN, TERMINATED};
use crate::index::SegmentReader;
use crate::postings::{FreqReadingOption, SegmentPostings};
use crate::query::disjunction::Disjunction;
use crate::query::explanation::does_not_match;
use crate::query::score_combiner::{DoNothingCombiner, ScoreCombiner};
use crate::query::JsonPathScorer;
use crate::query::phrase_query::PhraseScorer;
use crate::query::term_query::{JsonConstraintKey, TermScorer};
use crate::query::weight::{for_each_docset_buffered, for_each_pruning_scorer, for_each_scorer};
use crate::query::{
    intersect_scorers, BufferedUnionScorer, EmptyScorer, Exclude, Explanation, Intersection, Occur,
    RequiredOptionalScorer, Scorer, Weight,
};
use crate::{DocId, Score};

enum SpecializedScorer {
    TermUnion(Vec<TermScorer>),
    Other(Box<dyn Scorer>),
}

fn enforce_json_constraints(
    scorers: Vec<Box<dyn Scorer>>,
    num_docs: u32,
) -> Vec<Box<dyn Scorer>> {
    let mut grouped: FxHashMap<JsonConstraintKey, Vec<Box<dyn JsonPathScorer>>> =
        FxHashMap::default();
    let mut others = Vec::with_capacity(scorers.len());
    for scorer in scorers.into_iter() {
        match try_into_json_path_scorer(scorer) {
            Ok((key, json_scorer)) => {
                grouped.entry(key).or_default().push(json_scorer);
            }
            Err(original) => others.push(original),
        }
    }
    for (key, mut group) in grouped {
        if group.len() <= 1 {
            if let Some(single) = group.pop() {
                let scorer: Box<dyn Scorer> = single;
                others.push(scorer);
            }
        } else {
            others.push(build_json_constraint_scorer(group, num_docs));
        }
    }
    others
}

fn build_json_constraint_scorer(
    json_scorers: Vec<Box<dyn JsonPathScorer>>,
    num_docs: u32,
) -> Box<dyn Scorer> {
    Box::new(JsonConstraintScorer::new(json_scorers, num_docs))
}

fn try_into_json_path_scorer(
    scorer: Box<dyn Scorer>,
) -> Result<(JsonConstraintKey, Box<dyn JsonPathScorer>), Box<dyn Scorer>> {
    if scorer.is::<TermScorer>() {
        let term_scorer = *(scorer.downcast::<TermScorer>().map_err(|_| ()).unwrap());
        if let Some(key) = term_scorer.json_constraint_key() {
            let json_scorer: Box<dyn JsonPathScorer> = Box::new(term_scorer);
            return Ok((key, json_scorer));
        } else {
            let original: Box<dyn Scorer> = Box::new(term_scorer);
            return Err(original);
        }
    }
    if scorer.is::<PhraseScorer<SegmentPostings>>() {
        let phrase_scorer =
            *(scorer.downcast::<PhraseScorer<SegmentPostings>>().map_err(|_| ()).unwrap());
        if let Some(key) = phrase_scorer.json_constraint_key() {
            let json_scorer: Box<dyn JsonPathScorer> = Box::new(phrase_scorer);
            return Ok((key, json_scorer));
        } else {
            let original: Box<dyn Scorer> = Box::new(phrase_scorer);
            return Err(original);
        }
    }
    Err(scorer)
}

fn scorer_disjunction<TScoreCombiner>(
    scorers: Vec<Box<dyn Scorer>>,
    score_combiner: TScoreCombiner,
    minimum_match_required: usize,
) -> Box<dyn Scorer>
where
    TScoreCombiner: ScoreCombiner,
{
    debug_assert!(!scorers.is_empty());
    debug_assert!(minimum_match_required > 1);
    if scorers.len() == 1 {
        return scorers.into_iter().next().unwrap(); // Safe unwrap.
    }
    Box::new(Disjunction::new(
        scorers,
        score_combiner,
        minimum_match_required,
    ))
}

/// num_docs is the number of documents in the segment.
fn scorer_union<TScoreCombiner>(
    scorers: Vec<Box<dyn Scorer>>,
    score_combiner_fn: impl Fn() -> TScoreCombiner,
    num_docs: u32,
) -> SpecializedScorer
where
    TScoreCombiner: ScoreCombiner,
{
    assert!(!scorers.is_empty());
    if scorers.len() == 1 {
        return SpecializedScorer::Other(scorers.into_iter().next().unwrap()); //< we checked the size beforehand
    }

    {
        let is_all_term_queries = scorers.iter().all(|scorer| scorer.is::<TermScorer>());
        if is_all_term_queries {
            let scorers: Vec<TermScorer> = scorers
                .into_iter()
                .map(|scorer| *(scorer.downcast::<TermScorer>().map_err(|_| ()).unwrap()))
                .collect();
            if scorers
                .iter()
                .all(|scorer| scorer.freq_reading_option() == FreqReadingOption::ReadFreq)
            {
                // Block wand is only available if we read frequencies.
                return SpecializedScorer::TermUnion(scorers);
            } else {
                return SpecializedScorer::Other(Box::new(BufferedUnionScorer::build(
                    scorers,
                    score_combiner_fn,
                    num_docs,
                )));
            }
        }
    }
    SpecializedScorer::Other(Box::new(BufferedUnionScorer::build(
        scorers,
        score_combiner_fn,
        num_docs,
    )))
}

fn into_box_scorer<TScoreCombiner: ScoreCombiner>(
    scorer: SpecializedScorer,
    score_combiner_fn: impl Fn() -> TScoreCombiner,
    num_docs: u32,
) -> Box<dyn Scorer> {
    match scorer {
        SpecializedScorer::TermUnion(term_scorers) => {
            let union_scorer =
                BufferedUnionScorer::build(term_scorers, score_combiner_fn, num_docs);
            Box::new(union_scorer)
        }
        SpecializedScorer::Other(scorer) => scorer,
    }
}

/// Weight associated to the `BoolQuery`.
pub struct BooleanWeight<TScoreCombiner: ScoreCombiner> {
    weights: Vec<(Occur, Box<dyn Weight>)>,
    minimum_number_should_match: usize,
    scoring_enabled: bool,
    score_combiner_fn: Box<dyn Fn() -> TScoreCombiner + Sync + Send>,
}

impl<TScoreCombiner: ScoreCombiner> BooleanWeight<TScoreCombiner> {
    /// Creates a new boolean weight.
    pub fn new(
        weights: Vec<(Occur, Box<dyn Weight>)>,
        scoring_enabled: bool,
        score_combiner_fn: Box<dyn Fn() -> TScoreCombiner + Sync + Send + 'static>,
    ) -> BooleanWeight<TScoreCombiner> {
        BooleanWeight {
            weights,
            scoring_enabled,
            score_combiner_fn,
            minimum_number_should_match: 1,
        }
    }

    /// Create a new boolean weight with minimum number of required should clauses specified.
    pub fn with_minimum_number_should_match(
        weights: Vec<(Occur, Box<dyn Weight>)>,
        minimum_number_should_match: usize,
        scoring_enabled: bool,
        score_combiner_fn: Box<dyn Fn() -> TScoreCombiner + Sync + Send + 'static>,
    ) -> BooleanWeight<TScoreCombiner> {
        BooleanWeight {
            weights,
            minimum_number_should_match,
            scoring_enabled,
            score_combiner_fn,
        }
    }

    fn per_occur_scorers(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> crate::Result<HashMap<Occur, Vec<Box<dyn Scorer>>>> {
        let mut per_occur_scorers: HashMap<Occur, Vec<Box<dyn Scorer>>> = HashMap::new();
        for (occur, subweight) in &self.weights {
            let sub_scorer: Box<dyn Scorer> = subweight.scorer(reader, boost)?;
            per_occur_scorers
                .entry(*occur)
                .or_default()
                .push(sub_scorer);
        }
        Ok(per_occur_scorers)
    }

    fn complex_scorer<TComplexScoreCombiner: ScoreCombiner>(
        &self,
        reader: &SegmentReader,
        boost: Score,
        score_combiner_fn: impl Fn() -> TComplexScoreCombiner,
    ) -> crate::Result<SpecializedScorer> {
        let num_docs = reader.num_docs();
        let mut per_occur_scorers = self.per_occur_scorers(reader, boost)?;
        if let Some(must_scorers) = per_occur_scorers.get_mut(&Occur::Must) {
            let existing = std::mem::take(must_scorers);
            *must_scorers = enforce_json_constraints(existing, num_docs);
        }
        // Indicate how should clauses are combined with other clauses.
        enum CombinationMethod {
            Ignored,
            // Only contributes to final score.
            Optional(SpecializedScorer),
            Required(SpecializedScorer),
        }
        let mut must_scorers = per_occur_scorers.remove(&Occur::Must);
        let should_opt = if let Some(mut should_scorers) = per_occur_scorers.remove(&Occur::Should)
        {
            let num_of_should_scorers = should_scorers.len();
            if self.minimum_number_should_match > num_of_should_scorers {
                return Ok(SpecializedScorer::Other(Box::new(EmptyScorer)));
            }
            match self.minimum_number_should_match {
                0 => CombinationMethod::Optional(scorer_union(
                    should_scorers,
                    &score_combiner_fn,
                    num_docs,
                )),
                1 => CombinationMethod::Required(scorer_union(
                    should_scorers,
                    &score_combiner_fn,
                    num_docs,
                )),
                n if num_of_should_scorers == n => {
                    // When num_of_should_scorers equals the number of should clauses,
                    // they are no different from must clauses.
                    must_scorers = match must_scorers.take() {
                        Some(mut must_scorers) => {
                            must_scorers.append(&mut should_scorers);
                            Some(must_scorers)
                        }
                        None => Some(should_scorers),
                    };
                    CombinationMethod::Ignored
                }
                _ => CombinationMethod::Required(SpecializedScorer::Other(scorer_disjunction(
                    should_scorers,
                    score_combiner_fn(),
                    self.minimum_number_should_match,
                ))),
            }
        } else {
            // None of should clauses are provided.
            if self.minimum_number_should_match > 0 {
                return Ok(SpecializedScorer::Other(Box::new(EmptyScorer)));
            } else {
                CombinationMethod::Ignored
            }
        };
        let exclude_scorer_opt: Option<Box<dyn Scorer>> = per_occur_scorers
            .remove(&Occur::MustNot)
            .map(|scorers| scorer_union(scorers, DoNothingCombiner::default, num_docs))
            .map(|specialized_scorer: SpecializedScorer| {
                into_box_scorer(specialized_scorer, DoNothingCombiner::default, num_docs)
            });
        let positive_scorer = match (should_opt, must_scorers) {
            (CombinationMethod::Ignored, Some(must_scorers)) => {
                SpecializedScorer::Other(intersect_scorers(must_scorers, num_docs))
            }
            (CombinationMethod::Optional(should_scorer), Some(must_scorers)) => {
                let must_scorer = intersect_scorers(must_scorers, num_docs);
                if self.scoring_enabled {
                    SpecializedScorer::Other(Box::new(
                        RequiredOptionalScorer::<_, _, TScoreCombiner>::new(
                            must_scorer,
                            into_box_scorer(should_scorer, &score_combiner_fn, num_docs),
                        ),
                    ))
                } else {
                    SpecializedScorer::Other(must_scorer)
                }
            }
            (CombinationMethod::Required(should_scorer), Some(mut must_scorers)) => {
                must_scorers.push(into_box_scorer(should_scorer, &score_combiner_fn, num_docs));
                SpecializedScorer::Other(intersect_scorers(must_scorers, num_docs))
            }
            (CombinationMethod::Ignored, None) => {
                return Ok(SpecializedScorer::Other(Box::new(EmptyScorer)))
            }
            (CombinationMethod::Required(should_scorer), None) => should_scorer,
            // Optional options are promoted to required if no must scorers exists.
            (CombinationMethod::Optional(should_scorer), None) => should_scorer,
        };
        if let Some(exclude_scorer) = exclude_scorer_opt {
            let positive_scorer_boxed =
                into_box_scorer(positive_scorer, &score_combiner_fn, num_docs);
            Ok(SpecializedScorer::Other(Box::new(Exclude::new(
                positive_scorer_boxed,
                exclude_scorer,
            ))))
        } else {
            Ok(positive_scorer)
        }
    }
}

impl<TScoreCombiner: ScoreCombiner + Sync> Weight for BooleanWeight<TScoreCombiner> {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let num_docs = reader.num_docs();
        if self.weights.is_empty() {
            Ok(Box::new(EmptyScorer))
        } else if self.weights.len() == 1 {
            let &(occur, ref weight) = &self.weights[0];
            if occur == Occur::MustNot {
                Ok(Box::new(EmptyScorer))
            } else {
                weight.scorer(reader, boost)
            }
        } else if self.scoring_enabled {
            self.complex_scorer(reader, boost, &self.score_combiner_fn)
                .map(|specialized_scorer| {
                    into_box_scorer(specialized_scorer, &self.score_combiner_fn, num_docs)
                })
        } else {
            self.complex_scorer(reader, boost, DoNothingCombiner::default)
                .map(|specialized_scorer| {
                    into_box_scorer(specialized_scorer, DoNothingCombiner::default, num_docs)
                })
        }
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(does_not_match(doc));
        }
        if !self.scoring_enabled {
            return Ok(Explanation::new("BooleanQuery with no scoring", 1.0));
        }

        let mut explanation = Explanation::new("BooleanClause. sum of ...", scorer.score());
        for (occur, subweight) in &self.weights {
            if is_positive_occur(*occur) {
                if let Ok(child_explanation) = subweight.explain(reader, doc) {
                    explanation.add_detail(child_explanation);
                }
            }
        }
        Ok(explanation)
    }

    fn for_each(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score),
    ) -> crate::Result<()> {
        let scorer = self.complex_scorer(reader, 1.0, &self.score_combiner_fn)?;
        match scorer {
            SpecializedScorer::TermUnion(term_scorers) => {
                let mut union_scorer = BufferedUnionScorer::build(
                    term_scorers,
                    &self.score_combiner_fn,
                    reader.num_docs(),
                );
                for_each_scorer(&mut union_scorer, callback);
            }
            SpecializedScorer::Other(mut scorer) => {
                for_each_scorer(scorer.as_mut(), callback);
            }
        }
        Ok(())
    }

    fn for_each_no_score(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(&[DocId]),
    ) -> crate::Result<()> {
        let scorer = self.complex_scorer(reader, 1.0, || DoNothingCombiner)?;
        let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];

        match scorer {
            SpecializedScorer::TermUnion(term_scorers) => {
                let mut union_scorer = BufferedUnionScorer::build(
                    term_scorers,
                    &self.score_combiner_fn,
                    reader.num_docs(),
                );
                for_each_docset_buffered(&mut union_scorer, &mut buffer, callback);
            }
            SpecializedScorer::Other(mut scorer) => {
                for_each_docset_buffered(scorer.as_mut(), &mut buffer, callback);
            }
        }
        Ok(())
    }

    /// Calls `callback` with all of the `(doc, score)` for which score
    /// is exceeding a given threshold.
    ///
    /// This method is useful for the TopDocs collector.
    /// For all docsets, the blanket implementation has the benefit
    /// of prefiltering (doc, score) pairs, avoiding the
    /// virtual dispatch cost.
    ///
    /// More importantly, it makes it possible for scorers to implement
    /// important optimization (e.g. BlockWAND for union).
    fn for_each_pruning(
        &self,
        threshold: Score,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score) -> Score,
    ) -> crate::Result<()> {
        let scorer = self.complex_scorer(reader, 1.0, &self.score_combiner_fn)?;
        match scorer {
            SpecializedScorer::TermUnion(term_scorers) => {
                super::block_wand(term_scorers, threshold, callback);
            }
            SpecializedScorer::Other(mut scorer) => {
                for_each_pruning_scorer(scorer.as_mut(), threshold, callback);
            }
        }
        Ok(())
    }
}

fn is_positive_occur(occur: Occur) -> bool {
    match occur {
        Occur::Must | Occur::Should => true,
        Occur::MustNot => false,
    }
}

struct JsonConstraintScorer {
    intersection: Intersection<Box<dyn JsonPathScorer>, Box<dyn JsonPathScorer>>,
    num_terms: usize,
    common_indexes: Vec<u32>,
}

impl JsonConstraintScorer {
    fn new(json_scorers: Vec<Box<dyn JsonPathScorer>>, num_docs: u32) -> Self {
        let num_terms = json_scorers.len();
        let mut intersection = Intersection::new(json_scorers, num_docs);
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
        let num_terms = self.num_terms;
        let intersection = &mut self.intersection;
        let common_indexes = &mut self.common_indexes;
        common_indexes.clear();
        let mut initialized = false;
        for ord in 0..num_terms {
            let scorer = intersection.docset_mut_specialized(ord);
            let Some(indexes) = scorer.json_array_path_indexes_dyn() else {
                return true;
            };
            if indexes.is_empty() {
                return true;
            }
            if !initialized {
                populate_common_indexes(common_indexes, indexes);
                initialized = true;
                continue;
            }
            retain_common_indexes(common_indexes, indexes);
            if common_indexes.is_empty() {
                return false;
            }
        }
        initialized && !common_indexes.is_empty()
    }
}

fn populate_common_indexes(common: &mut Vec<u32>, indexes: &[u32]) {
    common.clear();
    for &idx in indexes {
        if !common.iter().any(|existing| *existing == idx) {
            common.push(idx);
        }
    }
}

fn retain_common_indexes(common: &mut Vec<u32>, indexes: &[u32]) {
    common.retain(|idx| indexes.iter().any(|candidate| candidate == idx));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::query::{BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
    use crate::schema::{IndexRecordOption, Schema, TEXT};
    use crate::{doc, Index, Term};
    use crate::serde_json::json;

    #[test]
    fn test_json_array_constraint_and_query() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let doc_body_field = schema_builder.add_json_field("doc_body", TEXT);
        let schema: Schema = schema_builder.build();
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

        let mut foo_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        foo_term.append_type_and_str("foo");
        let mut type_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.type", false);
        type_term.append_type_and_str("mp4");

        let boolean_query = BooleanQuery::from(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    foo_term.clone(),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    type_term.clone(),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ),
        ]);
        let top_docs = searcher.search(&boolean_query, &TopDocs::with_limit(10))?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);

        let mut codec_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        codec_term.append_type_and_str("codec");
        let mut foo_phrase_term =
            Term::from_field_json_path(doc_body_field, "videoInfo.extraData.name", false);
        foo_phrase_term.append_type_and_str("foo");
        let phrase_query: Box<dyn Query> =
            Box::new(PhraseQuery::new(vec![codec_term, foo_phrase_term]));
        let boolean_phrase_query = BooleanQuery::from(vec![
            (Occur::Must, phrase_query),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    type_term,
                    IndexRecordOption::WithFreqsAndPositions,
                )),
            ),
        ]);
        let top_docs = searcher.search(&boolean_phrase_query, &TopDocs::with_limit(10))?;
        assert_eq!(top_docs.len(), 1);
        assert_eq!(top_docs[0].1.doc_id, 0u32);

        Ok(())
    }
}
