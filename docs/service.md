---
title: "Running the service"
permalink: /service/
---

On Debian, Ubuntu and in a Proxmox container the receiver runs as the `railspotting-receiver`
systemd service, as the `railspotting` user. The installer turns it on at boot and starts it.
In a Proxmox container, run the commands below inside it, or from the host with
`pct exec <ctid> -- ` in front.

## Start, stop, restart

| Command | What it does |
|---|---|
| `sudo systemctl start railspotting-receiver` | start it |
| `sudo systemctl stop railspotting-receiver` | stop it and let go of the dongles and the scanner |
| `sudo systemctl restart railspotting-receiver` | stop and start, for example after editing `receiver.json` by hand |
| `sudo systemctl enable railspotting-receiver` | start it at boot (the installer does this) |
| `sudo systemctl disable railspotting-receiver` | don't start it at boot; it keeps running until stopped |
| `systemctl status railspotting-receiver` | whether systemd has it running, since when, and its last log lines |

Stopping is clean: the receiver writes its final status and closes the dongles. The upload queue
is kept on disk and sent once it starts again.

The service restarts by itself 2 s after it exits or crashes (`Restart=always`). So to stop it,
use `systemctl stop`: killing the process only makes systemd start it again.

## Status and logs

| Command | What it shows |
|---|---|
| `cd /opt/railspotting-receiver && bin/railspotting-receiver status` | the receiver's own view: link state, each dongle with its centre, gain, clipping and channels, uploads, recent events |
| `sudo railspotting-console` | the same live, plus events and settings ([the console](https://github.com/railspotting/Railspotting-Receiver#the-console-linux)) |
| `journalctl -u railspotting-receiver -f` | follow the log |
| `journalctl -u railspotting-receiver -n 100` | the last 100 lines |
| `journalctl -u railspotting-receiver --since "1 hour ago"` | the log since a time |
| `http://<machine>:8080/` | the status page (read only) |

`status` reads `state/status.json`, which the running receiver keeps rewriting. It shows
`NOT RUNNING` when the receiver has stopped, or when the file is more than 30 s old.
`systemctl status` only knows about the copy systemd started (see
[Running it by hand](#running-it-by-hand)).

## Settings changes

- **From the console** (`s` to save, `e` to edit the whole `receiver.json`): the receiver checks
  the change and restarts itself with it. No `systemctl` needed. If it won't start, the old
  settings go back.
- **Editing `receiver.json` by hand:** run `sudo systemctl restart railspotting-receiver`
  afterwards.
- **Updating:** running `sudo bash packaging/linux/install.sh` again installs the new binary and
  restarts the service. Settings, identity and history are kept.

## Using a dongle for something else

Only one program can open a dongle at a time. The service holds every dongle it has a job for, so
the commands that open a dongle directly need it free first:

- `railspotting-receiver itc survey`, `itc decode`, `itc record` and `itc bursts`;
- `rtl_test`, `rtl_eeprom` and other librtlsdr tools.

There are two ways to free one:

- **Stop the whole service** with `sudo systemctl stop railspotting-receiver`, and start it again
  when you are done. Every role stops receiving in between.
- **Free one dongle and keep the rest running:** in the console's Settings tab, set that dongle's
  job to off (or `"off"` for its serial in `sdr.assign`), then pass the dongle's serial to the
  command with `--serial`. Put its job back afterwards.

`railspotting-receiver sdr-list` shows each dongle's serial and the job it has.

## Running it by hand

`railspotting-receiver run` starts the receiver in the terminal, for testing. Two things to watch:

- **Stop the service first.** Two copies fight over the dongles and the state files. A copy
  started by hand doesn't show in `systemctl status`, which then says "inactive" even though
  the dongles are busy. `pgrep -a railspotting-receiver` lists every copy. Stop a copy started by
  hand with Ctrl-C in its terminal, or `sudo kill -INT <pid>`, which stops it cleanly.
- **Run it as the service user**, from the install folder:

  ```bash
  cd /opt/railspotting-receiver
  sudo -u railspotting bin/railspotting-receiver --config receiver.json run
  ```

  Run as root, it creates files in `state/` that belong to root, and the service can't write
  them afterwards. If that has happened, give them back before starting the service:

  ```bash
  sudo chown -R railspotting:railspotting /opt/railspotting-receiver
  sudo systemctl start railspotting-receiver
  ```

## macOS

The Mac app has no service. The receiver runs inside the app:

- **Start:** open RailSpotting. Turn on Launch at Login in its menu to start it at login.
- **Pause:** Pause Receiving in the menu bar menu stops receiving and keeps the app open. Resume
  Receiving starts it again.
- **Stop:** Quit (Cmd-Q), logging out or shutting down. Queued events get a few seconds to upload
  and the dongles are let go. Closing the window doesn't stop it.
- **Logs:** `~/Library/Logs/RailSpotting/`.
