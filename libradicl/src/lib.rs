/*
 * Copyright (c) 2020-2021 Rob Patro, Avi Srivastava, Hirak Sarkar, Dongze He, Mohsen Zakeri.
 *
 * This file is part of alevin-fry
 * (see https://github.com/COMBINE-lab/alevin-fry).
 *
 * License: 3-clause BSD, see https://opensource.org/licenses/BSD-3-Clause
 */

// scroll now, explore nom later
extern crate needletail;
extern crate num;
extern crate quickersort;
extern crate rust_htslib;
extern crate sce;
extern crate scroll;
use crate as libradicl;

use self::libradicl::schema::TempCellInfo;
use self::libradicl::utils::{MASK_LOWER_31_U32, MASK_TOP_BIT_U32};
#[allow(unused_imports)]
use ahash::{AHasher, RandomState};
use bio_types::strand::*;
use dashmap::DashMap;
use needletail::bitkmer::*;
use num::cast::AsPrimitive;
use rust_htslib::bam::HeaderView;
use scroll::Pread;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::io::{BufWriter, Write};
use std::mem;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

pub mod cellfilter;
pub mod collate;
pub mod convert;
pub mod em;
pub mod exit_codes;
pub mod infer;
pub mod pugutils;
pub mod quant;
pub mod schema;
pub mod utils;

// Name of the program, to be used in diagnostic messages.
static LIB_NAME: &str = "libradicl";

pub fn lib_name() -> &'static str {
    LIB_NAME
}

pub struct RadHeader {
    pub is_paired: u8,
    pub ref_count: u64,
    pub ref_names: Vec<String>,
    pub num_chunks: u64,
}

pub struct TagDesc {
    pub name: String,
    pub typeid: u8,
}

pub struct TagSection {
    pub tags: Vec<TagDesc>,
}

// The below are currently hard-coded
// until we decide how to solve this
// generally
#[derive(Debug)]
pub struct FileTags {
    pub bclen: u16,
    pub umilen: u16,
}
#[derive(Debug)]
pub struct ReadRecord {
    pub bc: u64,
    pub umi: u64,
    pub dirs: Vec<bool>,
    pub refs: Vec<u32>,
}
#[derive(Debug)]
pub struct Chunk {
    pub nbytes: u32,
    pub nrec: u32,
    pub reads: Vec<ReadRecord>,
}

#[derive(Debug)]
pub struct CorrectedCbChunk {
    remaining_records: u32,
    corrected_bc: u64,
    nrec: u32,
    data: Cursor<Vec<u8>>, /*,
                           umis: Vec<u64>,
                           ref_offsets: Vec<u32>,
                           ref_ids: Vec<u32>,
                           */
}

#[derive(Serialize, Deserialize, Debug)]
#[allow(dead_code)]
pub struct BarcodeLookupMap {
    pub barcodes: Vec<u64>,
    //pub counts: Vec<usize>,
    offsets: Vec<usize>,
    bclen: u32,
    prefix_len: u32,
    suffix_len: u32,
}

impl BarcodeLookupMap {
    pub fn new(mut kv: Vec<u64>, bclen: u32) -> BarcodeLookupMap {
        let prefix_len = ((bclen + 1) / 2) as u64;
        let suffix_len = bclen - prefix_len as u32;

        let prefix_bits = 2 * prefix_len;
        let suffix_bits = 2 * suffix_len;

        kv.sort_unstable();

        let pref_mask = ((4usize.pow(prefix_len as u32) - 1) as u64) << (suffix_bits);
        let mut offsets = vec![0; 4usize.pow(prefix_len as u32) + 1];
        let mut prev_ind = 0xFFFF;

        for (n, &v) in kv.iter().enumerate() {
            let ind = ((v & pref_mask) >> (prefix_bits)) as usize;
            if ind != prev_ind {
                for item in offsets.iter_mut().take(ind).skip(prev_ind + 1) {
                    *item = n;
                }
                offsets[ind] = n;
                prev_ind = ind;
            }
        }
        for item in offsets.iter_mut().skip(prev_ind + 1) {
            *item = kv.len();
        }

        //let nbc = kv.len();
        BarcodeLookupMap {
            barcodes: kv,
            //counts: vec![0usize; nbc],
            offsets,
            bclen,
            prefix_len: prefix_len as u32,
            suffix_len,
        }
    }

    #[allow(dead_code)]
    pub fn barcode_for_idx(&self, idx: usize) -> u64 {
        self.barcodes[idx]
    }

    pub fn find_exact(&self, query: u64) -> Option<usize> {
        let mut ret: Option<usize> = None;

        // extract the prefix we will use to search
        let suffix_bits = 2 * self.suffix_len;
        let query_pref = query >> suffix_bits;

        // the range of entries having query_pref as their prefix
        let qrange = std::ops::Range {
            start: self.offsets[query_pref as usize],
            end: self.offsets[(query_pref + 1) as usize],
        };

        let qs = qrange.start as usize;

        // if we can, then we return the found barcode and that there was 1 best hit
        if let Ok(res) = self.barcodes[qrange].binary_search(&query) {
            ret = Some(qs + res);
        }
        ret
    }

