# usb2xchange

Userspace driver for the Adaptec USBXchange and USB2Xchange USB-to-SCSI
adapters, in Rust. No kernel module, no patched kernel.

## No warranty

It should go without saying that this code while it has been tested with my own
devices extensively, I cannot guarantee it will work with yours.

I have successfully read and written many gigabytes of data on various hard
drives from the 1990s all the way up to 2016, Iomega Zip and Jaz drives,
Castlewood Orb and CD-ROM drives from multiple manufacturers.

Do not use this code with media that contains precious data as it may result in
total loss or corruption.

## How it differs from Bulk-Only Transport

The adapter enumerates with a vendor-specific interface class, so `usb-storage`
never binds to it. It also bends the transport:

* the command wrapper's LUN byte carries the SCSI **target ID**, not a logical
  unit number, which is how one adapter addresses a whole chain;
* bulk status `0x8A` means "nothing at this target" and `0x02` means "the
  command needs a REQUEST SENSE". The specification defines neither.
* transfers are capped at 62 KB, a limit the firmware does not advertise.

## Requirements

* Linux with `CONFIG_BLK_DEV_NBD` (the `nbd` module)
* Rust 1.75 or later
* `libclang-dev`, only for the optional `ublk` feature
* Adaptec's Windows driver, for the firmware (see [Firmware](#firmware))

## Installation

```sh
cargo build --release
sudo scripts/install.sh /path/to/Adpusbld.sys
```

Binaries go to `/usr/local/bin`, firmware to `/usr/local/share/adaptec`, plus a
udev rule, two systemd units and a modprobe fragment.

### Automatic startup

Two units, because the adapter changes identity part way through starting up:

| Unit | Triggered by | Does |
| --- | --- | --- |
| `xchange-firmware.service` | `03f3:2000` or `03f3:2002` | Starts the firmware, after which the adapter re-enumerates |
| `xchange-nbd.service` | `03f3:2001` or `03f3:2003` | Scans the bus and serves every block device found |

Plug the adapter in and both run in turn; udisks2-based tools then mount the
results like a USB stick. Neither unit is enabled at boot, since udev starts
them when the hardware appears.

Without systemd, run `xchange-fw load` then `xchange-nbd --all` by hand.

### Read-only by default

Devices are served read-only unless `--writable` is given. Mounting read-write
modifies the filesystem by itself: journals replayed, dirty bits cleared, access
times updated. On decades-old media that is often unrecoverable. Take an image
first, enable writes afterwards if you still need them.

For the service, use a drop-in rather than editing the unit, which the installer
overwrites:

```sh
sudo systemctl edit xchange-nbd
```

```ini
[Service]
ExecStart=
ExecStart=/usr/local/bin/xchange-nbd --all --writable
```

The empty `ExecStart=` is required, otherwise systemd appends rather than
replaces.

`xchange format` is guarded separately and sends nothing without `--yes`.

## Commands

### xchange

| Command | Description |
| --- | --- |
| `scan [--luns]` | Walk the bus. `--luns` also walks logical units 1-7. |
| `inquiry <target> [--lun N]` | Identify one device. |
| `capacity <target> [--lun N]` | Report medium size and block size. |
| `ready <target> [--lun N]` | Report readiness, with sense data if not. |
| `read <target> <lba> [blocks] [--lun N]` | Read blocks and hexdump them. |
| `probe [--from N] [--to N] [--luns]` | Determine what the address byte means. |
| `bench [--target N] [--seconds S] [--ceiling B]` | Sequential read throughput. |
| `format <target> [--dry-run] [--yes]` | Low-level format. Destructive. |
| `reset` | Reset the adapter over USB, after a probe crashes it. |

### xchange-fw

| Command | Description |
| --- | --- |
| `list` | Report adapters present and whether firmware is running. |
| `info <file>` | Summarise a `.fw` container or a driver image. |
| `extract <driver> --model <m> -o <file>` | Recover firmware from the Windows driver. `--expect-sha256` verifies it. |
| `load --firmware <file>` | Start the firmware. Accepts a `.fw` file or the driver. |

