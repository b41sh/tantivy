//! JSON array element constraints implemented as a segment plugin.
//!
//! The plugin stores term occurrence paths in a `.jarray` sidecar. Standard postings and
//! positions remain unchanged.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use common::{HasLen, OccurrencePathEntry, TerminatingWrite};
use sstable::{Dictionary, RangeSSTable, SSTable};

use crate::docset::TERMINATED;
use crate::index::{SegmentComponent, SegmentReader};
use crate::indexer::doc_id_mapping::DocIdMapping;
use crate::plugin::{
    PluginMergeContext, PluginWarmupFuture, PluginWriter, PluginWriterContext, SegmentPlugin,
    TermOccurrence, TermOccurrenceMetadata, TermOccurrenceProvider, TermOccurrenceReader,
};
use crate::query::{EnableScoring, Explanation, Occur, Query, QueryClone, Scorer, Weight};
use crate::schema::{Field, Term, Type};
use crate::{DocId, DocSet, Score, Segment, TantivyError};

/// Segment extension owned by [`JsonArrayPlugin`].
pub const JSON_ARRAY_EXTENSION: &str = "jarray";
const MAGIC: [u8; 4] = *b"JARY";
const VERSION: u32 = 1;
const TRAILER_LEN: usize = 8 + 4 + 4;

#[derive(Clone, Debug)]
struct RecordedOccurrence {
    position: u32,
    path: Arc<[OccurrencePathEntry]>,
}

type TermDocs = BTreeMap<DocId, Vec<RecordedOccurrence>>;
type TermMap = BTreeMap<Vec<u8>, TermDocs>;

fn term_key(field: Field, term_bytes: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + term_bytes.len());
    key.extend_from_slice(&field.field_id().to_be_bytes());
    key.extend_from_slice(term_bytes);
    key
}

fn read_u32(input: &mut &[u8]) -> io::Result<u32> {
    if input.len() < 4 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let value = u32::from_le_bytes(input[..4].try_into().unwrap());
    *input = &input[4..];
    Ok(value)
}

fn read_bytes<'a>(input: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let len = read_u32(input)? as usize;
    if input.len() < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let value = &input[..len];
    *input = &input[len..];
    Ok(value)
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn encode_term_docs(docs: &TermDocs) -> Vec<u8> {
    let mut output = Vec::new();
    write_u32(&mut output, docs.len() as u32);
    for (doc_id, occurrences) in docs {
        write_u32(&mut output, *doc_id);
        write_u32(&mut output, occurrences.len() as u32);
        for occurrence in occurrences {
            write_u32(&mut output, occurrence.position);
            write_u32(&mut output, occurrence.path.len() as u32);
            for entry in occurrence.path.iter() {
                write_u32(&mut output, entry.path.len() as u32);
                output.extend_from_slice(entry.path.as_bytes());
                write_u32(&mut output, entry.element_ord);
            }
        }
    }
    output
}

fn decode_term_docs(mut input: &[u8]) -> io::Result<TermDocs> {
    let num_docs = read_u32(&mut input)?;
    let mut docs = BTreeMap::new();
    for _ in 0..num_docs {
        let doc_id = read_u32(&mut input)?;
        let num_occurrences = read_u32(&mut input)?;
        let mut occurrences = Vec::with_capacity(num_occurrences as usize);
        for _ in 0..num_occurrences {
            let position = read_u32(&mut input)?;
            let depth = read_u32(&mut input)?;
            let mut path = Vec::with_capacity(depth as usize);
            for _ in 0..depth {
                let array_path = std::str::from_utf8(read_bytes(&mut input)?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let element_ord = read_u32(&mut input)?;
                path.push(OccurrencePathEntry {
                    path: Arc::from(array_path),
                    element_ord,
                });
            }
            occurrences.push(RecordedOccurrence {
                position,
                path: Arc::from(path),
            });
        }
        docs.insert(doc_id, occurrences);
    }
    if !input.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes in JSON array term payload",
        ));
    }
    Ok(docs)
}

