//! Firmware loading for the Adaptec USBXchange and USB2Xchange adapters.
//!
//! They ship with an empty Cypress EZ-USB 8051 and do nothing until a program
//! is pushed in over USB.

pub mod extract;
pub mod loader;
pub mod record;

/// Everything that can go wrong loading firmware.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("record {index} declares {length} bytes, but the maximum is 16")]
    RecordTooLong { index: usize, length: usize },

    #[error("record {index} targets {length} bytes at 0x{address:04x}, past the FX2's 16 KB of internal RAM")]
    AddressOutOfRange {
        index: usize,
        address: u32,
        length: usize,
    },

    #[error("firmware ended without a terminator record")]
    MissingTerminator,

    #[error("no firmware records present; this is not a .fw container")]
    EmptyFirmware,

    #[error("no firmware record array found in this driver image")]
    FirmwareNotFound,

    #[error("no Adaptec adapter awaiting firmware; is it plugged in, and has it already been loaded?")]
    NoLoaderDevice,

    #[error("control transfer to 0x{address:04x} failed: {source}")]
    ControlTransfer {
        address: u16,
        source: nusb::transfer::TransferError,
    },

    #[error("{model} did not re-enumerate after loading firmware")]
    RenumerationTimeout { model: &'static str },

    #[error("USB error: {0}")]
    Usb(#[source] nusb::Error),

    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Read a file, tagging failures with the path.
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })
}
