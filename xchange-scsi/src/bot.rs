//! Bulk-Only Transport, bent to suit the Adaptec USB2Xchange.
//!
//! Two departures from standard BOT, both from René Rebe's T2 SDE patch:
//!
//! 1. The command block's LUN byte carries the SCSI target ID, not a LUN.
//! 2. Status `0x8A` means "nothing at this target" and `0x02` means "needs a
//!    REQUEST SENSE". Standard code treats anything above `0x01` as a phase
//!    error and resets the device.
//!
//! Both are edits to core `usb-storage` in that patch, which is why it never
//! merged. Owning the transport makes them ordinary.

use std::time::{Duration, Instant};

use nusb::transfer::{Buffer, Bulk, ControlOut, ControlType, In, Out, Recipient};
use nusb::{Device, Endpoint, Interface, MaybeFuture};

use crate::scsi::{self, Address, Capacity, FormatPage, Inquiry, MediumChanges, Sense};
use crate::Error;

pub const VID_ADAPTEC: u16 = 0x03f3;
pub const PID_USB2XCHANGE_READY: u16 = 0x2003;
pub const PID_USBXCHANGE_READY: u16 = 0x2001;

/// Bulk endpoints reported by the running firmware.
const EP_BULK_OUT: u8 = 0x02;
const EP_BULK_IN: u8 = 0x86;

/// Sent with `wValue` 1 then 2 after re-enumeration. Without it the adapter
/// does not report its bus properly.
const REQ_INIT: u8 = 0x5A;

const CBW_SIGNATURE: u32 = 0x4342_5355; // 'USBC'
const CSW_SIGNATURE: u32 = 0x5342_5355; // 'USBS'
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;
const MAX_CDB_LEN: usize = 16;

/// Largest transfer we will ask the adapter to carry.
///
/// The firmware's buffer is under 64 KB and 64 KB requests crash it.
/// `xchange-conform --transfer-limit` measured 65024 as correct on both an
/// Iomega Jaz and an IBM WDS-L80, so the ceiling is the adapter's rather than
/// any drive's. 62 KB keeps two blocks of margin, which `xchange bench` shows
/// costs nothing: 0.78 MB/s against 0.79 at 65024.
pub const MAX_TRANSFER: usize = 62 * 1024;

/// The initiator sits at ID 7 by convention, so targets run 0 to 6.
pub const MAX_TARGET: u8 = 7;

/// Highest addressable target.
///
/// A narrow bus: three bits of target ID. The wrapper's address byte is eight
/// bits but the extra five encode nothing. The firmware does not reject the
/// meaningless values, it degrades, and does not recover on its own:
///
/// | address | behaviour |
/// |---|---|
/// | 0x00-0x08 | answers normally, `0x8A` where nothing is attached |
/// | 0x09-0x0D | data phase times out |
/// | 0x0E-0x0F | command blocks refused outright |
/// | after that | drops off USB entirely, needs a power cycle |
///
/// [`Adapter::command`] rejects them outright rather than letting anything put
/// the hardware there.
pub const MAX_ADDRESS: u8 = 7;

/// For initialisation and draining a desynchronised endpoint. Short, so an
/// adapter that has stopped answering is recognised quickly.
const HOUSEKEEPING_TIMEOUT: Duration = Duration::from_secs(5);

/// Long, deliberately. A drive of this vintage can spend tens of seconds
/// retrying a bad sector. Cutting it off replaces a MEDIUM ERROR with a
/// timeout and the retry spends the time again.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Set `XCHANGE_TRACE=1` to dump wrappers as they go past. Easier to reach for
/// than usbmon, which needs root.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCHANGE_TRACE").is_ok_and(|v| v != "0"))
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if tracing() {
            eprintln!("[xchange] {}", format!($($arg)*));
        }
    };
}

/// What came back on the bulk IN endpoint. Data and status share it, and this
/// adapter skips the data phase when a command fails, so a read intended for
/// data can legitimately return the status wrapper.
enum Phase {
    Data(Vec<u8>),
    Status(Status, u32),
    Stalled,
}

/// How a FORMAT UNIT should be carried out.
#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    /// Set DCRT, skipping the certification pass. That is where a format
    /// spends its time and all its retries, so this finishes one that would
    /// otherwise crawl, at the cost of the drive not knowing which sectors
    /// are bad. For recovering a cartridge, not for preparing a trusted one.
    pub skip_certification: bool,

    /// Set DPRY, ignoring the primary defect list. Needed when that list is
    /// damaged, which is what an unfinished format leaves behind. The factory
    /// defect map is then forgotten.
    pub ignore_primary: bool,

    /// Addresses to hand the drive as known defects, in block format.
    pub defects: Vec<u32>,
}

