//! Work out what an Adaptec USB2Xchange adapter passes through.
//!
//! This tests the adapter, not the drive, by separating two things that look
//! alike from outside: the drive refusing a command, which means the adapter
//! carried it both ways as it should, and the adapter failing to complete the
//! exchange at all. The second is the interesting one, since this firmware
//! only ever had to satisfy Adaptec's own driver, and one address already
//! stops it answering USB until power cycled.
//!
//! Safe commands only unless asked otherwise. Progress is written as it goes,
//! so a run ending in a power cycle resumes rather than restarts.

mod probe;

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use probe::{Flow, Origin, Outcome, Risk};
use xchange_scsi::bot::{Adapter, Direction, Ready, Status, MAX_ADDRESS};
use xchange_scsi::scsi::{Address, Sense};
use xchange_scsi::Error;

type Fallible = Result<(), Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    name = "xchange-conform",
    about = "Characterise what a USB2Xchange adapter passes through"
)]
struct Cli {
    /// SCSI target ID.
    #[arg(short, long, default_value_t = 1)]
    target: u8,

    /// Logical unit on that target.
    #[arg(short, long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
    lun: u8,

    /// Where to record results. Re-running with the same path resumes.
    #[arg(short, long, default_value = "conformance.log")]
    out: PathBuf,

    /// Start again rather than resuming from an existing log.
    #[arg(long)]
    restart: bool,

    /// Also send commands that change device state. Recoverable, not read-only.
    #[arg(long)]
    stateful: bool,

    /// Also send commands that can destroy data.
    #[arg(long)]
    destructive: bool,

    /// Search for the largest transfer the adapter will carry. Reads only, but
    /// large ones, with the driver's ceiling lifted: this walks into territory
    /// the firmware crashes in. Expect to power cycle.
    #[arg(long)]
    transfer_limit: bool,

    /// Also write the verdict as markdown, ready to paste into documentation.
    #[arg(long)]
    markdown: Option<PathBuf>,

    /// Verify the write path near the end of the medium. Saves the original
    /// blocks, writes a pattern, reads it back, restores and checks the
    /// restore. Never touches block 0. Only run it on a disc you can afford to
    /// lose: a power cut between write and restore leaves the pattern.
    #[arg(long)]
    write_test: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Records what has already been tried, so a power cycle is not a lost run.
struct Journal {
    file: std::fs::File,
    done: HashSet<String>,
}

impl Journal {
    fn open(path: &PathBuf, restart: bool) -> std::io::Result<Self> {
        let done = if restart {
            HashSet::new()
        } else {
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.split('\t').next().map(str::to_string))
                .collect()
        };

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(!restart)
            .write(true)
            .truncate(restart)
            .open(path)?;

        Ok(Self { file, done })
    }

    fn already_done(&self, name: &str) -> bool {
        self.done.contains(name)
    }

    fn record(&mut self, name: &str, outcome: &Outcome) {
        let _ = writeln!(self.file, "{name}\t{}", outcome.summary());
        let _ = self.file.flush();
        self.done.insert(name.to_string());
    }
}

/// Wait for the device to become ready. Probe an optical drive while it spins
/// up and almost everything comes back refused for reasons that say nothing
/// about support. The waiting lives in the transport, so this and
/// `xchange-nbd` cannot drift apart.
fn settle(adapter: &mut Adapter, at: Address) {
    match adapter.wait_until_ready(at, Duration::from_secs(45)) {
        Ready::Yes => {}
        other => println!("       device: {}\n", other.describe()),
    }
}

/// Is the adapter still answering at all?
fn healthy(adapter: &mut Adapter, at: Address) -> bool {
    matches!(adapter.inquiry(at), Ok(Some(_)) | Ok(None))
}

/// Bring a hung adapter back, or explain that it needs a power cycle.
fn revive() -> Result<Adapter, Error> {
    xchange_scsi::bot::reset_by_usb()?;

    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(adapter) = Adapter::open() {
            return Ok(adapter);
        }
    }

    Adapter::open()
}

