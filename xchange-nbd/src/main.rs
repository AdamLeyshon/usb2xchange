//! Present a SCSI disk behind an Adaptec USB2Xchange as a block device.
//!
//! This started on `ublk`, but `UBLK_CMD_ADD_DEV` NULL-dereferences the kernel
//! on this build (see `docs/ublk-depth1-oops.md`). NBD needs neither io_uring
//! nor a helper daemon: `/dev/nbdN` gets one end of a socketpair.
//!
//! Writes are refused without `--writable`. The media is usually
//! irreplaceable, and a read-only default costs nothing.

mod nbd;

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;
use std::time::Duration;

use xchange_scsi::bot::{Adapter, Ready};
use xchange_scsi::scsi::{Address, Capacity, Inquiry};
use xchange_scsi::Error;

#[derive(Parser)]
#[command(
    name = "xchange-nbd",
    about = "Serve a SCSI disk behind an Adaptec USB2Xchange as a block device"
)]
struct Cli {
    /// Serve every block device on the bus, one `/dev/nbdN` each. Only one
    /// process can hold the adapter, so a scan cannot run alongside a server
    /// and everything has to be found up front.
    #[arg(long, conflicts_with = "target")]
    all: bool,

    /// SCSI target ID, as reported by `xchange scan`.
    #[arg(short, long)]
    target: Option<u8>,

    /// Logical unit on that target. **Experimental**: the addressing reaches
    /// the drive, but no multi-unit device has been tested.
    #[arg(short, long, default_value_t = 0)]
    lun: u8,

    /// Block device to attach to, or the first of them with `--all`.
    #[arg(short, long, default_value = "/dev/nbd0")]
    device: PathBuf,

    /// Allow writes. Without this the device is read-only.
    #[arg(long)]
    writable: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Shared between the block devices we serve. One command travels at a time
/// regardless, so the lock costs nothing the hardware was not already.
type Shared = Arc<Mutex<Adapter>>;

/// Generous on purpose: an optical drive can take twenty seconds to recognise
/// a disc, and this costs nothing when the device is already awake.
const SPIN_UP_ALLOWANCE: Duration = Duration::from_secs(45);

/// How often to ask a drive what is in it. Also the socket read timeout, so
/// it sets how long the server blocks before it can look. The only way to
/// notice an eject while nothing is reading.
const MEDIA_POLL: Duration = Duration::from_secs(3);

/// How long a watch tick waits for a drive to settle. Short: the tick holds
/// the adapter, and with `--all` that stalls every other export.
const MEDIA_SETTLE: Duration = Duration::from_secs(1);

/// One SCSI device, and the block device standing in for it.
struct Export {
    at: Address,
    inquiry: Inquiry,
    /// What is loaded, or `None` for an empty drive. Empty is exported at size
    /// zero rather than dropped, like `/dev/sr0` with the tray open; otherwise
    /// the daemon could only start with a disc already in.
    capacity: Option<Capacity>,
    node: PathBuf,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let mut adapter = Adapter::open()?;
    let mut exports = if cli.all {
        discover(&mut adapter, &cli.device)?
    } else {
        let at = Address::new(cli.target.unwrap_or(1), cli.lun);
        let inquiry = adapter
            .inquiry(at)?
            .ok_or_else(|| format!("nothing at {at}"))?;
        let capacity = match adapter.wait_until_ready(at, SPIN_UP_ALLOWANCE) {
            Ready::Yes => Some(adapter.read_capacity(at)?),
            Ready::NoMedium => None,
            other => return Err(format!("{at}: {}", other.describe()).into()),
        };
        vec![Export { at, inquiry, capacity, node: cli.device.clone() }]
    };

    if exports.is_empty() {
        return Err("no block devices found on the bus".into());
    }

    // After the scan, so the module gets as many nodes as there are devices.
    // The scan needs only USB.
    let usable = ensure_nbd(exports.len())?;
    if usable < exports.len() {
        eprintln!(
            "only {usable} of {} devices can be served: nbd is already loaded with \
             too few nodes and is in use, so it cannot be reloaded",
            exports.len()
        );
        exports.truncate(usable);
    }

