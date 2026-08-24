//! SCSI command blocks and the replies we care about.

use std::fmt;

/// Ordinary SCSI-2 opcodes; the adapter bridges to a real bus.
pub mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const FORMAT_UNIT: u8 = 0x04;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SENSE_6: u8 = 0x1A;
    pub const START_STOP_UNIT: u8 = 0x1B;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
}

/// Where a command is going. The two halves travel separately: the target in
/// the wrapper's LUN byte, which this adapter repurposes for it, and the unit
/// in the CDB at byte 1 bits 5-7. Both are three bits, so both run 0 to 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address {
    pub target: u8,
    pub lun: u8,
}

impl Address {
    /// Unit 0, where a single-unit device lives.
    pub const fn target(target: u8) -> Self {
        Self { target, lun: 0 }
    }

    pub const fn new(target: u8, lun: u8) -> Self {
        Self { target, lun }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.lun == 0 {
            write!(f, "target {}", self.target)
        } else {
            write!(f, "target {} LUN {}", self.target, self.lun)
        }
    }
}

/// Set the legacy LUN field at byte 1, bits 5-7. Modern devices ignore it,
/// devices of this era often do not, and it is the only route to a second unit
/// here: the wrapper's LUN byte is spent on the target ID.
pub fn set_cdb_lun(cdb: &mut [u8], lun: u8) {
    if cdb.len() > 1 {
        cdb[1] = (cdb[1] & 0x1F) | ((lun & 0x07) << 5);
    }
}

/// Build a 6-byte command block.
pub fn cdb6(opcode: u8, lba: u32, length: u8, control: u8) -> Vec<u8> {
    vec![
        opcode,
        ((lba >> 16) & 0x1F) as u8,
        (lba >> 8) as u8,
        lba as u8,
        length,
        control,
    ]
}

/// Build a 10-byte command block.
pub fn cdb10(opcode: u8, lba: u32, length: u16) -> Vec<u8> {
    vec![
        opcode,
        0,
        (lba >> 24) as u8,
        (lba >> 16) as u8,
        (lba >> 8) as u8,
        lba as u8,
        0,
        (length >> 8) as u8,
        length as u8,
        0,
    ]
}

/// What kind of device sits at a target, from the INQUIRY peripheral type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeripheralType {
    DirectAccess,
    SequentialAccess,
    Printer,
    Processor,
    WriteOnce,
    CdRom,
    Scanner,
    OpticalMemory,
    MediumChanger,
    Communications,
    Other(u8),
}

impl PeripheralType {
    pub fn from_code(code: u8) -> Self {
        match code & 0x1F {
            0x00 => Self::DirectAccess,
            0x01 => Self::SequentialAccess,
            0x02 => Self::Printer,
            0x03 => Self::Processor,
            0x04 => Self::WriteOnce,
            0x05 => Self::CdRom,
            0x06 => Self::Scanner,
            0x07 => Self::OpticalMemory,
            0x08 => Self::MediumChanger,
            0x09 => Self::Communications,
            other => Self::Other(other),
        }
    }

    /// Whether this type addresses fixed-size blocks. A tape or scanner
    /// answers TEST UNIT READY happily and then has no capacity to report.
    pub fn is_block_like(self) -> bool {
        matches!(
            self,
            Self::DirectAccess | Self::WriteOnce | Self::CdRom | Self::OpticalMemory
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::DirectAccess => "disk",
            Self::SequentialAccess => "tape",
            Self::Printer => "printer",
            Self::Processor => "processor",
            Self::WriteOnce => "WORM",
            Self::CdRom => "CD-ROM",
            Self::Scanner => "scanner",
            Self::OpticalMemory => "optical",
            Self::MediumChanger => "changer",
            Self::Communications => "comms",
            Self::Other(_) => "unknown",
        }
    }
}

/// Peripheral qualifier, the top three bits of INQUIRY byte 0. How a target
/// says "I exist, but not at that unit": one that decodes the CDB's LUN field
/// answers `NoLun`, one that ignores it answers as itself every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qualifier {
    /// 000b: the device is here and connected.
    Connected,
    /// 001b: this type is supported, but nothing is connected.
    NotConnected,
    /// 011b: the target has no such logical unit.
    NoLun,
    /// Vendor specific or reserved.
    Other(u8),
}

impl Qualifier {
    pub fn from_code(code: u8) -> Self {
        match code >> 5 {
            0 => Self::Connected,
            1 => Self::NotConnected,
            3 => Self::NoLun,
            other => Self::Other(other),
        }
    }
}