/// Send one command and work out what the answer says about the adapter.
fn attempt(adapter: &mut Adapter, at: Address, cdb: &[u8], flow: Flow) -> Outcome {
    let (direction, length) = match flow {
        Flow::None => (Direction::None, 0),
        Flow::In(len) => (Direction::In, len),
    };

    // A unit attention is a one-shot notice, not a refusal. Without the retry
    // a probe reports "refused" for a command the device would have run.
    for attempt in 0..2 {
    match adapter.command(at, cdb, direction, length) {
        Ok(response) => match response.status {
            Status::Good => return Outcome::Executed,
            Status::Failed | Status::NeedsSense => match adapter.request_sense(at) {
                Ok(sense) if sense.is_clear() => return Outcome::Executed,
                Ok(sense) if sense.is_unit_attention() && attempt == 0 => continue,
                Ok(sense) => return Outcome::Refused(sense),
                Err(error) => {
                    return Outcome::AdapterFault(format!("sense unavailable: {error}"))
                }
            },
            Status::NoDevice => {
                return Outcome::Refused(Sense::new(0x05, 0x25, 0x00))
            }
            Status::Other(byte) => return Outcome::OddStatus(format!("status 0x{byte:02x}")),
        },
        Err(error) => return Outcome::AdapterFault(error.to_string()),
    }
    }

    Outcome::AdapterFault("unit attention did not clear".to_string())
}

fn run() -> Fallible {
    let cli = Cli::parse();
    let at = Address::new(cli.target, cli.lun);

    let mut adapter = Adapter::open()?;
    let inquiry = adapter
        .inquiry(at)?
        .ok_or(Error::CommandFailed { at, command: "INQUIRY" })?;
    // The first READ CAPACITY after opening often lands on a unit attention.
    let capacity = match adapter.read_capacity(at) {
        Ok(capacity) => Some(capacity),
        Err(_) => adapter.read_capacity(at).ok(),
    };

    println!("{at}: {inquiry}");
    println!("  device type: {}", inquiry.peripheral_type.name());
    match capacity {
        Some(capacity) => println!("  {capacity}"),
        None => println!(
            "  no capacity reported: normal for a device that is not block\n\
             \x20 addressed. Probes needing a medium size are skipped rather\n\
             \x20 than run against a guessed one"
        ),
    }
    println!();

    settle(&mut adapter, at);

    let mut journal = Journal::open(&cli.out, cli.restart)?;

    let last_lba = capacity.map(|c| c.last_lba);
    let block_size = capacity.map(|c| c.block_size).unwrap_or(512);
    let mut results: Vec<Finding> = Vec::new();
    let mut skipped = 0;

    println!("== command pass-through ==");
    println!("{:<38} {:<18} outcome", "command", "origin");

    for item in probe::probes(last_lba, block_size, inquiry.peripheral_type) {
        let allowed = match item.risk {
            Risk::Safe => true,
            Risk::Stateful => cli.stateful,
            Risk::Destructive => cli.destructive,
        };

        if !allowed {
            skipped += 1;
            continue;
        }
        if journal.already_done(item.name) {
            continue;
        }

        let mut outcome = attempt(&mut adapter, at, &item.cdb, item.flow);

        // So the command that hung it is recorded as the one that did,
        // rather than as a fault on whatever we try next. Only after something
        // went wrong, since the check costs a command.
        let mut hung = false;
        if matches!(outcome, Outcome::AdapterFault(_)) && !healthy(&mut adapter, at) {
            outcome = Outcome::Hung;
            hung = true;
        }

        println!(
            "  0x{:02x} {:<32} {:<18} {}",
            item.opcode,
            item.name,
            item.origin.label(),
            outcome.summary()
        );

        if hung {
            println!("       resetting the adapter");
            adapter = match revive() {
                Ok(adapter) => adapter,
                Err(_) => {
                    journal.record(item.name, &outcome);
                    println!(
                        "\n  The adapter is not coming back. Power cycle it and re-run;\n  \
                         progress is in {} and picks up where it stopped.",
                        cli.out.display()
                    );
                    return Ok(());
                }
            };
            // The reset disturbs the drive as well as the adapter.
            settle(&mut adapter, at);
        }

        // Some commands upset the device rather than merely being refused: a
        // CD-ROM sent a READ(6) it lacks resets itself and reports "becoming
        // ready" to everything after. One bad probe would invalidate the rest.
        let disturbed = match &outcome {
            Outcome::OddStatus(_) => true,
            Outcome::Refused(sense) => sense.is_unit_attention(),
            _ => false,
        };
        if disturbed {
            settle(&mut adapter, at);
        }

        results.push(Finding {
            name: Cow::Borrowed(item.name),
            opcode: Some(item.opcode),
            origin: item.origin,
            outcome: outcome.clone(),
        });
        journal.record(item.name, &outcome);
    }

    allocation_lengths(&mut adapter, at, &mut journal, &mut results);

    if cli.transfer_limit {
        transfer_limit(&mut adapter, at, block_size);
    }

    if cli.write_test {
        match last_lba {
            Some(last_lba) => write_test(&mut adapter, at, block_size, last_lba),
            None => println!("\n== write path ==\n  no capacity reported, so there is nowhere safe to write"),
        }
    }

    report(&results, skipped, &cli);

    if let Some(path) = &cli.markdown {
        let text = markdown(&results, &inquiry.to_string());
        std::fs::write(path, text)?;
        println!("  markdown written to {}", path.display());
    }

    Ok(())
}

