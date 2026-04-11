# bluetooth-getty: Spawn agetty on systemd for Bluetooth SPP

A small daemon that exposes a Bluetooth Serial Port Profile (SPP) and
attaches an interactive login prompt to each incoming RFCOMM connection by
handing the session off to systemd's `serial-getty@.service` template.

It replaces the legacy

```sh
sdptool add SP
rfcomm watch 0 1 /sbin/agetty rfcomm0 115200 linux
```

recipe so that `bluetoothd` can run on modern BlueZ without the deprecated
`--compat` flag.

## How it works

1. Registers an SPP profile on `org.bluez.ProfileManager1` via D-Bus.
   `bluetoothd` auto-generates the SDP record from the profile UUID and
   channel — no `sdptool`, no manual XML.
2. When a peer connects, `bluetoothd` calls `Profile1.NewConnection` with
   a connected RFCOMM socket fd.
3. The daemon issues `RFCOMMCREATEDEV` (`REUSE_DLC | RELEASE_ONHUP`) on
   the socket, which promotes the live DLC into a `/dev/rfcommN` tty
   without tearing down the connection.
4. It then calls `systemd1.Manager.StartUnit("serial-getty@rfcommN.service",
   "replace")`. systemd waits for the device unit, starts `agetty`, and
   the peer sees a normal `login:` prompt — PAM, utmp, job control, the
   whole deal, exactly like a physical serial port.
5. On peer disconnect, the kernel hangs up the tty, `RELEASE_ONHUP`
   destroys `/dev/rfcommN`, and the `BindsTo=dev-rfcommN.device` on the
   getty unit tears the instance down automatically. The daemon itself
   is not involved in teardown.

See `plan.md` for the full design rationale and references into the
BlueZ/kernel source.

## Requirements

- Linux with BlueZ ≥ 5 (`bluetoothd` running without `--compat`).
- systemd with `serial-getty@.service` available (standard on any
  systemd distro).
- Root privileges to run — the daemon needs system-bus access to
  `org.bluez` and authority to start `serial-getty@*` instances.

## Build

```sh
cargo build --release
```

The binary lands at `target/release/bluetooth-getty`.

## Install

### Debian / Ubuntu (.deb)

```sh
cargo install cargo-deb           # one-time
cargo deb                         # produces target/debian/bluetooth-getty_*.deb
sudo dpkg -i target/debian/bluetooth-getty_*.deb
sudo systemctl enable --now bluetooth-getty.service
```

The package drops the binary at `/usr/sbin/bluetooth-getty` and ships
all the ancillary config fragments into the standard vendor locations:

| File | Destination |
|---|---|
| `contrib/systemd/bluetooth-getty.service` | `/lib/systemd/system/bluetooth-getty.service` |
| `contrib/systemd/bluetooth-getty-session@.service` | `/lib/systemd/system/bluetooth-getty-session@.service` |
| `contrib/udev/99-bluetooth-getty.rules` | `/lib/udev/rules.d/99-bluetooth-getty.rules` |
| `contrib/dbus/jp.sorah.BluetoothGetty.conf` | `/usr/share/dbus-1/system.d/jp.sorah.BluetoothGetty.conf` |

`cargo-deb` wires up `systemctl daemon-reload` automatically on
install/upgrade and the package's own postinst runs `udevadm control
--reload-rules`. The daemon is intentionally **not** auto-enabled on
install — review the D-Bus policy and udev rule first, then run
`systemctl enable --now bluetooth-getty.service` yourself.

### Manual install (non-Debian)

```sh
sudo install -m 0755 target/release/bluetooth-getty /usr/local/sbin/
sudo install -m 0644 contrib/systemd/bluetooth-getty.service /etc/systemd/system/
sudo install -m 0644 contrib/systemd/bluetooth-getty-session@.service /etc/systemd/system/
sudo install -m 0644 contrib/dbus/jp.sorah.BluetoothGetty.conf /etc/dbus-1/system.d/
sudo install -m 0644 contrib/udev/99-bluetooth-getty.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo systemctl reload dbus
sudo systemctl daemon-reload
sudo systemctl enable --now bluetooth-getty.service
```

If you install the binary somewhere other than `/usr/sbin`, update
`ExecStart=` in `bluetooth-getty.service` accordingly.

### What gets installed

Two systemd unit files are shipped:

- `bluetooth-getty.service` — the daemon itself.
- `bluetooth-getty-session@.service` — a per-connection template
  instantiated by the daemon (via `Manager.StartUnit`) for each
  incoming connection, used in place of `serial-getty@.service`. It's
  a near-copy of the upstream serial-getty template with
  `TTYVHangup=yes` removed, because that directive calls `vhangup()`
  before exec'ing agetty, which on an rfcomm tty puts the driver into
  a state where agetty can't open it.

Two pieces of system config are also required:

- **D-Bus policy** (`contrib/dbus/jp.sorah.BluetoothGetty.conf`).
  Without it the system bus rejects name ownership with
  `AccessDenied: Connection is not allowed to own the service
  "jp.sorah.BluetoothGetty"`. It also permits `bluetoothd` to invoke
  `Profile1` methods on our object.
- **udev rule** (`contrib/udev/99-bluetooth-getty.rules`). Sets
  `ENV{ID_MM_DEVICE_IGNORE}="1"` on rfcomm ttys so ModemManager doesn't
  grab them and probe with AT commands when a peer connects.

## CLI flags

| Flag                         | Default                                   | Notes                                                |
|------------------------------|-------------------------------------------|------------------------------------------------------|
| `--name`                     | `"Bluetooth getty"`                       | Human-readable name in the SDP record.               |
| `--uuid`                     | `00001101-0000-1000-8000-00805f9b34fb`    | SPP UUID. Override to reuse the daemon for another RFCOMM profile. |
| `--channel`                  | `1`                                       | RFCOMM channel advertised in SDP.                    |
| `--object-path`              | `/jp/sorah/BluetoothGetty/spp`            | D-Bus path for our `Profile1` object.                |
| `--unit-template`            | `serial-getty@`                           | `rfcommN.service` is appended per connection.        |
| `--bus-name`                 | `jp.sorah.BluetoothGetty`                 | Well-known name requested on the system bus.         |
| `--require-authentication`   | off                                       | Ask bluetoothd to require pairing before accepting the channel. |
| `--require-authorization`    | off                                       | Ask bluetoothd to require authorization before accepting the channel. |