fn write_sidecar(
    segment: &Segment,
    terms: &TermMap,
    doc_id_map: Option<&DocIdMapping>,
) -> crate::Result<()> {
    let mut normalized = BTreeMap::<Vec<u8>, TermDocs>::new();
    for (key, docs) in terms {
        let target_docs = normalized.entry(key.clone()).or_default();
        for (doc_id, occurrences) in docs {
            let target_doc = doc_id_map.map_or(*doc_id, |mapping| mapping.get_new_doc_id(*doc_id));
            target_docs
                .entry(target_doc)
                .or_default()
                .extend(occurrences.iter().cloned());
        }
    }

    let mut payload = Vec::new();
    let mut ranges = Vec::with_capacity(normalized.len());
    for (key, docs) in &normalized {
        let start = payload.len() as u64;
        payload.extend_from_slice(&encode_term_docs(docs));
        ranges.push((key.clone(), start..payload.len() as u64));
    }

    let mut table = Vec::new();
    {
        let mut writer = RangeSSTable::writer(&mut table);
        for (key, range) in &ranges {
            writer.insert(key, range)?;
        }
        writer.finish()?;
    }

    let table_offset = payload.len() as u64;
    let mut output = payload;
    output.extend_from_slice(&table);
    output.extend_from_slice(&table_offset.to_le_bytes());
    output.extend_from_slice(&VERSION.to_le_bytes());
    output.extend_from_slice(&MAGIC);

    let component = SegmentComponent::Custom(JSON_ARRAY_EXTENSION.to_string());
    let mut write = segment.open_write(component)?;
    write.write_all(&output)?;
    write.terminate()?;
    Ok(())
}

struct Sidecar {
    file: crate::directory::FileSlice,
    dictionary: Dictionary<RangeSSTable>,
    payload_end: usize,
}

impl Sidecar {
    fn open_file(file: crate::directory::FileSlice) -> crate::Result<Self> {
        if file.len() < TRAILER_LEN {
            return Err(TantivyError::DataCorruption(
                crate::error::DataCorruption::comment_only(
                    "JSON array sidecar trailer is truncated",
                ),
            ));
        }
        let trailer = file.slice_from(file.len() - TRAILER_LEN).read_bytes()?;
        let table_offset = u64::from_le_bytes(trailer[..8].try_into().unwrap()) as usize;
        let version = u32::from_le_bytes(trailer[8..12].try_into().unwrap());
        if version != VERSION || trailer[12..16] != MAGIC {
            return Err(TantivyError::DataCorruption(
                crate::error::DataCorruption::comment_only(
                    "unsupported JSON array sidecar version",
                ),
            ));
        }
        if table_offset > file.len() - TRAILER_LEN {
            return Err(TantivyError::DataCorruption(
                crate::error::DataCorruption::comment_only(
                    "invalid JSON array sidecar table offset",
                ),
            ));
        }
        let table = file.slice(table_offset..file.len() - TRAILER_LEN);
        let dictionary = Dictionary::<RangeSSTable>::open(table)?;
        Ok(Self {
            file,
            dictionary,
            payload_end: table_offset,
        })
    }

    fn open_segment(segment: &Segment) -> crate::Result<Self> {
        let file = segment.open_read(SegmentComponent::Custom(JSON_ARRAY_EXTENSION.to_string()))?;
        Self::open_file(file)
    }

    fn open_reader(segment_reader: &SegmentReader) -> crate::Result<Self> {
        let file =
            segment_reader.open_read(SegmentComponent::Custom(JSON_ARRAY_EXTENSION.to_string()))?;
        Self::open_file(file)
    }

    fn get(&self, key: &[u8]) -> io::Result<Option<TermDocs>> {
        let Some(range) = self.dictionary.get(key)? else {
            return Ok(None);
        };
        if range.end as usize > self.payload_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON array payload range exceeds payload body",
            ));
        }
        let bytes = self
            .file
            .read_bytes_slice(range.start as usize..range.end as usize)?;
        decode_term_docs(bytes.as_ref()).map(Some)
    }

    async fn warm(&self, key: &[u8]) -> io::Result<()> {
        let Some(range) = self.dictionary.get_async(key).await? else {
            return Ok(());
        };
        self.file
            .read_bytes_slice_async(range.start as usize..range.end as usize)
            .await?;
        Ok(())
    }

    fn all_terms(&self) -> io::Result<TermMap> {
        let mut stream = self.dictionary.stream()?;
        let mut terms = TermMap::new();
        while stream.advance() {
            let range: Range<u64> = stream.value().clone();
            let bytes = self
                .file
                .read_bytes_slice(range.start as usize..range.end as usize)?;
            terms.insert(stream.key().to_vec(), decode_term_docs(bytes.as_ref())?);
        }
        Ok(terms)
    }
}