    for export in &exports {
        println!("{} <- {}: {}", export.node.display(), export.at, export.inquiry);
        match export.capacity {
            Some(capacity) => println!("   {capacity}"),
            None => println!("   no medium loaded, waiting for one"),
        }
    }
    println!(
        "\nmode: {}",
        if cli.writable { "read/write" } else { "read-only" }
    );
    if cli.lun != 0 {
        println!("note: non-zero logical units are experimental and untested");
    }

    let shared: Shared = Arc::new(Mutex::new(adapter));
    let mut running = Vec::new();

    for export in exports {
        let shared = Arc::clone(&shared);
        let writable = cli.writable;

        running.push(std::thread::spawn(move || match attach(shared, &export, writable) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("{}: {error}", export.node.display());
                false
            }
        }));
    }

    println!("\nserving — Ctrl-C to detach");

    // Under systemd a silent success while serving nothing looks identical to
    // working, and Restart=no means nothing would ever say otherwise.
    let mut served = 0;
    for handle in running {
        if handle.join().unwrap_or(false) {
            served += 1;
        }
    }

    if served == 0 {
        return Err("no block device could be served".into());
    }
    Ok(())
}

/// Bring the nbd module up with room for what we found, and report how many
/// can be served. `nbds_max` is fixed at load, so too few nodes means a
/// reload, which fails if anything is using it. That is the right outcome.
fn ensure_nbd(wanted: usize) -> Result<usize, Box<dyn std::error::Error>> {
    const NBDS_MAX: &str = "/sys/module/nbd/parameters/nbds_max";

    let loaded = std::fs::read_to_string(NBDS_MAX)
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok());

    match loaded {
        Some(current) if current >= wanted => return Ok(current),
        Some(current) => {
            println!("nbd is loaded with {current} nodes, {wanted} wanted; reloading");
            let _ = std::process::Command::new("rmmod").arg("nbd").status();
        }
        None => {}
    }

    let status = std::process::Command::new("modprobe")
        .arg("nbd")
        .arg(format!("nbds_max={wanted}"))
        .status()?;

    if !status.success() {
        return Err("could not load the nbd module".into());
    }

    // If the reload was refused as busy, modprobe succeeds having done nothing.
    let actual = std::fs::read_to_string(NBDS_MAX)?
        .trim()
        .parse::<usize>()
        .unwrap_or(wanted);

    Ok(actual)
}

/// Pair every block device with a free `/dev/nbdN`, once, at startup. SCSI is
/// not hot-pluggable and this adapter hangs if the bus changes underneath it,
/// so anything new arrives with a power cycle anyway.
fn discover(adapter: &mut Adapter, first: &std::path::Path) -> Result<Vec<Export>, Box<dyn std::error::Error>> {
    let base = nbd_index(first).unwrap_or(0);
    let found: Vec<(Address, Inquiry)> = adapter
        .scan()
        .into_iter()
        .filter_map(|(at, result)| result.ok().flatten().map(|inquiry| (at, inquiry)))
        .collect();

    let mut exports = Vec::new();

    for (at, inquiry) in found {
        // udev starts us the moment the adapter appears, well before a drive
        // powered up alongside it has spun. Without this, slow reads as dead.
        match adapter.wait_until_ready(at, SPIN_UP_ALLOWANCE) {
            Ready::Yes => {}

            // Empty is not a reason to drop it. Ejecting a cartridge used to
            // take the daemon down, since an empty export list is fatal.
            Ready::NoMedium
                if inquiry.removable && inquiry.peripheral_type.is_block_like() =>
            {
                println!("{at} ({}): no medium loaded, waiting", inquiry.product);
                exports.push(Export {
                    at,
                    inquiry,
                    capacity: None,
                    node: PathBuf::from(format!("/dev/nbd{}", base + exports.len())),
                });
                continue;
            }

            other => {
                println!("skipping {at} ({}): {}", inquiry.product, other.describe());
                continue;
            }
        }

        // No capacity, no block device: a tape or scanner lands here.
        let capacity = match adapter.read_capacity(at) {
            Ok(capacity) => Some(capacity),
            Err(error) => {
                println!("skipping {at} ({}): {error}", inquiry.product);
                continue;
            }
        };

        exports.push(Export {
            at,
            inquiry,
            capacity,
            node: PathBuf::from(format!("/dev/nbd{}", base + exports.len())),
        });
    }

    Ok(exports)
}

