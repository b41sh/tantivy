use std::convert::TryInto;
use std::io;
use std::sync::Arc;

use common::json_path_writer::JsonArrayPathEntry;
use common::{read_u32_vint, BinarySerializable, VInt};

use crate::directory::OwnedBytes;
use crate::positions::{COMPRESSION_BLOCK_SIZE, JSON_METADATA_MARKER};
use crate::postings::compression::{BlockDecoder, VIntDecoder};
use crate::DocId;

/// When accessing the positions of a term, we get a positions_idx from the `Terminfo`.
/// This means we need to skip to the `nth` position efficiently.
///
/// Blocks are compressed using bitpacking, so `skip_read` contains the number of bits
/// (values can go from 0 to 32 bits) required to decompress every block.
///
/// A given block obviously takes `(128 x  num_bit_for_the_block / num_bits_in_a_byte)`,
/// so skipping a block without decompressing it is just a matter of advancing that many
/// bytes.

#[derive(Clone)]
pub struct PositionReader {
    bit_widths: OwnedBytes,
    positions: OwnedBytes,

    block_decoder: BlockDecoder,

    // offset, expressed in positions, for the first position of the block currently loaded
    // block_offset is a multiple of COMPRESSION_BLOCK_SIZE.
    block_offset: u64,
    // offset, expressed in positions, for the position of the first block encoded
    // in the `self.positions` bytes, and if bitpacked, compressed using the bitwidth in
    // `self.bit_widths`.
    //
    // As we advance, anchor increases simultaneously with bit_widths and positions get consumed.
    anchor_offset: u64,

    // These are just copies used for .reset().
    original_bit_widths: OwnedBytes,
    original_positions: OwnedBytes,
    // Per-term JSON path metadata decoded from the positions trailer (if present).
    json_metadata: JsonMetadata,
}

impl PositionReader {
    /// Open and reads the term positions encoded into the positions_data owned bytes.
    pub fn open(
        mut positions_data: OwnedBytes,
        json_path_table: Option<Arc<Vec<Arc<[JsonArrayPathEntry]>>>>,
    ) -> io::Result<PositionReader> {
        let num_positions_bitpacked_blocks = VInt::deserialize(&mut positions_data)?.0 as usize;
        let (bit_widths, positions) = positions_data.split(num_positions_bitpacked_blocks);
        let mut positions = positions;
        let mut json_metadata = JsonMetadata::None;
        if positions.len() > 5 {
            let slice = positions.as_slice();
            let marker_idx = slice.len() - 5;
            if slice[marker_idx] == JSON_METADATA_MARKER {
                let meta_len =
                    u32::from_be_bytes(slice[marker_idx + 1..marker_idx + 5].try_into().unwrap())
                        as usize;
                if marker_idx >= meta_len {
                    let metadata_bytes = positions.slice(marker_idx - meta_len..marker_idx);
                    // If present, decode JSON metadata using the shared path table.
                    json_metadata =
                        parse_json_metadata(metadata_bytes.clone(), json_path_table.clone())?;
                    positions = positions.slice(0..marker_idx - meta_len);
                }
            }
        }

        Ok(PositionReader {
            bit_widths: bit_widths.clone(),
            positions: positions.clone(),
            block_decoder: BlockDecoder::default(),
            block_offset: i64::MAX as u64,
            anchor_offset: 0u64,
            original_bit_widths: bit_widths,
            original_positions: positions,
            json_metadata,
        })
    }

    fn reset(&mut self) {
        self.positions = self.original_positions.clone();
        self.bit_widths = self.original_bit_widths.clone();
        self.block_offset = i64::MAX as u64;
        self.anchor_offset = 0u64;
        self.json_metadata.reset();
    }

    /// Advance from num_blocks bitpacked blocks.
    ///
    /// Panics if there are not that many remaining blocks.
    fn advance_num_blocks(&mut self, num_blocks: usize) {
        let num_bits: usize = self.bit_widths.as_ref()[..num_blocks]
            .iter()
            .cloned()
            .map(|num_bits| num_bits as usize)
            .sum();
        let num_bytes_to_skip = num_bits * COMPRESSION_BLOCK_SIZE / 8;
        self.bit_widths.advance(num_blocks);
        self.positions.advance(num_bytes_to_skip);
        self.anchor_offset += (num_blocks * COMPRESSION_BLOCK_SIZE) as u64;
    }