    /// The find function searches for the barcode `query` in the
    /// BarcodeLookupMap.  It returns a tuple `(Option<usize>, usize)` where
    /// the first element is either Some(usize) or None.  If
    /// Some(usize) is returned, this is the *index* of a matching/neighboring barcode
    /// if None is returned, then no match was found.  The second element is either
    /// 0, 1 or 2.  If 0, no match was found; if 1 a unique match was found, if 2
    /// then 2 or more equally good matches were found.
    ///
    /// The parameter `try_exact` controls whether a an exact search is performed
    /// or not.  If this parameter is true, an exact search is performed before
    /// a neighbor search.  Otherwise, the exact search is skipped.
    pub fn find_neighbors(&self, query: u64, try_exact: bool) -> (Option<usize>, usize) {
        let mut ret: Option<usize> = None;

        // extract the prefix we will use to search
        let pref_bits = 2 * self.prefix_len;
        let suffix_bits = 2 * self.suffix_len;
        let mut query_pref = query >> suffix_bits;
        let mut num_neighbors = 0usize;

        // the range of entries having query_pref as their prefix
        let qrange = std::ops::Range {
            start: self.offsets[query_pref as usize],
            end: self.offsets[(query_pref + 1) as usize],
        };

        let qs = qrange.start as usize;

        if try_exact {
            // first, we try to find exactly.
            // if we can, then we return the found barcode and that there was 1 best hit
            if let Ok(res) = self.barcodes[qrange.clone()].binary_search(&query) {
                ret = Some(qs + res);
                num_neighbors += 1;
                return (ret, num_neighbors);
            }
        }

        // othwerwise, we fall back to the 1 mismatch search
        // NOTE: We stop here as soon as we find at most 2 neighbors
        // for the query.  Thus, we only distinguish between the
        // the cases where the query has 1 neighbor, or 2 or more neighbors.

        // if we match the prefix exactly, we will look for possible matches
        // that are 1 mismatch off in the suffix.
        if !(std::ops::Range::<usize>::is_empty(&qrange)) {
            // the initial offset of suffixes for this prefix
            let qs = qrange.start as usize;

            // for each position in the suffix
            for i in (0..suffix_bits).step_by(2) {
                let bit_mask = 3 << (i);

                // for each nucleotide
                for nmod in 1..4 {
                    let nucl = 0x3 & ((query >> i) + nmod);
                    let nquery = (query & (!bit_mask)) | (nucl << i);

                    if let Ok(res) = self.barcodes[qrange.clone()].binary_search(&nquery) {
                        ret = Some(qs + res);
                        num_neighbors += 1;
                        if num_neighbors >= 2 {
                            return (ret, num_neighbors);
                        }
                    }
                }
            }
        }

        {
            // if we get here we've had either 0 or 1 matches holding the prefix fixed
            // so we will now hold the suffix fixed and consider possible mutations of the prefix.

            // for each position in the prefix
            for i in (suffix_bits..(suffix_bits + pref_bits)).step_by(2) {
                let bit_mask = 3 << i;

                // for each nucleotide
                for nmod in 1..4 {
                    let nucl = 0x3 & ((query >> i) + nmod);
                    let nquery = (query & (!bit_mask)) | (nucl << i);

                    query_pref = nquery >> suffix_bits;

                    let qrange = std::ops::Range {
                        start: self.offsets[query_pref as usize],
                        end: self.offsets[(query_pref + 1) as usize],
                    };
                    let qs = qrange.start as usize;
                    if let Ok(res) = self.barcodes[qrange].binary_search(&nquery) {
                        ret = Some(qs + res);
                        num_neighbors += 1;
                        if num_neighbors >= 2 {
                            return (ret, num_neighbors);
                        }
                    }
                }
            }
        }

        (ret, num_neighbors)
    }
}

impl CorrectedCbChunk {
    pub fn from_label_and_counter(corrected_bc_in: u64, num_remain: u32) -> CorrectedCbChunk {
        let mut cc = CorrectedCbChunk {
            remaining_records: num_remain,
            corrected_bc: corrected_bc_in,
            nrec: 0u32,
            data: Cursor::new(Vec::<u8>::with_capacity((num_remain * 24) as usize))
            //umis: Vec::<u64>::with_capacity(num_remain as usize),
            //ref_offsets: Vec::<u32>::with_capacity(num_remain as usize),
            //ref_ids: Vec::<u32>::with_capacity(3 * num_remain as usize),
        };
        let dummy = 0u32;
        cc.data.write_all(&dummy.to_le_bytes()).unwrap();
        cc.data.write_all(&dummy.to_le_bytes()).unwrap();
        cc
    }
}

#[derive(Debug, Clone)]
pub struct GlobalEqCellList {
    cell_ids: Vec<usize>,
    count: u32,
}

impl GlobalEqCellList {
    pub fn from_umi_and_count(bc_mer: usize, count: u32) -> GlobalEqCellList {
        let mut cc = GlobalEqCellList {
            cell_ids: Vec::new(),
            count: 0,
        };
        cc.cell_ids.push(bc_mer);
        cc.count += count;
        cc
    }
    pub fn add_element(&mut self, bc_mer: usize, count: u32) {
        self.cell_ids.push(bc_mer);
        self.count += count;
    }
}

#[derive(Copy, Clone)]
pub enum RadIntId {
    U8,
    U16,
    U32,
    U64,
}

pub trait PrimitiveInteger:
    AsPrimitive<u8>
    + AsPrimitive<u16>
    + AsPrimitive<u32>
    + AsPrimitive<u64>
    + AsPrimitive<usize>
    + AsPrimitive<i8>
    + AsPrimitive<i16>
    + AsPrimitive<i32>
    + AsPrimitive<i64>
    + AsPrimitive<isize>
{
}

impl<
        T: AsPrimitive<u8>
            + AsPrimitive<u16>
            + AsPrimitive<u32>
            + AsPrimitive<u64>
            + AsPrimitive<usize>
            + AsPrimitive<i8>
            + AsPrimitive<i16>
            + AsPrimitive<i32>
            + AsPrimitive<i64>
            + AsPrimitive<isize>,
    > PrimitiveInteger for T
{
}

impl RadIntId {
    pub fn bytes_for_type(&self) -> usize {
        match self {
            Self::U8 => std::mem::size_of::<u8>(),
            Self::U16 => std::mem::size_of::<u16>(),
            Self::U32 => std::mem::size_of::<u32>(),
            Self::U64 => std::mem::size_of::<u64>(),
        }
    }