/// The interesting fields of a standard INQUIRY reply.
#[derive(Debug, Clone)]
pub struct Inquiry {
    pub qualifier: Qualifier,
    pub peripheral_type: PeripheralType,
    pub removable: bool,
    pub version: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

/// Trim the space-padded ASCII fields SCSI uses for identification.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

impl Inquiry {
    /// Decode a standard INQUIRY reply. Needs at least 36 bytes.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 36 {
            return None;
        }

        Some(Self {
            qualifier: Qualifier::from_code(data[0]),
            peripheral_type: PeripheralType::from_code(data[0]),
            removable: data[1] & 0x80 != 0,
            version: data[2],
            vendor: text(&data[8..16]),
            product: text(&data[16..32]),
            revision: text(&data[32..36]),
        })
    }
}

impl Inquiry {
    /// The target answered but has nothing at that unit. Its identification
    /// fields mean nothing, though plenty of drives fill them in anyway.
    pub fn is_absent(&self) -> bool {
        matches!(self.qualifier, Qualifier::NoLun | Qualifier::NotConnected)
    }
}

impl fmt::Display for Inquiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.qualifier == Qualifier::NoLun {
            return write!(f, "no such logical unit");
        }
        if self.qualifier == Qualifier::NotConnected {
            return write!(f, "{} declared but not connected", self.peripheral_type.name());
        }

        write!(
            f,
            "{:<8} {:<16} {:<4}  {}{}",
            self.vendor,
            self.product,
            self.revision,
            self.peripheral_type.name(),
            if self.removable { ", removable" } else { "" }
        )
    }
}

/// Which addresses have reported a medium change and not yet been asked about.
///
/// One bit per unit, `target * 8 + lun`. Eight targets of eight units fit in a
/// word, so nothing is allocated on a path that runs once per command. An
/// address the bus cannot encode has no bit and is ignored.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MediumChanges(u64);

impl MediumChanges {
    fn bit(at: Address) -> Option<u32> {
        if at.target > MAX_ENCODABLE || at.lun > MAX_ENCODABLE {
            return None;
        }
        Some(at.target as u32 * 8 + at.lun as u32)
    }

    /// Record that the medium at this address has been swapped.
    pub fn note(&mut self, at: Address) {
        if let Some(bit) = Self::bit(at) {
            self.0 |= 1 << bit;
        }
    }

    /// Has the medium changed since this was last asked? Asking clears it, so
    /// the caller does not re-read a capacity it has already refreshed.
    pub fn take(&mut self, at: Address) -> bool {
        match Self::bit(at) {
            Some(bit) if self.0 & (1 << bit) != 0 => {
                self.0 &= !(1 << bit);
                true
            }
            _ => false,
        }
    }
}

/// Highest target or logical unit a narrow bus can encode.
const MAX_ENCODABLE: u8 = 7;

/// A decoded REQUEST SENSE reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// Valid only when the device sets the VALID bit. For a medium error it
    /// carries the failing block, which is the only way to learn where a
    /// format gave up: progress is a fraction, this is an address.
    pub information: Option<u32>,

    /// Progress as a fraction of 65536, present only when SKSV is set. The
    /// only channel a FORMAT UNIT has for reporting how far it has got.
    pub progress: Option<u16>,
}

impl Sense {
    pub const NO_SENSE: Self =
        Self { key: 0, asc: 0, ascq: 0, information: None, progress: None };

    /// For where a status byte stands in for sense the device never sent.
    pub const fn new(key: u8, asc: u8, ascq: u8) -> Self {
        Self { key, asc, ascq, information: None, progress: None }
    }

    /// Decode a REQUEST SENSE reply in either layout. Byte 0 says which, and
    /// they agree on nothing: the key moves from byte 2 to byte 1 and the
    /// codes from 12-13 to 2-3, so decoding one as the other reports the wrong
    /// fields without failing.
    ///
    /// `None` means no sense was legible, not that nothing is wrong.
    pub fn parse(data: &[u8]) -> Option<Self> {
        // Bit 7 is VALID or reserved; the code is the low seven bits.
        match data.first()? & 0x7F {
            0x70 | 0x71 => Self::parse_fixed(data),
            0x72 | 0x73 => Self::parse_descriptor(data),
            _ => None,
        }
    }