/// One command's result, kept so the verdict can be sorted into buckets.
struct Finding {
    /// Borrowed for the static probe table, owned for the allocation sweep,
    /// which builds a name per size rather than leaking one.
    name: Cow<'static, str>,
    opcode: Option<u8>,
    origin: Origin,
    outcome: Outcome,
}

impl Finding {
    fn label(&self) -> String {
        match self.opcode {
            Some(opcode) => format!("0x{opcode:02x} {}", self.name),
            None => self.name.to_string(),
        }
    }
}

/// Does the adapter honour the allocation length? INQUIRY is the safest ask.
fn allocation_lengths(
    adapter: &mut Adapter,
    at: Address,
    journal: &mut Journal,
    results: &mut Vec<Finding>,
) {
    println!("\n== data-in allocation lengths (INQUIRY) ==");

    // 0 means "send nothing", 4 is shorter than the header, 36 is the standard
    // reply, 255 is more than the drive has.
    for allocation in [0u8, 1, 4, 5, 36, 96, 255] {
        let name = format!("INQUIRY alloc {allocation}");
        if journal.already_done(&name) {
            continue;
        }

        let cdb = probe::inquiry_for(allocation);
        // Asking for nothing has no data phase at all. The wrapper has to say
        // so, or the adapter waits for bytes neither side will send.
        let direction = if allocation == 0 {
            Direction::None
        } else {
            Direction::In
        };

        let outcome = match adapter.command(at, &cdb, direction, allocation as usize) {
            Ok(response) => {
                let got = response.data.len();
                if response.status == Status::Good && got <= allocation as usize {
                    println!("  asked {allocation:>3}: got {got:>3}, residue {}", response.residue);
                    Outcome::Executed
                } else if response.status == Status::Good {
                    let what = format!("asked {allocation}, got {got} — overrun");
                    println!("  asked {allocation:>3}: {what}");
                    Outcome::AdapterFault(what)
                } else {
                    println!("  asked {allocation:>3}: status {:?}", response.status);
                    Outcome::OddStatus(format!("{:?}", response.status))
                }
            }
            Err(error) => {
                println!("  asked {allocation:>3}: ADAPTER FAULT: {error}");
                Outcome::AdapterFault(error.to_string())
            }
        };

        journal.record(&name, &outcome);
        results.push(Finding {
            name: Cow::Owned(name),
            opcode: None,
            origin: Origin::Scsi2Mandatory,
            outcome,
        });
    }
}