    /// Based on the variant of the current enum, write the value `v`
    /// out using `owrite`.  Here, `v` is bound to be some primitive
    /// integer type.  It is the responsibility of the caller to ensure
    /// that, if `v` is wider than the enum type on which this function
    /// is called, no important information is lost by discarding the higher
    /// order bits.
    pub fn write_to<T: PrimitiveInteger, U: Write>(
        &self,
        v: T,
        owriter: &mut U,
    ) -> std::io::Result<()> {
        match self {
            Self::U8 => {
                let vo: u8 = v.as_();
                owriter.write_all(&vo.to_le_bytes())
            }
            Self::U16 => {
                let vo: u16 = v.as_();
                owriter.write_all(&vo.to_le_bytes())
            }
            Self::U32 => {
                let vo: u32 = v.as_();
                owriter.write_all(&vo.to_le_bytes())
            }
            Self::U64 => {
                let vo: u64 = v.as_();
                owriter.write_all(&vo.to_le_bytes())
            }
        }
    }
}

pub struct ChunkConfig {
    pub num_chunks: u64,
    pub bc_type: u8,
    pub umi_type: u8,
}

#[derive(Copy, Clone)]
pub enum RadType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

pub fn encode_type_tag(type_tag: RadType) -> Option<u8> {
    match type_tag {
        RadType::Bool => Some(0),
        RadType::U8 => Some(1),
        RadType::U16 => Some(2),
        RadType::U32 => Some(3),
        RadType::U64 => Some(4),
        RadType::F32 => Some(5),
        RadType::F64 => Some(6),
        //_ => None,
    }
}

pub fn decode_int_type_tag(type_id: u8) -> Option<RadIntId> {
    match type_id {
        1 => Some(RadIntId::U8),
        2 => Some(RadIntId::U16),
        3 => Some(RadIntId::U32),
        4 => Some(RadIntId::U64),
        _ => None,
    }
}

/*
pub fn collect_records<T: Read>(
    reader: &mut BufReader<T>,
    chunk_config: &ChunkConfig,
    correct_map: &HashMap<u64, u64>,
    expected_ori: &Strand,
    output_cache: &DashMap<u64, CorrectedCBChunk>,
) {
    // NOTE: since the chunks are independent, this part could be multithreaded
    let bc_type = decode_int_type_tag(chunk_config.bc_type).expect("unknown barcode type id.");
    let umi_type = decode_int_type_tag(chunk_config.umi_type).expect("unknown barcode type id.");

    for _ in 0..(chunk_config.num_chunks as usize) {
        process_corrected_cb_chunk(
            reader,
            &bc_type,
            &umi_type,
            correct_map,
            expected_ori,
            output_cache,
        );
    }
}
*/

fn read_into_u64<T: Read>(reader: &mut T, rt: &RadIntId) -> u64 {
    let mut rbuf = [0u8; 8];
    let v: u64;
    match rt {
        RadIntId::U8 => {
            reader.read_exact(&mut rbuf[0..1]).unwrap();
            v = rbuf.pread::<u8>(0).unwrap() as u64;
        }
        RadIntId::U16 => {
            reader.read_exact(&mut rbuf[0..2]).unwrap();
            v = rbuf.pread::<u16>(0).unwrap() as u64;
        }
        RadIntId::U32 => {
            reader.read_exact(&mut rbuf[0..4]).unwrap();
            v = rbuf.pread::<u32>(0).unwrap() as u64;
        }
        RadIntId::U64 => {
            reader.read_exact(&mut rbuf[0..8]).unwrap();
            v = rbuf.pread::<u64>(0).unwrap();
        }
    }
    v
}

impl ReadRecord {
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    pub fn from_bytes<T: Read>(reader: &mut T, bct: &RadIntId, umit: &RadIntId) -> Self {
        let mut rbuf = [0u8; 255];

        reader.read_exact(&mut rbuf[0..4]).unwrap();
        let na = rbuf.pread::<u32>(0).unwrap();
        let bc = read_into_u64(reader, bct);
        let umi = read_into_u64(reader, umit);

        let mut rec = Self {
            bc,
            umi,
            dirs: Vec::with_capacity(na as usize),
            refs: Vec::with_capacity(na as usize),
        };

        //println!("number of records : {:?}",na);

        for _ in 0..(na as usize) {
            reader.read_exact(&mut rbuf[0..4]).unwrap();
            let v = rbuf.pread::<u32>(0).unwrap();
            let dir = (v & MASK_LOWER_31_U32) != 0;
            rec.dirs.push(dir);
            rec.refs.push(v & MASK_TOP_BIT_U32);
        }

        rec
    }

    pub fn from_bytes_record_header<T: Read>(
        reader: &mut T,
        bct: &RadIntId,
        umit: &RadIntId,
    ) -> (u64, u64, u32) {
        let mut rbuf = [0u8; 4];
        reader.read_exact(&mut rbuf).unwrap();
        let na = u32::from_le_bytes(rbuf); //.pread::<u32>(0).unwrap();
        let bc = read_into_u64(reader, bct);
        let umi = read_into_u64(reader, umit);
        (bc, umi, na)
    }

    pub fn from_bytes_with_header_keep_ori<T: Read>(
        reader: &mut T,
        bc: u64,
        umi: u64,
        na: u32,
        expected_ori: &Strand,
    ) -> Self {
        let mut rbuf = [0u8; 255];
        let mut rec = Self {
            bc,
            umi,
            dirs: Vec::with_capacity(na as usize),
            refs: Vec::with_capacity(na as usize),
        };

        for _ in 0..(na as usize) {
            reader.read_exact(&mut rbuf[0..4]).unwrap();
            let v = rbuf.pread::<u32>(0).unwrap();

            // fw if the leftmost bit is 1, otherwise rc
            let strand = if (v & utils::MASK_LOWER_31_U32) > 0 {
                Strand::Forward
            } else {
                Strand::Reverse
            };

            if expected_ori.same(&strand) || expected_ori.is_unknown() {
                rec.refs.push(v & utils::MASK_TOP_BIT_U32);
            }
        }

        // make sure these are sorted in this step.
        quickersort::sort(&mut rec.refs[..]);
        rec
    }