    /// Fixed format, which is all this driver has seen from a real drive.
    fn parse_fixed(data: &[u8]) -> Option<Self> {
        if data.len() < 14 {
            return None;
        }

        // Bytes 15-17, valid only when the top bit of byte 15 is set.
        let progress = match data.get(15..18) {
            Some(bytes) if bytes[0] & 0x80 != 0 => {
                Some(u16::from_be_bytes([bytes[1], bytes[2]]))
            }
            _ => None,
        };

        // VALID; without it bytes 3-6 mean nothing.
        let information = if data[0] & 0x80 != 0 {
            Some(u32::from_be_bytes([data[3], data[4], data[5], data[6]]))
        } else {
            None
        };

        Some(Self {
            key: data[2] & 0x0F,
            asc: data[12],
            ascq: data[13],
            information,
            progress,
        })
    }

    /// Descriptor format, as SPC-3 defines it. Unlikely from hardware of this
    /// era, and here so it is decoded rather than misread. The key and codes
    /// are in the header; the rest arrives as typed descriptors in any order
    /// and any subset, so they are walked rather than indexed.
    fn parse_descriptor(data: &[u8]) -> Option<Self> {
        /// Carries the INFORMATION field the fixed format puts at bytes 3-6.
        const INFORMATION: u8 = 0x00;
        /// Carries the sense-key-specific field, progress included.
        const SENSE_KEY_SPECIFIC: u8 = 0x02;

        if data.len() < 8 {
            return None;
        }

        let mut sense = Self {
            key: data[1] & 0x0F,
            asc: data[2],
            ascq: data[3],
            information: None,
            progress: None,
        };

        // Byte 7 says how much follows. Trust the shorter of that and what
        // arrived, so a truncated reply is walked only as far as it goes.
        let end = (8 + data[7] as usize).min(data.len());
        let mut at = 8;

        while at + 2 <= end {
            let length = data[at + 1] as usize;
            // Clamped, so a descriptor claiming more body than arrived fails
            // its length test below rather than discarding a good header.
            let body = &data[at + 2..(at + 2 + length).min(end)];

            match data[at] {
                // VALID, a reserved byte, then eight bytes of information.
                // Only the low 32 bits, as `information` holds.
                INFORMATION if body.len() >= 10 && body[0] & 0x80 != 0 => {
                    sense.information =
                        Some(u32::from_be_bytes([body[6], body[7], body[8], body[9]]));
                }
                // Two reserved bytes, then fixed format's 15-17, SKSV included.
                SENSE_KEY_SPECIFIC if body.len() >= 5 && body[2] & 0x80 != 0 => {
                    sense.progress = Some(u16::from_be_bytes([body[3], body[4]]));
                }
                _ => {}
            }

            at += 2 + length;
        }

        Some(sense)
    }

    /// True when the device is working through a FORMAT UNIT.
    pub fn is_format_in_progress(self) -> bool {
        self.key == 0x02 && self.asc == 0x04 && self.ascq == 0x04
    }

    /// Progress through a long operation, as a percentage.
    pub fn progress_percent(self) -> Option<f32> {
        self.progress.map(|raw| raw as f32 * 100.0 / 65536.0)
    }

    /// True when the sense data says nothing actually went wrong.
    pub fn is_clear(self) -> bool {
        matches!(self.key, 0x00 | 0x01)
    }

    /// No medium loaded, which for a Jaz or Zip is everyday, not a fault.
    pub fn is_no_medium(self) -> bool {
        self.key == 0x02 && self.asc == 0x3A
    }

    /// Reset or medium change. The next command normally succeeds.
    pub fn is_unit_attention(self) -> bool {
        self.key == 0x06
    }

    /// A swap rather than a reset, which for removable media is the whole
    /// point. `0x29` means everything we knew still holds and a retry is the
    /// complete answer; `0x28` means capacity, block size and contents may all
    /// differ, and retrying past it answers the old cartridge from the new.
    pub fn is_medium_changed(self) -> bool {
        self.is_unit_attention() && self.asc == 0x28
    }