/// Find the largest read the adapter will carry. Walks upward rather than
/// bisecting, because a crash ends the search and the last success is then the
/// answer. The driver's ceiling is lifted first, or this measures
/// `MAX_TRANSFER` rather than the hardware.
fn transfer_limit(adapter: &mut Adapter, at: Address, block_size: u32) {
    println!("\n== transfer size limit ==");
    println!("  driver ceiling is {} bytes; lifting it for the search", adapter.transfer_limit());
    adapter.set_transfer_limit(64 * 1024);

    // A read that succeeds and returns the wrong bytes is worse than one that
    // fails, and a nearly-full firmware buffer is where that happens. Baseline
    // built a block at a time, sized to the largest transfer we will try and
    // no further: at 2048 bytes, 127 blocks would read 260 KB singly.
    let sample = ((64 * 1024 - 1) / block_size.max(1) as usize).clamp(1, 127);
    let mut baseline = Vec::with_capacity(sample * block_size as usize);
    for lba in 0..sample as u32 {
        match adapter.read(at, lba, 1, block_size) {
            Ok(block) => baseline.extend_from_slice(&block),
            Err(error) => {
                println!("  cannot build a baseline: {error}");
                return;
            }
        }
    }
    println!("  baseline built from {sample} single-block reads");

    let mut largest = 0usize;
    let mut crashed = false;

    for blocks in [1u16, 8, 16, 32, 64, 80, 96, 112, 120, 124, 126, 127] {
        let bytes = blocks as usize * block_size as usize;
        // Below 64 KB, not at it: 65536 is the crash point, and 32 blocks of
        // 2048 lands exactly there, where a `>` test would have walked in.
        if bytes >= 64 * 1024 {
            break;
        }

        match adapter.read(at, 0, blocks, block_size) {
            Ok(data) if data.len() == bytes && data[..] == baseline[..bytes] => {
                largest = bytes;
                println!("  {blocks:>3} blocks ({bytes:>6} bytes): ok, contents match");
            }
            Ok(data) if data.len() == bytes => {
                let differs = data
                    .iter()
                    .zip(&baseline[..bytes])
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                println!(
                    "  {blocks:>3} blocks ({bytes:>6} bytes): CORRUPT, first bad byte at {differs}"
                );
                break;
            }
            Ok(data) => {
                println!("  {blocks:>3} blocks ({bytes:>6} bytes): short, got {}", data.len());
                break;
            }
            Err(error) => {
                println!("  {blocks:>3} blocks ({bytes:>6} bytes): failed: {error}");
                crashed = !healthy(adapter, at);
                break;
            }
        }
    }

    adapter.set_transfer_limit(xchange_scsi::bot::MAX_TRANSFER);

    println!("\n  largest transfer carried: {largest} bytes");
    if crashed {
        println!("  the adapter stopped answering at the next size up, so that is the wall");
    }
    match largest.cmp(&xchange_scsi::bot::MAX_TRANSFER) {
        std::cmp::Ordering::Greater => println!(
            "  MAX_TRANSFER is {} and could be raised to {largest}",
            xchange_scsi::bot::MAX_TRANSFER
        ),
        std::cmp::Ordering::Equal => println!("  MAX_TRANSFER is set correctly"),
        std::cmp::Ordering::Less => println!(
            "  MAX_TRANSFER is {} and is too high; bring it down to {largest}",
            xchange_scsi::bot::MAX_TRANSFER
        ),
    }
}