/// Four bytes per defect, as a logical block address.
pub const DEFECT_FORMAT_BLOCK: u8 = 0b000;
/// Eight bytes per defect, counted in bytes from the index mark.
pub const DEFECT_FORMAT_INDEX: u8 = 0b100;
/// Eight bytes per defect, as cylinder, head and sector.
pub const DEFECT_FORMAT_PHYSICAL: u8 = 0b101;

/// The defect lists a device keeps.
#[derive(Debug, Clone)]
pub struct DefectList {
    pub primary_valid: bool,
    pub grown_valid: bool,
    /// Defect addresses, when the drive reports them in block format.
    pub addresses: Vec<u32>,
    /// Counted rather than listed: the head/cylinder/sector formats need
    /// drive geometry to mean anything.
    pub undecoded: usize,
}

impl DefectList {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }

        let format = data[1] & 0x07;
        let length = u16::from_be_bytes([data[2], data[3]]) as usize;
        let body = data.get(4..(4 + length).min(data.len()))?;

        let (addresses, undecoded) = if format == 0 {
            (
                body.chunks_exact(4)
                    .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                0,
            )
        } else {
            (Vec::new(), body.len() / 8)
        };

        Some(Self {
            primary_valid: data[1] & 0x10 != 0,
            grown_valid: data[1] & 0x08 != 0,
            addresses,
            undecoded,
        })
    }

    pub fn total(&self) -> usize {
        self.addresses.len() + self.undecoded
    }
}

/// What a device says about being formatted.
#[derive(Debug, Clone, Copy)]
pub enum FormatCapability {
    /// The device answered, whether or not the page itself decoded.
    Page {
        page: Option<FormatPage>,
        write_protected: bool,
    },
    /// No Format Device mode page. Says nothing either way about FORMAT UNIT.
    NoSuchPage,
}

/// How a running FORMAT UNIT is progressing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormatState {
    /// The percentage is present only if the drive reports one; plenty say
    /// they are busy without saying how busy.
    Running(Option<f32>),
    Finished,
    Failed(Sense),
}

/// How a device answered when asked whether it was ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    Yes,
    /// The drive is fine, there is simply nothing loaded.
    NoMedium,
    /// Still not ready when we ran out of patience.
    TimedOut,
    /// The drive gave a reason that is not going to resolve on its own.
    Refused(Sense),
}

impl Ready {
    pub fn describe(self) -> String {
        match self {
            Self::Yes => "ready".to_string(),
            Self::NoMedium => "no medium loaded".to_string(),
            Self::TimedOut => "did not become ready in time".to_string(),
            Self::Refused(sense) => sense.to_string(),
        }
    }
}

/// Which way data flows for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    None,
    In,
    Out,
}

/// Status byte from the command status wrapper, including the adapter's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Command completed.
    Good,
    /// Command failed; sense data explains why.
    Failed,
    /// Adaptec: the command needs a REQUEST SENSE to find out what happened.
    NeedsSense,
    /// Adaptec: nothing is attached at this target.
    NoDevice,
    /// Anything else, kept as-is rather than guessed at. This firmware uses
    /// more of the byte than BOT defines. Seen in `xchange-conform` runs:
    ///
    /// * `0x92` when the declared transfer length disagrees with the device,
    ///   as when asking a 2048-byte CD-ROM for one 512-byte block.
    /// * `0x04` from a Toshiba CD-ROM for PRE-FETCH(10); a Jaz refuses the
    ///   same command normally, so it is device dependent.
    Other(u8),
}

impl Status {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0x00 => Self::Good,
            0x01 => Self::Failed,
            0x02 => Self::NeedsSense,
            0x8A => Self::NoDevice,
            other => Self::Other(other),
        }
    }
}

/// Outcome of one command: its status and whatever data came back.
#[derive(Debug)]
pub struct Response {
    pub status: Status,
    pub data: Vec<u8>,
    pub residue: u32,
}

/// Reset a running adapter over USB without talking SCSI first.
///
/// [`Adapter::open`] sends initialisation requests a hung adapter will not
/// answer, so recovery cannot go through it. A port reset fixes a firmware
/// that has stopped answering SCSI but not a hard hang, which needs power
/// removed. This blocks while the kernel retries enumeration, so a long hang
/// here means "unplug it".
pub fn reset_by_usb() -> Result<(), Error> {
    let devices = nusb::list_devices().wait().map_err(Error::Usb)?;

    let info = devices
        .filter(|d| d.vendor_id() == VID_ADAPTEC)
        .find(|d| matches!(d.product_id(), PID_USB2XCHANGE_READY | PID_USBXCHANGE_READY))
        .ok_or(Error::NotFound)?;

    let device = info.open().wait().map_err(Error::Usb)?;
    device.reset().wait().map_err(Error::Usb)
}