    /// block_rel_id is counted relatively to the anchor.
    /// block_rel_id = 0 means the anchor block.
    /// block_rel_id = i means the ith block after the anchor block.
    fn load_block(&mut self, block_rel_id: usize) {
        let bit_widths = self.bit_widths.as_slice();
        let byte_offset: usize = bit_widths[0..block_rel_id]
            .iter()
            .map(|&b| b as usize)
            .sum::<usize>()
            * COMPRESSION_BLOCK_SIZE
            / 8;
        let compressed_data = &self.positions.as_slice()[byte_offset..];
        if bit_widths.len() > block_rel_id {
            // that block is bitpacked.
            let bit_width = bit_widths[block_rel_id];
            self.block_decoder
                .uncompress_block_unsorted(compressed_data, bit_width, false);
        } else {
            // that block is vint encoded.
            self.block_decoder
                .uncompress_vint_unsorted_until_end(compressed_data);
        }
        self.block_offset = self.anchor_offset + (block_rel_id * COMPRESSION_BLOCK_SIZE) as u64;
    }

    /// Fills a buffer with the positions `[offset..offset+output.len())` integers.
    ///
    /// This function is optimized to be called with increasing values of `offset`.
    pub fn read(&mut self, mut offset: u64, mut output: &mut [u32]) {
        if offset < self.anchor_offset {
            self.reset();
        }
        let delta_to_block_offset = offset as i64 - self.block_offset as i64;
        if !(0..128).contains(&delta_to_block_offset) {
            // The first position is not within the first block.
            // (Note that it could be before or after)
            // We need to possibly skip a few blocks, and decompress the first relevant  block.
            let delta_to_anchor_offset = offset - self.anchor_offset;
            let num_blocks_to_skip =
                (delta_to_anchor_offset / (COMPRESSION_BLOCK_SIZE as u64)) as usize;
            self.advance_num_blocks(num_blocks_to_skip);
            self.load_block(0);
        } else {
            // The request offset is within the loaded block.
            // We still need to advance anchor_offset to our current block.
            let num_blocks_to_skip =
                ((self.block_offset - self.anchor_offset) / COMPRESSION_BLOCK_SIZE as u64) as usize;
            self.advance_num_blocks(num_blocks_to_skip);
        }

        // At this point, the block containing offset is loaded, and anchor has
        // been updated to point to it as well.
        for i in 1.. {
            // we copy the part from block i - 1 that is relevant.
            let offset_in_block = (offset as usize) % COMPRESSION_BLOCK_SIZE;
            let remaining_in_block = COMPRESSION_BLOCK_SIZE - offset_in_block;
            if remaining_in_block >= output.len() {
                output.copy_from_slice(
                    &self.block_decoder.output_array()[offset_in_block..][..output.len()],
                );
                break;
            }
            output[..remaining_in_block]
                .copy_from_slice(&self.block_decoder.output_array()[offset_in_block..]);
            output = &mut output[remaining_in_block..];
            // we load block #i if necessary.
            offset += remaining_in_block as u64;
            self.load_block(i);
        }
    }

    /// Returns true if JSON metadata was encoded for this term.
    pub fn has_json_metadata(&self) -> bool {
        !matches!(self.json_metadata, JsonMetadata::None)
    }

    /// Fills `output` with the JSON array paths associated to the current doc/term positions.
    ///
    /// Returns `true` if metadata was found for the given `doc_id`/`doc_ord`, `false` otherwise.
    pub fn fill_doc_json_metadata_refs(
        &mut self,
        doc_id: DocId,
        doc_ord: u32,
        output: &mut Vec<Arc<[JsonArrayPathEntry]>>,
    ) -> bool {
        match &mut self.json_metadata {
            JsonMetadata::None => {
                output.clear();
                false
            }
            JsonMetadata::Indexed(indexed) => indexed.fill(doc_id, doc_ord, output),
        }
    }
}

