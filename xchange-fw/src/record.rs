//! Firmware record container, shared by `.fw` files and the Windows driver.
//!
//! Both hold the same Intel-HEX-derived records and differ only in encoding:
//! `.fw` uses `u32` fields at a 28-byte stride, `Adpusbld.sys` uses `u16`
//! length, `u16` address, `u8` type, `u8 data[16]` and a padding byte at 22.

use crate::Error;

pub const MAX_RECORD_DATA: usize = 16;

/// The FX2's internal RAM. We only issue `ANCHOR_LOAD_INTERNAL`, so everything
/// must land below this.
pub const FX2_INTERNAL_RAM_SIZE: u32 = 0x4000;

pub const FW_STRIDE: usize = 28;
pub const SYS_STRIDE: usize = 22;

const TYPE_DATA: u32 = 0;
const TYPE_EOF: u32 = 1;

/// One chunk of firmware bound for an address in the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub address: u16,
    pub data: Vec<u8>,
}

/// Validate a decoded record header and return its payload.
fn take_record(
    index: usize,
    length: u32,
    address: u32,
    kind: u32,
    payload: &[u8],
) -> Result<Option<Record>, Error> {
    // Any non-data type terminates. The `usbxchange.fw` dump in circulation
    // writes its terminator type as 0x01000000 rather than 1, and the vendor's
    // loader takes it because it only ever tests `type == 0`.
    if kind != TYPE_DATA {
        return Ok(None);
    }

    let length = length as usize;
    if length > MAX_RECORD_DATA {
        return Err(Error::RecordTooLong { index, length });
    }

    // Checked, and the address tested in its own right: `address` is a full
    // 32-bit field from the file, so a value near `u32::MAX` wraps the sum back
    // under the ceiling and then truncates to `u16` on the way into `Record`.
    let fits = address < FX2_INTERNAL_RAM_SIZE
        && address
            .checked_add(length as u32)
            .is_some_and(|end| end <= FX2_INTERNAL_RAM_SIZE);

    if !fits {
        return Err(Error::AddressOutOfRange { index, address, length });
    }

    Ok(Some(Record {
        address: address as u16,
        data: payload[..length].to_vec(),
    }))
}

/// Decode the `.fw` container: `u32` length, address and type, then 16 bytes.
pub fn parse_fw(bytes: &[u8]) -> Result<Vec<Record>, Error> {
    let mut records = Vec::new();

    for (index, chunk) in bytes.chunks_exact(FW_STRIDE).enumerate() {
        let length = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let address = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        let kind = u32::from_le_bytes(chunk[8..12].try_into().unwrap());

        match take_record(index, length, address, kind, &chunk[12..])? {
            Some(record) => records.push(record),
            None if records.is_empty() => return Err(Error::EmptyFirmware),
            None => return Ok(records),
        }
    }

    Err(Error::MissingTerminator)
}

/// Re-encode records as a `.fw` container.
///
/// Fails on an over-long record: there is no encoding for one, so writing it
/// would either overrun the stride or drop the tail.
pub fn to_fw(records: &[Record]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity((records.len() + 1) * FW_STRIDE);

    let mut push = |length: u32, address: u32, kind: u32, data: &[u8]| {
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&address.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(data);
        out.resize(out.len() + MAX_RECORD_DATA - data.len(), 0);
    };

    for (index, record) in records.iter().enumerate() {
        let length = record.data.len();
        if length > MAX_RECORD_DATA {
            return Err(Error::RecordTooLong { index, length });
        }
        push(length as u32, record.address as u32, TYPE_DATA, &record.data);
    }
    push(MAX_RECORD_DATA as u32, 0, TYPE_EOF, &[]);

    Ok(out)
}

/// Decode a record array in the driver's encoding at `offset`, returning the
/// records and the offset past the terminator.
pub fn try_parse_sys_at(bytes: &[u8], offset: usize) -> Result<(Vec<Record>, usize), Error> {
    let mut records = Vec::new();
    let mut cursor = offset;

    while cursor + SYS_STRIDE <= bytes.len() {
        let chunk = &bytes[cursor..cursor + SYS_STRIDE];
        let length = u16::from_le_bytes(chunk[0..2].try_into().unwrap()) as u32;
        let address = u16::from_le_bytes(chunk[2..4].try_into().unwrap()) as u32;
        let kind = chunk[4] as u32;

        let index = records.len();
        cursor += SYS_STRIDE;

        match take_record(index, length, address, kind, &chunk[5..21])? {
            Some(record) => records.push(record),
            None => return Ok((records, cursor)),
        }
    }

    Err(Error::MissingTerminator)
}

/// Flatten records into a contiguous 8051 image.
pub fn flatten(records: &[Record]) -> Vec<u8> {
    let size = records
        .iter()
        .map(|r| r.address as usize + r.data.len())
        .max()
        .unwrap_or(0);

    let mut image = vec![0u8; size];
    for record in records {
        let at = record.address as usize;
        image[at..at + record.data.len()].copy_from_slice(&record.data);
    }
    image
}