/// An open adapter with its firmware running.
pub struct Adapter {
    _device: Device,
    _interface: Interface,
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    tag: u32,
    transfer_limit: usize,
    command_timeout: Duration,

    /// Addresses whose medium has been swapped since anyone last asked.
    ///
    /// The retry in `command_checked` consumes the unit attention that carries
    /// a medium change, and that retry has to stay: the adapter raises one on
    /// first contact. Retrying is right for a reset and wrong for a swapped
    /// cartridge, so the fact is latched here instead.
    medium_changed: MediumChanges,
}

impl Adapter {
    /// Open an adapter that already has firmware running.
    pub fn open() -> Result<Self, Error> {
        let devices = nusb::list_devices().wait().map_err(Error::Usb)?;

        let info = devices
            .filter(|d| d.vendor_id() == VID_ADAPTEC)
            .find(|d| matches!(d.product_id(), PID_USB2XCHANGE_READY | PID_USBXCHANGE_READY))
            .ok_or(Error::NotFound)?;

        let device = info.open().wait().map_err(Error::Usb)?;
        let interface = device.claim_interface(0).wait().map_err(Error::Usb)?;

        let ep_out = interface
            .endpoint::<Bulk, Out>(EP_BULK_OUT)
            .map_err(Error::Usb)?;
        let ep_in = interface
            .endpoint::<Bulk, In>(EP_BULK_IN)
            .map_err(Error::Usb)?;

        let mut adapter = Self {
            _device: device,
            _interface: interface,
            ep_out,
            ep_in,
            tag: 1,
            transfer_limit: MAX_TRANSFER,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            medium_changed: MediumChanges::default(),
        };
        adapter.initialise()?;
        // Clear anything a previous, abandoned run left in the pipe.
        adapter.resynchronise();
        Ok(adapter)
    }

    /// The two vendor requests the firmware expects after re-enumeration.
    fn initialise(&mut self) -> Result<(), Error> {
        for value in [1u16, 2u16] {
            self._device
                .control_out(
                    ControlOut {
                        control_type: ControlType::Vendor,
                        recipient: Recipient::Device,
                        request: REQ_INIT,
                        value,
                        index: 0,
                        data: &[],
                    },
                    HOUSEKEEPING_TIMEOUT,
                )
                .wait()
                .map_err(|source| Error::Transfer {
                    stage: "initialisation",
                    source,
                })?;
        }
        Ok(())
    }

    /// Round an IN request up to whole packets, as the endpoint requires.
    fn packet_aligned(&self, len: usize) -> usize {
        let packet = self.ep_in.max_packet_size().max(1);
        len.div_ceil(packet) * packet
    }

    /// Run one command. The target goes into the wrapper's LUN byte, which is
    /// where this adapter expects it.
    pub fn command(
        &mut self,
        at: Address,
        cdb: &[u8],
        direction: Direction,
        length: usize,
    ) -> Result<Response, Error> {
        self.command_with(at, cdb, direction, length, &[])
    }

    /// As [`Self::command`], but carrying outbound data for a write.
    pub fn command_with(
        &mut self,
        at: Address,
        cdb: &[u8],
        direction: Direction,
        length: usize,
        payload: &[u8],
    ) -> Result<Response, Error> {
        if at.target > MAX_ADDRESS || at.lun > MAX_ADDRESS {
            return Err(Error::AddressUnencodable { at });
        }

        if length > self.transfer_limit {
            return Err(Error::TransferTooLarge {
                len: length,
                limit: self.transfer_limit,
            });
        }

        let tag = self.tag;
        self.tag = self.tag.wrapping_add(1).max(1);

        // Set here, so no command can go out accidentally addressed to unit 0.
        let mut cdb = cdb.to_vec();
        scsi::set_cdb_lun(&mut cdb, at.lun);

        trace!("CBW tag={tag} at={at} len={length} cdb={:02x?}", cdb);

        if let Err(error) = self.send_command(tag, at.target, &cdb, direction, length) {
            self.resynchronise();
            return Err(error);
        }

        if direction == Direction::Out && length > 0 {
            if let Err(error) = self.write_data(payload) {
                self.resynchronise();
                return Err(error);
            }
        }

        let mut data = Vec::new();
        let mut status = None;

        if direction == Direction::In && length > 0 {
            match self.read_phase(tag, length) {
                Ok(Phase::Data(bytes)) => data = bytes,
                Ok(Phase::Status(s, residue)) => {
                    // Failed, and the adapter went straight to status.
                    trace!("status arrived in place of data: {s:?}");
                    status = Some((s, residue));
                }
                Ok(Phase::Stalled) => {}
                Err(error) => {
                    self.resynchronise();
                    return Err(error);
                }
            }
        }

        let (status, residue) = match status {
            Some(pair) => pair,
            None => match self.read_status(tag) {
                Ok(pair) => pair,
                Err(error) => {
                    self.resynchronise();
                    return Err(error);
                }
            },
        };

        trace!("CSW tag={tag} status={status:?} residue={residue} data={} bytes", data.len());
        Ok(Response { status, data, residue })
    }