fn read_vint_and_update(cursor: &mut &[u8], consumed: &mut usize) -> u32 {
    let before = cursor.len();
    let val = read_u32_vint(cursor);
    *consumed += before - cursor.len();
    val
}

fn read_byte_and_update(cursor: &mut &[u8], consumed: &mut usize) -> u8 {
    let byte = cursor[0];
    *cursor = &cursor[1..];
    *consumed += 1;
    byte
}

fn parse_json_metadata(
    metadata_bytes: OwnedBytes,
    global_paths: Option<Arc<Vec<Arc<[JsonArrayPathEntry]>>>>,
) -> io::Result<JsonMetadata> {
    if metadata_bytes.is_empty() {
        return Ok(JsonMetadata::None);
    }
    let mut cursor = metadata_bytes.as_slice();
    let mut consumed = 0usize;
    let header = read_vint_and_update(&mut cursor, &mut consumed);
    let Some(global_paths) = global_paths else {
        return Ok(JsonMetadata::None);
    };
    if header != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported JSON metadata version",
        ));
    }

    let num_docs = read_vint_and_update(&mut cursor, &mut consumed) as usize;
    if num_docs == 0 {
        return Ok(JsonMetadata::None);
    }
    let counts = decode_bitpacked_values(&metadata_bytes, &mut cursor, &mut consumed, num_docs)?;
    if counts.iter().all(|count| *count == 0) {
        return Ok(JsonMetadata::None);
    }
    let total_indexes = read_vint_and_update(&mut cursor, &mut consumed) as usize;
    if total_indexes == 0 {
        return Ok(JsonMetadata::None);
    }
    let indexes = JsonIndexBlocks::parse(&mut cursor, &mut consumed, total_indexes)?;
    let mut prefix_sums = Vec::with_capacity(counts.len());
    let mut sum = 0u32;
    for count in &counts {
        sum += *count;
        prefix_sums.push(sum);
    }
    debug_assert_eq!(sum as usize, total_indexes);
    Ok(JsonMetadata::Indexed(Box::new(JsonIndexedMetadata {
        counts,
        prefix_sums,
        indexes,
        scratch: Vec::new(),
        global_paths,
    })))
}

fn decode_bitpacked_values(
    metadata_bytes: &OwnedBytes,
    cursor: &mut &[u8],
    consumed: &mut usize,
    num_values: usize,
) -> io::Result<Vec<u32>> {
    let num_blocks = read_vint_and_update(cursor, consumed) as usize;
    let mut bit_widths = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        bit_widths.push(read_byte_and_update(cursor, consumed));
    }
    let mut block_offsets = Vec::with_capacity(num_blocks);
    let mut total_block_bytes = 0usize;
    for &bit_width in &bit_widths {
        block_offsets.push(total_block_bytes);
        total_block_bytes += (bit_width as usize * COMPRESSION_BLOCK_SIZE) / 8;
    }
    let block_data = metadata_bytes.slice(*consumed..*consumed + total_block_bytes);
    *cursor = &cursor[total_block_bytes..];
    *consumed += total_block_bytes;
    let remainder = num_values % COMPRESSION_BLOCK_SIZE;
    let mut remainder_values = Vec::with_capacity(remainder);
    for _ in 0..remainder {
        remainder_values.push(read_vint_and_update(cursor, consumed));
    }
    let mut values = Vec::with_capacity(num_values);
    let mut decoder = BlockDecoder::default();
    let data_slice = block_data.as_slice();
    for (block_idx, &bit_width) in bit_widths.iter().enumerate() {
        let offset = block_offsets[block_idx];
        if bit_width == 0 {
            values.extend(std::iter::repeat_n(0u32, COMPRESSION_BLOCK_SIZE));
        } else {
            decoder.uncompress_block_unsorted(
                &data_slice[offset..offset + (bit_width as usize * COMPRESSION_BLOCK_SIZE / 8)],
                bit_width,
                false,
            );
            values.extend_from_slice(decoder.output_array());
        }
    }
    values.truncate(num_values - remainder);
    values.extend(remainder_values);
    Ok(values)
}