### xchange-nbd

| Option | Description |
| --- | --- |
| `--all` | Serve every block device found, one `/dev/nbdN` each. |
| `--target N` | Serve one device. |
| `--lun N` | Logical unit on that target. Experimental. |
| `--device PATH` | Block device to attach, or the first with `--all`. |
| `--writable` | Permit writes. Read-only otherwise. |

### xchange-conform

Characterises what the adapter passes through, separating refusals from the
drive from failures in the adapter.

| Option | Description |
| --- | --- |
| `--target N`, `--lun N` | Device to characterise. |
| `--stateful` | Include commands that change device state. |
| `--destructive` | Include commands that can destroy data. |
| `--transfer-limit` | Find the largest transfer the adapter will carry. |
| `--write-test` | Verify the write path, restoring original contents. |
| `--markdown FILE` | Write the verdict as markdown. |
| `--out FILE`, `--restart` | Journal progress; re-running resumes. |

## Firmware

Adaptec's firmware is not distributed here. `xchange-fw` reads it straight out
of `Adpusbld.sys` from the driver CD:

```sh
xchange-fw load --firmware /path/to/Adpusbld.sys
xchange-fw info /path/to/Adpusbld.sys
```

The driver holds two blobs, one per adapter model. They are told apart by the
register page their 8051 code addresses (`MOV DPTR,#imm16`, opcode `0x90`):
`0xE6xx` on the FX2, `0x7Fxx` on the original EZ-USB and FX. Selection does not
depend on file ordering.

`xchange-fw extract --model usb2xchange` writes a standalone `.fw` container in
the format the 2005 hotplug loader expected.

## Protocol reference

| Item | Value |
| --- | --- |
| Pre-firmware ID | `03f3:2002` (USB2Xchange), `03f3:2000` (USBXchange) |
| Post-firmware ID | `03f3:2003` (USB2Xchange), `03f3:2001` (USBXchange) |
| Firmware write | `bmRequestType=0x40`, `bRequest=0xA0`, `wValue=address` |
| CPUCS register | `0xE600` on FX2, `0x7F92` on FX |
| Halt / run 8051 | write `0x01` / `0x00` to CPUCS |
| Post-load init | `bmRequestType=0x40`, `bRequest=0x5A`, `wValue=0x01` then `0x02` |
| Bulk endpoints | `0x02` OUT, `0x86` IN |
| Target address | command wrapper's LUN byte, values 0-7 |
| Logical unit | CDB byte 1, bits 5-7, values 0-7 |
| Transfer ceiling | 63488 bytes |

## Reading failing media

Bad sectors are reported with detail:

```
read at LBA 2617080 failed: target 1 reported check condition:
  unrecovered read error (medium error, asc 0x11/0x00) at LBA 2617165
```

The address after `at LBA` comes from the sense data's INFORMATION field and is
the block that actually failed, not the block the read started at. Not every
drive fills that field in.

Requests spanning a bad block have to fail, since NBD has no partial answer, but
the block layer retries in smaller pieces until it isolates the defect.

For imaging, use `ddrescue` rather than `dd`. It keeps a map file, reads around
damage first and returns for the difficult parts:

```sh
sudo ddrescue -d /dev/nbd0 image.img image.map
```

`-d` bypasses the page cache. Expect slow progress near defects: the drive
spends real time on its own retries, and the command timeout allows for that.

## Erasing media

Jaz and similar media have sector headers pre-recorded, so `FORMAT UNIT` may be
refused with INCOMPATIBLE MEDIUM INSTALLED. Writing over every sector does what
the vendor utilities called a format:

```sh
sudo dd if=/dev/zero of=/dev/nbd0 bs=1M oflag=direct status=progress
```

Needs `--writable`, and destroys the partition table along with everything else.
`bs` only sets how much userspace hands over per call; the block layer splits at
`max_sectors_kb`, which `xchange-nbd` matches to the transfer ceiling so one
request becomes one SCSI command. `oflag=direct` keeps zeroed pages out of the
page cache. `blkdiscard` will not work, as the server does not advertise TRIM.

