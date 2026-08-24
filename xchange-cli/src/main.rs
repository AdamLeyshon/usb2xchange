//! Poking at SCSI devices through an Adaptec USB2Xchange.

use clap::{Parser, Subcommand};
use std::time::Duration;

use xchange_scsi::bot::{
    Adapter, FormatCapability, FormatOptions, FormatState, MAX_ADDRESS, MAX_TARGET,
};
use xchange_scsi::scsi::Address;
use xchange_scsi::Error;

#[derive(Parser)]
#[command(name = "xchange", about = "Talk to SCSI devices behind an Adaptec USB2Xchange")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk the SCSI bus and report what answers.
    Scan {
        /// Also walk units 1-7. Experimental: no multi-unit device tested.
        #[arg(long)]
        luns: bool,
    },

    /// Ask one target what it is.
    Inquiry {
        /// SCSI target ID.
        target: u8,

        /// Logical unit. Non-zero units are experimental and untested.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,
    },

    /// Report the size of the medium in a target.
    Capacity {
        target: u8,

        /// Logical unit. Non-zero units are experimental and untested.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,
    },

    /// Check whether a target is ready.
    Ready {
        target: u8,

        /// Logical unit. Non-zero units are experimental and untested.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,
    },

    /// Work out what the adapter's address byte means.
    ///
    /// Sweeps the wrapper's LUN byte, which carries the target ID here, and
    /// optionally the CDB's legacy LUN field. Sends nothing but INQUIRY.
    Probe {
        /// First target to try.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        from: u8,

        /// Last target to try. A narrow bus has nothing above 7.
        #[arg(long, default_value_t = MAX_ADDRESS, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        to: u8,

        /// Also sweep CDB LUNs 0-7 at each responding address.
        #[arg(long)]
        luns: bool,
    },

    /// Reset the adapter over USB. Use after a probe crashes it.
    Reset,

    /// Read the whole medium and report every block that will not read.
    ///
    /// Read-only. Builds the defect list the drive declines to provide, using
    /// the sense INFORMATION field where given and narrowing block by block
    /// where not.
    Surface {
        /// SCSI target ID.
        target: u8,

        /// Logical unit on that target.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,

        /// Start here rather than at the beginning.
        #[arg(long, default_value_t = 0)]
        from: u32,

        /// Stop after this many blocks.
        #[arg(long)]
        blocks: Option<u32>,

        /// Also report non-zero blocks, for a medium just zeroed.
        #[arg(long)]
        expect_zeros: bool,
    },

    /// Check that a region reads the same however it is asked for.
    ///
    /// Once as one large transfer, once block by block, then compared. A
    /// difference means addresses are not reaching the drive intact.
    Verify {
        /// SCSI target ID.
        target: u8,

        /// First block of the region to check.
        lba: u32,

        /// How many blocks. Kept under the adapter's transfer ceiling.
        #[arg(default_value_t = 64)]
        blocks: u16,

        /// Logical unit on that target.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,
    },

    /// Low-level format a device. **This destroys everything on the medium.**
    ///
    /// FORMAT UNIT with IMMED, so the drive formats in the background while
    /// progress is reported. Without --yes nothing is sent.
    Format {
        /// SCSI target ID.
        target: u8,

        /// Logical unit on that target.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,

        /// Report what a format would do and stop. Sends nothing destructive.
        #[arg(long)]
        dry_run: bool,

        /// Required to actually start a format.
        #[arg(long)]
        yes: bool,

        /// Skip the certification pass by setting DCRT. Finishes a format
        /// that would otherwise crawl, but leaves the drive not knowing which
        /// sectors are bad.
        #[arg(long)]
        no_certify: bool,

        /// Ignore the primary defect list by setting DPRY. Needed when an
        /// unfinished format has destroyed it and the drive refuses without.
        #[arg(long)]
        ignore_primary: bool,

        /// Hand the drive a known bad block. Repeatable.
        #[arg(long = "defect", value_name = "LBA")]
        defects: Vec<u32>,

        /// Send a defect list even though it is known to hang the adapter.
        #[arg(long)]
        force_defect_list: bool,

        /// Hand the drive a run of blocks as defects, as START:COUNT. A defect
        /// large enough to halt certification spans more than one block. The
        /// list rides in parameter data, so the ceiling bounds it.
        #[arg(long, value_name = "START:COUNT")]
        defect_range: Option<String>,
    },

    /// Report the defect lists a device is keeping.
    Defects {
        /// SCSI target ID.
        target: u8,

        /// Logical unit on that target.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,

        /// List every address rather than summarising.
        #[arg(long)]
        list: bool,
    },

    /// Measure sequential read throughput at a range of transfer sizes. One
    /// command travels at a time, so every transfer pays a fixed round trip;
    /// several sizes separate that from the rate data moves at.
    Bench {
        /// SCSI target ID.
        #[arg(short, long, default_value_t = 1)]
        target: u8,

        /// Logical unit on that target.
        #[arg(short, long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,

        /// Seconds to spend on each transfer size.
        #[arg(long, default_value_t = 2.0)]
        seconds: f64,

        /// First block to read from.
        #[arg(long, default_value_t = 0)]
        start_lba: u32,

        /// Raise the ceiling past the driver's own, to measure what the margin
        /// below the firmware's wall costs. See `xchange-conform`.
        #[arg(long)]
        ceiling: Option<usize>,
    },

    /// Read blocks and hexdump the start of them.
    Read {
        target: u8,
        /// First logical block to read.
        lba: u32,
        /// How many blocks.
        #[arg(default_value_t = 1)]
        blocks: u16,

        /// Logical unit. Non-zero units are experimental and untested.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=MAX_ADDRESS as i64))]
        lun: u8,
    },
}