/// The trailing number of `/dev/nbd3`, so `--device` picks the first node.
fn nbd_index(path: &std::path::Path) -> Option<usize> {
    path.file_name()?
        .to_str()?
        .strip_prefix("nbd")?
        .parse()
        .ok()
}

/// Hand one export's socket to the kernel and answer its requests.
fn attach(shared: Shared, export: &Export, writable: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Size zero, and a placeholder block size until a medium states one.
    let block_size = export.capacity.map_or(512, |capacity| capacity.block_size);
    let blocks = export.capacity.map_or(0, |capacity| capacity.last_lba as u64 + 1);

    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&export.node)?;
    let nbd_fd = device.as_raw_fd();

    let pair = nbd::socket_pair()?;
    let server = pair.server;

    let mut flags = nbd::NBD_FLAG_HAS_FLAGS | nbd::NBD_FLAG_SEND_FLUSH;
    if !writable {
        flags |= nbd::NBD_FLAG_READ_ONLY;
    }

    nbd::ioctl(nbd_fd, nbd::NBD_SET_BLKSIZE, block_size as u64, "NBD_SET_BLKSIZE")?;
    nbd::ioctl(nbd_fd, nbd::NBD_SET_SIZE_BLOCKS, blocks, "NBD_SET_SIZE_BLOCKS")?;
    nbd::ioctl(nbd_fd, nbd::NBD_SET_FLAGS, flags, "NBD_SET_FLAGS")?;
    nbd::ioctl(nbd_fd, nbd::NBD_SET_SOCK, nbd::as_arg(&pair.kernel), "NBD_SET_SOCK")?;

    // Or the block layer builds 4 MB requests to be chunked into dozens of
    // SCSI commands. Matching the limit makes it one request, one command, so
    // a failure blames the right request rather than everything merged in.
    if let Some(name) = export.node.file_name().and_then(|n| n.to_str()) {
        let limit = format!("/sys/block/{name}/queue/max_sectors_kb");
        let kb = xchange_scsi::bot::MAX_TRANSFER / 1024;
        if let Err(error) = std::fs::write(&limit, kb.to_string()) {
            eprintln!("{}: could not set max_sectors_kb: {error}", export.node.display());
        }
    }

    // NBD_SET_SOCK took its own reference, so this is safe, and necessary:
    // holding a descriptor keeps both ends open, so our end would never see
    // EOF and the serve loop would never notice the kernel had finished.
    drop(pair.kernel);

    let worker = std::thread::spawn(move || {
        let result = nbd::ioctl(nbd_fd, nbd::NBD_DO_IT, 0, "NBD_DO_IT");
        let _ = nbd::ioctl(nbd_fd, nbd::NBD_CLEAR_SOCK, 0, "NBD_CLEAR_SOCK");
        result
    });

    let outcome = serve(shared, export.at, export.capacity, nbd_fd, writable, server);

    let _ = nbd::ioctl(nbd_fd, nbd::NBD_DISCONNECT, 0, "NBD_DISCONNECT");
    let _ = worker.join();

    outcome.map_err(Into::into)
}

/// Why a request failed, and who was responsible. A drive refusing a read is
/// one bad sector; the adapter refusing means nothing more will work.
struct Failure {
    code: u32,
    adapter_at_fault: bool,
}

/// What one NBD request turned into.
enum Outcome {
    Done,
    Failed(Failure),

    /// A medium change during this request, which fails it whatever the
    /// command said. The transport retries past a unit attention, so even a
    /// Good read answered from the new cartridge at the old one's offset.
    MediumChanged,
}

/// Consecutive transport failures before the adapter is declared lost. Once it
/// stops accepting command blocks it does not come back without a power cycle,
/// and serving on costs a full timeout per request while the kernel queues.
const FAILURES_BEFORE_GIVING_UP: u32 = 3;

