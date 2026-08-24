//! The commands we send, and what we make of the answers.

use xchange_scsi::scsi::{self, PeripheralType, Sense};

/// Whether a command can change anything on the medium or the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads or reports only.
    Safe,
    /// Settings, reservations, media position. Recoverable, but not nothing.
    Stateful,
    /// Can destroy data.
    Destructive,
}

/// Which way data moves. No outbound variant, because every probe here reads
/// or reports; a data-out probe would need [`Probe`] to carry a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    None,
    In(usize),
}

/// Which device types a probe makes sense for. Opcodes are reused with
/// different meanings: 0x01 is REZERO UNIT on a disk and REWIND on a tape.
/// Sending one to the wrong type does not merely fail, it can move media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Meaningful on any device, whatever it is.
    Any,
    /// Devices addressed by logical block: disks, optical, WORM, CD.
    BlockLike,
    /// Sequential access: tape.
    Sequential,
    /// CD and DVD, for the MMC command set.
    Optical,
    /// Medium changers.
    Changer,
}

impl Scope {
    fn covers(self, kind: PeripheralType) -> bool {
        match self {
            Self::Any => true,
            Self::BlockLike => matches!(
                kind,
                PeripheralType::DirectAccess
                    | PeripheralType::WriteOnce
                    | PeripheralType::CdRom
                    | PeripheralType::OpticalMemory
            ),
            Self::Sequential => kind == PeripheralType::SequentialAccess,
            Self::Optical => kind == PeripheralType::CdRom,
            Self::Changer => kind == PeripheralType::MediumChanger,
        }
    }
}

/// One command to try, and what we expect to learn from it.
pub struct Probe {
    pub opcode: u8,
    pub name: &'static str,
    /// The standard that first defined it for direct-access devices.
    pub origin: Origin,
    pub risk: Risk,
    pub scope: Scope,
    pub cdb: Vec<u8>,
    pub flow: Flow,
}

/// Where a command comes from: SCSI-2 failing is a different story to SCSI-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// SCSI-2, mandatory for direct-access devices.
    Scsi2Mandatory,
    /// SCSI-2, optional for direct-access devices.
    Scsi2Optional,
    /// SCSI-2 for other device types. A disk rejecting these is correct.
    Scsi2OtherType,
    /// SCSI-3 and later, to see whether longer command blocks pass at all.
    Scsi3,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scsi2Mandatory => "SCSI-2 mandatory",
            Self::Scsi2Optional => "SCSI-2 optional",
            Self::Scsi2OtherType => "SCSI-2 other type",
            Self::Scsi3 => "SCSI-3",
        }
    }
}

/// What happened, classified by what it says about the adapter, not the drive.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The drive carried it out.
    Executed,
    /// Sense data came back, so the adapter carried both ways as it should.
    Refused(Sense),
    /// The drive reported a status the adapter's own protocol does not define.
    OddStatus(String),
    /// The exchange did not complete. The adapter's fault, not the drive's.
    AdapterFault(String),
    /// The adapter stopped answering entirely and had to be reset.
    Hung,
}

impl Outcome {
    pub fn summary(&self) -> String {
        match self {
            Self::Executed => "executed".to_string(),
            // The sense says what went wrong, so only who refused is left.
            Self::Refused(sense) => format!("refused: {sense}"),
            Self::OddStatus(what) => format!("odd status: {what}"),
            Self::AdapterFault(what) => format!("ADAPTER FAULT: {what}"),
            Self::Hung => "ADAPTER HUNG".to_string(),
        }
    }
}

fn cdb(bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec()
}

