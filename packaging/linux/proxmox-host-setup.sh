#!/usr/bin/env bash
# RailSpotting receiver: prepare a Proxmox VE host so an unprivileged LXC container
# can use the RTL-SDR dongles (and optionally the SR30C scanner cable).
#
#   usage: proxmox-host-setup.sh <ctid> [--scanner]
#
# Run as root on the Proxmox host. Safe to run again: nothing is added twice.
#
# For tests, these can point somewhere else:
#   RS_LXC_DIR (default /etc/pve/lxc), RS_MODPROBE_DIR (/etc/modprobe.d),
#   RS_UDEV_DIR (/etc/udev/rules.d), RS_NO_SYSTEM=1 skips commands that change the host.
set -euo pipefail

usage() { echo "usage: $0 <ctid> [--scanner]" >&2; exit 2; }

CTID="${1:-}"
[[ "$CTID" =~ ^[0-9]+$ ]] || usage
SCANNER=0
[[ "${2:-}" == "--scanner" ]] && SCANNER=1
[[ $# -le 2 ]] || usage

LXC_DIR="${RS_LXC_DIR:-/etc/pve/lxc}"
MODPROBE_DIR="${RS_MODPROBE_DIR:-/etc/modprobe.d}"
UDEV_DIR="${RS_UDEV_DIR:-/etc/udev/rules.d}"
NO_SYSTEM="${RS_NO_SYSTEM:-0}"
CONF="$LXC_DIR/$CTID.conf"

run() {
    if [[ "$NO_SYSTEM" == "1" ]]; then echo "  (skipped: $*)"; else "$@"; fi
}

if [[ "$NO_SYSTEM" != "1" && $EUID -ne 0 ]]; then
    echo "run this as root on the Proxmox host" >&2
    exit 1
fi
if [[ ! -f "$CONF" ]]; then
    echo "container $CTID not found ($CONF). Create it first (see packaging/linux/lxc-setup.md)." >&2
    exit 1
fi

echo "== 1. Blacklisting the DVB-T TV drivers that grab RTL2832U dongles"
BLACKLIST="$MODPROBE_DIR/blacklist-rtlsdr.conf"
mkdir -p "$MODPROBE_DIR"
NEW_BLACKLIST=$'# RailSpotting: keep the TV drivers off the RTL-SDR dongles\nblacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n'
if [[ ! -f "$BLACKLIST" ]] || [[ "$(cat "$BLACKLIST")"$'\n' != "$NEW_BLACKLIST" ]]; then
    printf '%s' "$NEW_BLACKLIST" > "$BLACKLIST"
    echo "  wrote $BLACKLIST"
    run update-initramfs -u
else
    echo "  $BLACKLIST already in place"
fi
for mod in rtl2832_sdr dvb_usb_rtl28xxu rtl2832 rtl2830; do
    if [[ "$NO_SYSTEM" != "1" ]] && lsmod | grep -q "^$mod "; then
        rmmod "$mod" && echo "  unloaded $mod" || echo "  could not unload $mod (unplug the dongles or reboot the host)"
    fi
done

echo "== 2. udev rules so container users can open the devices"
RULES="$UDEV_DIR/99-railspotting.rules"
mkdir -p "$UDEV_DIR"
NEW_RULES='# RailSpotting receiver: RTL-SDR dongles and the SR30C CP2104 cable
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2832", MODE="0666"
SUBSYSTEM=="tty", ATTRS{idVendor}=="10c4", ATTRS{idProduct}=="ea60", MODE="0666", SYMLINK+="railspotting-scanner"'
if [[ ! -f "$RULES" ]] || [[ "$(cat "$RULES")" != "$NEW_RULES" ]]; then
    printf '%s\n' "$NEW_RULES" > "$RULES"
    echo "  wrote $RULES"
else
    echo "  $RULES already in place"
fi
run udevadm control --reload
run udevadm trigger

echo "== 3. Container $CTID configuration"
# Lines go in the main section, before any [snapshot] section.
add_line() {
    if sed '/^\[/,$d' "$CONF" | grep -qxF "$1"; then
        echo "  present: $1"
        return
    fi
    if grep -q '^\[' "$CONF"; then
        local tmp
        tmp=$(mktemp)
        awk -v line="$1" '!done && /^\[/ { print line; done = 1 } { print }' "$CONF" > "$tmp"
        cat "$tmp" > "$CONF"
        rm -f "$tmp"
    else
        echo "$1" >> "$CONF"
    fi
    echo "  added:   $1"
}
add_line "lxc.cgroup2.devices.allow: c 189:* rwm"
add_line "lxc.mount.entry: /dev/bus/usb dev/bus/usb none bind,optional,create=dir"
if [[ $SCANNER -eq 1 ]]; then
    add_line "lxc.cgroup2.devices.allow: c 188:* rwm"
    add_line "lxc.mount.entry: /dev/railspotting-scanner dev/ttyUSB0 none bind,optional,create=file"
fi

echo "== 4. Checks"
if ! grep -qE '^unprivileged:[[:space:]]*1' "$CONF"; then
    echo "  WARNING: container $CTID is privileged. The receiver needs no privileges;"
    echo "           an unprivileged container is recommended."
fi
if command -v lsusb >/dev/null 2>&1; then
    found=$(lsusb | grep -iE 'ID 0bda:283[28]' || true)
    if [[ -n "$found" ]]; then echo "  RTL-SDR dongles:"; echo "$found" | sed 's/^/    /'
    else echo "  no RTL-SDR dongles visible to the host (check the USB cables or hub)"; fi
    scanner=$(lsusb | grep -i 'ID 10c4:ea60' || true)
    if [[ -n "$scanner" ]]; then echo "  scanner cable:"; echo "$scanner" | sed 's/^/    /'
    elif [[ $SCANNER -eq 1 ]]; then echo "  no SR30C cable (10c4:ea60) visible"; fi
fi
if [[ -d /dev/bus/usb ]]; then
    total=$(find /dev/bus/usb -type c | wc -l)
    open=$(find /dev/bus/usb -type c -perm -o+rw | wc -l)
    echo "  $open of $total USB device nodes are world-accessible (dongles need to be among them)"
fi

echo "== 5. Restarting container $CTID"
if [[ "$NO_SYSTEM" == "1" ]]; then
    echo "  (skipped: pct reboot $CTID)"
elif pct status "$CTID" | grep -q running; then
    pct reboot "$CTID"
else
    echo "  container is stopped; start it with: pct start $CTID"
fi
echo "done. Inside the container, build and run packaging/linux/install.sh (see lxc-setup.md)."