/// Answer requests until the kernel closes the socket. `capacity` is refreshed
/// in place on a swap, so it is the live answer rather than the startup one.
fn serve(
    shared: Shared,
    at: Address,
    mut capacity: Option<Capacity>,
    nbd_fd: std::os::fd::RawFd,
    writable: bool,
    mut stream: std::os::unix::net::UnixStream,
) -> std::io::Result<()> {
    let mut scratch = Vec::new();
    let mut partial = Vec::new();
    let mut consecutive_failures = 0u32;

    // Always on, so the code path cannot change shape underneath a request
    // that is already part-delivered.
    stream.set_read_timeout(Some(MEDIA_POLL))?;

    loop {
        let request = match nbd::Request::read_from(&mut stream, &mut partial)? {
            nbd::Incoming::Request(request) => request,
            nbd::Incoming::Closed => return Ok(()),
            nbd::Incoming::Idle => {
                if !watch(&shared, at, nbd_fd, &mut capacity) {
                    return Ok(());
                }
                continue;
            }
        };

        if consecutive_failures >= FAILURES_BEFORE_GIVING_UP {
            eprintln!(
                "{at}: adapter stopped accepting commands after {consecutive_failures} \
                 attempts; disconnecting"
            );
            eprintln!("{at}: it will not recover without a power cycle");
            return Ok(());
        }

        match request.command {
            nbd::CMD_DISCONNECT => return Ok(()),

            nbd::CMD_FLUSH => {
                // Nothing cached here, and the drive writes through.
                nbd::reply(&mut stream, request.handle, 0, None)?;
            }

            nbd::CMD_TRIM => {
                // Never advertised, so this should not arrive. Answering OK
                // would make `blkdiscard` look like it had erased a cartridge
                // it never touched.
                nbd::reply(&mut stream, request.handle, nbd::EOPNOTSUPP, None)?;
            }

            nbd::CMD_READ => {
                let Some(current) = capacity else {
                    // Size zero, so it is asking about a medium that is not there.
                    nbd::reply(&mut stream, request.handle, nbd::EIO, None)?;
                    continue;
                };

                match transfer(&shared, at, current.block_size, &request, None, &mut scratch) {
                    Outcome::Done => {
                        consecutive_failures = 0;
                        nbd::reply(&mut stream, request.handle, 0, Some(&scratch))?
                    }
                    Outcome::Failed(Failure { code, adapter_at_fault }) => {
                        // One bad sector, or the end of the session.
                        consecutive_failures =
                            if adapter_at_fault { consecutive_failures + 1 } else { 0 };
                        nbd::reply(&mut stream, request.handle, code, None)?
                    }
                    Outcome::MediumChanged => {
                        consecutive_failures = 0;
                        nbd::reply(&mut stream, request.handle, nbd::EIO, None)?;
                        // Worth waiting out: we know something moved, and the
                        // sooner the export is usable again the better.
                        if !adopt(&shared, at, nbd_fd, &mut capacity, SPIN_UP_ALLOWANCE) {
                            return Ok(());
                        }
                    }
                }
            }

            nbd::CMD_WRITE => {
                // Consumed whether or not we honour it, or the stream desyncs.
                let mut payload = vec![0u8; request.length as usize];
                nbd::read_payload(&mut stream, &mut payload)?;

                if !writable {
                    nbd::reply(&mut stream, request.handle, nbd::EPERM, None)?;
                    continue;
                }

                let Some(current) = capacity else {
                    nbd::reply(&mut stream, request.handle, nbd::EIO, None)?;
                    continue;
                };

                match transfer(&shared, at, current.block_size, &request, Some(&payload), &mut scratch) {
                    Outcome::Done => {
                        consecutive_failures = 0;
                        nbd::reply(&mut stream, request.handle, 0, None)?
                    }
                    Outcome::Failed(Failure { code, adapter_at_fault }) => {
                        consecutive_failures =
                            if adapter_at_fault { consecutive_failures + 1 } else { 0 };
                        nbd::reply(&mut stream, request.handle, code, None)?
                    }
                    Outcome::MediumChanged => {
                        consecutive_failures = 0;
                        nbd::reply(&mut stream, request.handle, nbd::EIO, None)?;
                        // Worth waiting out: we know something moved, and the
                        // sooner the export is usable again the better.
                        if !adopt(&shared, at, nbd_fd, &mut capacity, SPIN_UP_ALLOWANCE) {
                            return Ok(());
                        }
                    }
                }
            }

            other => {
                eprintln!("unsupported NBD command {other}");
                nbd::reply(&mut stream, request.handle, nbd::EINVAL, None)?;
            }
        }
    }
}