    pub fn from_bytes_keep_ori<T: Read>(
        reader: &mut T,
        bct: &RadIntId,
        umit: &RadIntId,
        expected_ori: &Strand,
    ) -> Self {
        let mut rbuf = [0u8; 255];

        reader.read_exact(&mut rbuf[0..4]).unwrap();
        let na = rbuf.pread::<u32>(0).unwrap();

        let bc = read_into_u64(reader, bct);
        let umi = read_into_u64(reader, umit);

        let mut rec = Self {
            bc,
            umi,
            dirs: Vec::with_capacity(na as usize),
            refs: Vec::with_capacity(na as usize),
        };

        for _ in 0..(na as usize) {
            reader.read_exact(&mut rbuf[0..4]).unwrap();
            let v = rbuf.pread::<u32>(0).unwrap();

            // fw if the leftmost bit is 1, otherwise rc
            let strand = if (v & utils::MASK_LOWER_31_U32) > 0 {
                Strand::Forward
            } else {
                Strand::Reverse
            };

            if expected_ori.same(&strand) || expected_ori.is_unknown() {
                rec.refs.push(v & utils::MASK_TOP_BIT_U32);
            }
        }

        // make sure these are sorted in this step.
        quickersort::sort(&mut rec.refs[..]);
        rec
    }
}

#[inline]
pub fn dump_chunk(v: &mut CorrectedCbChunk, owriter: &Mutex<BufWriter<File>>) {
    v.data.set_position(0);
    let nbytes = (v.data.get_ref().len()) as u32;
    let nrec = v.nrec;
    v.data.write_all(&nbytes.to_le_bytes()).unwrap();
    v.data.write_all(&nrec.to_le_bytes()).unwrap();
    owriter.lock().unwrap().write_all(v.data.get_ref()).unwrap();
}

pub fn collate_temporary_bucket_twopass<T: Read + Seek, U: Write>(
    reader: &mut BufReader<T>,
    bct: &RadIntId,
    umit: &RadIntId,
    nrec: u32,
    owriter: &Mutex<U>,
    compress: bool,
    cb_byte_map: &mut HashMap<u64, TempCellInfo, ahash::RandomState>,
) -> usize {
    let mut tbuf = vec![0u8; 65536];
    let mut total_bytes = 0usize;
    let header_size = 2 * std::mem::size_of::<u32>() as u64;
    let size_of_u32 = std::mem::size_of::<u32>();
    let size_of_bc = bct.bytes_for_type();
    let size_of_umi = umit.bytes_for_type();

    let calc_record_bytes = |num_aln: usize| -> usize {
        size_of_u32 + size_of_bc + size_of_umi + (size_of_u32 * num_aln)
    };

    // read each record
    for _ in 0..(nrec as usize) {
        // read the header of the record
        // we don't bother reading the whole thing here
        // because we will just copy later as need be
        let tup = ReadRecord::from_bytes_record_header(reader, &bct, &umit);

        // get the entry for this chunk, or create a new one
        let v = cb_byte_map.entry(tup.0).or_insert(TempCellInfo {
            offset: header_size,
            nbytes: header_size as u32,
            nrec: 0_u32,
        });

        // read the alignment records from the input file
        let na = tup.2 as usize;
        let req_size = size_of_u32 * na;
        if tbuf.len() < req_size {
            tbuf.resize(req_size, 0);
        }
        reader.read_exact(&mut tbuf[0..(size_of_u32 * na)]).unwrap();
        // compute the total number of bytes this record requires
        let nbytes = calc_record_bytes(na);
        (*v).offset += nbytes as u64;
        (*v).nbytes += nbytes as u32;
        (*v).nrec += 1;
        total_bytes += nbytes as usize;
    }

    // each cell will have a header (8 bytes each)
    total_bytes += cb_byte_map.len() * header_size as usize;
    let mut output_buffer = Cursor::new(vec![0u8; total_bytes]);

    let mut next_offset = 0u64;
    for (_, v) in cb_byte_map.iter_mut() {
        // jump to the position where this chunk should start
        // and write the header
        output_buffer.set_position(next_offset);
        let cell_bytes = (*v).nbytes as u32;
        let cell_rec = (*v).nrec as u32;
        output_buffer.write_all(&cell_bytes.to_le_bytes()).unwrap();
        output_buffer.write_all(&cell_rec.to_le_bytes()).unwrap();
        // where we will start writing records for this cell
        (*v).offset = output_buffer.position();
        // the number of bytes allocated to this chunk
        let nbytes = (*v).nbytes as u64;
        // the next record will start after this one
        next_offset += nbytes;
    }

    // now each key points to where we should write the next record for the CB
    // reset the input pointer
    reader
        .get_mut()
        .seek(SeekFrom::Start(0))
        .expect("could not get read pointer.");

    // for each record, read it
    for _ in 0..(nrec as usize) {
        // read the header of the record
        // we don't bother reading the whole thing here
        // because we will just copy later as need be
        let tup = ReadRecord::from_bytes_record_header(reader, &bct, &umit);

        // get the entry for this chunk, or create a new one
        if let Some(v) = cb_byte_map.get_mut(&tup.0) {
            output_buffer.set_position(v.offset);

            // write the num align
            let na = tup.2 as usize;
            let nau32 = na as u32;
            output_buffer.write_all(&nau32.to_le_bytes()).unwrap();

            // write the corrected barcode
            bct.write_to(tup.0, &mut output_buffer).unwrap();
            umit.write_to(tup.1, &mut output_buffer).unwrap();

            // read the alignment records
            reader
                .read_exact(&mut tbuf[0..(size_of_u32 as usize * na)])
                .unwrap();
            // write them
            output_buffer
                .write_all(&tbuf[..(size_of_u32 as usize * na)])
                .unwrap();

            (*v).offset = output_buffer.position();
        } else {
            panic!("should not have any barcodes we can't find");
        }
    }

    output_buffer.set_position(0);

    if compress {
        // compress the contents of output_buffer to compressed_output
        let mut compressed_output =
            snap::write::FrameEncoder::new(Cursor::new(Vec::<u8>::with_capacity(total_bytes)));
        compressed_output
            .write_all(output_buffer.get_ref())
            .expect("could not compress the output chunk.");

        output_buffer = compressed_output
            .into_inner()
            .expect("couldn't unwrap the FrameEncoder.");
        output_buffer.set_position(0);
    }

    owriter
        .lock()
        .unwrap()
        .write_all(output_buffer.get_ref())
        .unwrap();

    cb_byte_map.len()
}