## Limitations

Most of these are caused by the adapter/chipset itself, not by the driver.

**Unexpected device responses can crash the adapter** Unfortunately there is
nothing I can do about these through code, this is purely a limitation of the
Adaptec firmware and or the Cyress chipset.

**Seven targets.** Three bits of target ID, and nothing carries a high byte.
Addresses above 7 are refused, because the firmware does not reject them cleanly
and eventually stops answering USB.

**No SCSI expanders.** Anything behind one needs a target ID of its own.

**Transfers below 64 KB.** The firmware's buffer is smaller than that; the
driver uses 62 KB for margin.

**Command parameter data must fit one bulk packet.** More than 512 bytes of
parameter list hangs the adapter until it is power cycled, though ordinary 32 KB
`WRITE` transfers are fine. `xchange format` caps a defect list at 127 addresses
and refuses more without `--force-defect-list`.

**A hung adapter ends the session.** It will not come back without a power
cycle, so after three consecutive command-block failures `xchange-nbd`
disconnects rather than answering every further request with an I/O error.

**No hot-swapping.** Changing the SCSI bus while the adapter is powered crashes
it; only removing power clears it. Adaptec's own documentation says to shut the
machine down first.

**Media changes are handled**, and are distinct from changing the bus.
`xchange-nbd` notices them two ways: unit attention with additional sense code
`0x28`, and TEST UNIT READY every three seconds on an idle device. On a change
it fails the request in flight, re-reads the capacity, flushes the block layer's
cache and resizes `/dev/nbdN`.

**Changing media while the block device is mounted is considered undefined
behaviour** It will likely corrupt the media and or crash the adapter. Pressing
Eject on the drive will not automatically unmount it.

**An empty drive is exported at size zero** rather than dropped, like `/dev/sr0`
with the tray open, and picks up a medium when one arrives. Only removable
fixed-block devices are held open this way; tapes and scanners are skipped. A
medium replaced *in place* by one with a different block size disconnects the
export, since queued offsets were computed against the old size. Eject then
insert works, because the empty state clears the geometry.

**Block devices only.** NBD carries read, write, flush and trim and no SCSI at
all, so there is no `/dev/sg` node. Tapes, scanners and optical writing would
need TCMU with `tcm_loop`. Not currently implemented.

**Multiple logical units are untested.** The addressing reaches the drive, which
answers "logical unit not supported" for units it lacks. What is unproven is a
device presenting genuinely different units.

**GParted will not list these devices**, since libparted has no notion of NBD.
udisks2-based tools list them normally, and `fdisk`, `sfdisk` and `cfdisk` work.

## Files

| Path | Purpose |
| --- | --- |
| `/etc/udev/rules.d/60-adaptec-usbxchange.rules` | Starts the firmware, then the server |
| `/etc/systemd/system/xchange-firmware.service` | Wakes a sleeping adapter |
| `/etc/systemd/system/xchange-nbd.service` | Serves the bus |
| `/etc/modprobe.d/xchange-nbd.conf` | `nbd` partition scanning |
| `/usr/local/share/adaptec/Adpusbld.sys` | Firmware source |

## Building the ublk target

Temporarily broken, needs fixing.

~~An alternative to NBD exists but is disabled, because `UBLK_CMD_ADD_DEV`
NULL-dereferences the kernel on 6.17.0-35-generic with stock parameters~~


## Further reading

* [docs/hardware-notes.md](docs/hardware-notes.md) — measurements from real
  devices
* [docs/conformance.md](docs/conformance.md) and siblings — what passes through,
  per device

## Licence

GPL-2.0-only, following the reference implementation this derives from: René
Rebe's `usbxchange` patch, itself building on work by Beier & Dauskardt IT and
on `emi26.c` by Tapio Laxström.

Adaptec's firmware is neither included nor covered by that licence.
