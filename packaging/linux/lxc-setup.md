# RailSpotting receiver on Proxmox VE (LXC)

This guide takes a fresh Proxmox VE node with the radios plugged in to a receiver that is
approved and reporting. Allow about 45 minutes, most of it the first build.

```
Proxmox VE host ── USB ── NESDR (VHF) ── long antenna
                    ├──── NESDR (UHF) ── short antenna
                    └──── SR30C (optional, CP2104 cable)
  └── LXC "railspotting": unprivileged Debian 12, 2 cores, 2 GB RAM, 8 GB disk, onboot
        └── railspotting-receiver.service (systemd, user "railspotting")
              └── HTTPS → api.railspotting.com
```

## 1. Create the container

| Setting | Value |
|---|---|
| Template | Debian 12 standard (`debian-12-standard_..._amd64.tar.zst`) |
| Unprivileged | yes |
| Hostname | `railspotting` |
| Cores | 2 |
| Memory | 2048 MB (swap 512 MB is fine) |
| Root disk | 8 GB |
| Network | `eth0` on `vmbr0`, DHCP (or a static address) |
| Start at boot | yes |

Download the template once (**Datacenter > node > local > CT Templates > Templates**), or run
`pveam update && pveam available | grep debian-12` and then `pveam download local <name>`. Then
create the container. Pick a free ID, for example 120:

```
pct create 120 local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst \
  --hostname railspotting --unprivileged 1 --cores 2 --memory 2048 \
  --rootfs local-lvm:8 --net0 name=eth0,bridge=vmbr0,ip=dhcp --onboot 1
```

Use the template file name `pveam` actually downloaded.

### Keep the container's MAC address

Proxmox gives `eth0` a MAC address from its own prefix (`BC:24:11:...`). The receiver turns that MAC
into its device ID (`rs-bc2411xxxxxx`), and the headend knows the receiver by it. **Never
regenerate the MAC or recreate `net0` without copying the old `hwaddr=` value back.** A new MAC, or
moving the receiver to a new container, means a new device that needs approval again.

The receiver signs requests, which needs the clock within 300 s of the server. A container uses
the host's clock, so the **host** must keep time. Proxmox ships `chrony`; check it with
`chronyc tracking` on the host.

## 2. Run the host script, then reboot the container

Copy `packaging/linux/proxmox-host-setup.sh` to the host (for example with `curl -O https://raw.githubusercontent.com/railspotting/Railspotting-Receiver/main/packaging/linux/proxmox-host-setup.sh` once the repository is public, or `scp` while it is private) and run it as root:

```
bash proxmox-host-setup.sh 120             # SDR dongles only
bash proxmox-host-setup.sh 120 --scanner   # also pass the SR30C cable
```

It blacklists the DVB-T TV drivers that otherwise claim the dongles, installs a udev rule that lets
unprivileged container users open them, binds the whole `/dev/bus/usb` into the container (so
replugging into another port needs no change), and reboots the container. It is safe to run
again; nothing is added twice. If it says the TV driver could not be unloaded, unplug and replug
the dongles or reboot the host once.

## 3. Build the receiver in the container

The receiver is one binary built from this repository. Inside the container (`pct enter 120`):

```
apt-get install -y git curl build-essential cmake clang pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
git clone https://github.com/railspotting/Railspotting-Receiver.git /root/src
cd /root/src
cargo build --release -p railspotting-receiver --features speech
```

The first build takes 10 to 20 minutes on a small CPU (whisper.cpp is most of it). On a container
with 2 GB of RAM, `CARGO_BUILD_JOBS=2` keeps the build from running out of memory.

## 4. Install, then connect the antennas

```
bash /root/src/packaging/linux/install.sh
```

Inside the container the command line finds `/opt/railspotting-receiver/receiver.json` by
itself. The installer adds the packages (`rtl-sdr`, `librtlsdr0`), creates the `railspotting` user,
installs the binary in `/opt/railspotting-receiver/bin`, creates `receiver.json` from the example,
downloads the speech model (about 265 MB), starts the service and lists the dongles it can see:

