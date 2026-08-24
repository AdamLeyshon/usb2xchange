# Hardware notes

Observations from real devices, kept separate from the reference documentation.
Three devices have been through the conformance suite: an Iomega Jaz 2GB, a
Toshiba CD-ROM XM-5201TA and an IBM WDS-L80 hard disk.

## What the hardware turned out to be

On first power-up, the adapter identifies as `03f3:2002` because its 8051 is
held in reset, not because its RAM is empty. Writing `0x00` to CPUCS starts it,
after which it re-enumerates as `03f3:2003`. The firmware download is required
for units with a blank EEPROM.

The interface is vendor-specific, not mass storage. `bInterfaceClass` is 255
with a single bulk pair on EP `0x02` OUT and EP `0x86` IN, so `usb-storage`
never binds. That is why the 2005 patch had to force the class through
`UNUSUAL_DEV`.

The data and status phases share the bulk IN endpoint, and the adapter skips the
data phase entirely when a command fails. This is in none of the reference
material. A read posted for data can return the 13-byte status wrapper instead;
code that does not expect it consumes the status as data, then blocks for a
status already gone, and the leftover desynchronises the *next* command as an
inexplicable tag mismatch. `bot.rs` checks every data-phase read for a status
signature and drains the endpoint when a command is abandoned.

## How the adapter addresses the bus

It is a host bus adapter, arbitrating as the initiator and bridging to USB. Its
addressing is split across two places.

**Targets** ride in the command block wrapper's LUN byte, where standard
  Bulk-Only Transport puts a logical unit number. The `US_FL_SCM_MULT_TARG`
  convention packs `id << 4 | lun` into that byte; this firmware does not, and
  spends the whole byte on the target.

**Logical units** ride in the CDB, byte 1 bits 5-7, where SCSI-1 and SCSI-2 put
  them. The adapter passes the CDB through untouched, so this still works.

Probing an Iomega Jaz settled it:

```
address 0x01, CDB LUN sweep:
  LUN 0: iomega   jaz 2GB          E.17  disk, removable
  LUN 1: no such logical unit (qualifier 011b, type 0x1f)
  ...
  LUN 7: no such logical unit (qualifier 011b, type 0x1f)
```

A drive that never saw the field would have answered as itself at all eight.

```
$ xchange capacity 1 --lun 3
error: target 1 LUN 3 reported check condition: illegal request (key 0x05, asc 0x25, ascq 0x00)
```

ASC 0x25 is LOGICAL UNIT NOT SUPPORTED, from the drive rather than the adapter.

### Logical unit support is experimental

`--lun` is threaded through `inquiry`, `ready`, `capacity`, `read` and
`xchange-nbd`, and `xchange scan --luns` walks every unit on every target.

**None of it has been exercised against a multi-unit device.** The addressing
demonstrably arrives; what is unverified is a device returning genuinely
different units. That needs a CD changer (one disc per logical unit) or a tape
autoloader in combined mode (drive at unit 0, medium changer at unit 1).
Duplicator towers will not do, being independent targets at unit 0. Testing
needs nothing but INQUIRY, so a changer with a dead mechanism would settle it.
Reports welcome.

### Bus width

Three bits of target ID, so 0 to 7. The wrapper's address byte is eight bits
wide but the extra five encode nothing, and no part of the protocol carries a
high byte. Addresses above 7 are unrepresentable and the code correctly refuses
them.

Attemping to do so causes the adapter internals to get corrupted rather than it
rejecting the value:

| address   | behaviour                                          |
| --------- | -------------------------------------------------- |
| 0x00-0x08 | answers normally, `0x8A` where nothing is attached |
| 0x09-0x0D | data phase times out                               |
| 0x0E-0x0F | command blocks refused outright                    |
| beyond    | drops off USB entirely; needs a power cycle        |

A bad address evidently gets far enough in to crash it. In fairness, the only
thing that ever spoke to this adapter was probably only Cyress/Adaptec's own
driver.

## What the adapter actually supports

`xchange-conform` sorts refusals by _who_ refused. A drive replying "invalid
opcode" means the adapter carried the command there and the refusal back
faithfully.

An adapter fault means no device will ever make it work.

**Command blocks of every length are carried.**

`READ(12)` executes on the CD-ROM, and `READ(16)` reaches both drives to be
refused as an invalid opcode, so `bCBWCBLength` is honoured and SCSI-3 commands
reach a device implementing them.

**2048-byte blocks work.**

The CD-ROM reports 296118 blocks of 2048 and reads correctly at that size.

**The adapter can be crashed by a device's response, not by a command.**

