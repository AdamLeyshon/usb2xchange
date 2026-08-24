#!/usr/bin/env bash
# Bring up a SCSI target behind the Adaptec USB2Xchange as a block device.
#
#   scripts/attach.sh [target] [--writable]
#
# Starts the adapter's firmware if it is still asleep, then serves the target
# over NBD. Read-only unless --writable is passed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$here/target/release"
driver="${XCHANGE_FIRMWARE:-/usr/local/share/adaptec/Adpusbld.sys}"
dev="${XCHANGE_NBD_DEV:-/dev/nbd0}"

target="${1:-1}"
shift || true

if lsusb -d 03f3:2002 >/dev/null 2>&1; then
    echo "adapter is asleep; starting its firmware"
    "$bin/xchange-fw" load --firmware "$driver"
elif lsusb -d 03f3:2003 >/dev/null 2>&1; then
    echo "adapter already running"
else
    echo "no Adaptec adapter found on the bus" >&2
    exit 1
fi

echo
"$bin/xchange" scan
echo

# max_part must be set when the module loads, or the kernel never scans the
# partition table and you get /dev/nbd0 with no /dev/nbd0p1 behind it.
if ! lsmod | grep -qw nbd; then
    sudo modprobe nbd max_part=16
fi

exec sudo "$bin/xchange-nbd" --target "$target" --device "$dev" "$@"
