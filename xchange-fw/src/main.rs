//! Command line front end for the Adaptec adapter firmware loader.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use nusb::MaybeFuture;
use sha2::{Digest, Sha256};

use xchange_fw::extract::Chip;
use xchange_fw::{extract, loader, read_file, record, Error};

/// How long to wait for the adapter to come back.
const RENUMERATION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(
    name = "xchange-fw",
    about = "Load firmware into Adaptec USBXchange / USB2Xchange SCSI adapters"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise a firmware file without touching any hardware.
    Info {
        /// Path to a `.fw` container.
        firmware: PathBuf,
    },

    /// Recover the firmware from Adaptec's Windows loader driver (Adpusbld.sys).
    Extract {
        /// Path to Adpusbld.sys from the driver CD.
        driver: PathBuf,

        /// Where to write the extracted `.fw` container.
        #[arg(short, long)]
        output: PathBuf,

        /// Which model's blob to pull out. Omit to list what is present.
        #[arg(long, value_parser = ["usbxchange", "usb2xchange"])]
        model: Option<String>,

        /// Expected SHA-256, to verify the extraction.
        #[arg(long)]
        expect_sha256: Option<String>,
    },

    /// List any Adaptec adapters currently on the bus.
    List,

    /// Upload firmware to an adapter awaiting it.
    Load {
        /// A `.fw` container, or Adpusbld.sys to extract from directly.
        #[arg(short, long)]
        firmware: PathBuf,

        /// Parse and report, but do not touch the device.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Accept a `.fw` file or the Windows driver to mine. The driver holds a blob
/// per model, so mining needs to know which part we are loading.
fn load_records(path: &std::path::Path, chip: Chip) -> Result<Vec<record::Record>, Error> {
    let bytes = read_file(path)?;

    match record::parse_fw(&bytes) {
        Ok(records) => Ok(records),
        Err(_) => {
            let found = extract::scan_for(&bytes, chip)?;
            eprintln!(
                "  recovered {} records for {} from driver image at offset {}",
                found.records.len(),
                found.chip.name(),
                found.offset
            );
            Ok(found.records)
        }
    }
}

fn describe(records: &[record::Record]) {
    let bytes: usize = records.iter().map(|r| r.data.len()).sum();
    let highest = records
        .iter()
        .map(|r| r.address as usize + r.data.len())
        .max()
        .unwrap_or(0);

    println!("  records : {}", records.len());
    println!("  payload : {bytes} bytes");
    println!("  span    : 0x0000..0x{highest:04x}");
    if let Some(first) = records.first() {
        let head: Vec<String> = first.data.iter().take(3).map(|b| format!("{b:02x}")).collect();
        println!("  vector  : {} at 0x{:04x}", head.join(" "), first.address);
    }
}

fn run() -> Result<(), Error> {
    match Cli::parse().command {
        Command::Info { firmware } => {
            let bytes = read_file(&firmware)?;
            println!("{}", firmware.display());

            match record::parse_fw(&bytes) {
                Ok(records) => describe(&records),
                Err(_) => {
                    let blobs = extract::scan_all(&bytes);
                    if blobs.is_empty() {
                        return Err(Error::FirmwareNotFound);
                    }
                    for blob in blobs {
                        println!("\n  blob at {}..{} for {}", blob.offset, blob.end, blob.chip.name());
                        describe(&blob.records);
                    }
                }
            }
        }

        Command::Extract {
            driver,
            output,
            model,
            expect_sha256,
        } => {
            let bytes = read_file(&driver)?;
            println!("{}", driver.display());

            let blobs = extract::scan_all(&bytes);
            if blobs.is_empty() {
                return Err(Error::FirmwareNotFound);
            }
            for blob in &blobs {
                println!(
                    "  blob at {}..{}: {} records for {}",
                    blob.offset,
                    blob.end,
                    blob.records.len(),
                    blob.chip.name()
                );
            }

            let Some(model) = model else {
                println!("\npass --model to choose one of the above; nothing written");
                return Ok(());
            };
            let wanted = if model == "usb2xchange" { Chip::Fx2 } else { Chip::Fx };
            let found = blobs
                .into_iter()
                .find(|b| b.chip == wanted)
                .ok_or(Error::FirmwareNotFound)?;

            println!("\nselected {} blob at {}..{}", found.chip.name(), found.offset, found.end);
            describe(&found.records);

            let encoded = record::to_fw(&found.records)?;
            let digest = format!("{:x}", Sha256::digest(&encoded));
            println!("  sha256  : {digest}");

            if let Some(expected) = expect_sha256 {
                if digest != expected.to_lowercase() {
                    eprintln!("  MISMATCH: expected {expected}");
                    std::process::exit(1);
                }
                println!("  verified against expected digest");
            }

            std::fs::write(&output, &encoded).map_err(|source| Error::Io {
                path: output.display().to_string(),
                source,
            })?;
            println!("  wrote {} ({} bytes)", output.display(), encoded.len());
        }

        Command::List => {
            let devices = nusb::list_devices().wait().map_err(Error::Usb)?;
            let mut seen = false;

            for info in devices {
                if info.vendor_id() != loader::VID_ADAPTEC {
                    continue;
                }
                seen = true;

                let pid = info.product_id();
                let state = loader::MODELS
                    .iter()
                    .find_map(|m| {
                        if m.loader_pid == pid {
                            Some(format!("{}, awaiting firmware", m.name))
                        } else if m.ready_pid == pid {
                            Some(format!("{}, firmware loaded", m.name))
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "unrecognised Adaptec device".to_string());

                println!(
                    "bus {} addr {}: {:04x}:{:04x}  {state}",
                    info.bus_id(),
                    info.device_address(),
                    info.vendor_id(),
                    pid
                );
            }

            if !seen {
                println!("no Adaptec devices found");
            }
        }

        Command::Load { firmware, dry_run } => {
            let (info, model) = loader::find_loader()?;
            let records = load_records(&firmware, model.chip)?;

            println!(
                "found {} at bus {} addr {} ({:04x}:{:04x})",
                model.name,
                info.bus_id(),
                info.device_address(),
                info.vendor_id(),
                info.product_id()
            );
            describe(&records);

            if dry_run {
                println!("dry run: nothing sent to the device");
                return Ok(());
            }

            let device = info.open().wait().map_err(Error::Usb)?;
            println!("uploading to CPUCS 0x{:04x}...", model.cpucs);
            loader::upload(&device, model, &records)?;
            drop(device);

            println!("waiting for re-enumeration as {:04x}:{:04x}...", loader::VID_ADAPTEC, model.ready_pid);
            let ready = loader::await_renumeration(model, RENUMERATION_TIMEOUT)?;
            println!(
                "device is up: bus {} addr {} ({:04x}:{:04x})",
                ready.bus_id(),
                ready.device_address(),
                ready.vendor_id(),
                ready.product_id()
            );
        }
    }

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        let mut source = std::error::Error::source(&error);
        while let Some(inner) = source {
            eprintln!("  caused by: {inner}");
            source = inner.source();
        }
        std::process::exit(1);
    }
}