/// Prove the write path without leaving anything behind. The pattern carries
/// each block's address, so a write landing elsewhere shows as a mismatch
/// rather than a pass. The restore is checked too, being otherwise a claim.
fn write_test(adapter: &mut Adapter, at: Address, block_size: u32, last_lba: u32) {
    const BLOCKS: u16 = 4;

    println!("\n== write path ==");

    let span = BLOCKS as usize * block_size as usize;
    // Clear of block 0 and of the last, so an off-by-one stays on the medium.
    let Some(lba) = last_lba.checked_sub(BLOCKS as u32 + 8) else {
        println!("  medium too small to test safely");
        return;
    };

    println!("  target: {BLOCKS} blocks at LBA {lba} of {last_lba}");

    let original = match adapter.read(at, lba, BLOCKS, block_size) {
        Ok(data) if data.len() == span => data,
        Ok(data) => {
            println!("  cannot read the original: got {} of {span} bytes", data.len());
            return;
        }
        Err(error) => {
            println!("  cannot read the original, so not writing: {error}");
            return;
        }
    };
    println!("  original saved ({span} bytes)");

    let mut pattern = Vec::with_capacity(span);
    for block in 0..BLOCKS as u32 {
        let mut buf = vec![0u8; block_size as usize];
        let marker = b"xchange-conform ";
        buf[..marker.len()].copy_from_slice(marker);
        buf[16..20].copy_from_slice(&(lba + block).to_be_bytes());
        for (index, byte) in buf.iter_mut().enumerate().skip(20) {
            *byte = (index as u8).wrapping_mul(31).wrapping_add(block as u8);
        }
        pattern.extend_from_slice(&buf);
    }

    if let Err(error) = adapter.write(at, lba, BLOCKS, block_size, &pattern) {
        println!("  WRITE(10) failed: {error}");
        println!("  nothing was changed");
        return;
    }
    println!("  pattern written");

    let readback = match adapter.read(at, lba, BLOCKS, block_size) {
        Ok(data) => data,
        Err(error) => {
            println!("  cannot read back: {error}");
            restore(adapter, at, lba, BLOCKS, block_size, &original);
            return;
        }
    };

    if readback == pattern {
        println!("  read back and matches, byte for byte");
    } else {
        let at_byte = readback
            .iter()
            .zip(&pattern)
            .position(|(a, b)| a != b)
            .unwrap_or(readback.len().min(pattern.len()));
        println!("  MISMATCH: first difference at byte {at_byte} of {span}");
    }

    restore(adapter, at, lba, BLOCKS, block_size, &original);
}

/// Put the original contents back, and check that they went back.
fn restore(
    adapter: &mut Adapter,
    at: Address,
    lba: u32,
    blocks: u16,
    block_size: u32,
    original: &[u8],
) {
    match adapter.write(at, lba, blocks, block_size, original) {
        Ok(()) => match adapter.read(at, lba, blocks, block_size) {
            Ok(check) if check == original => println!("  original restored and verified"),
            Ok(_) => println!("  RESTORE DID NOT VERIFY: the pattern may still be on the disc"),
            Err(error) => println!("  restore written but could not be checked: {error}"),
        },
        Err(error) => println!("  RESTORE FAILED: {error}\n  the pattern is still at LBA {lba}"),
    }
}

/// Sort findings into what documentation needs to say, which is not pass and
/// fail. What matters is who refused: a drive answering "invalid opcode" means
/// the adapter worked and another device might accept it, while an adapter
/// fault means no device ever will.
struct Buckets<'a> {
    supported: Vec<&'a Finding>,
    impossible: Vec<&'a Finding>,
    device_dependent: Vec<&'a Finding>,
    /// The drive objected to how we built the CDB, which measures the probe.
    probe_at_fault: Vec<&'a Finding>,
}

/// ASC 0x20, INVALID COMMAND OPERATION CODE: the device does not implement it.
const ASC_INVALID_OPCODE: u8 = 0x20;
/// ASC 0x24, INVALID FIELD IN CDB: it does implement it, and disliked our CDB.
const ASC_INVALID_FIELD: u8 = 0x24;

fn buckets(results: &[Finding]) -> Buckets<'_> {
    let mut b = Buckets {
        supported: Vec::new(),
        impossible: Vec::new(),
        device_dependent: Vec::new(),
        probe_at_fault: Vec::new(),
    };

    for finding in results {
        match &finding.outcome {
            Outcome::Executed => b.supported.push(finding),
            Outcome::Refused(sense) if sense.asc == ASC_INVALID_FIELD => {
                b.probe_at_fault.push(finding)
            }
            Outcome::Refused(sense) if sense.asc == ASC_INVALID_OPCODE => {
                b.device_dependent.push(finding)
            }
            // The drive had some other reason; the adapter still carried it.
            Outcome::Refused(_) => b.device_dependent.push(finding),
            Outcome::OddStatus(_) | Outcome::AdapterFault(_) | Outcome::Hung => {
                b.impossible.push(finding)
            }
        }
    }

    b
}

