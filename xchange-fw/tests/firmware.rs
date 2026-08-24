//! Tests for the firmware container and the driver-image extractor.

use std::path::{Path, PathBuf};

use xchange_fw::extract::{self, Chip};
use xchange_fw::record::{self, Record, MAX_RECORD_DATA, SYS_STRIDE};

/// The USB2Xchange blob from Adaptec's v2.00 driver.
const USB2XCHANGE_SHA256: &str =
    "d0967ef81e71e9293d0499c91d687e2409f8ca13b14ffe2c4f35f07685d25fbd";

/// Adaptec's driver, if the tester has said where it is.
fn test_driver() -> Option<PathBuf> {
    match std::env::var("XCHANGE_TEST_DRIVER") {
        Ok(path) => Some(PathBuf::from(path)),
        Err(_) => {
            eprintln!("skipping: set XCHANGE_TEST_DRIVER to Adpusbld.sys to run this");
            None
        }
    }
}

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(name)
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn parses_the_shipped_usb2xchange_firmware() {
    let bytes = std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap();
    let records = record::parse_fw(&bytes).unwrap();

    assert_eq!(records.len(), 588);
    // Record zero is the 8051 reset vector: LJMP 0x1748.
    assert_eq!(records[0].address, 0);
    assert_eq!(&records[0].data[..3], &[0x02, 0x17, 0x48]);
    // Everything must land inside the FX2's internal RAM.
    let highest = records.iter().map(|r| r.address as u32 + r.data.len() as u32).max().unwrap();
    assert!(highest <= record::FX2_INTERNAL_RAM_SIZE);
}

#[test]
fn fw_encoding_round_trips() {
    let bytes = std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap();
    let records = record::parse_fw(&bytes).unwrap();
    assert_eq!(record::to_fw(&records).unwrap(), bytes);
}

#[test]
fn firmware_identifies_its_own_target_chip() {
    let fx2 = record::parse_fw(&std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap()).unwrap();
    assert_eq!(extract::identify(&fx2), Chip::Fx2);

    let fx = record::parse_fw(&std::fs::read(repo_file("firmware/usbxchange.fw")).unwrap()).unwrap();
    assert_eq!(fx.len(), 438);
    assert_eq!(extract::identify(&fx), Chip::Fx);
}

/// Encode records as the driver stores them, so the scanner can be tested
/// without shipping Adaptec's driver as a fixture.
fn encode_as_sys(records: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&(record.data.len() as u16).to_le_bytes());
        out.extend_from_slice(&record.address.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&record.data);
        out.resize(out.len() + MAX_RECORD_DATA - record.data.len(), 0);
        out.push(0); // trailing padding byte
    }
    out.extend_from_slice(&(MAX_RECORD_DATA as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.push(1); // terminator
    out.resize(out.len() + MAX_RECORD_DATA + 1, 0);
    out
}

#[test]
fn finds_a_record_array_buried_in_surrounding_data() {
    let records = record::parse_fw(&std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap()).unwrap();

    let mut image = vec![0xCCu8; 4096];
    let embedded = encode_as_sys(&records);
    let offset = image.len();
    image.extend_from_slice(&embedded);
    image.extend_from_slice(&[0x77u8; 2048]);

    let found = extract::scan_for(&image, Chip::Fx2).unwrap();
    assert_eq!(found.offset, offset);
    assert_eq!(found.records, records);
    assert_eq!(digest(&record::to_fw(&found.records).unwrap()), USB2XCHANGE_SHA256);
}

#[test]
fn finds_both_blobs_when_a_driver_holds_two() {
    let fx2 = record::parse_fw(&std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap()).unwrap();
    let fx = record::parse_fw(&std::fs::read(repo_file("firmware/usbxchange.fw")).unwrap()).unwrap();
    assert_eq!(fx.len(), 438);

    let mut image = vec![0u8; 512];
    image.extend_from_slice(&encode_as_sys(&fx));
    image.extend_from_slice(&[0u8; 64]);
    image.extend_from_slice(&encode_as_sys(&fx2));

    let blobs = extract::scan_all(&image);
    assert_eq!(blobs.len(), 2);
    assert_eq!(blobs[0].chip, Chip::Fx);
    assert_eq!(blobs[1].chip, Chip::Fx2);
}

#[test]
fn a_truncated_container_is_rejected_rather_than_read_past_its_end() {
    let bytes = std::fs::read(repo_file("firmware/usb2xchange.fw")).unwrap();
    // Lop off the terminator. The C reference walks past it; we must not.
    let truncated = &bytes[..bytes.len() - record::FW_STRIDE];
    assert!(matches!(
        record::parse_fw(truncated),
        Err(xchange_fw::Error::MissingTerminator)
    ));
}

#[test]
fn an_over_long_record_is_rejected() {
    let mut bytes = record::to_fw(&[Record { address: 0, data: vec![0xAA; 16] }]).unwrap();
    bytes[0] = 17; // claim more payload than a record can hold
    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::RecordTooLong { length: 17, .. })
    ));
}