    /// Decode a 13-byte command status wrapper, if that is what this is.
    fn as_status(bytes: &[u8], tag: u32) -> Option<(Status, u32)> {
        if bytes.len() < CSW_LEN {
            return None;
        }
        if u32::from_le_bytes(bytes[0..4].try_into().ok()?) != CSW_SIGNATURE {
            return None;
        }
        if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != tag {
            return None;
        }
        Some((
            Status::from_byte(bytes[12]),
            u32::from_le_bytes(bytes[8..12].try_into().ok()?),
        ))
    }

    /// Discard anything still queued on the bulk IN endpoint. A command
    /// abandoned mid-flight leaves its status wrapper in the pipe, and it
    /// survives into the next CLI run as an unexplained tag mismatch.
    fn resynchronise(&mut self) {
        for _ in 0..4 {
            let buffer = Buffer::new(self.packet_aligned(CSW_LEN));
            let completion = self
                .ep_in
                .transfer_blocking(buffer, Duration::from_millis(150));

            match completion.status {
                Ok(()) if completion.actual_len > 0 => {
                    trace!("drained {} stale bytes", completion.actual_len);
                }
                Err(nusb::transfer::TransferError::Stall) => {
                    let _ = self.ep_in.clear_halt().wait();
                }
                _ => return,
            }
        }
    }

    /// Build and send the 31-byte command block wrapper.
    fn send_command(
        &mut self,
        tag: u32,
        target: u8,
        cdb: &[u8],
        direction: Direction,
        length: usize,
    ) -> Result<(), Error> {
        let mut cbw = Vec::with_capacity(CBW_LEN);
        cbw.extend_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw.extend_from_slice(&tag.to_le_bytes());
        cbw.extend_from_slice(&(length as u32).to_le_bytes());
        cbw.push(if direction == Direction::In { 0x80 } else { 0x00 });
        // Quirk 1: the LUN byte carries the target ID on this adapter.
        cbw.push(target);
        cbw.push(cdb.len().min(MAX_CDB_LEN) as u8);
        cbw.extend_from_slice(&cdb[..cdb.len().min(MAX_CDB_LEN)]);
        cbw.resize(CBW_LEN, 0);

        let mut buffer = Buffer::new(CBW_LEN);
        buffer.extend_from_slice(&cbw);

        let completion = self.ep_out.transfer_blocking(buffer, self.command_timeout);
        completion.status.map_err(|source| Error::Transfer {
            stage: "command block",
            source,
        })?;

        if completion.actual_len != CBW_LEN {
            return Err(Error::ShortCommand {
                sent: completion.actual_len,
                expected: CBW_LEN,
            });
        }
        Ok(())
    }

    /// Send the outbound data phase for a write.
    fn write_data(&mut self, payload: &[u8]) -> Result<(), Error> {
        let mut buffer = Buffer::new(payload.len());
        buffer.extend_from_slice(payload);

        let completion = self.ep_out.transfer_blocking(buffer, self.command_timeout);
        completion.status.map_err(|source| Error::Transfer {
            stage: "write data",
            source,
        })?;

        if completion.actual_len != payload.len() {
            return Err(Error::ShortCommand {
                sent: completion.actual_len,
                expected: payload.len(),
            });
        }
        Ok(())
    }

    /// Read what follows the command block: data, or the status wrapper if the
    /// adapter skipped the data phase. A stall means the device had less to say
    /// than we asked for, so clear it and carry on.
    fn read_phase(&mut self, tag: u32, length: usize) -> Result<Phase, Error> {
        let buffer = Buffer::new(self.packet_aligned(length));
        let completion = self.ep_in.transfer_blocking(buffer, self.command_timeout);

        match completion.status {
            Ok(()) => {
                let mut data = completion.buffer.into_vec();
                data.truncate(completion.actual_len);

                if let Some((status, residue)) = Self::as_status(&data, tag) {
                    return Ok(Phase::Status(status, residue));
                }

                data.truncate(length);
                Ok(Phase::Data(data))
            }
            Err(nusb::transfer::TransferError::Stall) => {
                self.ep_in.clear_halt().wait().map_err(Error::Usb)?;
                Ok(Phase::Stalled)
            }
            Err(source) => Err(Error::Transfer {
                stage: "data",
                source,
            }),
        }
    }

