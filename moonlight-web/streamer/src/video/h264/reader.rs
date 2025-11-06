use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use log::info;

use crate::video::{
    annexb::AnnexBSplitter,
    h264::{Nal, NalHeader, NalUnitType},
};

// Global counters for NAL unit statistics
static NAL_COUNTER_IDR: AtomicUsize = AtomicUsize::new(0);
static NAL_COUNTER_NON_IDR: AtomicUsize = AtomicUsize::new(0);
static NAL_COUNTER_SPS: AtomicUsize = AtomicUsize::new(0);
static NAL_COUNTER_PPS: AtomicUsize = AtomicUsize::new(0);
static NAL_COUNTER_SEI: AtomicUsize = AtomicUsize::new(0);

pub struct H264Reader<R: Read> {
    annex_b: AnnexBSplitter<R>,
    frame_count: usize,
}

impl<R> H264Reader<R>
where
    R: Read,
{
    pub fn new(reader: R, capacity: usize) -> Self {
        Self {
            annex_b: AnnexBSplitter::new(reader, capacity),
            frame_count: 0,
        }
    }

    pub fn next_nal(&mut self) -> Result<Option<Nal>, io::Error> {
        loop {
            if let Some(annex_b) = self.annex_b.next()? {
                let header_range = annex_b.payload_range.start..(annex_b.payload_range.start + 1);

                let mut header = [0u8; 1];
                header.copy_from_slice(&annex_b.full[header_range.clone()]);
                let header = NalHeader::parse(header);

                // Count NAL unit types for debugging
                match header.nal_unit_type {
                    NalUnitType::CodedSliceIDR => {
                        let count = NAL_COUNTER_IDR.fetch_add(1, Ordering::Relaxed) + 1;
                        if count % 60 == 0 {  // Log every second @ 60 FPS
                            eprintln!("[H264 Stats] I-frames: {}, P-frames: {}, SPS: {}, PPS: {}, SEI: {}",
                                count,
                                NAL_COUNTER_NON_IDR.load(Ordering::Relaxed),
                                NAL_COUNTER_SPS.load(Ordering::Relaxed),
                                NAL_COUNTER_PPS.load(Ordering::Relaxed),
                                NAL_COUNTER_SEI.load(Ordering::Relaxed));
                        }
                    }
                    NalUnitType::CodedSliceNonIDR => { NAL_COUNTER_NON_IDR.fetch_add(1, Ordering::Relaxed); }
                    NalUnitType::Sps => { NAL_COUNTER_SPS.fetch_add(1, Ordering::Relaxed); }
                    NalUnitType::Pps => { NAL_COUNTER_PPS.fetch_add(1, Ordering::Relaxed); }
                    NalUnitType::Sei => { NAL_COUNTER_SEI.fetch_add(1, Ordering::Relaxed); }
                    _ => {}
                }

                if header.nal_unit_type == NalUnitType::Sei {
                    continue;
                }

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