struct JsonArrayTermReader {
    docs: TermDocs,
}

impl TermOccurrenceReader for JsonArrayTermReader {
    fn read_doc(&self, doc_id: DocId, output: &mut Vec<TermOccurrenceMetadata>) -> bool {
        output.clear();
        let Some(occurrences) = self.docs.get(&doc_id) else {
            return false;
        };
        output.extend(occurrences.iter().map(|occurrence| TermOccurrenceMetadata {
            position: occurrence.position,
            array_path: occurrence.path.clone(),
        }));
        true
    }
}

impl TermOccurrenceProvider for Sidecar {
    fn open_term(&self, term: &Term) -> io::Result<Option<Arc<dyn TermOccurrenceReader>>> {
        if term.typ() != Type::Json {
            return Ok(None);
        }
        let key = term_key(term.field(), term.serialized_value_bytes());
        Ok(self
            .get(&key)?
            .map(|docs| Arc::new(JsonArrayTermReader { docs }) as Arc<dyn TermOccurrenceReader>))
    }

    fn warm_term<'a>(&'a self, term: &'a Term) -> PluginWarmupFuture<'a> {
        Box::pin(async move {
            if term.typ() != Type::Json {
                return Ok(());
            }
            self.warm(&term_key(term.field(), term.serialized_value_bytes()))
                .await
        })
    }
}

/// Plugin storing JSON array occurrence metadata in a `.jarray` sidecar.
///
/// Register this plugin when creating an index and re-register it after [`crate::Index::open`].
/// With remote directories, [`crate::InvertedIndexReader::warm_postings`] also warms the matching
/// sidecar payload before synchronous search.
#[derive(Default)]
pub struct JsonArrayPlugin;

impl SegmentPlugin for JsonArrayPlugin {
    fn extensions(&self) -> &[&str] {
        &[JSON_ARRAY_EXTENSION]
    }

    fn create_writer(&self, ctx: &PluginWriterContext) -> crate::Result<Box<dyn PluginWriter>> {
        Ok(Box::new(JsonArrayPluginWriter::new(ctx)?))
    }

    fn merge(&self, ctx: PluginMergeContext) -> crate::Result<()> {
        let mut old_to_new = vec![HashMap::<DocId, DocId>::new(); ctx.readers.len()];
        for (new_doc_id, old_doc_addr) in ctx.doc_id_mapping.iter_old_doc_addrs().enumerate() {
            old_to_new[old_doc_addr.segment_ord as usize]
                .insert(old_doc_addr.doc_id, new_doc_id as DocId);
        }

        let mut merged = TermMap::new();
        for (segment_ord, reader) in ctx.readers.iter().enumerate() {
            let source = Sidecar::open_reader(reader)?;
            for (key, docs) in source.all_terms()? {
                let target_docs = merged.entry(key).or_default();
                for (old_doc_id, occurrences) in docs {
                    if let Some(new_doc_id) = old_to_new[segment_ord].get(&old_doc_id) {
                        target_docs.insert(*new_doc_id, occurrences);
                    }
                }
            }
        }
        write_sidecar(ctx.target_segment, &merged, None)
    }

    fn open_term_occurrence_provider(
        &self,
        segment: &Segment,
    ) -> crate::Result<Option<Arc<dyn TermOccurrenceProvider>>> {
        Ok(Some(Arc::new(Sidecar::open_segment(segment)?)))
    }
}

#[derive(Default)]
struct JsonArrayPluginWriter {
    terms: TermMap,
}

impl JsonArrayPluginWriter {
    fn new(_ctx: &PluginWriterContext) -> crate::Result<Self> {
        Ok(Self::default())
    }

