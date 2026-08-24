//! Medium-change detection: a swapped cartridge against a reset device, and
//! which address it happened at. Neither needs an adapter, which is the point:
//! the hardware path needs a cartridge swapped mid-transfer.

use xchange_scsi::scsi::{Address, MediumChanges, Sense};

/// An 18-byte fixed-format sense reply.
fn sense(key: u8, asc: u8, ascq: u8) -> Sense {
    let mut data = [0u8; 18];
    data[0] = 0x70;
    data[2] = key;
    data[7] = 10;
    data[12] = asc;
    data[13] = ascq;
    Sense::parse(&data).expect("18 bytes is enough to parse")
}

#[test]
fn medium_change_is_told_apart_from_a_reset() {
    // 0x28 is the swap; everything the caller knows is stale.
    assert!(sense(0x06, 0x28, 0x00).is_medium_changed());

    // 0x29 is a reset; nothing moved, so the transport's retry is the answer.
    assert!(sense(0x06, 0x29, 0x00).is_unit_attention());
    assert!(!sense(0x06, 0x29, 0x00).is_medium_changed());

    // 0x3F is "operating conditions changed", which is neither.
    assert!(!sense(0x06, 0x3F, 0x00).is_medium_changed());
}

#[test]
fn medium_change_needs_the_unit_attention_key() {
    // A medium error carrying 0x28 is not a swap. Keying off the ASC alone
    // would restart the medium-change dance on a bad sector.
    assert!(!sense(0x03, 0x28, 0x00).is_medium_changed());
    assert!(!sense(0x02, 0x28, 0x00).is_medium_changed());
}

#[test]
fn a_noted_change_is_reported_once() {
    let at = Address::target(1);
    let mut changes = MediumChanges::default();

    assert!(!changes.take(at), "nothing has happened yet");

    changes.note(at);
    assert!(changes.take(at));
    assert!(
        !changes.take(at),
        "asking clears it, or the server re-reads a capacity it just refreshed"
    );
}

#[test]
fn changes_do_not_leak_between_addresses() {
    let mut changes = MediumChanges::default();

    // One bit per address. A collision here would resize the wrong export.
    for target in 0..=7 {
        for lun in 0..=7 {
            changes.note(Address::new(target, lun));
        }
    }

    for target in 0..=7 {
        for lun in 0..=7 {
            assert!(
                changes.take(Address::new(target, lun)),
                "target {target} LUN {lun} lost its bit"
            );
        }
    }
}

#[test]
fn one_address_at_a_time() {
    let mut changes = MediumChanges::default();
    changes.note(Address::new(3, 0));

    assert!(!changes.take(Address::new(0, 3)), "target and LUN are not interchangeable");
    assert!(!changes.take(Address::new(3, 1)));
    assert!(changes.take(Address::new(3, 0)));
}

#[test]
fn unencodable_addresses_are_ignored_rather_than_wrapping() {
    let mut changes = MediumChanges::default();

    // Three bits each way. An address beyond that has no bit, and masking it
    // into range would land on a real device's flag.
    changes.note(Address::new(8, 0));
    changes.note(Address::new(0, 8));

    assert!(!changes.take(Address::new(8, 0)));
    assert!(!changes.take(Address::new(0, 0)), "target 8 must not alias target 0");
    assert_eq!(changes, MediumChanges::default(), "nothing was recorded at all");
}