    /// Read and validate the 13-byte command status wrapper.
    fn read_status(&mut self, tag: u32) -> Result<(Status, u32), Error> {
        let mut completion = {
            let buffer = Buffer::new(self.packet_aligned(CSW_LEN));
            self.ep_in.transfer_blocking(buffer, self.command_timeout)
        };

        // A stalled status phase is recoverable: clear the halt and ask again.
        if let Err(nusb::transfer::TransferError::Stall) = completion.status {
            self.ep_in.clear_halt().wait().map_err(Error::Usb)?;
            let buffer = Buffer::new(self.packet_aligned(CSW_LEN));
            completion = self.ep_in.transfer_blocking(buffer, self.command_timeout);
        }

        completion.status.map_err(|source| Error::Transfer {
            stage: "status",
            source,
        })?;

        if completion.actual_len < CSW_LEN {
            return Err(Error::ShortStatus {
                len: completion.actual_len,
            });
        }

        let csw = completion.buffer.into_vec();
        let signature = u32::from_le_bytes(csw[0..4].try_into().unwrap());
        if signature != CSW_SIGNATURE {
            return Err(Error::BadStatusSignature(signature));
        }

        let got = u32::from_le_bytes(csw[4..8].try_into().unwrap());
        if got != tag {
            return Err(Error::TagMismatch { got, expected: tag });
        }

        let residue = u32::from_le_bytes(csw[8..12].try_into().unwrap());
        Ok((Status::from_byte(csw[12]), residue))
    }

    /// Ask what is at an address.
    pub fn inquiry(&mut self, at: Address) -> Result<Option<Inquiry>, Error> {
        const LEN: usize = 36;
        let cdb = scsi::cdb6(scsi::op::INQUIRY, 0, LEN as u8, 0);

        let response = self.command(at, &cdb, Direction::In, LEN)?;

        match response.status {
            Status::NoDevice => Ok(None),
            _ if response.data.len() < LEN => Ok(None),
            // A target that answers "no such logical unit" is not a device, and
            // reporting its identification fields would invent one.
            _ => Ok(Inquiry::parse(&response.data).filter(|found| !found.is_absent())),
        }
    }

    /// The undecorated INQUIRY reply. [`Self::inquiry`] drops replies whose
    /// qualifier says the unit is absent, but that answer is the one that
    /// proves the CDB's LUN field reached the drive.
    pub fn inquiry_raw(&mut self, at: Address) -> Result<Option<Vec<u8>>, Error> {
        const LEN: usize = 36;
        let cdb = scsi::cdb6(scsi::op::INQUIRY, 0, LEN as u8, 0);

        let response = self.command(at, &cdb, Direction::In, LEN)?;
        if response.status == Status::NoDevice || response.data.len() < LEN {
            return Ok(None);
        }
        Ok(Some(response.data))
    }

    /// Fetch sense data after a command that asked for it.
    ///
    /// An illegible reply is an error, not NO_SENSE. NO_SENSE is the device
    /// saying nothing went wrong; an empty reply is its explanation never
    /// arriving. Folding the two together made a refused command look like a
    /// successful one.
    pub fn request_sense(&mut self, at: Address) -> Result<Sense, Error> {
        const LEN: usize = 18;
        let cdb = scsi::cdb6(scsi::op::REQUEST_SENSE, 0, LEN as u8, 0);
        let response = self.command(at, &cdb, Direction::In, LEN)?;

        let sense = Sense::parse(&response.data).ok_or(Error::UnreadableSense {
            at,
            len: response.data.len(),
        })?;

        // Every sense reply passes through here, and the drive reports a
        // medium change exactly once. Miss it and it is gone.
        if sense.is_medium_changed() {
            self.medium_changed.note(at);
        }

        Ok(sense)
    }

    /// Has the medium changed since this was last asked? Asking clears it.
    /// Only ever set by a failed command that had its sense read: an idle
    /// drive says nothing until something asks.
    pub fn take_medium_change(&mut self, at: Address) -> bool {
        self.medium_changed.take(at)
    }