    fn occurrence_key(occurrence: &TermOccurrence<'_>) -> Vec<u8> {
        let mut key = Vec::with_capacity(
            4 + occurrence.json_path.len() + 1 + occurrence.term_value_bytes.len(),
        );
        key.extend_from_slice(&occurrence.field.field_id().to_be_bytes());
        key.extend_from_slice(occurrence.json_path.as_bytes());
        key.push(0);
        key.extend_from_slice(occurrence.term_value_bytes);
        key
    }
}

impl PluginWriter for JsonArrayPluginWriter {
    fn add_term_occurrence(&mut self, occurrence: &TermOccurrence<'_>) {
        self.terms
            .entry(Self::occurrence_key(occurrence))
            .or_default()
            .entry(occurrence.doc_id)
            .or_default()
            .push(RecordedOccurrence {
                position: occurrence.position,
                path: Arc::from(occurrence.array_path.to_vec()),
            });
    }

    fn serialize(
        self: Box<Self>,
        segment: &Segment,
        doc_id_map: Option<&DocIdMapping>,
    ) -> crate::Result<()> {
        write_sidecar(segment, &self.terms, doc_id_map)
    }

    fn mem_usage(&self) -> usize {
        self.terms
            .iter()
            .map(|(key, docs)| {
                key.len()
                    + docs
                        .values()
                        .flatten()
                        .map(|occurrence| {
                            std::mem::size_of::<RecordedOccurrence>()
                                + occurrence
                                    .path
                                    .iter()
                                    .map(|entry| entry.path.len())
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Conjunction query that requires JSON terms to resolve to a common array element.
pub struct JsonArrayQuery {
    subqueries: Vec<Box<dyn Query>>,
    constraints: Vec<JsonArrayConstraint>,
}

#[derive(Clone)]
enum ConstraintAlternative {
    Term(Term),
    Phrase(Vec<(usize, Term)>),
}

#[derive(Clone)]
struct JsonArrayConstraint {
    alternatives: Vec<ConstraintAlternative>,
}

impl Clone for JsonArrayQuery {
    fn clone(&self) -> Self {
        Self {
            subqueries: self
                .subqueries
                .iter()
                .map(|query| query.box_clone())
                .collect(),
            constraints: self.constraints.clone(),
        }
    }
}

impl std::fmt::Debug for JsonArrayQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JsonArrayQuery")
            .field("subqueries", &self.subqueries)
            .finish()
    }
}

impl JsonArrayQuery {
    fn alternatives(query: &dyn Query) -> Option<Vec<ConstraintAlternative>> {
        if let Some(term_query) = query.downcast_ref::<crate::query::TermQuery>() {
            return Some(vec![ConstraintAlternative::Term(term_query.term().clone())]);
        }
        if let Some(phrase_query) = query.downcast_ref::<crate::query::PhraseQuery>() {
            if phrase_query.slop() != 0 {
                return None;
            }
            return Some(vec![ConstraintAlternative::Phrase(
                phrase_query.phrase_terms_with_offsets().to_vec(),
            )]);
        }
        let boolean = query.downcast_ref::<crate::query::BooleanQuery>()?;
        if boolean.clauses().is_empty()
            || boolean
                .clauses()
                .iter()
                .any(|(occur, _)| *occur != Occur::Should)
        {
            return None;
        }
        let mut alternatives = Vec::new();
        for (_, subquery) in boolean.clauses() {
            alternatives.extend(Self::alternatives(subquery.as_ref())?);
        }
        Some(alternatives)
    }

    /// Attempts to build an array-constrained query from `Must` term/phrase clauses.
    pub fn try_from_clauses(clauses: Vec<(Occur, Box<dyn Query>)>) -> Option<Self> {
        if clauses.len() < 2 || clauses.iter().any(|(occur, _)| *occur != Occur::Must) {
            return None;
        }
        let mut field = None;
        let mut constraints = Vec::with_capacity(clauses.len());
        for (_, query) in &clauses {
            let alternatives = Self::alternatives(query.as_ref())?;
            if alternatives.is_empty() {
                return None;
            }
            let mut term_field = None;
            for alternative in &alternatives {
                match alternative {
                    ConstraintAlternative::Term(term) => {
                        if term.typ() != Type::Json {
                            return None;
                        }
                        if term_field.is_some_and(|field| field != term.field()) {
                            return None;
                        }
                        term_field = Some(term.field());
                    }
                    ConstraintAlternative::Phrase(terms) => {
                        if terms.is_empty() {
                            return None;
                        }
                        for (_, term) in terms {
                            if term.typ() != Type::Json {
                                return None;
                            }
                            if term_field.is_some_and(|field| field != term.field()) {
                                return None;
                            }
                            term_field = Some(term.field());
                        }
                    }
                }
            }
            let term_field = term_field?;
            if field.is_some_and(|field| field != term_field) {
                return None;
            }
            field = Some(term_field);
            constraints.push(JsonArrayConstraint { alternatives });
        }
        Some(Self {
            subqueries: clauses.into_iter().map(|(_, query)| query).collect(),
            constraints,
        })
    }
}

struct JsonArrayWeight {
    weights: Vec<Box<dyn Weight>>,
    constraints: Vec<JsonArrayConstraint>,
}

impl Query for JsonArrayQuery {
    fn weight(&self, scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let weights = self
            .subqueries
            .iter()
            .map(|query| query.weight(scoring))
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(Box::new(JsonArrayWeight {
            weights,
            constraints: self.constraints.clone(),
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for query in &self.subqueries {
            query.query_terms(visitor);
        }
    }
}

struct ConstraintTermReader {
    offset: usize,
    reader: Option<Arc<dyn TermOccurrenceReader>>,
    buffer: Vec<TermOccurrenceMetadata>,
}

enum ConstraintAlternativeReaders {
    Term(ConstraintTermReader),
    Phrase(Vec<ConstraintTermReader>),
}

struct ConstraintReaders {
    alternatives: Vec<ConstraintAlternativeReaders>,
}

struct JsonArrayScorer {
    scorer: Box<dyn Scorer>,
    constraints: Vec<ConstraintReaders>,
}

enum ConstraintPaths {
    Unconstrained,
    Paths(Vec<Arc<[OccurrencePathEntry]>>),
    NoMatch,
}

fn paths_share_prefix(left: &[OccurrencePathEntry], right: &[OccurrencePathEntry]) -> bool {
    let shared_len = left.len().min(right.len());
    shared_len > 0
        && left
            .iter()
            .zip(right)
            .take(shared_len)
            .all(|(left, right)| left == right)
}

impl ConstraintAlternativeReaders {
    fn paths_for_doc(&mut self, doc: DocId) -> ConstraintPaths {
        match self {
            Self::Term(term) => {
                let Some(reader) = &term.reader else {
                    return ConstraintPaths::NoMatch;
                };
                if !reader.read_doc(doc, &mut term.buffer) {
                    return ConstraintPaths::NoMatch;
                }
                if term.buffer.iter().any(|entry| entry.array_path.is_empty()) {
                    return ConstraintPaths::Unconstrained;
                }
                let mut paths = Vec::new();
                for entry in &term.buffer {
                    if !paths.iter().any(|path: &Arc<[OccurrencePathEntry]>| {
                        path.as_ref() == entry.array_path.as_ref()
                    }) {
                        paths.push(entry.array_path.clone());
                    }
                }
                if paths.is_empty() {
                    ConstraintPaths::NoMatch
                } else {
                    ConstraintPaths::Paths(paths)
                }
            }
            Self::Phrase(terms) => {
                for term in terms.iter_mut() {
                    let Some(reader) = &term.reader else {
                        return ConstraintPaths::NoMatch;
                    };
                    if !reader.read_doc(doc, &mut term.buffer) {
                        return ConstraintPaths::NoMatch;
                    }
                }

                let first = &terms[0];
                let mut candidates = first
                    .buffer
                    .iter()
                    .filter_map(|entry| {
                        let normalized_position =
                            entry.position.checked_sub(first.offset as u32)?;
                        let path = (!entry.array_path.is_empty()).then(|| entry.array_path.clone());
                        Some((normalized_position, path))
                    })
                    .collect::<Vec<_>>();

                for term in &terms[1..] {
                    candidates.retain_mut(|(normalized_position, candidate_path)| {
                        term.buffer.iter().any(|entry| {
                            if entry.position.checked_sub(term.offset as u32)
                                != Some(*normalized_position)
                            {
                                return false;
                            }
                            if candidate_path.is_none() || entry.array_path.is_empty() {
                                *candidate_path = None;
                                return true;
                            }
                            paths_share_prefix(
                                candidate_path.as_ref().unwrap().as_ref(),
                                entry.array_path.as_ref(),
                            )
                        })
                    });
                    if candidates.is_empty() {
                        return ConstraintPaths::NoMatch;
                    }
                }

                if candidates.iter().any(|(_, path)| path.is_none()) {
                    return ConstraintPaths::Unconstrained;
                }
                let mut paths = Vec::new();
                for (_, path) in candidates {
                    let path = path.unwrap();
                    if !paths.iter().any(|existing: &Arc<[OccurrencePathEntry]>| {
                        existing.as_ref() == path.as_ref()
                    }) {
                        paths.push(path);
                    }
                }
                ConstraintPaths::Paths(paths)
            }
        }
    }
}

impl ConstraintReaders {
    fn paths_for_doc(&mut self, doc: DocId) -> ConstraintPaths {
        let mut paths = Vec::<Arc<[OccurrencePathEntry]>>::new();
        for alternative in &mut self.alternatives {
            match alternative.paths_for_doc(doc) {
                ConstraintPaths::Unconstrained => return ConstraintPaths::Unconstrained,
                ConstraintPaths::NoMatch => {}
                ConstraintPaths::Paths(alternative_paths) => {
                    for path in alternative_paths {
                        if !paths
                            .iter()
                            .any(|existing| existing.as_ref() == path.as_ref())
                        {
                            paths.push(path);
                        }
                    }
                }
            }
        }
        if paths.is_empty() {
            ConstraintPaths::NoMatch
        } else {
            ConstraintPaths::Paths(paths)
        }
    }
}

impl JsonArrayScorer {
    fn satisfies(&mut self) -> bool {
        let doc = self.scorer.doc();
        let mut common: Vec<Arc<[OccurrencePathEntry]>> = Vec::new();
        let mut constrained = false;
        for constraint in &mut self.constraints {
            let constraint_paths = match constraint.paths_for_doc(doc) {
                ConstraintPaths::Unconstrained => continue,
                ConstraintPaths::NoMatch => return false,
                ConstraintPaths::Paths(paths) => paths,
            };
            if !constrained {
                common = constraint_paths;
                constrained = true;
            } else {
                common.retain(|candidate| {
                    constraint_paths
                        .iter()
                        .any(|path| paths_share_prefix(candidate.as_ref(), path.as_ref()))
                });
                if common.is_empty() {
                    return false;
                }
            }
        }
        !constrained || !common.is_empty()
    }

    fn advance_to_match(&mut self) -> DocId {
        while self.scorer.doc() != TERMINATED && !self.satisfies() {
            self.scorer.advance();
        }
        self.scorer.doc()
    }
}

impl DocSet for JsonArrayScorer {
    fn advance(&mut self) -> DocId {
        self.scorer.advance();
        self.advance_to_match()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.scorer.seek(target);
        self.advance_to_match()
    }

    fn doc(&self) -> DocId {
        self.scorer.doc()
    }

    fn size_hint(&self) -> u32 {
        self.scorer.size_hint()
    }

    fn cost(&self) -> u64 {
        self.scorer.cost()
    }
}

impl Scorer for JsonArrayScorer {
    fn score(&mut self) -> Score {
        self.scorer.score()
    }
}

impl Weight for JsonArrayWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let scorers = self
            .weights
            .iter()
            .map(|weight| weight.scorer(reader, boost))
            .collect::<crate::Result<Vec<_>>>()?;
        let scorer: Box<dyn Scorer> = match scorers.len() {
            0 => return Ok(Box::new(crate::query::EmptyScorer)),
            1 => scorers.into_iter().next().unwrap(),
            _ => Box::new(crate::query::Intersection::new(scorers, reader.max_doc())),
        };
        if !reader.has_term_occurrence_provider() {
            return Ok(scorer);
        }

        let mut constraints = Vec::with_capacity(self.constraints.len());
        for constraint in &self.constraints {
            let mut alternatives = Vec::with_capacity(constraint.alternatives.len());
            for alternative in &constraint.alternatives {
                match alternative {
                    ConstraintAlternative::Term(term) => {
                        alternatives.push(ConstraintAlternativeReaders::Term(
                            ConstraintTermReader {
                                offset: 0,
                                reader: reader.term_occurrence_reader(term)?,
                                buffer: Vec::new(),
                            },
                        ));
                    }
                    ConstraintAlternative::Phrase(terms) => {
                        let mut term_readers = Vec::with_capacity(terms.len());
                        for (offset, term) in terms {
                            term_readers.push(ConstraintTermReader {
                                offset: *offset,
                                reader: reader.term_occurrence_reader(term)?,
                                buffer: Vec::new(),
                            });
                        }
                        alternatives.push(ConstraintAlternativeReaders::Phrase(term_readers));
                    }
                }
            }
            constraints.push(ConstraintReaders { alternatives });
        }
        let mut scorer = JsonArrayScorer {
            scorer,
            constraints,
        };
        scorer.advance_to_match();
        Ok(Box::new(scorer))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match"
            )));
        }
        Ok(Explanation::new("JsonArrayQuery", scorer.score()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{JsonArrayPlugin, JsonArrayQuery, JSON_ARRAY_EXTENSION};
    use crate::collector::TopDocs;
    use crate::index::SegmentComponent;
    use crate::indexer::NoMergePolicy;
    use crate::query::QueryParser;
    use crate::schema::{Field, NumericOptions, Schema, Term, TEXT};
    use crate::{Directory, Index, IndexSettings, IndexSortByField, IndexWriter, Order};

    fn create_index() -> crate::Result<(Index, Field)> {
        let mut schema_builder = Schema::builder();
        let json_field = schema_builder.add_json_field("json", TEXT);
        let index = Index::builder()
            .schema(schema_builder.build())
            .register_plugin(Arc::new(JsonArrayPlugin))
            .create_in_ram()?;
        Ok((index, json_field))
    }

    fn search(index: &Index, field: Field, query: &str) -> crate::Result<Vec<u32>> {
        let parsed = QueryParser::for_index(index, vec![field]).parse_query(query)?;
        assert!(parsed.is::<JsonArrayQuery>());
        let docs = index
            .reader()?
            .searcher()
            .search(&parsed, &TopDocs::with_limit(20).order_by_score())?;
        Ok(docs
            .into_iter()
            .map(|(_, address)| address.doc_id)
            .collect())
    }

    #[test]
    fn test_same_and_cross_array_elements() -> crate::Result<()> {
        let (index, field) = create_index()?;
        let mut writer: IndexWriter = index.writer_for_tests()?;
        writer.add_document(doc!(
            field => json!({"items": [{"name": "foo", "kind": "mp4"}]})
        ))?;
        writer.add_document(doc!(
            field => json!({"items": [{"name": "foo", "kind": "jpg"}, {"name": "bar", "kind": "mp4"}]})
        ))?;
        writer.add_document(doc!(field => json!({"name": "foo", "kind": "mp4"})))?;
        writer.commit()?;

        let segments = index.searchable_segments()?;
        assert_eq!(segments.len(), 1);
        let sidecar =
            segments[0].relative_path(SegmentComponent::Custom(JSON_ARRAY_EXTENSION.to_string()));
        assert!(index.directory().exists(&sidecar)?);

        assert_eq!(
            search(&index, field, "json.items.name:foo AND json.items.kind:mp4")?,
            vec![0]
        );
        assert_eq!(
            search(&index, field, "json.name:foo AND json.kind:mp4")?,
            vec![2]
        );
        Ok(())
    }

    #[test]
    fn test_scalar_values_respect_array_elements() -> crate::Result<()> {
        let (index, field) = create_index()?;
        let mut writer: IndexWriter = index.writer_for_tests()?;
        writer.add_document(doc!(
            field => json!({"items": [{"rank": 1, "enabled": true}]})
        ))?;
        writer.add_document(doc!(
            field => json!({"items": [{"rank": 1, "enabled": false}, {"rank": 2, "enabled": true}]})
        ))?;
        writer.commit()?;

        assert_eq!(
            search(
                &index,
                field,
                "json.items.rank:1 AND json.items.enabled:true"
            )?,
            vec![0]
        );
        Ok(())
    }

    #[test]
    fn test_sorted_segment_remaps_sidecar_doc_ids() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let json_field = schema_builder.add_json_field("json", TEXT);
        let sort_field = schema_builder.add_u64_field("sort", NumericOptions::default().set_fast());
        let settings = IndexSettings {
            sort_by_field: Some(IndexSortByField {
                field: "sort".to_string(),
                order: Order::Asc,
            }),
            ..Default::default()
        };
        let index = Index::builder()
            .schema(schema_builder.build())
            .settings(settings)
            .register_plugin(Arc::new(JsonArrayPlugin))
            .create_in_ram()?;
        let mut writer: IndexWriter = index.writer_for_tests()?;
        writer.add_document(doc!(
            sort_field => 20u64,
            json_field => json!({"items": [{"name": "foo", "kind": "mp4"}]})
        ))?;
        writer.add_document(doc!(
            sort_field => 10u64,
            json_field => json!({"items": [{"name": "foo", "kind": "jpg"}, {"name": "bar", "kind": "mp4"}]})
        ))?;
        writer.commit()?;

        // Sorting places the cross-element document at doc 0 and the valid match at doc 1.
        assert_eq!(
            search(
                &index,
                json_field,
                "json.items.name:foo AND json.items.kind:mp4"
            )?,
            vec![1]
        );
        Ok(())
    }

    #[test]
    fn test_phrase_and_nested_array() -> crate::Result<()> {
        let (index, field) = create_index()?;
        let mut writer: IndexWriter = index.writer_for_tests()?;
        writer.add_document(doc!(
            field => json!({"groups": [{"items": [{"name": "hello world", "kind": "mp4"}]}]})
        ))?;
        writer.add_document(doc!(
            field => json!({"groups": [{"items": [{"name": "hello world", "kind": "jpg"}, {"name": "other", "kind": "mp4"}]}]})
        ))?;
        writer.commit()?;

        assert_eq!(
            search(
                &index,
                field,
                "json.groups.items.name:\"hello world\" AND json.groups.items.kind:mp4"
            )?,
            vec![0]
        );
        Ok(())
    }

    #[test]
    fn test_term_occurrence_warmup_and_read() -> crate::Result<()> {
        futures::executor::block_on(async {
            let (index, field) = create_index()?;
            let mut writer: IndexWriter = index.writer_for_tests()?;
            writer.add_document(doc!(
                field => json!({"items": [{"name": "foo", "kind": "mp4"}]})
            ))?;
            writer.commit()?;

            let searcher = index.reader()?.searcher();
            let segment_reader = searcher.segment_reader(0);
            let mut term = Term::from_field_json_path(field, "items.name", false);
            term.append_type_and_str("foo");
            assert!(
                segment_reader
                    .inverted_index(field)?
                    .warm_postings(&term, false)
                    .await?
            );

            let occurrence_reader = segment_reader
                .term_occurrence_reader(&term)?
                .expect("the JSON array plugin should provide metadata for the indexed term");
            let mut metadata = Vec::new();
            assert!(occurrence_reader.read_doc(0, &mut metadata));
            assert_eq!(metadata.len(), 1);
            assert_eq!(metadata[0].array_path[0].path.as_ref(), "items");
            assert_eq!(metadata[0].array_path[0].element_ord, 0);
            Ok(())
        })
    }

    #[test]
    fn test_merge_preserves_array_constraints() -> crate::Result<()> {
        let (index, field) = create_index()?;
        let mut writer: IndexWriter = index.writer_for_tests()?;
        writer.set_merge_policy(Box::new(NoMergePolicy));
        writer.add_document(doc!(
            field => json!({"items": [{"name": "foo", "kind": "mp4"}]})
        ))?;
        writer.commit()?;
        writer.add_document(doc!(
            field => json!({"items": [{"name": "foo", "kind": "jpg"}, {"name": "bar", "kind": "mp4"}]})
        ))?;
        writer.commit()?;
        assert_eq!(index.searchable_segments()?.len(), 2);

        writer.merge(&index.searchable_segment_ids()?).wait()?;
        assert_eq!(index.searchable_segments()?.len(), 1);
        let matched = search(&index, field, "json.items.name:foo AND json.items.kind:mp4")?;
        assert_eq!(matched.len(), 1);
        Ok(())
    }
}