pub fn collate_temporary_bucket<T: Read>(
    reader: &mut T,
    bct: &RadIntId,
    umit: &RadIntId,
    _nchunks: u32,
    nrec: u32,
    output_cache: &mut HashMap<u64, CorrectedCbChunk, ahash::RandomState>,
) {
    let mut tbuf = [0u8; 65536];
    // estimated average number of records per barcode
    // this is just for trying to pre-allocate buffers
    // right; should not affect correctness
    let est_num_rec = 1; //(nrec / nchunks) + 1;

    // for each record, read it
    for _ in 0..(nrec as usize) {
        // read the header of the record
        // we don't bother reading the whole thing here
        // because we will just copy later as need be
        let tup = ReadRecord::from_bytes_record_header(reader, &bct, &umit);

        // get the entry for this chunk, or create a new one
        let v = output_cache
            .entry(tup.0)
            .or_insert_with(|| CorrectedCbChunk::from_label_and_counter(tup.0, est_num_rec));

        // keep track of the number of records we're writing
        (*v).nrec += 1;
        // write the num align
        let na = tup.2;
        (*v).data.write_all(&na.to_le_bytes()).unwrap();
        // write the corrected barcode
        bct.write_to(tup.0, &mut (*v).data).unwrap();
        umit.write_to(tup.1, &mut (*v).data).unwrap();
        // read the alignment records
        reader.read_exact(&mut tbuf[0..(4 * na as usize)]).unwrap();
        // write them
        (*v).data.write_all(&tbuf[..(4 * na as usize)]).unwrap();
    }
}

pub fn process_corrected_cb_chunk<T: Read>(
    reader: &mut T,
    bct: &RadIntId,
    umit: &RadIntId,
    correct_map: &HashMap<u64, u64>,
    expected_ori: &Strand,
    output_cache: &DashMap<u64, CorrectedCbChunk>,
    owriter: &Mutex<BufWriter<File>>,
) {
    let mut buf = [0u8; 8];
    let mut tbuf = [0u8; 65536];

    // get the number of bytes and records for
    // the next chunk
    reader.read_exact(&mut buf).unwrap();
    let _nbytes = buf.pread::<u32>(0).unwrap();
    let nrec = buf.pread::<u32>(4).unwrap();
    // for each record, read it
    for _ in 0..(nrec as usize) {
        let tup = ReadRecord::from_bytes_record_header(reader, &bct, &umit);
        //let rr = ReadRecord::from_bytes_keep_ori(reader, &bct, &umit, expected_ori);
        // if this record had a correct or correctable barcode
        if let Some(corrected_id) = correct_map.get(&tup.0) {
            let rr = ReadRecord::from_bytes_with_header_keep_ori(
                reader,
                tup.0,
                tup.1,
                tup.2,
                expected_ori,
            );

            if let Some(mut v) = output_cache.get_mut(corrected_id) {
                // update the corresponding corrected chunk entry
                v.remaining_records -= 1;
                let last_record = v.remaining_records == 0;
                // if there are no alignments in the record
                // (potentially b/c of orientation filtering)
                // then don't push info on to the vector.
                if rr.is_empty() {
                    if last_record {
                        dump_chunk(&mut v, owriter);
                    }
                    continue;
                }
                v.nrec += 1;
                let na = rr.refs.len() as u32;
                v.data.write_all(&na.to_le_bytes()).unwrap();
                bct.write_to(*corrected_id, &mut v.data).unwrap();
                umit.write_to(rr.umi, &mut v.data).unwrap();
                v.data.write_all(as_u8_slice(&rr.refs[..])).unwrap();
                if last_record {
                    dump_chunk(&mut v, owriter);
                }
            }
        } else {
            reader
                .read_exact(&mut tbuf[0..(4 * (tup.2 as usize))])
                .unwrap();
        }
    }
}

pub struct TempBucket {
    pub bucket_id: u32,
    pub bucket_writer: Arc<Mutex<BufWriter<File>>>,
    pub num_chunks: u32,
    pub num_records: u32,
    pub num_records_written: AtomicU32,
    pub num_bytes_written: AtomicU64,
}