    /// Run a command, resolving the adapter's "needs sense" status into real
    /// sense data, and retrying once through a unit attention.
    pub fn command_checked(
        &mut self,
        at: Address,
        cdb: &[u8],
        direction: Direction,
        length: usize,
        what: &'static str,
    ) -> Result<Response, Error> {
        for attempt in 0..2 {
            let response = match self.command(at, cdb, direction, length) {
                Ok(response) => response,
                // With a unit attention pending this adapter sometimes drops
                // the data phase and the status wrapper both, leaving us
                // waiting. The attempt clears the condition, so the retry
                // works. Reliable on the first READ CAPACITY after opening.
                Err(Error::Transfer { .. }) if attempt == 0 => continue,
                Err(error) => return Err(error),
            };

            match response.status {
                Status::Good => return Ok(response),
                Status::NoDevice => return Err(Error::CommandFailed { at, command: what }),
                Status::NeedsSense | Status::Failed => {
                    let sense = self.request_sense(at)?;
                    if sense.is_clear() {
                        return Ok(response);
                    }
                    // Reported once after a reset or a swap; the retry works.
                    if sense.is_unit_attention() && attempt == 0 {
                        continue;
                    }
                    return Err(Error::CheckCondition { at, sense });
                }
                Status::Other(_) => return Err(Error::CommandFailed { at, command: what }),
            }
        }

        Err(Error::CommandFailed { at, command: what })
    }

    /// Is the target ready to transfer data?
    pub fn test_unit_ready(&mut self, at: Address) -> Result<Result<(), Sense>, Error> {
        let cdb = scsi::cdb6(scsi::op::TEST_UNIT_READY, 0, 0, 0);
        let response = self.command(at, &cdb, Direction::None, 0)?;

        match response.status {
            Status::Good => Ok(Ok(())),
            _ => Ok(Err(self.request_sense(at)?)),
        }
    }

    /// Read a MODE SENSE(6) page, header included, so the caller can read the
    /// write-protect bit as well.
    pub fn mode_sense(&mut self, at: Address, page: u8) -> Result<Vec<u8>, Error> {
        const LEN: usize = 192;
        let cdb = scsi::cdb6(scsi::op::MODE_SENSE_6, 0, LEN as u8, 0);
        let mut cdb = cdb;
        cdb[2] = page;

        let response = self.command_checked(at, &cdb, Direction::In, LEN, "MODE SENSE(6)")?;
        Ok(response.data)
    }

    /// What a FORMAT UNIT here would produce, if the device says. Read-only,
    /// and the nearest thing to a support check that does not destroy the
    /// medium. No page is a normal answer, not a failure: devices return
    /// INVALID FIELD IN CDB for mode pages they do not implement.
    pub fn format_capability(&mut self, at: Address) -> Result<FormatCapability, Error> {
        let data = match self.mode_sense(at, 0x03) {
            Ok(data) => data,
            Err(Error::CheckCondition { sense, .. }) if sense.asc == 0x24 => {
                return Ok(FormatCapability::NoSuchPage)
            }
            Err(error) => return Err(error),
        };

        Ok(FormatCapability::Page {
            page: FormatPage::parse(&data),
            write_protected: scsi::write_protected(&data).unwrap_or(false),
        })
    }

    /// Read the defect lists a device keeps: `primary` is the factory list,
    /// `grown` the sectors it has had to reassign in service.
    pub fn read_defect_data(
        &mut self,
        at: Address,
        primary: bool,
        grown: bool,
    ) -> Result<DefectList, Error> {
        const HEADER: usize = 4;

        // One attempt, deliberately. Retrying in all three list formats made
        // a Jaz 2GB seek and read for each, return nothing, and desynchronise
        // the endpoint until the adapter needed a power cycle.
        let header = self.defect_data(at, primary, grown, DEFECT_FORMAT_BLOCK, HEADER)?;

        if header.len() < HEADER {
            // The drive works and returns nothing, so the data phase was lost
            // rather than the command refused: a refusal arrives as sense.
            self.resynchronise();
            return Err(Error::NoDefectLists { at });
        }

        let listed = u16::from_be_bytes([header[2], header[3]]) as usize;
        if listed == 0 {
            return DefectList::parse(&header).ok_or(Error::NoDefectLists { at });
        }

        let wanted = (HEADER + listed).min(self.transfer_limit).min(u16::MAX as usize);
        let full = self.defect_data(at, primary, grown, DEFECT_FORMAT_BLOCK, wanted)?;

        DefectList::parse(&full).ok_or(Error::ShortResponse {
            at,
            len: full.len(),
            what: "READ DEFECT DATA(10)",
        })
    }