#[derive(Clone)]
enum JsonMetadata {
    /// No metadata stored for this term.
    None,
    /// Per-doc list of path ids compressed in blocks.
    Indexed(Box<JsonIndexedMetadata>),
}

impl JsonMetadata {
    fn reset(&mut self) {
        if let JsonMetadata::Indexed(indexed) = self {
            indexed.reset();
        }
    }
}

#[derive(Clone)]
struct JsonIndexedMetadata {
    // Number of paths per doc and prefix sums allow slicing into the flat index stream.
    counts: Vec<u32>,
    prefix_sums: Vec<u32>,
    indexes: JsonIndexBlocks,
    scratch: Vec<u32>,
    global_paths: Arc<Vec<Arc<[JsonArrayPathEntry]>>>,
}

impl JsonIndexedMetadata {
    fn fill(
        &mut self,
        _doc_id: DocId,
        doc_ord: u32,
        output: &mut Vec<Arc<[JsonArrayPathEntry]>>,
    ) -> bool {
        let doc_ord = doc_ord as usize;
        if doc_ord >= self.counts.len() {
            output.clear();
            return false;
        }
        let count = self.counts[doc_ord] as usize;
        if count == 0 {
            output.clear();
            return false;
        }
        let start = if doc_ord == 0 {
            0
        } else {
            self.prefix_sums[doc_ord - 1] as usize
        };
        self.indexes.read_range(start, count, &mut self.scratch);
        output.clear();
        for idx in &self.scratch {
            if *idx == 0 {
                continue;
            }
            if let Some(path) = self.global_paths.get(*idx as usize) {
                output.push(path.clone());
            }
        }
        !output.is_empty()
    }

    fn reset(&mut self) {
        // nothing to reset
    }
}

#[derive(Clone)]
struct JsonIndexBlocks {
    bit_widths: Vec<u8>,
    block_offsets: Vec<usize>,
    blocks_data: OwnedBytes,
    tail_values: Vec<u32>,
    block_decoder: BlockDecoder,
    decoded_block: Vec<u32>,
    decoded_block_idx: Option<usize>,
}

impl JsonIndexBlocks {
    fn parse(
        cursor: &mut &[u8],
        consumed: &mut usize,
        total_indexes: usize,
    ) -> io::Result<JsonIndexBlocks> {
        let num_blocks = read_vint_and_update(cursor, consumed) as usize;
        let mut bit_widths = Vec::with_capacity(num_blocks);
        for _ in 0..num_blocks {
            bit_widths.push(read_byte_and_update(cursor, consumed));
        }
        let mut block_offsets = Vec::with_capacity(num_blocks);
        let mut total_block_bytes = 0usize;
        for &bit_width in &bit_widths {
            block_offsets.push(total_block_bytes);
            total_block_bytes += (bit_width as usize * COMPRESSION_BLOCK_SIZE) / 8;
        }
        let block_data = OwnedBytes::new(cursor[..total_block_bytes].to_vec());
        *cursor = &cursor[total_block_bytes..];
        *consumed += total_block_bytes;
        let remainder = total_indexes % COMPRESSION_BLOCK_SIZE;
        let mut tail_values = Vec::with_capacity(remainder);
        for _ in 0..remainder {
            tail_values.push(read_vint_and_update(cursor, consumed));
        }
        Ok(JsonIndexBlocks {
            bit_widths,
            block_offsets,
            blocks_data: block_data,
            tail_values,
            block_decoder: BlockDecoder::default(),
            decoded_block: vec![0u32; COMPRESSION_BLOCK_SIZE],
            decoded_block_idx: None,
        })
    }