fn hexdump(data: &[u8], limit: usize) {
    for (offset, chunk) in data.iter().take(limit).collect::<Vec<_>>().chunks(16).enumerate() {
        let bytes: Vec<u8> = chunk.iter().map(|b| **b).collect();
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let text: String = bytes
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        println!("{:08x}  {:<47}  |{}|", offset * 16, hex.join(" "), text);
    }
    if data.len() > limit {
        println!("... {} more bytes", data.len() - limit);
    }
}

/// Reset a hung adapter and wait for it. Takes the handle only to drop it:
/// a hung adapter will not answer the requests reopening would send.
fn recover(adapter: Adapter) -> Result<Adapter, Error> {
    drop(adapter);
    xchange_scsi::bot::reset_by_usb()?;

    // It re-enumerates on the same product ID, still running its firmware,
    // but the handle we had is gone.
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(adapter) = Adapter::open() {
            return Ok(adapter);
        }
    }

    Adapter::open()
}

/// Expand a `START:COUNT` defect range. Bounded by the transfer ceiling, since
/// the list rides in parameter data; a truncated one would have the drive
/// reassign the wrong blocks.
fn parse_defect_range(range: &str) -> Result<Vec<u32>, Error> {
    const HEADER: usize = 4;
    let limit = (xchange_scsi::bot::MAX_TRANSFER - HEADER) / 4;

    let bad = || Error::CommandFailed {
        at: Address::target(0),
        command: "--defect-range expects START:COUNT",
    };

    let (start, count) = range.split_once(':').ok_or_else(bad)?;
    let start: u32 = start.trim().parse().map_err(|_| bad())?;
    let count: usize = count.trim().parse().map_err(|_| bad())?;

    if count == 0 || count > limit {
        eprintln!("a defect range must be between 1 and {limit} addresses");
        std::process::exit(1);
    }

    Ok((0..count as u32).filter_map(|n| start.checked_add(n)).collect())
}