    /// One READ DEFECT DATA(10) with a given list format and allocation length.
    fn defect_data(
        &mut self,
        at: Address,
        primary: bool,
        grown: bool,
        format: u8,
        length: usize,
    ) -> Result<Vec<u8>, Error> {
        let mut cdb = vec![0x37u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        // Byte 2: which lists to return, and in what address format.
        cdb[2] = (u8::from(primary) << 4) | (u8::from(grown) << 3) | (format & 0x07);
        cdb[7] = (length >> 8) as u8;
        cdb[8] = length as u8;

        let response =
            self.command_checked(at, &cdb, Direction::In, length, "READ DEFECT DATA(10)")?;
        Ok(response.data)
    }

    /// Begin a FORMAT UNIT, returning once the drive accepts it.
    ///
    /// IMMED is what makes this usable: without it the command runs for the
    /// length of the format, the transfer timeout fires, and the endpoint is
    /// resynchronised under a drive midway through erasing itself. With it,
    /// progress comes back through REQUEST SENSE.
    ///
    /// **This destroys everything on the medium.**
    pub fn format_unit(&mut self, at: Address, options: FormatOptions) -> Result<(), Error> {
        // FmtData set, so a defect list header follows in block format.
        let cdb = vec![scsi::op::FORMAT_UNIT, 0x10, 0, 0, 0, 0];

        // IMMED is always set; the rest need FOV to mean anything. FOV also
        // makes the drive honour DPRY as supplied, and DPRY clear means "use
        // the primary defect list" — which a half-formatted medium no longer
        // has, giving an immediate `defect list not found`. Hence
        // `ignore_primary`.
        let mut flags = 0x02u8;
        if options.skip_certification || options.ignore_primary {
            flags |= 0x80;
        }
        if options.ignore_primary {
            flags |= 0x40;
        }
        if options.skip_certification {
            flags |= 0x20;
        }

        let bytes = (options.defects.len() * 4) as u16;
        let mut params = vec![0x00, flags, (bytes >> 8) as u8, bytes as u8];
        for lba in &options.defects {
            params.extend_from_slice(&lba.to_be_bytes());
        }

        let response =
            self.command_with(at, &cdb, Direction::Out, params.len(), &params)?;

        match response.status {
            Status::Good => Ok(()),
            Status::NeedsSense | Status::Failed => {
                let sense = self.request_sense(at)?;
                if sense.is_clear() || sense.is_format_in_progress() {
                    Ok(())
                } else {
                    Err(Error::CheckCondition { at, sense })
                }
            }
            _ => Err(Error::CommandFailed {
                at,
                command: "FORMAT UNIT",
            }),
        }
    }

    /// How a format that is already running is getting on.
    pub fn format_progress(&mut self, at: Address) -> Result<FormatState, Error> {
        let sense = self.request_sense(at)?;

        if sense.is_format_in_progress() {
            return Ok(FormatState::Running(sense.progress_percent()));
        }
        if sense.is_clear() {
            return Ok(FormatState::Finished);
        }
        Ok(FormatState::Failed(sense))
    }

    /// Wait for a device to finish becoming ready.
    ///
    /// A drive spinning up reports NOT READY, and removable media raises a
    /// unit attention on first contact. Neither is a failure, but a data-in
    /// command issued during spin-up may not get a status wrapper back at all,
    /// so this uses TEST UNIT READY, which has no data phase. No medium is not
    /// an error, just not ready.
    pub fn wait_until_ready(&mut self, at: Address, timeout: Duration) -> Ready {
        let deadline = Instant::now() + timeout;
        let mut attentions = 0u32;

        loop {
            match self.test_unit_ready(at) {
                Ok(Ok(())) => return Ready::Yes,
                Ok(Err(sense)) => {
                    if sense.is_unit_attention() {
                        // Cleared by the asking, so the first retries go
                        // straight round. The deadline is checked here too: a
                        // device raising one per command would otherwise pin
                        // the loop, and `xchange-nbd` holds the shared adapter
                        // across this call.
                        if Instant::now() >= deadline {
                            return Ready::TimedOut;
                        }
                        attentions += 1;
                        if attentions > 2 {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        continue;
                    }
                    if sense.is_no_medium() {
                        return Ready::NoMedium;
                    }
                    // 0x04 is "becoming ready", which resolves on its own.
                    if sense.key == 0x02 && sense.asc == 0x04 {
                        if Instant::now() >= deadline {
                            return Ready::TimedOut;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    return Ready::Refused(sense);
                }
                // Usually the device still coming up. Retry while there is
                // time left.
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                Err(_) => return Ready::TimedOut,
            }
        }
    }

    /// How big is the medium currently loaded?
    pub fn read_capacity(&mut self, at: Address) -> Result<Capacity, Error> {
        const LEN: usize = 8;
        let cdb = scsi::cdb10(scsi::op::READ_CAPACITY_10, 0, 0);
        let response = self.command_checked(at, &cdb, Direction::In, LEN, "READ CAPACITY")?;

        Capacity::parse(&response.data).ok_or(Error::ShortResponse {
            at,
            len: response.data.len(),
            what: "READ CAPACITY",
        })
    }

    /// Read blocks from a target.
    pub fn read(
        &mut self,
        at: Address,
        lba: u32,
        blocks: u16,
        block_size: u32,
    ) -> Result<Vec<u8>, Error> {
        let length = blocks as usize * block_size as usize;
        let cdb = scsi::cdb10(scsi::op::READ_10, lba, blocks);
        let response = self.command_checked(at, &cdb, Direction::In, length, "READ(10)")?;
        Ok(response.data)
    }

    /// Write blocks to a target.
    pub fn write(
        &mut self,
        at: Address,
        lba: u32,
        blocks: u16,
        block_size: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        let length = blocks as usize * block_size as usize;
        if data.len() != length {
            return Err(Error::ShortResponse {
                at,
                len: data.len(),
                what: "WRITE(10) payload",
            });
        }

        let cdb = scsi::cdb10(scsi::op::WRITE_10, lba, blocks);

        for attempt in 0..2 {
            let response = self.command_with(at, &cdb, Direction::Out, length, data)?;

            match response.status {
                Status::Good => return Ok(()),
                Status::NeedsSense | Status::Failed => {
                    let sense = self.request_sense(at)?;
                    if sense.is_clear() {
                        return Ok(());
                    }
                    if sense.is_unit_attention() && attempt == 0 {
                        continue;
                    }
                    return Err(Error::CheckCondition { at, sense });
                }
                _ => {
                    return Err(Error::CommandFailed {
                        at,
                        command: "WRITE(10)",
                    })
                }
            }
        }

        Err(Error::CommandFailed {
            at,
            command: "WRITE(10)",
        })
    }

    /// Largest number of blocks that fits inside the ceiling in force. Reading
    /// [`MAX_TRANSFER`] instead meant a caller could be handed a size that
    /// [`Self::command_with`] then refused.
    pub fn max_blocks(&self, block_size: u32) -> u16 {
        (self.transfer_limit / block_size.max(1) as usize).clamp(1, u16::MAX as usize) as u16
    }

    /// Raise this when the medium is suspect: a drive grinding through its own
    /// retries needs to finish, or every bad sector costs a timeout.
    pub fn set_command_timeout(&mut self, timeout: Duration) {
        self.command_timeout = timeout;
    }

    /// The transfer ceiling currently in force.
    pub fn transfer_limit(&self) -> usize {
        self.transfer_limit
    }

    /// Move the transfer ceiling, for finding where the real one is. Raising
    /// it walks towards the firmware's crash point, and overshooting costs a
    /// power cycle. Nothing in normal operation should call this.
    pub fn set_transfer_limit(&mut self, bytes: usize) {
        self.transfer_limit = bytes;
    }

    /// Reset the adapter over USB and let go of it. Clears a hang from an
    /// out-of-range address without a power cycle; the firmware keeps running,
    /// so it returns on the same product ID and the caller should reopen.
    pub fn reset(self) -> Result<(), Error> {
        self._device.reset().wait().map_err(Error::Usb)
    }

    /// Walk the bus at logical unit 0, which is where single-unit devices live.
    pub fn scan(&mut self) -> Vec<(Address, Result<Option<Inquiry>, Error>)> {
        (0..MAX_TARGET)
            .map(|target| {
                let at = Address::target(target);
                (at, self.inquiry(at))
            })
            .collect()
    }

    /// Walk every target and every logical unit.
    ///
    /// **Experimental and untested against real hardware.** The addressing is
    /// confirmed: a drive probed at LUNs 1-7 answers "no such logical unit",
    /// which needs the field decoded. What is unverified is a device returning
    /// different units.
    pub fn scan_all(&mut self) -> Vec<(Address, Result<Option<Inquiry>, Error>)> {
        let mut found = Vec::new();

        for target in 0..MAX_TARGET {
            for lun in 0..=MAX_ADDRESS {
                let at = Address::new(target, lun);
                let result = self.inquiry(at);

                // Nothing at unit 0 means nothing on this target.
                let empty = matches!(result, Ok(None));
                found.push((at, result));
                if lun == 0 && empty {
                    break;
                }
            }
        }

        found
    }
}