    fn read_range(&mut self, start: usize, len: usize, output: &mut Vec<u32>) {
        output.clear();
        if len == 0 {
            return;
        }
        let tail_start = self.bit_widths.len() * COMPRESSION_BLOCK_SIZE;
        let mut offset = start;
        let mut remaining = len;
        while remaining > 0 {
            if offset >= tail_start {
                let tail_idx = offset - tail_start;
                let take = remaining.min(self.tail_values.len().saturating_sub(tail_idx));
                output.extend_from_slice(&self.tail_values[tail_idx..tail_idx + take]);
                offset += take;
                remaining -= take;
                continue;
            }
            let block_idx = offset / COMPRESSION_BLOCK_SIZE;
            self.ensure_block(block_idx);
            let within_block = offset % COMPRESSION_BLOCK_SIZE;
            let take = remaining.min(COMPRESSION_BLOCK_SIZE - within_block);
            output.extend_from_slice(&self.decoded_block[within_block..within_block + take]);
            offset += take;
            remaining -= take;
        }
    }

    fn ensure_block(&mut self, block_idx: usize) {
        if self.decoded_block_idx == Some(block_idx) {
            return;
        }
        if block_idx >= self.bit_widths.len() {
            return;
        }
        let bit_width = self.bit_widths[block_idx];
        if bit_width == 0 {
            self.decoded_block.fill(0u32);
            self.decoded_block_idx = Some(block_idx);
            return;
        }
        let start = self.block_offsets[block_idx];
        let num_bytes = (bit_width as usize * COMPRESSION_BLOCK_SIZE) / 8;
        let data = &self.blocks_data.as_slice()[start..start + num_bytes];
        self.block_decoder
            .uncompress_block_unsorted(data, bit_width, false);
        self.decoded_block
            .copy_from_slice(self.block_decoder.output_array());
        self.decoded_block_idx = Some(block_idx);
    }
}

#[cfg(test)]
mod tests {
    use common::write_u32_vint;

    use super::*;
    use crate::positions::serializer::PositionSerializer;

    #[test]
    fn test_position_reader_bitpacked_zero_and_remainder_with_metadata() {
        // Build positions with one full zero block (bit_width = 0) and a short remainder.
        let mut buf = Vec::new();
        {
            let mut serializer = PositionSerializer::new(&mut buf);
            serializer.write_positions_delta(&vec![0u32; COMPRESSION_BLOCK_SIZE]);
            serializer.write_positions_delta(&[1u32, 2u32]);
            serializer.close_term().unwrap();
        }

        // Manually append minimal JSON metadata for 1 doc with 1 path id (=1).
        let mut metadata = Vec::new();
        write_u32_vint(1, &mut metadata).unwrap(); // version
        write_u32_vint(1, &mut metadata).unwrap(); // num_docs
        write_u32_vint(0, &mut metadata).unwrap(); // counts num_blocks
        write_u32_vint(1, &mut metadata).unwrap(); // counts remainder
        write_u32_vint(1, &mut metadata).unwrap(); // total_indexes
        write_u32_vint(0, &mut metadata).unwrap(); // indexes num_blocks
        write_u32_vint(1, &mut metadata).unwrap(); // index value
        buf.extend_from_slice(&metadata);
        buf.push(JSON_METADATA_MARKER);
        buf.extend_from_slice(&(metadata.len() as u32).to_be_bytes());

        let path_table = Arc::new(vec![
            Arc::from(Vec::<JsonArrayPathEntry>::new().into_boxed_slice()),
            Arc::from(
                vec![JsonArrayPathEntry {
                    path_id: 1,
                    element_ord: 0,
                }]
                .into_boxed_slice(),
            ),
        ]);

        let mut reader = PositionReader::open(OwnedBytes::new(buf), Some(path_table)).unwrap();
        assert!(reader.has_json_metadata());

        let mut positions = vec![0u32; COMPRESSION_BLOCK_SIZE + 2];
        reader.read(0, &mut positions);
        let mut expected = vec![0u32; COMPRESSION_BLOCK_SIZE];
        expected.extend_from_slice(&[1u32, 2u32]);
        assert_eq!(positions, expected);

        let mut paths = Vec::new();
        assert!(reader.fill_doc_json_metadata_refs(0, 0, &mut paths));
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0][0].path_id, 1);
        assert_eq!(paths[0][0].element_ord, 0);
    }
}
