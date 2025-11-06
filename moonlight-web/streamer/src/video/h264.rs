//! Specifications:
//! - https://datatracker.ietf.org/doc/html/rfc3984

use std::{
    io::{self, Read},
    ops::Range,
};

use bytes::BytesMut;
use num::FromPrimitive;
use num_derive::FromPrimitive;

use crate::video::annexb::{AnnexBSplitter, AnnexBStartCode};

#[allow(unused)]
pub struct Nal {
    pub payload_range: Range<usize>,
    pub header: NalHeader,
    pub header_range: Range<usize>,
    pub start_code: AnnexBStartCode,
    pub start_code_range: Range<usize>,
    pub full: BytesMut,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive)]
pub enum NalUnitType {
    // VCL NAL units
    Unspecified = 0,
    CodedSliceNonIDR = 1,
    CodedSliceDataPartitionA = 2,
    CodedSliceDataPartitionB = 3,
    CodedSliceDataPartitionC = 4,
    CodedSliceIDR = 5,
    Sei = 6,
    Sps = 7,
    Pps = 8,
    AccessUnitDelimiter = 9,
    EndOfSequence = 10,
    EndOfStream = 11,
    FillerData = 12,
    SPSEx = 13,
    PrefixNALUnit = 14,
    SubsetSPS = 15,
    DepthParameterSet = 16,
    Reserved17 = 17,
    Reserved18 = 18,
    CodedSliceAux = 19,
    CodedSliceExt = 20,
    CodedSliceExtDepth = 21,
    Reserved22 = 22,
    Reserved23 = 23,
    Unspecified24 = 24,
    Unspecified25 = 25,
    Unspecified26 = 26,
    Unspecified27 = 27,
    Unspecified28 = 28,
    Unspecified29 = 29,
    Unspecified30 = 30,
    Unspecified31 = 31,
}

#[allow(unused)]
#[derive(Debug, Clone, Copy)]
pub struct NalHeader {
    pub forbidden_zero_bit: bool,
    pub nal_ref_idc: u8,
    pub nal_unit_type: NalUnitType,
}

impl NalHeader {
    fn parse(header: [u8; 1]) -> Self {
        // F: 1 bit
        let forbidden_zero_bit = ((header[0] & 0b10000000) >> 7) == 1;

        // NRI: 2 bits
        let nal_ref_idc = (header[0] & 0b01100000) >> 5;

        // Type: 5 bits
        let nal_unit_type = header[0] & 0b00011111;

        Self {
            forbidden_zero_bit,
            nal_ref_idc,
            #[allow(clippy::unwrap_used)]
            nal_unit_type: NalUnitType::from_u8(nal_unit_type).unwrap(),
        }
    }
}

// https://datatracker.ietf.org/doc/html/rfc3984#section-1.3
pub struct H264Reader<R: Read> {
    annex_b: AnnexBSplitter<R>,
}

impl<R> H264Reader<R>
where
    R: Read,
{
    pub fn new(reader: R, capacity: usize) -> Self {
        Self {
            annex_b: AnnexBSplitter::new(reader, capacity),
        }
    }

    pub fn next_nal(&mut self) -> Result<Option<Nal>, io::Error> {
        loop {
            if let Some(annex_b) = self.annex_b.next()? {
                let header_range = annex_b.payload_range.start..(annex_b.payload_range.start + 1);

                let mut header = [0u8; 1];
                header.copy_from_slice(&annex_b.full[header_range.clone()]);
                let header = NalHeader::parse(header);

                // Don't skip SEI - it contains critical color metadata!
                // Skipping SEI causes color reproduction issues (flickering, wrong colors)
                // if header.nal_unit_type == NalUnitType::Sei {
                //     continue;
                // }

                let payload_range = header_range.end..annex_b.payload_range.end;

                return Ok(Some(Nal {
                    payload_range,
                    header,
                    header_range,
                    start_code: annex_b.start_code,
                    start_code_range: annex_b.start_code_range,
                    full: annex_b.full,
                }));
            } else {
                return Ok(None);
            }
        }
    }

    pub fn reset(&mut self, new_reader: R) {
        self.annex_b.reset(new_reader);
    }
}