/// The command set we try, in opcode order.
///
/// Excluded even under `--destructive`: WRITE BUFFER, which writes firmware on
/// some devices, and MODE SELECT, whose changes outlive the run.
///
/// `last_lba` is optional because reading past the end of a medium whose size
/// we do not know is just reading, so that probe is omitted rather than
/// guessed. `block_size` has to match the device's, or the wrapper declares a
/// length the drive disagrees with and the resulting status reads as an
/// adapter fault when it is not one.
pub fn probes(last_lba: Option<u32>, block_size: u32, kind: PeripheralType) -> Vec<Probe> {
    let block = block_size.max(1) as usize;
    let mut probes = vec![
        Probe {
            opcode: 0x00,
            name: "TEST UNIT READY",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x00, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x01,
            name: "REZERO UNIT",
            origin: Origin::Scsi2Optional,
            risk: Risk::Stateful,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x01, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x03,
            name: "REQUEST SENSE",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x03, 0, 0, 0, 18, 0]),
            flow: Flow::In(18),
        },
        Probe {
            opcode: 0x04,
            name: "FORMAT UNIT",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Destructive,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x04, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x08,
            name: "READ(6)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x08, 0, 0, 0, 1, 0]),
            flow: Flow::In(block),
        },
        Probe {
            opcode: 0x0b,
            name: "SEEK(6)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Stateful,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x0b, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x12,
            name: "INQUIRY",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x12, 0, 0, 0, 36, 0]),
            flow: Flow::In(36),
        },
        Probe {
            opcode: 0x16,
            name: "RESERVE(6)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Stateful,
            scope: Scope::Any,
            cdb: cdb(&[0x16, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x17,
            name: "RELEASE(6)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Stateful,
            scope: Scope::Any,
            cdb: cdb(&[0x17, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x1a,
            name: "MODE SENSE(6)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Any,
            // Every page the device has.
            cdb: cdb(&[0x1a, 0, 0x3f, 0, 192, 0]),
            flow: Flow::In(192),
        },
        Probe {
            opcode: 0x1b,
            name: "START STOP UNIT (start)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Stateful,
            scope: Scope::Any,
            // Start bit set, so this spins up rather than ejecting.
            cdb: cdb(&[0x1b, 0, 0, 0, 0x01, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x1c,
            name: "RECEIVE DIAGNOSTIC RESULTS",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x1c, 0, 0, 0, 32, 0]),
            flow: Flow::In(32),
        },
        Probe {
            opcode: 0x1d,
            name: "SEND DIAGNOSTIC",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Any,
            // No self-test bit, no parameter list: a no-op.
            cdb: cdb(&[0x1d, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x1e,
            name: "PREVENT ALLOW MEDIUM REMOVAL (allow)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Stateful,
            scope: Scope::Any,
            // Prevent bit clear, so this leaves the media releasable.
            cdb: cdb(&[0x1e, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x25,
            name: "READ CAPACITY(10)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            flow: Flow::In(8),
        },
        Probe {
            opcode: 0x28,
            name: "READ(10)",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0]),
            flow: Flow::In(block),
        },
        Probe {
            opcode: 0x2b,
            name: "SEEK(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Stateful,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x2b, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x2f,
            name: "VERIFY(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            // BytChk clear: verify the medium without comparing sent data.
            cdb: cdb(&[0x2f, 0, 0, 0, 0, 0, 0, 0, 1, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x34,
            name: "PRE-FETCH(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x34, 0, 0, 0, 0, 0, 0, 0, 1, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x35,
            name: "SYNCHRONIZE CACHE(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        Probe {
            opcode: 0x37,
            name: "READ DEFECT DATA(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x37, 0, 0, 0, 0, 0, 0, 0, 4, 0]),
            flow: Flow::In(4),
        },
        Probe {
            opcode: 0x3c,
            name: "READ BUFFER",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x3c, 0x02, 0, 0, 0, 0, 0, 0, 32, 0]),
            flow: Flow::In(32),
        },
        Probe {
            opcode: 0x4d,
            name: "LOG SENSE",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x4d, 0, 0x3f, 0, 0, 0, 0, 0, 32, 0]),
            flow: Flow::In(32),
        },
        Probe {
            opcode: 0x5a,
            name: "MODE SENSE(10)",
            origin: Origin::Scsi2Optional,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0x5a, 0, 0x3f, 0, 0, 0, 0, 0, 192, 0]),
            flow: Flow::In(192),
        },
        Probe {
            opcode: 0xa8,
            name: "READ(12)",
            origin: Origin::Scsi2OtherType,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0xa8, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]),
            flow: Flow::In(block),
        },
        Probe {
            opcode: 0x88,
            name: "READ(16)",
            origin: Origin::Scsi3,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]),
            flow: Flow::In(block),
        },
        Probe {
            opcode: 0x9e,
            name: "READ CAPACITY(16)",
            origin: Origin::Scsi3,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: cdb(&[0x9e, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0]),
            flow: Flow::In(32),
        },
        Probe {
            opcode: 0xa0,
            name: "REPORT LUNS",
            origin: Origin::Scsi3,
            risk: Risk::Safe,
            scope: Scope::Any,
            cdb: cdb(&[0xa0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0]),
            flow: Flow::In(16),
        },
        // --- sequential access (tape) ---
        Probe {
            opcode: 0x05,
            name: "READ BLOCK LIMITS",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::Sequential,
            cdb: cdb(&[0x05, 0, 0, 0, 0, 0]),
            flow: Flow::In(6),
        },
        Probe {
            opcode: 0x01,
            name: "REWIND",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Stateful,
            scope: Scope::Sequential,
            cdb: cdb(&[0x01, 0, 0, 0, 0, 0]),
            flow: Flow::None,
        },
        // --- optical (CD/DVD, MMC) ---
        Probe {
            opcode: 0x43,
            name: "READ TOC",
            origin: Origin::Scsi2OtherType,
            risk: Risk::Safe,
            scope: Scope::Optical,
            cdb: cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 12, 0]),
            flow: Flow::In(12),
        },
        Probe {
            opcode: 0x42,
            name: "READ SUB-CHANNEL",
            origin: Origin::Scsi2OtherType,
            risk: Risk::Safe,
            scope: Scope::Optical,
            cdb: cdb(&[0x42, 0x02, 0x40, 0x01, 0, 0, 0, 0, 16, 0]),
            flow: Flow::In(16),
        },
        // --- medium changer ---
        Probe {
            opcode: 0xb8,
            name: "READ ELEMENT STATUS",
            origin: Origin::Scsi2OtherType,
            risk: Risk::Safe,
            scope: Scope::Changer,
            cdb: cdb(&[0xb8, 0, 0, 0, 0, 1, 0, 0, 0, 32, 0, 0]),
            flow: Flow::In(32),
        },
    ];

    // Out of range, to see whether the error is sense or an adapter fault.
    if let Some(last_lba) = last_lba {
        let beyond = last_lba.saturating_add(1_000);
        probes.push(Probe {
            opcode: 0x28,
            name: "READ(10) past end of medium",
            origin: Origin::Scsi2Mandatory,
            risk: Risk::Safe,
            scope: Scope::BlockLike,
            cdb: {
                let mut c = vec![0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0];
                c[2..6].copy_from_slice(&beyond.to_be_bytes());
                c
            },
            flow: Flow::In(block),
        });
    }

    // Last, deliberately. On a Toshiba CD-ROM this stops the adapter answering
    // and costs a USB reset, after which everything reports "becoming ready"
    // and measures the recovery instead of itself. A Jaz and an IBM WDS-L80
    // refuse it cleanly, so the fault is in handling that drive's answer.
    probes.push(Probe {
        opcode: 0xff,
        name: "vendor-reserved opcode 0xFF",
        origin: Origin::Scsi2Optional,
        risk: Risk::Safe,
        scope: Scope::Any,
        cdb: cdb(&[0xff, 0, 0, 0, 0, 0]),
        flow: Flow::None,
    });

    // An opcode means different things to different device types, so a probe
    // written for one must never reach another.
    probes.retain(|probe| probe.scope.covers(kind));
    probes
}

/// Build an INQUIRY asking for a given number of bytes, for allocation-length
/// checks.
pub fn inquiry_for(allocation: u8) -> Vec<u8> {
    scsi::cdb6(scsi::op::INQUIRY, 0, allocation, 0)
}
