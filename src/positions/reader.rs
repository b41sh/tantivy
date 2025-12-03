use std::convert::TryInto;
use std::io;

use common::json_path_writer::JsonArrayPathEntry;
use common::{read_u32_vint, BinarySerializable, VInt};

use crate::directory::OwnedBytes;
use crate::positions::{
    COMPRESSION_BLOCK_SIZE, JSON_METADATA_FLAG_BITMAP, JSON_METADATA_FLAG_SINGLE_PATH,
    JSON_METADATA_FLAG_TWO_PATHS, JSON_METADATA_MARKER,
};
use crate::postings::compression::{BlockDecoder, VIntDecoder};

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
    json_paths: Vec<Vec<JsonArrayPathEntry>>,
    json_doc_mappings: Vec<Vec<u32>>,
    json_doc_cursor: usize,
    last_metadata: Vec<Vec<JsonArrayPathEntry>>,

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
}

impl PositionReader {
    /// Open and reads the term positions encoded into the positions_data owned bytes.
    pub fn open(mut positions_data: OwnedBytes) -> io::Result<PositionReader> {
        let num_positions_bitpacked_blocks = VInt::deserialize(&mut positions_data)?.0 as usize;
        let (bit_widths, positions) = positions_data.split(num_positions_bitpacked_blocks);
        let mut json_paths = Vec::new();
        let mut json_doc_mappings = Vec::new();
        let mut positions = positions;
        if positions.len() > 5 {
            let slice = positions.as_slice();
            let marker_idx = slice.len() - 5;
            if slice[marker_idx] == JSON_METADATA_MARKER {
                let meta_len = u32::from_be_bytes(
                    slice[marker_idx + 1..marker_idx + 5]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if marker_idx >= meta_len {
                    let metadata_slice =
                        &slice[marker_idx - meta_len..marker_idx];
                    let mut cursor = metadata_slice;
                    if !cursor.is_empty() {
                        let path_count = read_u32_vint(&mut cursor) as usize;
                        json_paths.reserve(path_count);
                        for _ in 0..path_count {
                            let depth = read_u32_vint(&mut cursor) as usize;
                            let mut path = Vec::with_capacity(depth);
                            for _ in 0..depth {
                                let path_id = read_u32_vint(&mut cursor);
                                let element_ord = read_u32_vint(&mut cursor);
                                path.push(JsonArrayPathEntry {
                                    path_id,
                                    element_ord,
                                });
                            }
                            json_paths.push(path);
                        }
                        let doc_count = read_u32_vint(&mut cursor) as usize;
                        json_doc_mappings.reserve(doc_count);
                        if path_count == 1 {
                            for _ in 0..doc_count {
                                json_doc_mappings.push(vec![0u32]);
                            }
                        } else if path_count == 2 {
                            let mut processed = 0;
                            while processed < doc_count {
                                let word = read_u32_vint(&mut cursor);
                                for shift in 0..16 {
                                    if processed == doc_count {
                                        break;
                                    }
                                    let state = (word >> (shift * 2)) & 0b11;
                                    let mut indexes = Vec::new();
                                    if state & 0b01 != 0 {
                                        indexes.push(0u32);
                                    }
                                    if state & 0b10 != 0 {
                                        indexes.push(1u32);
                                    }
                                    json_doc_mappings.push(indexes);
                                    processed += 1;
                                }
                            }
                        } else if path_count <= 4 {
                            let bits_per_doc = 4;
                            let docs_per_byte = 8 / bits_per_doc;
                            let docs_per_byte = docs_per_byte.max(1);
                            let mut processed = 0;
                            while processed < doc_count {
                                let chunk = read_u32_vint(&mut cursor) as u8;
                                for i in 0..docs_per_byte {
                                    if processed == doc_count {
                                        break;
                                    }
                                    let mask =
                                        (chunk >> (i * bits_per_doc)) & ((1 << bits_per_doc) - 1);
                                    let mut indexes = Vec::new();
                                    if mask & 0x1 != 0 {
                                        indexes.push(0u32);
                                    }
                                    if mask & 0x2 != 0 {
                                        indexes.push(1u32);
                                    }
                                    if mask & 0x4 != 0 {
                                        indexes.push(2u32);
                                    }
                                    if mask & 0x8 != 0 {
                                        indexes.push(3u32);
                                    }
                                    json_doc_mappings.push(indexes);
                                    processed += 1;
                                }
                            }
                        } else if path_count <= 32 {
                            for _ in 0..doc_count {
                                let bitmap = read_u32_vint(&mut cursor);
                                let mut indexes = Vec::new();
                                for idx in 0..path_count.min(32) {
                                    if (bitmap >> idx) & 1 == 1 {
                                        indexes.push(idx as u32);
                                    }
                                }
                                json_doc_mappings.push(indexes);
                            }
                        } else {
                            for _ in 0..doc_count {
                                let num_paths =
                                    read_u32_vint(&mut cursor) as usize;
                                let mut indexes =
                                    Vec::with_capacity(num_paths);
                                for _ in 0..num_paths {
                                    indexes.push(read_u32_vint(&mut cursor));
                                }
                                json_doc_mappings.push(indexes);
                            }
                        }
                    }
                    positions = positions.slice(0..marker_idx - meta_len);
                }
            }
        }
        Ok(PositionReader {
            bit_widths: bit_widths.clone(),
            positions: positions.clone(),
            json_paths,
            json_doc_mappings,
            json_doc_cursor: 0,
            last_metadata: Vec::new(),
            block_decoder: BlockDecoder::default(),
            block_offset: i64::MAX as u64,
            anchor_offset: 0u64,
            original_bit_widths: bit_widths,
            original_positions: positions,
        })
    }

    fn reset(&mut self) {
        self.positions = self.original_positions.clone();
        self.bit_widths = self.original_bit_widths.clone();
        self.block_offset = i64::MAX as u64;
        self.anchor_offset = 0u64;
        self.json_doc_cursor = 0;
        self.last_metadata.clear();
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
        let requested_offset = offset;
        let requested_len = output.len();
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
        self.fill_last_metadata(requested_offset, requested_len);
    }

    pub fn has_json_metadata(&self) -> bool {
        !self.json_paths.is_empty() && !self.json_doc_mappings.is_empty()
    }

    fn fill_last_metadata(&mut self, _offset: u64, _num_positions: usize) {
        if self.json_paths.is_empty() {
            self.last_metadata.clear();
            return;
        }
        if self.json_doc_cursor >= self.json_doc_mappings.len() {
            self.disable_metadata();
            return;
        }
        self.last_metadata.clear();
        let doc_paths = &self.json_doc_mappings[self.json_doc_cursor];
        for &path_idx in doc_paths {
            if let Some(path) = self.json_paths.get(path_idx as usize) {
                self.last_metadata.push(path.clone());
            } else {
                self.disable_metadata();
                return;
            }
        }
        self.json_doc_cursor += 1;
    }

    fn disable_metadata(&mut self) {
        self.json_paths.clear();
        self.json_doc_mappings.clear();
        self.last_metadata.clear();
        self.json_doc_cursor = 0;
    }

    pub fn last_json_metadata(&self) -> Option<&[Vec<JsonArrayPathEntry>]> {
        if self.last_metadata.is_empty() {
            None
        } else {
            Some(&self.last_metadata)
        }
    }
}
