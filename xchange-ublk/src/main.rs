//! Present a SCSI disk behind an Adaptec USB2Xchange as a block device, using
//! ublk.
//!
//! **Behind the `ublk` feature, off by default.** On 6.17.0-35-generic,
//! `UBLK_CMD_ADD_DEV` NULL-dereferences the kernel in `ublk_init_queues()`
//! with stock parameters; see `docs/ublk-depth1-oops.md`. `xchange-nbd` does
//! the same job today. Kept because ublk is the better long-term answer.
//!
//! ```text
//! cargo build --release -p xchange-ublk --features ublk
//! ```
//!
//! That build also fails for an unrelated reason: this crate predates
//! `xchange-scsi` moving to `scsi::Address`, and `required-features` meant the
//! default build never caught the drift. Retesting starts by bringing `serve`
//! and `add` up to the current signatures, which the compiler will list, and
//! `xchange-nbd` shows what they should look like.
//!
//! Writes are refused without `--writable`. The media is usually
//! irreplaceable, and a read-only default costs nothing.

use clap::{Parser, Subcommand};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkFlags, UblkIORes};
use std::rc::Rc;

use xchange_scsi::bot::{Adapter, MAX_TRANSFER};

#[derive(Parser)]
#[command(
    name = "xchange-ublk",
    about = "Serve a SCSI disk behind an Adaptec USB2Xchange as a block device"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Attach a target as a block device. Runs in the foreground.
    Add {
        /// SCSI target ID, as reported by `xchange scan`.
        #[arg(short, long)]
        target: u8,

        /// Allow writes. Without this the device is read-only.
        #[arg(long)]
        writable: bool,

        /// Block device id to request; -1 lets the kernel choose.
        #[arg(long, default_value_t = -1)]
        id: i32,
    },

    /// Remove a block device previously added.
    Del {
        id: i32,
    },
}

/// What the queue handler needs, all of it copyable into the closure.
#[derive(Clone, Copy)]
struct Target {
    id: u8,
    block_size: u32,
    writable: bool,
}

/// Serve one request. Returns the byte count, or a negative errno.
fn serve(adapter: &mut Adapter, target: Target, q: &UblkQueue, tag: u16, buf: &mut [u8]) -> i32 {
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;

    // ublk always speaks 512-byte sectors, so convert through a byte offset.
    // They match on this Jaz; a CD-ROM's 2048-byte blocks would not.
    let offset = iod.start_sector << 9;
    let bytes = (iod.nr_sectors as usize) << 9;
    let block_size = target.block_size as u64;

    if !offset.is_multiple_of(block_size) || !(bytes as u64).is_multiple_of(block_size) {
        return -libc::EINVAL;
    }
    if bytes > buf.len() {
        return -libc::EINVAL;
    }

    let lba = (offset / block_size) as u32;
    let blocks = (bytes as u64 / block_size) as u32;
    let per_command = Adapter::max_blocks(target.block_size) as u32;

    match op {
        libublk::sys::UBLK_IO_OP_READ => {
            let mut done = 0usize;
            let mut block = 0u32;

            while block < blocks {
                let chunk = per_command.min(blocks - block);
                let span = chunk as usize * target.block_size as usize;

                match adapter.read(target.id, lba + block, chunk as u16, target.block_size) {
                    Ok(data) if data.len() >= span => {
                        buf[done..done + span].copy_from_slice(&data[..span]);
                    }
                    Ok(_) => return -libc::EIO,
                    Err(error) => {
                        eprintln!("read at LBA {} failed: {error}", lba + block);
                        return -libc::EIO;
                    }
                }

                done += span;
                block += chunk;
            }

            bytes as i32
        }

        libublk::sys::UBLK_IO_OP_WRITE => {
            if !target.writable {
                return -libc::EROFS;
            }

            let mut done = 0usize;
            let mut block = 0u32;

            while block < blocks {
                let chunk = per_command.min(blocks - block);
                let span = chunk as usize * target.block_size as usize;

                if let Err(error) = adapter.write(
                    target.id,
                    lba + block,
                    chunk as u16,
                    target.block_size,
                    &buf[done..done + span],
                ) {
                    eprintln!("write at LBA {} failed: {error}", lba + block);
                    return -libc::EIO;
                }

                done += span;
                block += chunk;
            }

            bytes as i32
        }

        // Nothing cached here, and the drive writes through.
        libublk::sys::UBLK_IO_OP_FLUSH => 0,

        _ => -libc::EINVAL,
    }
}

