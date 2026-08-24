# Contributing

I would be very grateful for the donation of obscure SCSI devices to aid in
testing and then for preservation. Though this does not guarantee support since
it maybe a bug in the firmware beyond my control that prevents it from working.

## Hardware reports

`xchange-conform` separates refusals that come from the drive from failures that
come from the adapter, which is the distinction that matters:

```sh
xchange scan
xchange-conform --target N --markdown report.md
```

Attach the markdown to an issue along with the device's make and model. Several
questions are open only for want of the right device:

* **Multiple logical units.** The addressing is confirmed to reach the drive,
  which correctly answers "logical unit not supported" for units it lacks. No
  device presenting genuinely different units has been tested. A CD changer puts
  one disc per logical unit on a single target; a tape autoloader in combined
  mode puts the drive at unit 0 and the changer at unit 1. Testing needs nothing
  but INQUIRY, so a changer with a dead mechanism would answer it.
* **Non-block devices.** Adaptec's documentation lists tape drives and scanners
  as supported. These are not yet implemented as I have no devices to test with.
* **SCSI-3 commands.** `MODE SENSE(10)`, `LOG SENSE`, `READ(16)` and `REPORT
  LUNS` are not supported by the devices tested so far. The adapter appears to
  forward them properly, I am awaiting delivery of the correct cable
  (HD50->HD68) to test with my SCSI-3 enclosure.

## Before opening a pull request

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

Set `XCHANGE_TEST_DRIVER` to a copy of `Adpusbld.sys` to run the tests that
check firmware extraction against a known digest. They skip without it.

## What is deliberately absent

Firmware is not shipped, and pull requests adding it will not be merged. It is
Adaptec's, and the tools read it out of the driver you already have.

Addresses above target 7 are refused rather than attempted. The bus is narrow,
so there is nothing up there, and the firmware does not reject them cleanly:
probing past 7 eventually crashes the adapter.