#[test]
fn a_record_past_internal_ram_is_rejected() {
    let bytes = record::to_fw(&[Record { address: 0x3FF8, data: vec![0xAA; 16] }]).unwrap();
    // 0x3FF8 + 16 = 0x4008, past the FX2's 16 KB.
    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::AddressOutOfRange { .. })
    ));
}

/// A `u32` address can wrap the sum rather than exceed the ceiling. Release
/// builds passed the bounds test and uploaded to a truncated address.
#[test]
fn a_record_whose_end_wraps_is_rejected_rather_than_overflowing() {
    let mut bytes = record::to_fw(&[Record { address: 0, data: vec![0xAA] }]).unwrap();
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());

    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::AddressOutOfRange { address: u32::MAX, length: 1, .. })
    ));
}

/// The start is checked as well as the end: a zero-length record at the
/// ceiling passes a sum-only test.
#[test]
fn a_record_starting_at_the_ceiling_is_rejected() {
    let mut bytes = record::to_fw(&[Record { address: 0, data: Vec::new() }]).unwrap();
    bytes[4..8].copy_from_slice(&record::FX2_INTERNAL_RAM_SIZE.to_le_bytes());

    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::AddressOutOfRange { .. })
    ));
}

/// `Record` is public, so the encoder cannot lean on the decoder's limit. The
/// padding calculation used to underflow.
#[test]
fn the_encoder_refuses_a_record_it_cannot_represent() {
    assert!(matches!(
        record::to_fw(&[Record { address: 0, data: vec![0xAA; 17] }]),
        Err(xchange_fw::Error::RecordTooLong { index: 0, length: 17 })
    ));
}

/// `scan_all` walked one position too many on a short slice and indexed off
/// the end, so `xchange-fw info` panicked on a truncated file.
#[test]
fn a_slice_too_short_to_hold_a_record_finds_nothing() {
    for length in 0..SYS_STRIDE {
        // Header-shaped, so the cheap test would match if it ran.
        let mut bytes = vec![16u8, 0, 0, 0, 0];
        bytes.resize(length, 0);

        assert!(
            extract::scan_all(&bytes).is_empty(),
            "{length} bytes is not enough to hold a record"
        );
    }

    assert!(matches!(
        extract::scan_for(&[], Chip::Fx2),
        Err(xchange_fw::Error::FirmwareNotFound)
    ));
}

/// Point `XCHANGE_TEST_DRIVER` at Adpusbld.sys to check the extractor against
/// the real blob. Skipped when unset; the firmware is Adaptec's to ship.
#[test]
fn extracts_the_known_blob_from_the_real_driver_when_present() {
    let Some(driver) = test_driver() else { return };
    let Ok(bytes) = std::fs::read(&driver) else {
        eprintln!("skipping: {} not readable", driver.display());
        return;
    };

    let found = extract::scan_for(&bytes, Chip::Fx2).unwrap();
    assert_eq!(found.records.len(), 588);
    assert_eq!(digest(&record::to_fw(&found.records).unwrap()), USB2XCHANGE_SHA256);

    // Records are stored back to back at the driver's 22-byte stride.
    assert_eq!(found.end - found.offset, (found.records.len() + 1) * SYS_STRIDE);
}

/// A driver image is not a `.fw` container. The PE header used to decode as an
/// immediate terminator, so the loader "succeeded" having uploaded nothing.
#[test]
fn a_driver_image_is_not_mistaken_for_a_fw_container() {
    let Some(driver) = test_driver() else { return };
    let Ok(bytes) = std::fs::read(&driver) else {
        eprintln!("skipping: {} not readable", driver.display());
        return;
    };
    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::EmptyFirmware)
    ));
}

#[test]
fn an_immediate_terminator_is_not_a_valid_container() {
    let mut bytes = vec![0u8; record::FW_STRIDE];
    bytes[8] = 1; // type = EOF on the very first record
    assert!(matches!(
        record::parse_fw(&bytes),
        Err(xchange_fw::Error::EmptyFirmware)
    ));
}