Opcode `0xFF` stops it answering with the Toshiba attached and needs a USB
reset; the same opcode against the Jaz and the IBM returns an ordinary
invalid-opcode refusal. Which device you have decides whether you see it.

**The status byte is not the one that the Bulk-Only Transport documents.**

Beyond `0x8A` for an unused target, I have produced `0x92` when the wrapper's
declared transfer length disagrees with what the device sends, and `0x04` from
the Toshiba for `PRE-FETCH(10)`. These previously undocumented codes are now
listed in `bot.rs`.

### Across the three devices tested

| command                                        | Jaz 2GB | CD-ROM | IBM WDS-L80 |
| ---------------------------------------------- | ------- | ------ | ----------- |
| `TEST UNIT READY`, `REQUEST SENSE`, `INQUIRY`  | yes     | yes    | yes         |
| `READ(6)`, `READ(10)`, `READ CAPACITY(10)`     | yes     | yes    | yes         |
| `MODE SENSE(6)`, `VERIFY(10)`                  | yes     | yes    | yes         |
| `READ DEFECT DATA(10)`, `READ BUFFER`          | yes     | no     | yes         |
| `SYNCHRONIZE CACHE(10)`                        | yes     | no     | no          |
| `RECEIVE DIAGNOSTIC RESULTS`                   | no      | yes    | no          |
| `READ(12)`                                     | no      | yes    | no          |
| `READ TOC`, `READ SUB-CHANNEL`                 | n/a     | yes    | n/a         |
| `MODE SENSE(10)`, `LOG SENSE`                  | no      | no     | no          |
| `READ(16)`, `READ CAPACITY(16)`, `REPORT LUNS` | no      | no     | no          |
| `REZERO UNIT`, `SEEK(6)`, `SEEK(10)`           | —       | —      | yes         |
| `RESERVE(6)`, `RELEASE(6)`, `START STOP UNIT`  | —       | —      | yes         |
| `FORMAT UNIT`                                  | yes     | —      | —           |

State-changing commands were only run against the IBM, which had nothing on it.
A dash means not tried rather than not supported. Every "no" is the drive
refusing, not the adapter failing.

Residue reporting is right across three reply lengths: the Jaz returns 136 bytes
of INQUIRY data, the CD-ROM 96 and the IBM 54, and asking for 255 gives residues
of 119, 159 and 201.

Reports are in `docs/conformance.md`, `docs/conformance-cdrom.md` and
`docs/conformance-harddisk.md`, regenerated with:

```sh
xchange-conform --transfer-limit --markdown docs/conformance.md
```

`--stateful` and `--destructive` add commands that change state or destroy data.
Neither runs by default. Progress is journalled, so a run ending in a power
cycle resumes rather than restarts.

### Transfer size

The reference notes say the buffer is under 64 KB and that 64 KB crashes it,
without saying where the wall stands. The search walks upward and checks each
size against a baseline read one block at a time, so firmware returning wrong
bytes while reporting success cannot pass:

```
  124 blocks ( 63488 bytes): ok, contents match
  126 blocks ( 64512 bytes): ok, contents match
  127 blocks ( 65024 bytes): ok, contents match
```

65024 is verified correct, one 512-byte block short of the documented crash
point. The same figure comes back from the Jaz and the IBM, drives a decade
apart, so the ceiling belongs to the adapter's buffer rather than any device.

The driver uses 62 KB, taking two blocks of margin, costs nothing measurable:

```sh
xchange bench --target 0                    # 0.78 MB/s at 62 KiB
xchange bench --target 0 --ceiling 65024    # 0.79 MB/s at 63 KiB
```

The fixed per-command cost varies by more than that between runs of the same
size. `--ceiling` lifts the driver's limit for one run, to re-check this on
other hardware.

## What Adaptec's own readme says

*"Narrow (8 bit) SCSI bus, synchronous and asynchronous transfer modes,
  disconnect disabled."** The first half confirms the eight-ID address space.
  The second matters more: with disconnect disabled the adapter holds the bus
  for the whole of every command, a plausible reason a device appearing or
  vanishing mid-transaction leaves the firmware stuck.

**"multiple LUNs supported"**, corroborating the CDB LUN probe.

**"Connect up to four SCSI devices under Windows 2000 and Windows XP"**, but
**"up to seven under Windows 98SE and Windows Me"**. Seven is what a narrow bus
  allows with the initiator at ID 7, so the four is a limit of Adaptec's own
  driver. This driver has no such limit.