```
#0  serial 30002222   Nooelec NESDR SMArt v5  ->  role vhf (automatic): connect the VHF antenna (the longer one, ...)
#1  serial 30001111   Nooelec NESDR SMArt v5  ->  role eot (automatic): connect the UHF antenna (the shorter one, ...)
```

The service gives each dongle a role by itself, so nothing needs editing. Your serial numbers will
differ, and which dongle gets which role is arbitrary. The roles are saved in
`state/sdr_roles.json` and stay the same across restarts.

**Connect the antennas to match.** To tell the physical dongles apart, unplug one dongle: within a
few seconds its card on the status page (`http://<container IP>:8080/`) shows "not connected",
and `railspotting-receiver status` shows `NOT CONNECTED`. Connect the long VHF antenna to the
`vhf` dongle and the short UHF antenna to the `eot` dongle. Swapping the antennas is all it
takes; nothing on the receiver changes.

To pin the roles instead, put the serial numbers into `sdr.devices` in
`/opt/railspotting-receiver/receiver.json` (in place of `"auto"`) and restart the service.

## 5. Link the receiver to your account

The service asks the headend for a link code by itself and shows it every 30 s while it waits:

```
pct exec 120 -- journalctl -u railspotting-receiver -n 20
...
INFO LINK CODE WDJB-MJHT  (open https://railspotting.com/link, sign in and enter the code to add this receiver; expires in 15 min)
```

The status page (`http://<container IP>:8080/`) shows the same code with a QR code. Open the
address, sign in, enter the code and choose **Add as a new receiver** (or **Replace the hardware**
of an existing one). Then set its location. Within a few seconds the log shows
`receiver linked as rs-...` and `channel plan version N applied`. A code that expires is replaced
automatically.

## 6. Check that it works

```
pct exec 120 -- journalctl -u railspotting-receiver -f
```

Within 5 minutes you should see a line like:

```
INFO signal: NOAA WXJ21 Paris Mountain +36 dB (min +34, max +38 over 300 s); vhf floor -21..-19 dB; eot floor -24..-24 dB
```

Above +20 dB is good. Then run the transcription test:

```
pct exec 120 -- /opt/railspotting-receiver/bin/railspotting-receiver test-transcription
transcription test: "This is the NOAA weather radio, all hazards. WXJ-21 ..." (81%)
```

70% or better with a recognizable forecast means the VHF path is healthy.

## Updating

Pull and build the new code, then run the installer again:

```
pct exec 120 -- bash -c '. "$HOME/.cargo/env" && cd /root/src && git pull && \
    cargo build --release -p railspotting-receiver --features speech && bash packaging/linux/install.sh'
```

Settings (`receiver.json`), identity and key (`state/`, `credentials.json`), recordings and the
speech model are kept.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `no RTL-SDR dongles found` | not passed through | run the host script, then `pct reboot` |
| `usb_claim_interface error -6` | DVB driver loaded | check the blacklist or reboot the host |
| `LIBUSB_ERROR_ACCESS` | udev rule missing | rule plus replug |
| `cannot load the RTL-SDR driver` | `librtlsdr0` missing | install it |
| short reads | USB power or bandwidth | powered hub |
| overruns climbing | CPU starved | more cores, or `"transcription": false` |
| scanner not found | not passed | `--scanner` |
| upload handshake timeouts | API not reachable | DNS or routing from the container |

The API host may resolve to a private address on the owner's LAN (split DNS). The container must
then be on that network, or have a route to it. Check from inside the container with
`getent hosts api.railspotting.com` and `curl -sI https://api.railspotting.com/`.

Useful checks inside the container:

- `ls -l /dev/bus/usb/*/*`: the dongles' nodes should be `crw-rw-rw-`.
- `rtl_test -t`: opens the first dongle (stop the service first, only one program can use it).
- `railspotting-receiver status`: per-dongle overruns, short reads, stalls and clipping. Clipping
  above 0.1% means the gain is too high; an FM broadcast band-stop filter helps the VHF dongle.
