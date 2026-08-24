//! Decoding REQUEST SENSE replies. Two things are pinned down: that the two
//! layouts are told apart rather than assumed, and that an undecodable reply
//! stays distinct from one saying nothing is wrong. The transport turns the
//! first into an error and the second into a success.

use xchange_scsi::scsi::Sense;

/// A fixed-format reply, as SCSI-2 hardware sends.
fn fixed(key: u8, asc: u8, ascq: u8) -> [u8; 18] {
    let mut data = [0u8; 18];
    data[0] = 0x70;
    data[2] = key;
    data[7] = 10; // additional sense length
    data[12] = asc;
    data[13] = ascq;
    data
}

#[test]
fn a_fixed_format_reply_decodes_from_its_own_offsets() {
    let sense = Sense::parse(&fixed(0x06, 0x28, 0x00)).expect("fixed format is decodable");

    assert_eq!(sense.key, 0x06);
    assert_eq!(sense.asc, 0x28);
    assert_eq!(sense.ascq, 0x00);
    assert!(sense.is_medium_changed());
}

#[test]
fn a_fixed_format_reply_carries_information_only_when_valid_is_set() {
    let mut data = fixed(0x03, 0x11, 0x00);
    data[3..7].copy_from_slice(&1234u32.to_be_bytes());

    // Without VALID those four bytes mean nothing.
    assert_eq!(Sense::parse(&data).unwrap().information, None);

    data[0] |= 0x80;
    assert_eq!(Sense::parse(&data).unwrap().information, Some(1234));
}

#[test]
fn a_fixed_format_reply_reports_progress_when_sksv_is_set() {
    let mut data = fixed(0x02, 0x04, 0x04);
    data[15] = 0x80; // SKSV
    data[16..18].copy_from_slice(&32768u16.to_be_bytes());

    let sense = Sense::parse(&data).unwrap();
    assert!(sense.is_format_in_progress());
    assert_eq!(sense.progress_percent(), Some(50.0));
}

/// Read as fixed format, this reply reports an illegal request, confidently,
/// when the drive said the medium was not present.
#[test]
fn a_descriptor_format_reply_is_decoded_rather_than_misread() {
    let mut data = vec![0x72, 0x02, 0x3A, 0x00, 0x00, 0x00, 0x00, 0x00];
    data.resize(18, 0x05);
    data[7] = 0; // no descriptors follow

    let sense = Sense::parse(&data).expect("descriptor format is decodable");
    assert_eq!(sense.key, 0x02);
    assert_eq!(sense.asc, 0x3A);
    assert_eq!(sense.ascq, 0x00);
    assert!(sense.is_no_medium());
}

#[test]
fn a_descriptor_reply_carries_information_and_progress_in_its_descriptors() {
    let mut data = vec![0x72, 0x03, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00];

    // Type, length, VALID, reserved, then eight bytes of address.
    let mut information = vec![0x00, 0x0A, 0x80, 0x00];
    information.extend_from_slice(&0u32.to_be_bytes());
    information.extend_from_slice(&4321u32.to_be_bytes());

    // Type, length, two reserved, then fixed format's bytes 15 to 17.
    let specific = vec![0x02, 0x06, 0x00, 0x00, 0x80, 0x40, 0x00];

    data[7] = (information.len() + specific.len()) as u8;
    data.extend_from_slice(&information);
    data.extend_from_slice(&specific);

    let sense = Sense::parse(&data).unwrap();
    assert_eq!(sense.key, 0x03);
    assert_eq!(sense.information, Some(4321));
    assert_eq!(sense.progress_percent(), Some(25.0));
}

#[test]
fn a_descriptor_claiming_more_body_than_arrived_still_yields_its_header() {
    // Only the trailing descriptor is cut short. Refusing the whole reply
    // would throw away a header that decoded.
    let data = [0x72, 0x05, 0x24, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x0A, 0x80];

    let sense = Sense::parse(&data).unwrap();
    assert_eq!(sense.key, 0x05);
    assert_eq!(sense.asc, 0x24);
    assert_eq!(sense.information, None, "a truncated descriptor is not decoded");
}

/// `request_sense` turns `None` into an error. Folding it into NO_SENSE
/// instead reported a refused WRITE(10) as a successful one.
#[test]
fn an_illegible_reply_is_not_mistaken_for_no_sense() {
    // Nothing back at all: a dropped data phase, not a quiet device.
    assert!(Sense::parse(&[]).is_none());

    // Too short to hold the codes.
    assert!(Sense::parse(&[0x70, 0x00, 0x00]).is_none());
    assert!(Sense::parse(&[0x72, 0x00, 0x00]).is_none());

    // Undefined response code. All-zero data used to decode as "fine".
    assert!(Sense::parse(&[0u8; 18]).is_none());
    assert!(Sense::parse(&[0xFF; 18]).is_none());

    // A device that really did say NO SENSE decodes and says so.
    let quiet = Sense::parse(&fixed(0x00, 0x00, 0x00)).expect("a real NO SENSE reply decodes");
    assert!(quiet.is_clear());
    assert_eq!(quiet, Sense::NO_SENSE);
}

#[test]
fn deferred_response_codes_decode_as_their_immediate_counterparts() {
    // 0x71 and 0x73 blame an earlier command. Same layout, so both decode;
    // who to blame is the caller's problem.
    let mut data = fixed(0x04, 0x44, 0x00);
    data[0] = 0x71;
    assert_eq!(Sense::parse(&data).unwrap().key, 0x04);

    let deferred = [0x73, 0x04, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(Sense::parse(&deferred).unwrap().key, 0x04);
}