impl TempBucket {
    pub fn from_id_and_parent(bucket_id: u32, parent: &std::path::Path) -> Self {
        TempBucket {
            bucket_id,
            bucket_writer: Arc::new(Mutex::new(BufWriter::with_capacity(
                4096_usize,
                File::create(parent.join(&format!("bucket_{}.tmp", bucket_id))).unwrap(),
            ))),
            num_chunks: 0u32,
            num_records: 0u32,
            num_records_written: AtomicU32::new(0u32),
            num_bytes_written: AtomicU64::new(0u64),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dump_corrected_cb_chunk_to_temp_file<T: Read>(
    reader: &mut BufReader<T>,
    bct: &RadIntId,
    umit: &RadIntId,
    correct_map: &HashMap<u64, u64>,
    expected_ori: &Strand,
    output_cache: &HashMap<u64, Arc<TempBucket>>,
    local_buffers: &mut [Cursor<&mut [u8]>],
    flush_limit: usize,
) {
    let mut buf = [0u8; 8];
    let mut tbuf = vec![0u8; 4096];
    //let mut tcursor = Cursor::new(tbuf);
    //tcursor.set_position(0);

    // get the number of bytes and records for
    // the next chunk
    reader.read_exact(&mut buf).unwrap();
    let _nbytes = buf.pread::<u32>(0).unwrap();
    let nrec = buf.pread::<u32>(4).unwrap();

    let bc_bytes = bct.bytes_for_type();
    let umi_bytes = umit.bytes_for_type();
    let na_bytes = std::mem::size_of::<u32>();
    let target_id_bytes = std::mem::size_of::<u32>();

    // for each record, read it
    for _ in 0..(nrec as usize) {
        let tup = ReadRecord::from_bytes_record_header(reader, &bct, &umit);

        // if this record had a correct or correctable barcode
        if let Some(corrected_id) = correct_map.get(&tup.0) {
            let rr = ReadRecord::from_bytes_with_header_keep_ori(
                reader,
                tup.0,
                tup.1,
                tup.2,
                expected_ori,
            );

            if rr.is_empty() {
                continue;
            }
            if let Some(v) = output_cache.get(corrected_id) {
                // if this is a valid barcode, then
                // write the corresponding entry to the
                // thread-local buffer for this bucket

                // the total number of bytes this record will take
                let nb = (rr.refs.len() * target_id_bytes + na_bytes + bc_bytes + umi_bytes) as u64;

                // the buffer index for this corrected barcode
                let buffidx = v.bucket_id as usize;
                // the current cursor for this buffer
                let bcursor = &mut local_buffers[buffidx];
                // the current position of the cursor
                let len = bcursor.position() as usize;

                // if writing the next record (nb bytes) will put us over
                // the flush size for the thread-local buffer for this bucket
                // then first flush the buffer to file.
                if len + nb as usize >= flush_limit {
                    let mut filebuf = v.bucket_writer.lock().unwrap();
                    filebuf
                        .write_all(&bcursor.get_ref()[0..len as usize])
                        .unwrap();
                    // and reset the local buffer cursor
                    bcursor.set_position(0);
                }

                // now, write the record to the buffer
                let na = rr.refs.len() as u32;
                bcursor.write_all(&na.to_le_bytes()).unwrap();
                bct.write_to(*corrected_id, bcursor).unwrap();
                umit.write_to(rr.umi, bcursor).unwrap();
                bcursor.write_all(as_u8_slice(&rr.refs[..])).unwrap();

                // update number of written records
                v.num_records_written.fetch_add(1, Ordering::SeqCst);
                // update number of written bytes
                v.num_bytes_written.fetch_add(nb, Ordering::SeqCst);
            }
        } else {
            // in this branch, we don't have access to a correct barcode for
            // what we observed, so we need to discard the remaining part of
            // the record.
            let req_len = target_id_bytes * (tup.2 as usize);
            let do_resize = req_len > tbuf.len();

            if do_resize {
                tbuf.resize(req_len, 0);
            }

            reader
                .read_exact(&mut tbuf[0..(target_id_bytes * (tup.2 as usize))])
                .unwrap();

            if do_resize {
                tbuf.resize(4096, 0);
                tbuf.shrink_to_fit();
            }
        }
    }
}

pub(crate) fn as_u8_slice(v: &[u32]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            v.as_ptr() as *const u8,
            v.len() * std::mem::size_of::<u32>(),
        )
    }
}

//pub fn dump_output_cache(
//    mut owriter: &mut BufWriter<File>,
//    output_cache: &DashMap<u64, CorrectedCBChunk>,
//    chunk_config: &ChunkConfig,
//) {
//    // NOTE: since the chunks are independent, this part could be multithreaded
//    let bc_type = decode_int_type_tag(chunk_config.bc_type).expect("unknown barcode type id.");
//    let umi_type = decode_int_type_tag(chunk_config.umi_type).expect("unknown barcode type id.");
//
//    for entry_ref in output_cache.iter() {
//        let _bc = entry_ref.key();
//        let chunk = entry_ref.value();
//        // number of bytes
//        let mut nbytes: u32 = 0;
//        let bytes_for_u32 = std::mem::size_of::<u32>();
//
//        let bytes_for_bc = bc_type.bytes_for_type();
//        let bytes_for_umi = umi_type.bytes_for_type();
//
//        // new
//        /*nbytes += chunk.data.get_ref().len() as u32;
//        owriter
//            .write_all(&nbytes.to_le_bytes())
//            .expect("couldn't write output.");
//        let nrec = chunk.nrec;
//        owriter
//            .write_all(&nrec.to_le_bytes())
//            .expect("couldn't write output.");
//        */
//        owriter
//            .write_all(&chunk.data.get_ref())
//            .expect("couldn't write output.");
//        // end new
//        /*
//        // for reference IDs in this chunk
//        nbytes += (chunk.ref_ids.len() * bytes_for_u32) as u32;
//        // umis
//        nbytes += (chunk.umis.len() * bytes_for_umi) as u32;
//        // barcodes
//        nbytes += (chunk.umis.len() * bytes_for_bc) as u32;
//        // num alignment fields
//        nbytes += (chunk.umis.len() * bytes_for_u32) as u32;
//
//        let nrec = chunk.umis.len() as u32;
//
//        owriter
//            .write_all(&nbytes.to_le_bytes())
//            .expect("couldn't write output.");
//        owriter
//            .write_all(&nrec.to_le_bytes())
//            .expect("couldn't write output.");
//
//        for i in 0..chunk.umis.len() {
//            let s = chunk.ref_offsets[i];
//            let e = if i == chunk.umis.len() - 1 {
//                chunk.ref_ids.len() as u32
//            } else {
//                chunk.ref_offsets[i + 1]
//            };
//
//            // num alignments
//            let num_aln = (e - s) as u32;
//            owriter
//                .write_all(&num_aln.to_le_bytes())
//                .expect("couldn't write output.");
//
//            bc_type
//                .write_to(*_bc, &mut owriter)
//                .expect("couldn't write output.");
//            umi_type
//                .write_to(chunk.umis[i], &mut owriter)
//                .expect("couldn't write output.");
//            owriter
//                .write_all(as_u8_slice(&chunk.ref_ids[(s as usize)..(e as usize)]))
//                .expect("couldn't write output.");
//        }
//        */
//    }
//}

impl Chunk {
    pub fn read_header<T: Read>(reader: &mut T) -> (u32, u32) {
        let mut buf = [0u8; 8];

        reader.read_exact(&mut buf).unwrap();
        let nbytes = buf.pread::<u32>(0).unwrap();
        let nrec = buf.pread::<u32>(4).unwrap();
        (nbytes, nrec)
    }