    /// Plain text for the additional sense code, where the reason lives: the
    /// key alone rarely separates "no such page" from "impossible". Covers
    /// what this driver has met plus the common neighbours; the rest by
    /// number.
    pub fn asc_name(self) -> Option<&'static str> {
        Some(match (self.asc, self.ascq) {
            (0x04, 0x00) => "not ready, no reason reported",
            (0x04, 0x01) => "becoming ready",
            (0x04, 0x02) => "initialising command required",
            (0x04, 0x03) => "manual intervention required",
            (0x04, 0x04) => "format in progress",
            (0x0c, _) => "write error",
            (0x10, _) => "id CRC or ECC error",
            (0x11, 0x00) => "unrecovered read error",
            (0x11, 0x01) => "read retries exhausted",
            (0x11, 0x02) => "error too long to correct",
            (0x11, 0x04) => "unrecovered read error, auto reallocate failed",
            (0x11, _) => "unrecovered read error",
            (0x12, _) => "address mark not found for id field",
            (0x13, _) => "address mark not found for data field",
            (0x14, _) => "recorded entity not found",
            (0x15, _) => "random positioning error",
            (0x16, _) => "data synchronisation mark error",
            (0x17, _) => "recovered data with retries",
            (0x18, _) => "recovered data with ECC",
            (0x19, _) => "defect list error",
            (0x1a, _) => "parameter list length error",
            (0x1c, _) => "defect list not found",
            (0x20, _) => "invalid command operation code",
            (0x21, _) => "logical block address out of range",
            (0x24, _) => "invalid field in CDB",
            (0x25, _) => "logical unit not supported",
            (0x26, _) => "invalid field in parameter list",
            (0x27, _) => "write protected",
            (0x28, _) => "medium changed",
            (0x29, _) => "power on, reset or bus device reset",
            (0x2c, _) => "command sequence error",
            (0x30, 0x00) => "incompatible medium installed",
            (0x30, 0x01) => "cannot read medium, unknown format",
            (0x30, 0x02) => "cannot read medium, incompatible format",
            (0x31, _) => "medium format corrupted",
            (0x3a, _) => "medium not present",
            (0x64, _) => "illegal mode for this track",
            _ => return None,
        })
    }

    pub fn key_name(self) -> &'static str {
        match self.key {
            0x00 => "no sense",
            0x01 => "recovered error",
            0x02 => "not ready",
            0x03 => "medium error",
            0x04 => "hardware error",
            0x05 => "illegal request",
            0x06 => "unit attention",
            0x07 => "data protect",
            0x08 => "blank check",
            0x0B => "aborted command",
            0x0D => "volume overflow",
            0x0E => "miscompare",
            _ => "reserved",
        }
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.asc_name() {
            Some(reason) => write!(
                f,
                "{reason} ({}, asc {:#04x}/{:#04x})",
                self.key_name(),
                self.asc,
                self.ascq
            ),
            None => write!(
                f,
                "{} (key {:#04x}, asc {:#04x}/{:#04x})",
                self.key_name(),
                self.key,
                self.asc,
                self.ascq
            ),
        }?;

        if let Some(lba) = self.information {
            write!(f, " at LBA {lba}")?;
        }
        Ok(())
    }
}

/// The Format Device mode page. A drive returning it expects to be formatted,
/// which is evidence rather than proof: the only certain test destroys the
/// medium.
#[derive(Debug, Clone, Copy)]
pub struct FormatPage {
    pub sectors_per_track: u16,
    pub bytes_per_sector: u16,
    pub interleave: u16,
    pub removable: bool,
}

impl FormatPage {
    /// Pull page 0x03 out of a MODE SENSE(6) reply.
    pub fn parse(data: &[u8]) -> Option<Self> {
        // Four header bytes, then block descriptors, then the pages.
        let descriptors = *data.get(3)? as usize;
        let mut at = 4 + descriptors;

        while at + 2 <= data.len() {
            let code = data[at] & 0x3F;
            let length = data[at + 1] as usize;
            let page = data.get(at..at + 2 + length)?;

            if code == 0x03 && page.len() >= 21 {
                return Some(Self {
                    sectors_per_track: u16::from_be_bytes([page[10], page[11]]),
                    bytes_per_sector: u16::from_be_bytes([page[12], page[13]]),
                    interleave: u16::from_be_bytes([page[14], page[15]]),
                    removable: page[20] & 0x20 != 0,
                });
            }
            at += 2 + length;
        }
        None
    }
}

/// Whether the medium can be written to, from a MODE SENSE header.
pub fn write_protected(data: &[u8]) -> Option<bool> {
    data.get(2).map(|byte| byte & 0x80 != 0)
}

/// Result of READ CAPACITY (10).
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub last_lba: u32,
    pub block_size: u32,
}

impl Capacity {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        Some(Self {
            last_lba: u32::from_be_bytes(data[0..4].try_into().ok()?),
            block_size: u32::from_be_bytes(data[4..8].try_into().ok()?),
        })
    }

    /// `last_lba` is the final block's address, so the count is one more.
    pub fn bytes(self) -> u64 {
        (self.last_lba as u64 + 1) * self.block_size as u64
    }
}

impl fmt::Display for Capacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mib = self.bytes() as f64 / (1024.0 * 1024.0);
        write!(
            f,
            "{} blocks of {} bytes ({:.1} MiB)",
            self.last_lba as u64 + 1,
            self.block_size,
            mib
        )
    }
}
