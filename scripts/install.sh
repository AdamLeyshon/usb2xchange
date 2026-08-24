#!/usr/bin/env bash
# Install the tools, udev rules and systemd units.
#
#   sudo scripts/install.sh [path/to/Adpusbld.sys]
#
# The firmware is Adaptec's and not ours to ship, so point this at the driver
# on your own installation CD.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
driver="${1:-}"

if [ "$EUID" -ne 0 ]; then
    echo "needs root: sudo $0 $*" >&2
    exit 1
fi

if [ -z "$driver" ]; then
    echo "usage: $0 /path/to/Adpusbld.sys" >&2
    echo >&2
    echo "The firmware is Adaptec's and is not shipped with this project." >&2
    echo "Adpusbld.sys is on the driver CD, or inside usb2xchg_win_drv_v200.exe." >&2
    exit 1
fi

if [ ! -f "$driver" ]; then
    echo "no such file: $driver" >&2
    exit 1
fi

echo "building"
sudo -u "${SUDO_USER:-$USER}" cargo build --release --manifest-path "$here/Cargo.toml"

echo "installing binaries"
install -m 0755 "$here/target/release/xchange" /usr/local/bin/
install -m 0755 "$here/target/release/xchange-fw" /usr/local/bin/
install -m 0755 "$here/target/release/xchange-nbd" /usr/local/bin/
install -m 0755 "$here/target/release/xchange-conform" /usr/local/bin/

echo "installing firmware source"
install -d /usr/local/share/adaptec
install -m 0644 "$driver" /usr/local/share/adaptec/Adpusbld.sys

echo "installing udev rules and systemd units"
install -m 0644 "$here/udev/60-adaptec-usbxchange.rules" /etc/udev/rules.d/
install -m 0644 "$here/systemd/xchange-firmware.service" /etc/systemd/system/
install -m 0644 "$here/systemd/xchange-nbd.service" /etc/systemd/system/
install -m 0644 "$here/modprobe.d/xchange-nbd.conf" /etc/modprobe.d/

systemctl daemon-reload
udevadm control --reload-rules

echo
echo "done. Unplug and replug the adapter to bring it up, or start it now with:"
echo "  systemctl start xchange-nbd.service"
echo
echo "Watch it with:  journalctl -fu xchange-nbd.service"