    pub fn from_bytes<T: Read>(reader: &mut T, bct: &RadIntId, umit: &RadIntId) -> Self {
        let mut buf = [0u8; 8];

        reader.read_exact(&mut buf).unwrap();
        let nbytes = buf.pread::<u32>(0).unwrap();
        let nrec = buf.pread::<u32>(4).unwrap();
        let mut c = Self {
            nbytes,
            nrec,
            reads: Vec::with_capacity(nrec as usize),
        };

        for _ in 0..(nrec as usize) {
            c.reads.push(ReadRecord::from_bytes(reader, &bct, &umit));
        }

        c
    }

    /// peeks to the first record in the buffer `buf`, and returns
    /// the barcode and umi associated with this record.  It is assumed
    /// that there is at least one record present in the buffer.
    pub fn peek_record(buf: &[u8], bct: &RadIntId, umit: &RadIntId) -> (u64, u64) {
        let na_size = mem::size_of::<u32>();
        let bc_size = bct.bytes_for_type();

        let _na = buf.pread::<u32>(0).unwrap();
        let bc;
        match bct {
            RadIntId::U8 => {
                bc = buf.pread::<u8>(na_size).unwrap() as u64;
            }
            RadIntId::U16 => {
                bc = buf.pread::<u16>(na_size).unwrap() as u64;
            }
            RadIntId::U32 => {
                bc = buf.pread::<u32>(na_size).unwrap() as u64;
            }
            RadIntId::U64 => {
                bc = buf.pread::<u64>(na_size).unwrap();
            }
        }
        let umi;
        match umit {
            RadIntId::U8 => {
                umi = buf.pread::<u8>(na_size + bc_size).unwrap() as u64;
            }
            RadIntId::U16 => {
                umi = buf.pread::<u16>(na_size + bc_size).unwrap() as u64;
            }
            RadIntId::U32 => {
                umi = buf.pread::<u32>(na_size + bc_size).unwrap() as u64;
            }
            RadIntId::U64 => {
                umi = buf.pread::<u64>(na_size + bc_size).unwrap();
            }
        }
        (bc, umi)
    }
}

impl FileTags {
    pub fn from_bytes<T: Read>(reader: &mut T) -> Self {
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();

        Self {
            bclen: buf.pread::<u16>(0).unwrap(),
            umilen: buf.pread::<u16>(2).unwrap(),
        }
    }
}

impl TagDesc {
    pub fn from_bytes<T: Read>(reader: &mut T) -> TagDesc {
        // space for the string length (1 byte)
        // the longest string possible (255 char)
        // and the typeid
        let mut buf = [0u8; 257];
        reader.read_exact(&mut buf[0..2]).unwrap();
        let str_len = buf.pread::<u16>(0).unwrap() as usize;

        // read str_len + 1 to get the type id that follows the string
        reader.read_exact(&mut buf[0..str_len + 1]).unwrap();
        TagDesc {
            name: std::str::from_utf8(&buf[0..str_len]).unwrap().to_string(),
            typeid: buf.pread(str_len).unwrap(),
        }
    }
}

impl TagSection {
    pub fn from_bytes<T: Read>(reader: &mut T) -> TagSection {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        let num_tags = buf.pread::<u16>(0).unwrap() as usize;

        let mut ts = TagSection {
            tags: Vec::with_capacity(num_tags),
        };

        for _ in 0..num_tags {
            ts.tags.push(TagDesc::from_bytes(reader));
        }

        ts
    }
}

impl RadHeader {
    pub fn from_bytes<T: Read>(reader: &mut T) -> RadHeader {
        let mut rh = RadHeader {
            is_paired: 0,
            ref_count: 0,
            ref_names: vec![],
            num_chunks: 0,
        };

        // size of the longest allowable string.
        let mut buf = [0u8; 65536];
        reader.read_exact(&mut buf[0..9]).unwrap();
        rh.is_paired = buf.pread(0).unwrap();
        rh.ref_count = buf.pread::<u64>(1).unwrap();

        // we know how many names we will read in.
        rh.ref_names.reserve_exact(rh.ref_count as usize);

        let mut num_read = 0u64;
        while num_read < rh.ref_count {
            reader.read_exact(&mut buf[0..2]).unwrap();
            let l: usize = buf.pread::<u16>(0).unwrap() as usize;
            reader.read_exact(&mut buf[0..l]).unwrap();
            rh.ref_names
                .push(std::str::from_utf8(&buf[0..l]).unwrap().to_string());
            num_read += 1;
        }

        reader.read_exact(&mut buf[0..8]).unwrap();
        rh.num_chunks = buf.pread::<u64>(0).unwrap();
        rh
    }
    pub fn from_bam_header(header: &HeaderView) -> RadHeader {
        let mut rh = RadHeader {
            is_paired: 0,
            ref_count: 0,
            ref_names: vec![],
            num_chunks: 0,
        };

        rh.ref_count = header.target_count() as u64;
        // we know how many names we will read in.
        rh.ref_names.reserve_exact(rh.ref_count as usize);
        for (_i, t) in header
            .target_names()
            .iter()
            .map(|a| std::str::from_utf8(a).unwrap())
            .enumerate()
        {
            rh.ref_names.push(t.to_owned());
        }
        rh
    }
    pub fn get_size(&self) -> usize {
        let mut tot_size = 0usize;
        tot_size += std::mem::size_of::<u8>() + std::mem::size_of::<u64>();
        for (_i, t) in self.ref_names.iter().map(|a| a.len()).enumerate() {
            tot_size += t;
        }
        tot_size += std::mem::size_of::<u64>();
        tot_size
    }
}

pub fn update_barcode_hist_unfiltered(
    hist: &mut HashMap<u64, u64, ahash::RandomState>,
    unmatched_bc: &mut Vec<u64>,
    max_ambiguity_read: &mut usize,
    chunk: &Chunk,
    expected_ori: &Strand,
) -> usize {
    let mut num_strand_compat_reads = 0usize;
    match expected_ori {
        Strand::Unknown => {
            for r in &chunk.reads {
                num_strand_compat_reads += 1;
                *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                // lookup the barcode in the map of unfiltered known
                // barcodes
                match hist.get_mut(&r.bc) {
                    // if we find a match, increment the count
                    Some(c) => *c += 1,
                    // otherwise, push this into the unmatched list
                    None => {
                        unmatched_bc.push(r.bc);
                    }
                }
            }
        }
        Strand::Forward => {
            for r in &chunk.reads {
                if r.dirs.iter().any(|&x| x) {
                    num_strand_compat_reads += 1;
                    *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                    // lookup the barcode in the map of unfiltered known
                    // barcodes
                    match hist.get_mut(&r.bc) {
                        // if we find a match, increment the count
                        Some(c) => *c += 1,
                        // otherwise, push this into the unmatched list
                        None => {
                            unmatched_bc.push(r.bc);
                        }
                    }
                }
            }
        }
        Strand::Reverse => {
            for r in &chunk.reads {
                if r.dirs.iter().any(|&x| !x) {
                    num_strand_compat_reads += 1;
                    *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                    // lookup the barcode in the map of unfiltered known
                    // barcodes
                    match hist.get_mut(&r.bc) {
                        // if we find a match, increment the count
                        Some(c) => *c += 1,
                        // otherwise, push this into the unmatched list
                        None => {
                            unmatched_bc.push(r.bc);
                        }
                    }
                }
            }
        }
    }
    num_strand_compat_reads
}

pub fn update_barcode_hist(
    hist: &mut HashMap<u64, u64, ahash::RandomState>,
    max_ambiguity_read: &mut usize,
    chunk: &Chunk,
    expected_ori: &Strand,
) {
    match expected_ori {
        Strand::Unknown => {
            for r in &chunk.reads {
                *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                *hist.entry(r.bc).or_insert(0) += 1;
            }
        }
        Strand::Forward => {
            for r in &chunk.reads {
                if r.dirs.iter().any(|&x| x) {
                    *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                    *hist.entry(r.bc).or_insert(0) += 1;
                }
            }
        }
        Strand::Reverse => {
            for r in &chunk.reads {
                if r.dirs.iter().any(|&x| !x) {
                    *max_ambiguity_read = r.refs.len().max(*max_ambiguity_read);
                    *hist.entry(r.bc).or_insert(0) += 1;
                }
            }
        }
    }
}

pub fn permit_list_from_threshold(
    hist: &HashMap<u64, u64, ahash::RandomState>,
    min_freq: u64,
) -> Vec<u64> {
    let valid_bc: Vec<u64> = hist
        .iter()
        .filter_map(|(k, v)| if v >= &min_freq { Some(*k) } else { None })
        .collect();
    valid_bc
}

pub fn permit_list_from_file(ifile: String, bclen: u16) -> Vec<u64> {
    let f = File::open(ifile).expect("couldn't open input barcode file.");
    let br = BufReader::new(f);
    let mut bc = Vec::<u64>::with_capacity(10_000);

    for l in br.lines() {
        let line = l.expect("couldn't read line from barcode file.");
        let mut bnk = BitNuclKmer::new(line.as_bytes(), bclen as u8, false);
        let (_, k, _) = bnk.next().expect("can't extract kmer");
        bc.push(k.0);
    }
    bc
}

pub fn write_str_bin(v: &str, type_id: &RadIntId, owriter: &mut Cursor<Vec<u8>>) {
    match type_id {
        RadIntId::U8 => {
            owriter
                .write_all(&(v.len() as u8).to_le_bytes())
                .expect("coudn't write to output file");
        }
        RadIntId::U16 => {
            owriter
                .write_all(&(v.len() as u16).to_le_bytes())
                .expect("coudn't write to output file");
        }
        RadIntId::U32 => {
            owriter
                .write_all(&(v.len() as u32).to_le_bytes())
                .expect("coudn't write to output file");
        }
        RadIntId::U64 => {
            owriter
                .write_all(&(v.len() as u64).to_le_bytes())
                .expect("coudn't write to output file");
        }
    }
    owriter
        .write_all(v.as_bytes())
        .expect("coudn't write to output file");
}
