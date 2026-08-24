//! Push firmware into the adapter's Cypress EZ-USB and wait for it to return.
//!
//! On power-up the EEPROM supplies only the IDs and the 8051 has no program.
//! We halt it, write the program into internal RAM a record at a time, then
//! let it run; it re-enumerates under a new product ID.

use std::time::{Duration, Instant};

use nusb::transfer::{ControlOut, ControlType, Recipient};
use nusb::{Device, DeviceInfo, MaybeFuture};

use crate::extract::Chip;
use crate::record::Record;
use crate::Error;

pub const VID_ADAPTEC: u16 = 0x03f3;

/// USBXchange (USB 1.1), before and after loading.
pub const PID_USBXCHANGE_LOADER: u16 = 0x2000;
pub const PID_USBXCHANGE_READY: u16 = 0x2001;

/// USB2Xchange (USB 2.0), before and after loading.
pub const PID_USB2XCHANGE_LOADER: u16 = 0x2002;
pub const PID_USB2XCHANGE_READY: u16 = 0x2003;

/// Cypress vendor request that writes into the 8051's internal RAM.
pub const ANCHOR_LOAD_INTERNAL: u8 = 0xA0;

/// Control and status register; bit 0 holds the 8051 in reset. The FX2 moved
/// it from where the FX kept it.
pub const CPUCS_REG_FX: u16 = 0x7F92;
pub const CPUCS_REG_FX2: u16 = 0xE600;

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Which adapter we are talking to, and what differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub name: &'static str,
    pub loader_pid: u16,
    pub ready_pid: u16,
    pub cpucs: u16,
    pub firmware_name: &'static str,
    /// Picks the right blob from a driver image holding both.
    pub chip: Chip,
}

pub const USBXCHANGE: Model = Model {
    name: "USBXchange",
    loader_pid: PID_USBXCHANGE_LOADER,
    ready_pid: PID_USBXCHANGE_READY,
    cpucs: CPUCS_REG_FX,
    firmware_name: "usbxchange.fw",
    chip: Chip::Fx,
};

pub const USB2XCHANGE: Model = Model {
    name: "USB2Xchange",
    loader_pid: PID_USB2XCHANGE_LOADER,
    ready_pid: PID_USB2XCHANGE_READY,
    cpucs: CPUCS_REG_FX2,
    firmware_name: "usb2xchange.fw",
    chip: Chip::Fx2,
};

pub const MODELS: [Model; 2] = [USB2XCHANGE, USBXCHANGE];

impl Model {
    pub fn from_loader_pid(pid: u16) -> Option<Self> {
        MODELS.into_iter().find(|m| m.loader_pid == pid)
    }
}

/// Find an adapter sitting in its pre-firmware state.
pub fn find_loader() -> Result<(DeviceInfo, Model), Error> {
    let devices = nusb::list_devices().wait().map_err(Error::Usb)?;

    for info in devices {
        if info.vendor_id() != VID_ADAPTEC {
            continue;
        }
        if let Some(model) = Model::from_loader_pid(info.product_id()) {
            return Ok((info, model));
        }
    }

    Err(Error::NoLoaderDevice)
}

/// Write a block into the adapter's memory.
fn write_memory(device: &Device, address: u16, data: &[u8]) -> Result<(), Error> {
    device
        .control_out(
            ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: ANCHOR_LOAD_INTERNAL,
                value: address,
                index: 0,
                data,
            },
            CONTROL_TIMEOUT,
        )
        .wait()
        .map_err(|source| Error::ControlTransfer { address, source })
}

/// Assert or release the 8051 reset line. The C reference sends this twice
/// per side and discards the first result; once suffices if it is checked.
fn set_reset(device: &Device, cpucs: u16, halted: bool) -> Result<(), Error> {
    write_memory(device, cpucs, &[u8::from(halted)])
}

/// Halt the 8051, upload every record, then let it run.
pub fn upload(device: &Device, model: Model, records: &[Record]) -> Result<(), Error> {
    if records.is_empty() {
        return Err(Error::EmptyFirmware);
    }

    set_reset(device, model.cpucs, true)?;

    for record in records {
        write_memory(device, record.address, &record.data)?;
    }

    set_reset(device, model.cpucs, false)
}

/// Block until the adapter reappears under its post-firmware product ID.
/// It has to enumerate again, so watch rather than guess at a sleep.
pub fn await_renumeration(model: Model, timeout: Duration) -> Result<DeviceInfo, Error> {
    let deadline = Instant::now() + timeout;

    loop {
        let devices = nusb::list_devices().wait().map_err(Error::Usb)?;
        for info in devices {
            if info.vendor_id() == VID_ADAPTEC && info.product_id() == model.ready_pid {
                return Ok(info);
            }
        }

        if Instant::now() >= deadline {
            return Err(Error::RenumerationTimeout { model: model.name });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