/// Limits that hold whatever is plugged in, whatever the probes return.
const ARCHITECTURAL_LIMITS: &[&str] = &[
    "targets above 7: the bus is narrow, and nothing in the protocol carries a high byte",
    "devices behind a SCSI expander: they need a target ID of their own, and there are only eight",
    "transfers of 64 KB or more: the firmware's internal buffer is smaller than that",
];

fn report(results: &[Finding], skipped: usize, cli: &Cli) {
    let b = buckets(results);
    let (supported, impossible, device_dependent) = (&b.supported, &b.impossible, &b.device_dependent);

    println!("\n== what this adapter supports ==");
    for finding in supported {
        println!("  {}", finding.label());
    }

    println!("\n== what it cannot support, whatever is attached ==");
    if impossible.is_empty() {
        println!("  Nothing found: the adapter carried every command it was given.");
    } else {
        for finding in impossible {
            println!("  {}: {}", finding.label(), finding.outcome.summary());
        }
    }
    for limit in ARCHITECTURAL_LIMITS {
        println!("  {limit}");
    }

    println!("\n== what might work with different hardware ==");
    if device_dependent.is_empty() {
        println!("  Nothing: this drive accepted everything the adapter carried.");
    } else {
        println!("  The adapter carried these faithfully; this drive declined them.");
        for finding in device_dependent {
            println!(
                "  {} [{}]: {}",
                finding.label(),
                finding.origin.label(),
                finding.outcome.summary()
            );
        }
    }

    if !b.probe_at_fault.is_empty() {
        println!("\n== probes that need fixing ==");
        println!("  The drive recognised these and objected to how we built the CDB,");
        println!("  so they measure this tool rather than the hardware.");
        for finding in &b.probe_at_fault {
            println!("  {}: {}", finding.label(), finding.outcome.summary());
        }
    }

    if skipped > 0 {
        println!(
            "\n  {skipped} command(s) skipped as too risky. Add {}{}to include them.",
            if !cli.stateful { "--stateful " } else { "" },
            if !cli.destructive { "--destructive " } else { "" }
        );
    }
    println!("\n  results recorded in {}", cli.out.display());
}

/// The same verdict as markdown, for pasting into documentation.
fn markdown(results: &[Finding], device: &str) -> String {
    let b = buckets(results);
    let (supported, impossible, device_dependent) = (&b.supported, &b.impossible, &b.device_dependent);
    let mut out = String::new();

    out.push_str("## Adapter conformance\n\n");
    out.push_str(&format!("Measured against `{device}`.\n\n"));
    out.push_str(
        "A refusal from the drive is not an adapter limitation: it means the command was \n\
         carried there and the answer carried back. Only the middle table lists things no \n\
         attached device can change.\n\n",
    );

    out.push_str("### Supported\n\n| command | |\n|---|---|\n");
    for finding in supported {
        out.push_str(&format!("| `{}` | executed |\n", finding.label()));
    }

    out.push_str("\n### Not supported, and cannot be\n\n| limit | why |\n|---|---|\n");
    for finding in impossible {
        out.push_str(&format!(
            "| `{}` | {} |\n",
            finding.label(),
            finding.outcome.summary()
        ));
    }
    for limit in ARCHITECTURAL_LIMITS {
        let (what, why) = limit.split_once(": ").unwrap_or((limit, ""));
        out.push_str(&format!("| {what} | {why} |\n"));
    }

    out.push_str("\n### May work with other devices\n\n| command | origin | this drive |\n|---|---|---|\n");
    for finding in device_dependent {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            finding.label(),
            finding.origin.label(),
            finding.outcome.summary()
        ));
    }

    out
}