/// Take the adapter, ignoring poisoning. Every command resynchronises the
/// endpoint first, so a panicked thread leaves nothing the next would notice,
/// and dropping every other export over one death is the worse outcome.
fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Adapter> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One tick of the media watch, run when the socket goes quiet. Returns
/// whether serving can continue. Nothing reports a change until something
/// asks, so without this an emptied drive keeps advertising old geometry.
fn watch(
    shared: &Shared,
    at: Address,
    nbd_fd: std::os::fd::RawFd,
    capacity: &mut Option<Capacity>,
) -> bool {
    let (ready, changed) = {
        let mut adapter = lock(shared);
        let ready = adapter.wait_until_ready(at, MEDIA_SETTLE);
        let changed = adapter.take_medium_change(at);
        (ready, changed)
    };

    match ready {
        // The common tick, costing one TEST UNIT READY.
        Ready::Yes if capacity.is_some() && !changed => true,

        // Empty and known to be.
        Ready::NoMedium if capacity.is_none() => true,

        // Settling. Look again next tick rather than tearing down an export.
        Ready::TimedOut => true,

        // Anything else means the drive and the block device disagree.
        _ => adopt(shared, at, nbd_fd, capacity, MEDIA_SETTLE),
    }
}

/// Make the block device match what is in the drive, returning whether serving
/// can continue. A change we know about is worth the allowance; a speculative
/// watch tick is not, since it holds the adapter while it waits.
fn adopt(
    shared: &Shared,
    at: Address,
    nbd_fd: std::os::fd::RawFd,
    capacity: &mut Option<Capacity>,
    allowance: Duration,
) -> bool {
    let found = {
        let mut adapter = lock(shared);
        let ready = adapter.wait_until_ready(at, allowance);

        // So the next request does not report the same change again.
        adapter.take_medium_change(at);

        match ready {
            Ready::Yes => match adapter.read_capacity(at) {
                Ok(fresh) => Some(fresh),
                Err(error) => {
                    eprintln!("{at}: cannot read the new capacity: {error}; disconnecting");
                    return false;
                }
            },
            Ready::NoMedium => None,
            // Still coming up; the watch will be back.
            Ready::TimedOut => return true,
            other => {
                eprintln!("{at}: {} after the change; disconnecting", other.describe());
                return false;
            }
        }
    };

    // Every queued offset was divided by the old block size and the filesystem
    // above was mounted against the old geometry, so serving on would answer
    // from somewhere else entirely without reporting an error. Only catches a
    // swap seen as one event; an eject and insert pass through empty, which
    // clears the geometry.
    if let (Some(old), Some(new)) = (*capacity, found) {
        if old.block_size != new.block_size {
            eprintln!(
                "{at}: block size changed from {} to {} in place; disconnecting",
                old.block_size, new.block_size
            );
            return false;
        }
    }

    // What it holds belongs to a medium that has gone, and for a replacement
    // of the same size the resize below does nothing, so this is all that
    // stops the old contents being served from memory as the new one's.
    if let Err(error) = nbd::ioctl(nbd_fd, nbd::BLKFLSBUF, 0, "BLKFLSBUF") {
        eprintln!("{at}: could not flush the block cache: {error}");
    }

    // From empty there is no geometry to contradict, so this is the one moment
    // a block size can be set. It is also the moment that matters: an optical
    // drive is empty at boot and its 2048-byte discs arrive later.
    if capacity.is_none() {
        if let Some(new) = found {
            let size = new.block_size as u64;
            if let Err(error) = nbd::ioctl(nbd_fd, nbd::NBD_SET_BLKSIZE, size, "NBD_SET_BLKSIZE") {
                eprintln!("{at}: could not set the block size to {size}: {error}; disconnecting");
                return false;
            }
        }
    }

    // Empty is zero-size, as a CD-ROM with the tray open. Resizing a connected
    // device relies on the driver dropping its config lock while NBD_DO_IT
    // waits, which it does; a refusal would leave us serving a size the block
    // layer disagrees with, so treat it as fatal.
    let blocks = found.map_or(0, |fresh| fresh.last_lba as u64 + 1);
    if let Err(error) =
        nbd::ioctl(nbd_fd, nbd::NBD_SET_SIZE_BLOCKS, blocks, "NBD_SET_SIZE_BLOCKS")
    {
        eprintln!("{at}: could not resize for the new medium: {error}; disconnecting");
        return false;
    }

    match found {
        Some(fresh) => eprintln!("{at}: medium loaded, now {fresh}"),
        None => eprintln!("{at}: medium removed, waiting for the next one"),
    }

    *capacity = found;
    true
}