fn add(target_id: u8, writable: bool, id: i32) -> Result<(), Box<dyn std::error::Error>> {
    // Before creating the device, so a missing drive fails readably rather
    // than half way through bringing one up.
    let mut adapter = Adapter::open()?;

    let inquiry = adapter
        .inquiry(target_id)?
        .ok_or_else(|| format!("nothing at target {target_id}"))?;
    let capacity = adapter.read_capacity(target_id)?;
    drop(adapter);

    println!("target {target_id}: {inquiry}");
    println!("  {capacity}");
    println!("  mode: {}", if writable { "read/write" } else { "read-only" });

    let target = Target {
        id: target_id,
        block_size: capacity.block_size,
        writable,
    };
    let size = capacity.bytes();

    let ctrl = UblkCtrlBuilder::default()
        .name("usb2xchange")
        .id(id)
        // One queue, since the transport carries one command at a time. The
        // depth stays at libublk's default: `depth(1)` looks right for a serial
        // transport but NULL-dereferences the kernel, and nothing range-checks
        // the lower bound. Requests queue behind the handler regardless.
        .nr_queues(1)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()?;

    let tgt_init = |dev: &mut UblkDev| {
        dev.set_default_params(size);
        // So the block layer splits requests instead of `serve` issuing a long
        // chain of commands per request. `serve` still chunks, as a backstop.
        dev.tgt.params.basic.max_sectors = (MAX_TRANSFER >> 9) as u32;
        Ok(())
    };

    let queue_handler = move |qid: u16, dev: &UblkDev| {
        // This runs on the queue's own thread; the handle must not be shared.
        let mut adapter = match Adapter::open() {
            Ok(adapter) => adapter,
            Err(error) => {
                eprintln!("queue {qid}: cannot open adapter: {error}");
                return;
            }
        };

        let bufs = Rc::new(dev.alloc_queue_io_bufs());
        let queue = match UblkQueue::new(qid, dev)
            .unwrap()
            .submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs)))
        {
            Ok(queue) => queue,
            Err(error) => {
                eprintln!("queue {qid}: cannot submit fetch commands: {error}");
                return;
            }
        };

        queue.wait_and_handle_io(|q: &UblkQueue, tag: u16, _io: &UblkIOCtx| {
            let buf = &bufs[tag as usize];

            // SAFETY: registered buffers reach us behind an `Rc`, so only a
            // shared reference is available; `IoBuf` exposes `as_mut_ptr` from
            // `&self` for this. While we hold a tag its buffer is ours alone
            // and the kernel will not touch it until the command completes.
            let slice = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };

            let result = serve(&mut adapter, target, q, tag, slice);
            let _ = q.complete_io_cmd_unified(
                tag,
                BufDesc::Slice(buf.as_slice()),
                Ok(UblkIORes::Result(result)),
            );
        });
    };

    let announce = |ctrl: &UblkCtrl| {
        ctrl.dump();
        println!("\nready. Ctrl-C to detach.");
    };

    ctrl.run_target(tgt_init, queue_handler, announce)?;
    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Add { target, writable, id } => add(target, writable, id),
        Command::Del { id } => UblkCtrl::new_simple(id)
            .and_then(|ctrl| ctrl.del_dev())
            .map(|_| ())
            .map_err(Into::into),
    };

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
