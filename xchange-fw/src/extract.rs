//! Recover firmware record arrays from Adaptec's `Adpusbld.sys`.
//!
//! The driver holds a blob per model, since its INF binds it to both PID_2000
//! and PID_2002. We try every offset and keep the ones that decode into a long
//! run ending in a terminator. Extracting from your own installation CD avoids
//! redistributing firmware that is not ours to hand out.

use crate::record::{flatten, try_parse_sys_at, Record, SYS_STRIDE};
use crate::Error;

/// Shorter than this is a coincidence. The known blobs hold 438 and 588.
const MIN_PLAUSIBLE_RECORDS: usize = 64;

/// Which Cypress part a blob was built for.
///
/// The firmware says so itself: `MOV DPTR,#imm16` reaches the USB registers,
/// which sit at 0xE6xx on the FX2 and 0x7Fxx on the FX. Counting which page
/// the blob loads identifies it wherever it sits in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Fx,
    Fx2,
    /// Neither page dominates; the blob could not be attributed.
    Unknown,
}

impl Chip {
    pub fn name(self) -> &'static str {
        match self {
            Chip::Fx => "EZ-USB / FX",
            Chip::Fx2 => "EZ-USB FX2",
            Chip::Unknown => "unrecognised",
        }
    }
}

const MOV_DPTR_IMM16: u8 = 0x90;
const FX2_REGISTER_PAGE: u8 = 0xE6;
const FX_REGISTER_PAGE: u8 = 0x7F;

/// Attribute a blob by the register page its code addresses.
pub fn identify(records: &[Record]) -> Chip {
    let image = flatten(records);

    let count = |page: u8| {
        image
            .windows(2)
            .filter(|w| w[0] == MOV_DPTR_IMM16 && w[1] == page)
            .count()
    };

    match (count(FX2_REGISTER_PAGE), count(FX_REGISTER_PAGE)) {
        (fx2, fx) if fx2 > fx * 2 => Chip::Fx2,
        (fx2, fx) if fx > fx2 * 2 => Chip::Fx,
        _ => Chip::Unknown,
    }
}

/// One firmware blob found inside a driver image.
#[derive(Debug)]
pub struct Extraction {
    pub offset: usize,
    pub end: usize,
    pub chip: Chip,
    pub records: Vec<Record>,
}

/// How every array opens: a full-length record at address 0, the 8051 reset
/// vector. Cheap test before attempting the walk.
const ARRAY_HEADER: [u8; 5] = [16, 0, 0, 0, 0];

/// Find every firmware record array in a driver image.
pub fn scan_all(bytes: &[u8]) -> Vec<Extraction> {
    let mut found = Vec::new();
    let mut offset = 0;

    // Only positions with a whole record behind them. A saturating limit left
    // it at zero for anything shorter than one record, ran the body once and
    // indexed off the end.
    while offset + SYS_STRIDE <= bytes.len() {
        if bytes[offset..offset + ARRAY_HEADER.len()] == ARRAY_HEADER {
            if let Ok((records, end)) = try_parse_sys_at(bytes, offset) {
                if records.len() >= MIN_PLAUSIBLE_RECORDS {
                    found.push(Extraction {
                        offset,
                        end,
                        chip: identify(&records),
                        records,
                    });
                    offset = end;
                    continue;
                }
            }
        }

        offset += 1;
    }

    found
}

/// Find the blob built for a particular Cypress part.
pub fn scan_for(bytes: &[u8], chip: Chip) -> Result<Extraction, Error> {
    scan_all(bytes)
        .into_iter()
        .find(|found| found.chip == chip)
        .ok_or(Error::FirmwareNotFound)
}