/// Did the adapter fail to carry the command, rather than the drive refuse it?
fn is_transport_failure(error: &Error) -> bool {
    matches!(
        error,
        Error::Transfer { .. }
            | Error::ShortCommand { .. }
            | Error::ShortStatus { .. }
            | Error::BadStatusSignature(_)
            | Error::TagMismatch { .. }
            | Error::Usb(_)
    )
}

/// Carry one request to the drive, split to fit the adapter. `out` is filled
/// for a read; `payload` holds the data for a write.
fn transfer(
    shared: &Shared,
    at: Address,
    block_size: u32,
    request: &nbd::Request,
    payload: Option<&[u8]>,
    out: &mut Vec<u8>,
) -> Outcome {
    let drive = |code| Outcome::Failed(Failure { code, adapter_at_fault: false });
    let adapter_lost = |code| Outcome::Failed(Failure { code, adapter_at_fault: true });

    let length = request.length as u64;
    let bs = block_size as u64;

    if !request.offset.is_multiple_of(bs) || !length.is_multiple_of(bs) {
        return drive(nbd::EINVAL);
    }

    let lba = (request.offset / bs) as u32;
    let blocks = (length / bs) as u32;

    // Held for the whole request: the adapter carries one command at a time
    // anyway, so releasing between chunks only lets another export interleave.
    let mut adapter = lock(shared);

    // Asked of the adapter, so the chunk can never exceed what it enforces.
    let per_command = adapter.max_blocks(block_size) as u32;

    out.clear();
    let mut done = 0usize;
    let mut block = 0u32;

    while block < blocks {
        let chunk = per_command.min(blocks - block);
        let span = chunk as usize * block_size as usize;

        let failure = match payload {
            None => match adapter.read(at, lba + block, chunk as u16, block_size) {
                Ok(data) if data.len() >= span => {
                    out.extend_from_slice(&data[..span]);
                    None
                }
                Ok(short) => {
                    eprintln!("short read at LBA {}: {} of {span} bytes", lba + block, short.len());
                    Some(drive(nbd::EIO))
                }
                Err(error) => {
                    eprintln!("read at LBA {} failed: {error}", lba + block);
                    Some(if is_transport_failure(&error) {
                        adapter_lost(nbd::EIO)
                    } else {
                        drive(nbd::EIO)
                    })
                }
            },
            Some(data) => match adapter.write(
                at,
                lba + block,
                chunk as u16,
                block_size,
                &data[done..done + span],
            ) {
                Ok(()) => None,
                Err(error) => {
                    eprintln!("write at LBA {} failed: {error}", lba + block);
                    Some(if is_transport_failure(&error) {
                        adapter_lost(nbd::EIO)
                    } else {
                        drive(nbd::EIO)
                    })
                }
            },
        };

        // Before the failure is returned, and on the way out of one that went
        // well, because a swap outranks both: what matters is that the medium
        // moved, not what this command made of it.
        if adapter.take_medium_change(at) {
            return Outcome::MediumChanged;
        }

        if let Some(failure) = failure {
            return failure;
        }

        done += span;
        block += chunk;
    }

    Outcome::Done
}
