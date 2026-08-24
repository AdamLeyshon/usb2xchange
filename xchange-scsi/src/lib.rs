//! Talking SCSI through an Adaptec USB2Xchange adapter.
//!
//! With firmware running it speaks Bulk-Only Transport, but the interface is
//! vendor-specific so `usb-storage` never binds, and it bends the transport in
//! two ways standard code treats as protocol errors. So we drive it ourselves.

pub mod bot;
pub mod scsi;

pub use bot::{Adapter, Status};
pub use scsi::{Address, Inquiry, PeripheralType};

/// Everything that can go wrong talking to the adapter or the bus.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no USB2Xchange found with firmware running; run `xchange-fw load` first")]
    NotFound,

    #[error("USB error: {0}")]
    Usb(#[source] nusb::Error),

    #[error("transfer failed on {stage}: {source}")]
    Transfer {
        stage: &'static str,
        #[source]
        source: nusb::transfer::TransferError,
    },

    #[error("command block wrapper was only partly accepted ({sent} of {expected} bytes)")]
    ShortCommand { sent: usize, expected: usize },

    #[error("status wrapper was {len} bytes, expected 13")]
    ShortStatus { len: usize },

    #[error("status wrapper had signature {0:#010x}, expected 0x53425355")]
    BadStatusSignature(u32),

    #[error("status wrapper carried tag {got}, expected {expected}")]
    TagMismatch { got: u32, expected: u32 },

    #[error("{at} reported check condition: {sense}")]
    CheckCondition { at: scsi::Address, sense: scsi::Sense },

    #[error("{at} failed {command}")]
    CommandFailed {
        at: scsi::Address,
        command: &'static str,
    },

    #[error("{at} returned {len} bytes, too few to decode {what}")]
    ShortResponse {
        at: scsi::Address,
        len: usize,
        what: &'static str,
    },

    /// A command failed and its REQUEST SENSE came back illegible. Distinct
    /// from NO SENSE, which is the device saying nothing is wrong; this is its
    /// explanation not arriving, usually a dropped data phase. Treating the
    /// two alike reported a refused write as a successful one.
    #[error("{at} failed a command, and its sense data was unreadable ({len} bytes)")]
    UnreadableSense { at: scsi::Address, len: usize },

    #[error("transfer of {len} bytes exceeds the adapter's {limit} byte limit")]
    TransferTooLarge { len: usize, limit: usize },

    #[error("{at} cannot be encoded: this is a narrow bus, so targets and LUNs both run 0 to 7")]
    AddressUnencodable { at: scsi::Address },

    #[error("{at} accepts READ DEFECT DATA but returns nothing, in any list format")]
    NoDefectLists { at: scsi::Address },
}