**"SCSI devices supported include Removable drives, Hard disk drives,
  Magneto-optical (MO) drives, CD-ROM drives, CD-R/RW drives, DVD-RAM drives,
  tape drives and Scanners."** So the firmware is not block-only, as was guessed
  here earlier. Tape and scanner command sets should pass through, though that
  is untested for want of hardware.

**"receives its electrical power from the termination power of the attached SCSI
  devices. If this power is not available, the adapter automatically switches to
  receive power from the USB bus."** The adapter's power source can change
  underneath it.

### Hot-swapping

Swapping a device while the adapter stays powered crashes it: every target times
out on the data phase, and shortly afterwards it stops answering even the vendor
initialisation requests. A USB reset does not clear it, because the USB side is
not what is stuck.

The user's guide covers this under Hot-Plugging:

> The USB2Xchange adapter is hot-pluggable, which means you can connect and
> disconnect it from a live system. However, SCSI devices are not hot-pluggable.
> You must shut down your device **and your computer** before disconnecting a
> SCSI device from the USB2Xchange adapter.

And again in Troubleshooting, describing this exact failure, where every remedy
offered amounts to unplugging the adapter.

**External DC power does not help.** A brown-out would restart the adapter and
  it would come back at `03f3:2002` wanting firmware. It does not: it stays at
  `03f3:2003` with firmware running and only its SCSI state ruined, so it never
  lost power. The manual offers the DC jack for insufficient power, not for
  surviving a bus change. Power the adapter down before changing the bus.

## Throughput

```sh
xchange bench --target 0
```

Reads sequentially at a range of transfer sizes, marching forward through the
medium so the drive's cache cannot flatter the result. The transport carries one
command at a time, so every transfer pays a fixed round-trip cost; sampling
several sizes separates that from the rate data actually moves at.

Iomega Jaz 2GB:

```
    transfer   commands   elapsed    throughput   per command
       512 B       2651     3.00s       0.43 MB/s       1.13 ms
       8 KiB       1274     3.00s       3.32 MB/s       2.36 ms
      32 KiB        499     3.00s       5.19 MB/s       6.02 ms
      62 KiB        281     3.00s       5.66 MB/s      10.69 ms

  fixed cost per command : 1.10 ms
  rate once moving       : 6.32 MB/s
  62 KiB beats 512 B by 13.1x
```

5.66 MB/s is close to what a Jaz 2GB manages natively, so the adapter costs
little here and the narrow bus's 10 MB/s is the next thing that would limit.

IBM WDS-L80, an early-90s 80 MB drive:

```
    transfer   commands   elapsed    throughput   per command
       512 B        475     2.00s       0.12 MB/s       4.21 ms
       8 KiB        136     2.00s       0.53 MB/s      14.73 ms
      32 KiB         42     2.01s       0.65 MB/s      47.80 ms
      62 KiB         27     2.07s       0.79 MB/s      76.66 ms

  fixed cost per command : 4.57 ms
  rate once moving       : 0.81 MB/s
  62 KiB beats 512 B by 6.8x
```

Comparing the two is what makes the fixed cost meaningful: 1.10 ms on the Jaz
against 4.6 ms on the IBM. Because it varies so much between drives, most of the
IBM's 4.6 ms is that drive's own latency; the adapter's share is under 1.1 ms.
Either way, large transfers pay the cost once instead of many times.

### GParted will not show these devices

GParted and `parted` both go through libparted, which has no idea what an NBD
device is: `loop`, `nvme`, `pmem` and `virtblk` all appear by name in
`libparted.so`, and `nbd` appears nowhere.

```sh
$ strings /usr/lib/x86_64-linux-gnu/libparted.so.2 | grep -c nbd
0
```

It still probes the device, which is why you see the drive working while GParted
scans: libparted walks `/proc/partitions`, opens each entry and reads its
geometry, then discards the types it cannot place.

Anything built on udisks2 enumerates through udev instead and lists the device
normally. Mounting, `fsck` and `dd` are unaffected. To partition one, use a
util-linux tool: `fdisk`, `sfdisk` and `cfdisk` are built on libfdisk and work
on any block device.

### Why not ublk

`ublk` is the better answer on paper, being the modern interface and skipping
the socket round trip. `xchange-ublk` implements it but is behind the `ublk`
feature and off by default, because `UBLK_CMD_ADD_DEV` NULL-dereferences the
kernel on 6.17.0-35-generic before the device is created. It reproduces with
stock parameters, so no tuning avoids it. To build the backend against a working
kernel:

```sh
cargo build --release -p xchange-ublk --features ublk
```

That also keeps `libublk`, and the `libclang-dev` its bindgen step needs, out of
a default build.