/// Low-level format a device, or report what one would do. Formatting a disc
/// in use is impossible for free: only one process can hold the adapter, so
/// nothing here runs while `xchange-nbd` is serving it.
fn format_unit(
    adapter: &mut Adapter,
    at: Address,
    dry_run: bool,
    yes: bool,
    options: FormatOptions,
) -> Result<(), Error> {
    let inquiry = adapter.inquiry(at)?.ok_or(Error::CommandFailed {
        at,
        command: "INQUIRY",
    })?;

    println!("{at}: {inquiry}");
    let capacity = adapter.read_capacity(at).ok();
    if let Some(capacity) = capacity {
        println!("  {capacity}");
    }

    // Whichever mode: knowing the drive expects a format is worth having.
    match adapter.format_capability(at) {
        Ok(FormatCapability::Page { page, write_protected }) => {
            if write_protected {
                println!("  medium is write protected; a format will be refused");
            }
            match page {
                Some(page) => {
                    println!("  format device page:");
                    println!("    sectors per track  : {}", page.sectors_per_track);
                    println!("    bytes per sector   : {}", page.bytes_per_sector);
                    println!("    interleave         : {}", page.interleave);
                    println!("    removable          : {}", page.removable);
                }
                None => println!("  the device returned mode data with no format device page"),
            }
        }
        Ok(FormatCapability::NoSuchPage) => {
            println!("  no format device page: this device does not implement it,");
            println!("  which says nothing either way about FORMAT UNIT support");
        }
        Err(error) => println!("  cannot read mode pages: {error}"),
    }

    if let Ok(defects) = adapter.read_defect_data(at, false, true) {
        println!("  grown defects      : {}", defects.total());
    }

    if dry_run {
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
        let bytes = options.defects.len() * 4;
        println!("\nwould send FORMAT UNIT:");
        println!("  cdb            : 04 10 00 00 00 00   (FmtData set)");
        println!(
            "  defect header  : 00 {flags:02x} {:02x} {:02x}         (IMMED{}, {} defect address(es))",
            bytes >> 8,
            bytes & 0xff,
            match (options.skip_certification, options.ignore_primary) {
                (true, true) => ", FOV, DPRY, DCRT",
                (true, false) => ", FOV, DCRT",
                (false, true) => ", FOV, DPRY",
                (false, false) => "",
            },
            options.defects.len()
        );
        println!("\ndry run: nothing sent. The only certain test of FORMAT UNIT support");
        println!("is to run one, and running one destroys the medium, so the page above");
        println!("is evidence rather than proof.");
        return Ok(());
    }

    if !yes {
        eprintln!("\nThis erases everything on the medium. Re-run with --yes to proceed,");
        eprintln!("or --dry-run to see what would be sent.");
        std::process::exit(1);
    }

    println!("\nstarting format");
    if options.skip_certification {
        println!("certification disabled: the drive will not check what it writes");
    }
    println!("IMMED is set, so the drive formats on its own.");
    println!("Interrupting this command stops the reporting, not the format, and a");
    println!("format that never completes leaves the medium unreadable until one does.");

    adapter.format_unit(at, options)?;

    // Nothing but REQUEST SENSE is answered until it finishes, so the capacity
    // is the one taken beforehand. Kept so a failure can say roughly where it
    // happened when the drive reports no address.
    let mut last_percent: Option<f32> = None;
    let blocks = capacity.map(|c| c.last_lba as u64 + 1).unwrap_or(0);

    loop {
        std::thread::sleep(Duration::from_secs(2));

        match adapter.format_progress(at)? {
            FormatState::Running(Some(percent)) => {
                last_percent = Some(percent);
                print!("\r  {percent:5.1}% complete");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            FormatState::Running(None) => {
                print!("\r  formatting (this drive reports no percentage)");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            FormatState::Finished => {
                println!("\r  done                                        ");
                return Ok(());
            }
            FormatState::Failed(sense) => {
                println!();
                eprintln!("error: {at} reported check condition: {sense}");

                if sense.information.is_none() {
                    if let (Some(percent), true) = (last_percent, blocks > 0) {
                        let estimate = (blocks as f64 * percent as f64 / 100.0) as u64;
                        eprintln!();
                        eprintln!("The drive reported no address. Progress reached {percent:.1}%, so");
                        eprintln!("the failure was near LBA {estimate} of {blocks}, assuming progress");
                        eprintln!("tracks the block address, which is a guess rather than a promise.");
                        eprintln!();
                        eprintln!("To have the drive reassign that region before certification reaches");
                        eprintln!("it, retry with a defect range around that address, for example:");
                        eprintln!(
                            "  xchange format {} --yes --defect-range {}:{}",
                            at.target,
                            estimate.saturating_sub(8_000),
                            16_000
                        );
                    }
                }

                eprintln!();
                eprintln!("The medium is left mid-format and will not read until a format");
                eprintln!("completes. This drive rejects DPRY, so --ignore-primary is not");
                eprintln!("an option, and if the defect is physical the cartridge may be");
                eprintln!("beyond recovery.");
                std::process::exit(1);
            }
        }
    }
}

/// Read every block and report the ones that will not read.
fn surface(
    adapter: &mut Adapter,
    at: Address,
    from: u32,
    limit: Option<u32>,
    expect_zeros: bool,
) -> Result<(), Error> {
    let capacity = adapter.read_capacity(at)?;
    let block_size = capacity.block_size;
    // One more than the final block's address. Saturating, because a drive
    // reporting 0xFFFFFFFF would otherwise wrap to nothing and scan none of
    // the medium.
    let total = capacity.last_lba.saturating_add(1);
    let end = limit.map_or(total, |n| from.saturating_add(n).min(total));
    let step = adapter.max_blocks(block_size);

    // Nothing to do, and `end - 1` below would underflow.
    if end <= from {
        println!("{at}: {total} blocks, so there is nothing to read from LBA {from}");
        return Ok(());
    }

    println!("{at}: reading blocks {from} to {} of {total}", end - 1);
    println!("this is read-only, and slow where the medium is damaged\n");

    let started = std::time::Instant::now();
    let mut bad = Vec::new();
    let mut nonzero = Vec::new();
    let mut lba = from;

    while lba < end {
        let want = step.min((end - lba).min(u16::MAX as u32) as u16);

        match adapter.read(at, lba, want, block_size) {
            Ok(data) => {
                if expect_zeros {
                    for (index, chunk) in data.chunks(block_size as usize).enumerate() {
                        if chunk.iter().any(|byte| *byte != 0) {
                            nonzero.push(lba + index as u32);
                        }
                    }
                }
            }
            Err(error) => {
                // Narrow it to the blocks responsible: the rest of a failed
                // request is usually fine, and not every drive names one.
                println!("\r  {lba}: {error}");
                for block in lba..lba + want as u32 {
                    if adapter.read(at, block, 1, block_size).is_err() {
                        bad.push(block);
                    }
                }
            }
        }

        lba += want as u32;

        let done = lba - from;
        if done.is_multiple_of(step as u32 * 64) {
            let percent = done as f64 * 100.0 / (end - from) as f64;
            print!("\r  {percent:5.1}%, {} bad block(s)", bad.len());
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }

    println!("\r  done in {:.0}s                    ", started.elapsed().as_secs_f64());
    println!();
    println!("unreadable blocks : {}", bad.len());
    if expect_zeros {
        println!("non-zero blocks   : {}", nonzero.len());
    }

    if !bad.is_empty() {
        println!("\nbad:");
        for block in &bad {
            println!("  {block}");
        }
        println!("\nA drive that had reassigned these would have read them back without");
        println!("complaint, so these are defects it has not been able to spare out.");
    }

    if expect_zeros && !nonzero.is_empty() {
        println!("\nread back as something other than zero:");
        for block in nonzero.iter().take(64) {
            println!("  {block}");
        }
        if nonzero.len() > 64 {
            println!("  ... and {} more", nonzero.len() - 64);
        }
        println!("\nThese read without error but do not contain what was written,");
        println!("which is worse than an unreadable block: nothing reports them.");
    }

    Ok(())
}

/// One transfer size, measured.
struct Sample {
    bytes_per_command: usize,
    commands: u64,
    elapsed: f64,
}

impl Sample {
    fn throughput_mb(&self) -> f64 {
        (self.commands as f64 * self.bytes_per_command as f64) / self.elapsed / 1_048_576.0
    }

    fn per_command_ms(&self) -> f64 {
        self.elapsed * 1000.0 / self.commands as f64
    }
}

/// Measure sequential reads across a range of transfer sizes. They march
/// forward and wrap rather than re-reading one spot, so the drive's cache
/// cannot flatter the result.
fn bench(
    adapter: &mut Adapter,
    at: Address,
    seconds: f64,
    start_lba: u32,
) -> Result<(), Error> {
    let inquiry = adapter.inquiry(at)?.ok_or(Error::CommandFailed {
        at,
        command: "INQUIRY",
    })?;
    let capacity = adapter.read_capacity(at)?;
    let block_size = capacity.block_size;
    let blocks_total = capacity.last_lba as u64 + 1;
    // The ceiling in force, so --ceiling is reflected in the sizes tried.
    let ceiling = adapter.max_blocks(block_size);

    println!("{at}: {inquiry}");
    println!("  {capacity}");
    println!("  reading for {seconds}s at each size, from LBA {start_lba}\n");
    println!("  {:>10}  {:>9}  {:>8}  {:>12}  {:>12}", "transfer", "commands", "elapsed", "throughput", "per command");

    let mut sizes: Vec<u16> = [1u16, 2, 4, 8, 16, 32, 64]
        .into_iter()
        .filter(|blocks| *blocks <= ceiling)
        .collect();
    if !sizes.contains(&ceiling) {
        sizes.push(ceiling);
    }

    let mut samples = Vec::new();

    for blocks in sizes {
        let budget = std::time::Duration::from_secs_f64(seconds);
        let started = std::time::Instant::now();
        let mut lba = start_lba;
        let mut commands = 0u64;

        while started.elapsed() < budget {
            adapter.read(at, lba, blocks, block_size)?;
            commands += 1;

            lba = lba.wrapping_add(blocks as u32);
            if lba as u64 + blocks as u64 >= blocks_total {
                lba = start_lba;
            }
        }

        let sample = Sample {
            bytes_per_command: blocks as usize * block_size as usize,
            commands,
            elapsed: started.elapsed().as_secs_f64(),
        };

        println!(
            "  {:>10}  {:>9}  {:>7.2}s  {:>9.2} MB/s  {:>9.2} ms",
            format_size(sample.bytes_per_command),
            sample.commands,
            sample.elapsed,
            sample.throughput_mb(),
            sample.per_command_ms()
        );
        samples.push(sample);
    }

    analyse(&samples);
    Ok(())
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// Separate the fixed cost of a command from the rate data moves at. Time per
/// command is roughly `overhead + bytes / rate`, so a least-squares fit gives
/// both; the intercept is what decides whether the ceiling is worth raising.
fn analyse(samples: &[Sample]) {
    if samples.len() < 3 {
        return;
    }

    let n = samples.len() as f64;
    let xs: Vec<f64> = samples.iter().map(|s| s.bytes_per_command as f64).collect();
    let ys: Vec<f64> = samples.iter().map(|s| s.per_command_ms() / 1000.0).collect();

    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let covariance: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mean_x) * (y - mean_y)).sum();
    let variance: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();

    if variance <= 0.0 {
        return;
    }

    let slope = covariance / variance;
    let intercept = mean_y - slope * mean_x;

    println!();
    if intercept > 0.0 {
        println!("  fixed cost per command : {:.2} ms", intercept * 1000.0);
    }
    if slope > 0.0 {
        println!("  rate once moving       : {:.2} MB/s", 1.0 / slope / 1_048_576.0);
    }

    let best = samples
        .iter()
        .max_by(|a, b| a.throughput_mb().total_cmp(&b.throughput_mb()));

    let Some(best) = best else { return };
    println!(
        "  best measured          : {:.2} MB/s at {}",
        best.throughput_mb(),
        format_size(best.bytes_per_command)
    );

    // What a bigger transfer buys, and the argument for a high ceiling.
    if let Some(smallest) = samples.first() {
        let gain = best.throughput_mb() / smallest.throughput_mb();
        println!(
            "  {} beats {} by {gain:.1}x, because the {:.2} ms per command is paid either way",
            format_size(best.bytes_per_command),
            format_size(smallest.bytes_per_command),
            intercept * 1000.0
        );
    }

    // Narrow SCSI-2 runs to 10 MB/s, USB 2.0 to roughly 40. Which is in reach
    // says whether a faster drive would help.
    const NARROW_SCSI_MB: f64 = 10.0;
    println!();
    if best.throughput_mb() < NARROW_SCSI_MB * 0.5 {
        println!(
            "  Well under the {NARROW_SCSI_MB:.0} MB/s a narrow SCSI-2 bus can carry, and further"
        );
        println!("  still under USB 2.0, so the drive is the limit here rather than the link.");
    } else if best.throughput_mb() < NARROW_SCSI_MB {
        println!("  Approaching the {NARROW_SCSI_MB:.0} MB/s ceiling of a narrow SCSI-2 bus, which");
        println!("  is likely what is binding rather than USB or the adapter.");
    } else {
        println!("  Past what narrow SCSI-2 alone should carry, so the bus is running fast-wide");
        println!("  or the drive is serving from cache.");
    }
}

fn run() -> Result<(), Error> {
    let cli = Cli::parse();

    // Before opening, since a crashed adapter cannot be opened.
    if matches!(cli.command, Command::Reset) {
        xchange_scsi::bot::reset_by_usb()?;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(200));
            if Adapter::open().is_ok() {
                println!("adapter reset and responding");
                return Ok(());
            }
        }
        println!("adapter was reset but is not answering; unplug it and plug it back in");
        return Ok(());
    }

    let mut adapter = Adapter::open()?;

    match cli.command {
        Command::Scan { luns } => {
            println!("scanning targets 0..{}", MAX_TARGET - 1);
            let mut found = 0;

            let results = if luns { adapter.scan_all() } else { adapter.scan() };

            for (at, result) in results {
                match result {
                    Ok(Some(inquiry)) => {
                        found += 1;
                        println!("  {at}: {inquiry}");
                    }
                    Ok(None) => {}
                    Err(error) => println!("  {at}: error: {error}"),
                }
            }

            if found == 0 {
                println!("nothing responded; check termination, power and cabling");
            }
        }

        Command::Probe { from, to, luns } => {
            println!("sweeping target {from}..={to}");
            let mut hits = Vec::new();
            let mut held = Some(adapter);

            for value in from..=to {
                let current = held.as_mut().expect("adapter present");

                match current.inquiry(Address::target(value)) {
                    Ok(Some(inquiry)) => {
                        println!("  0x{value:02x} ({value:3}): {inquiry}");
                        hits.push(value);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        println!("  0x{value:02x} ({value:3}): error: {error}");
                        // The firmware has stopped answering, so every later
                        // address would say the same. Reset, or the rest of
                        // the sweep means nothing.
                        println!("       recovering the adapter...");
                        held = Some(recover(held.take().expect("adapter present"))?);
                    }
                }
            }

            let mut adapter = held.expect("adapter present");

            if hits.is_empty() {
                println!("nothing responded in that range");
                return Ok(());
            }

            println!(
                "\n{} address(es) responded: {}",
                hits.len(),
                hits.iter().map(|v| format!("0x{v:02x}")).collect::<Vec<_>>().join(", ")
            );

            if !luns {
                println!("pass --luns to also sweep the legacy CDB LUN field");
                return Ok(());
            }

            // What matters is whether the drive decodes the CDB's LUN field.
            // One that cannot answers as itself at all eight; one that can
            // replies 011b for the seven it lacks. Only the raw reply shows
            // it, so this bypasses the filtering `inquiry` does.
            for value in hits {
                println!("\naddress 0x{value:02x}, CDB LUN sweep:");
                let mut present = 0;
                let mut rejected = 0;
                let mut identities = std::collections::BTreeSet::new();

                for lun in 0..8u8 {
                    match adapter.inquiry_raw(Address::new(value, lun)) {
                        Ok(Some(data)) => {
                            let qualifier = data[0] >> 5;
                            let kind = data[0] & 0x1f;

                            match qualifier {
                                0 => {
                                    present += 1;
                                    let inquiry = xchange_scsi::scsi::Inquiry::parse(&data);
                                    identities.insert(format!("{:?}", &data[8..36]));
                                    match inquiry {
                                        Some(inquiry) => println!("  LUN {lun}: {inquiry}"),
                                        None => println!("  LUN {lun}: type 0x{kind:02x}"),
                                    }
                                }
                                3 => {
                                    rejected += 1;
                                    println!("  LUN {lun}: no such logical unit (qualifier 011b, type 0x{kind:02x})");
                                }
                                other => println!("  LUN {lun}: qualifier {other:03b}, type 0x{kind:02x}"),
                            }
                        }
                        Ok(None) => println!("  LUN {lun}: no reply"),
                        Err(error) => println!("  LUN {lun}: error: {error}"),
                    }
                }

                println!();
                if rejected > 0 {
                    println!(
                        "  => the drive decoded the LUN field and rejected {rejected} of 8, so the\n\
                              adapter passes it through. Multi-LUN devices are addressable this way."
                    );
                } else if present == 8 && identities.len() == 1 {
                    println!(
                        "  => the same device answered at all 8 LUNs, so the field is being ignored.\n\
                              Multi-LUN devices are not addressable."
                    );
                } else {
                    println!("  => {present} present, {rejected} rejected; inconclusive on this device.");
                }
            }
        }

        Command::Reset => unreachable!(),

        Command::Format {
            target,
            lun,
            dry_run,
            yes,
            no_certify,
            ignore_primary,
            mut defects,
            defect_range,
            force_defect_list,
        } => {
            if let Some(range) = defect_range {
                defects.extend(parse_defect_range(&range)?);
            }

            // A parameter list within one 512-byte packet is carried; one that
            // spans two crashes the adapter until power cycled. 2 addresses
            // work, 200 do not. Where it sits between 127 and 128 is untested,
            // so the last that certainly fits is the limit.
            const PACKET: usize = 512;
            const HEADER: usize = 4;
            let max_defects = (PACKET - HEADER) / 4;

            if defects.len() > max_defects && !force_defect_list {
                eprintln!(
                    "A defect list of {} addresses needs {} bytes of parameter data,",
                    defects.len(),
                    HEADER + defects.len() * 4
                );
                eprintln!("which is more than one 512-byte bulk packet.");
                eprintln!();
                eprintln!("Parameter data spanning more than one packet hangs this adapter: it");
                eprintln!("stops answering everything, including its own reset, and only removing");
                eprintln!("power recovers it. Lists that fit a single packet are carried, so the");
                eprintln!("limit here is {max_defects} addresses.");
                eprintln!();
                eprintln!("--force-defect-list sends it regardless, at the cost of a power cycle");
                eprintln!("if this adapter behaves like the one it was tested against.");
                std::process::exit(1);
            }

            let options = FormatOptions {
                skip_certification: no_certify,
                ignore_primary,
                defects,
            };
            format_unit(&mut adapter, Address::new(target, lun), dry_run, yes, options)?;
        }

        Command::Defects { target, lun, list } => {
            let at = Address::new(target, lun);

            for (name, primary, grown) in [
                ("primary (factory)", true, false),
                ("grown (in service)", false, true),
            ] {
                match adapter.read_defect_data(at, primary, grown) {
                    Ok(defects) => {
                        let valid = if primary { defects.primary_valid } else { defects.grown_valid };
                        println!("{at} {name}: {} defect(s)", defects.total());

                        if !valid {
                            println!("  the drive did not mark this list valid, so the count");
                            println!("  above may mean 'none recorded' rather than 'none exist'");
                        }
                        if defects.undecoded > 0 {
                            println!("  reported in a format this tool does not decode");
                        }
                        if list {
                            for lba in &defects.addresses {
                                println!("  {lba}");
                            }
                        } else if !defects.addresses.is_empty() {
                            println!("  pass --list to see the addresses");
                        }
                    }
                    Err(error) => println!("{at} {name}: {error}"),
                }
            }
        }

        Command::Surface { target, lun, from, blocks, expect_zeros } => {
            surface(&mut adapter, Address::new(target, lun), from, blocks, expect_zeros)?;
        }

        Command::Verify { target, lba, blocks, lun } => {
            let at = Address::new(target, lun);
            let capacity = adapter.read_capacity(at)?;
            let block_size = capacity.block_size;

            let most = adapter.max_blocks(block_size);
            let blocks = blocks.min(most);
            println!("{at}: checking {blocks} blocks from LBA {lba}");

            let bulk = adapter.read(at, lba, blocks, block_size)?;

            let mut single = Vec::with_capacity(bulk.len());
            for offset in 0..blocks as u32 {
                single.extend_from_slice(&adapter.read(at, lba + offset, 1, block_size)?);
            }

            if bulk == single {
                println!("  identical: {} bytes read both ways", bulk.len());
                println!("  addresses are reaching the drive intact at this range");
            } else {
                let differs = bulk
                    .iter()
                    .zip(&single)
                    .position(|(a, b)| a != b)
                    .unwrap_or(bulk.len().min(single.len()));
                println!("  MISMATCH at byte {differs} of {}", bulk.len());
                println!("  block {} of the region differs", differs / block_size as usize);
                println!("  the two ways of asking do not agree, so addressing is at fault");
            }
        }

        Command::Bench { target, lun, seconds, start_lba, ceiling } => {
            if let Some(ceiling) = ceiling {
                println!("  transfer ceiling lifted to {ceiling} bytes for this run\n");
                adapter.set_transfer_limit(ceiling);
            }
            bench(&mut adapter, Address::new(target, lun), seconds, start_lba)?;
        }

        Command::Inquiry { target, lun } => match adapter.inquiry(Address::new(target, lun))? {
            Some(inquiry) => {
                println!("{}: {inquiry}", Address::new(target, lun));
                println!("  peripheral type: {}", inquiry.peripheral_type.name());
                println!("  removable      : {}", inquiry.removable);
                println!("  SCSI version   : {}", inquiry.version);
            }
            None => println!("{}: nothing there", Address::new(target, lun)),
        },

        Command::Capacity { target, lun } => {
            let at = Address::new(target, lun);
            let capacity = adapter.read_capacity(at)?;
            println!("{at}: {capacity}");
        }

        Command::Ready { target, lun } => {
            let at = Address::new(target, lun);
            match adapter.test_unit_ready(at)? {
                Ok(()) => println!("{at}: ready"),
                Err(sense) => println!("{at}: not ready: {sense}"),
            }
        }

        Command::Read { target, lba, blocks, lun } => {
            let at = Address::new(target, lun);
            let capacity = adapter.read_capacity(at)?;
            let data = adapter.read(at, lba, blocks, capacity.block_size)?;
            println!("read {} bytes from LBA {lba}", data.len());
            hexdump(&data, 512);
        }
    }

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
