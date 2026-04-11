# bluetooth-getty — implementation plan

A small Rust daemon that exposes a Bluetooth Serial Port Profile (SPP) and
attaches an interactive login prompt to each incoming RFCOMM connection, using
systemd's `serial-getty@.service` template. Replaces the legacy
`sdptool add SP` + `rfcomm watch 0 1 /sbin/agetty rfcomm0 115200 linux`
recipe so that `bluetoothd` can run without the deprecated `--compat` flag.

## Goal

When a remote Bluetooth device connects to this host over RFCOMM on the SPP
UUID, the user at the remote end should see a normal `login:` prompt and be
able to log in to a real pty-backed session (PAM, utmp, job control, etc.),
exactly as if they had connected to a physical serial port configured with
`serial-getty@ttyS0.service`.

No `sdptool`, no `rfcomm` tool, no `bluetoothd --compat`. Pure D-Bus on the
modern `org.bluez.ProfileManager1` / `org.bluez.Profile1` API, plus one
kernel ioctl to hand the connection off to systemd.

## Background — why this shape

BlueZ's modern, non-compat path for registering a service over SDP is
`org.bluez.ProfileManager1.RegisterProfile(object, uuid, options)`. When a
peer connects to that profile, `bluetoothd` invokes
`org.bluez.Profile1.NewConnection(device, fd, properties)` on the caller's
object, handing over an already-connected kernel `BTPROTO_RFCOMM` socket as a
UNIX fd. The receiving process owns the fd.

systemd's `serial-getty@%i.service` is hard-wired to a real tty device
(`BindsTo=dev-%i.device`) — it runs `agetty` on `/dev/%i`, not on an injected
stdin fd. So the daemon cannot just hand the fd to systemd. Instead, it uses
the kernel RFCOMM TTY layer to promote the live RFCOMM socket into a
`/dev/rfcommN` tty node via the `RFCOMMCREATEDEV` ioctl with the
`RFCOMM_REUSE_DLC` flag. That flag tells the kernel: "don't dial a new
connection; reuse the session this socket already owns, and wrap a tty around
it." Combined with `RFCOMM_RELEASE_ONHUP`, the tty device is automatically
destroyed when the peer hangs up.

Once `/dev/rfcommN` exists, starting `serial-getty@rfcommN.service` via the
systemd D-Bus API is routine.

References in the BlueZ source tree (read these to cross-check layouts and
flags):

- `lib/bluetooth/rfcomm.h:48-66` — `RFCOMMCREATEDEV` ioctl, `struct
  rfcomm_dev_req`, and the flag bit definitions.
- `tools/rfcomm.c:480-505` — the canonical server-side
  `REUSE_DLC | RELEASE_ONHUP` call pattern. Note the ioctl is issued on the
  *connected socket itself*, not on a control fd.
- `test/test-profile` — minimal Python reference that calls
  `ProfileManager1.RegisterProfile` and implements `Profile1`. Useful as a
  protocol-level sanity check for our Rust implementation.
- `src/profile.c:227-263` — the SDP record template bluetoothd auto-generates
  from the `Channel` option when you do not supply your own `ServiceRecord`.
- `doc/org.bluez.ProfileManager.rst` — authoritative list of
  `RegisterProfile` option keys and their types.

## Architecture

```
remote device
    |  RFCOMM connect (channel 1, SPP UUID)
    v
bluetoothd (no --compat; publishes SDP record from RegisterProfile options)
    |  D-Bus: Profile1.NewConnection(device, OwnedFd, props)
    v
bluetooth-getty (this daemon, running as root, owns /spp D-Bus object)
    |  ioctl(fd, RFCOMMCREATEDEV, { REUSE_DLC | RELEASE_ONHUP }) -> N
    |  close(fd)    # kernel keeps the DLC alive via the tty
    |  systemd1.Manager.StartUnit("serial-getty@rfcommN.service", "replace")
    v
serial-getty@rfcommN.service
    |  agetty -> /bin/login -> user shell
    v
peer sees login prompt
```

Teardown on peer disconnect:

1. Kernel RFCOMM layer receives DISC / link loss, hangs up the tty.
2. `RFCOMM_RELEASE_ONHUP` destroys `/dev/rfcommN`.
3. `serial-getty@rfcommN.service` has `BindsTo=dev-rfcommN.device`; systemd
   stops the instance automatically.
4. The daemon itself is not involved in teardown — it just keeps serving
   further `NewConnection` calls.

Teardown on daemon shutdown (SIGTERM / SIGINT):

1. Call `ProfileManager1.UnregisterProfile(/spp)` so `bluetoothd` withdraws
   the SDP record.
2. Drop the zbus connection.
3. Existing `serial-getty@rfcommN` instances keep running until their peers
   disconnect — we deliberately do not kill active sessions.

## Project layout

The crate is already scaffolded at the repo root with `Cargo.toml` and
`src/main.rs`. Grow it into roughly:

```
Cargo.toml
src/
  main.rs        # arg parsing, signal handling, wiring
  profile.rs     # Profile1 interface impl + ProfileManager1 proxy
  rfcomm.rs      # RfcommDevReq, ioctl wrapper, flag constants
  systemd.rs     # Manager1 proxy, StartUnit helper
units/
  bluetooth-getty.service  # systemd unit for the daemon itself
```

Do not add a `README.md` unless asked. Do not add integration tests unless
asked — see "Testing" below for the manual verification loop.

## Dependencies

Replace the existing `dbus = "0.9.10"` in `Cargo.toml` with:

- `zbus = "5"` — async D-Bus, first-class UNIX fd support.
- `tokio = { version = "1", features = ["rt-multi-thread", "macros",
  "signal"] }` — zbus 5 defaults to the tokio runtime.
- `nix = { version = "0.29", features = ["ioctl"] }` — for
  `ioctl_write_ptr_bad!`.
- `libc = "0.2"` — for raw types used in the ioctl struct.
- `clap = { version = "4", features = ["derive"] }` — CLI flags.
- `anyhow = "1"` — error plumbing.
- `tracing = "0.1"` + `tracing-subscriber = { version = "0.3", features =
  ["env-filter"] }` — logging; the daemon runs under systemd so structured
  stderr logging is sufficient.

Pin exact minor versions as you see fit; these are floors.

## D-Bus surface

### `org.bluez.Profile1` (server — we implement this)

Implement with zbus's `#[interface]` attribute on a struct held in an
`ObjectServer` at path `/jp/sorah/BluetoothGetty/spp` (or similar — the
exact path only needs to be stable for the lifetime of the process).

Methods, all required:

```text
Release()
NewConnection(device: ObjectPath, fd: OwnedFd, options: a{sv})
RequestDisconnection(device: ObjectPath)
```

Notes:

- `fd` arrives as `zbus::zvariant::OwnedFd`. Extract the raw fd with
  `.as_raw_fd()` for the ioctl, then let the `OwnedFd` drop (which closes
  it). The kernel keeps the DLC alive through the tty created by the ioctl,
  so closing our userspace handle is correct and required — otherwise the
  daemon process would hold an extra reference to a socket it no longer uses.
- `options` may contain `Version` and `Features` (uint16). Log them at debug
  level; they do not affect behavior.
- `Release` is called when bluetoothd shuts down or forcibly unregisters the
  profile. Log and exit cleanly.
- `RequestDisconnection` is a hint that the daemon should tear down a
  specific device's session. We can log and return — the kernel tty layer
  will drop the connection when the peer actually goes away, and we don't
  proactively kill live getty sessions.

### `org.bluez.ProfileManager1` (client — we call this)

```text
RegisterProfile(profile: ObjectPath, uuid: String, options: a{sv})
UnregisterProfile(profile: ObjectPath)
```

- Service: `org.bluez`
- Object path: `/org/bluez`

Options dict we pass:

| Key     | Type   | Value                                          |
|---------|--------|------------------------------------------------|
| Name    | string | CLI-configurable; default `"Bluetooth getty"`  |
| Role    | string | `"server"`                                     |
| Channel | uint16 | CLI-configurable; default `1`                  |

Leave `ServiceRecord` unset — `bluetoothd` will synthesize an SPP record from
`Channel` using the template at `src/profile.c:227`, which is exactly what we
want. Do **not** pass `PSM` (SPP is RFCOMM-only).

UUID argument: `"00001101-0000-1000-8000-00805f9b34fb"` (SPP). Also make this
a CLI flag so the daemon can be reused for other RFCOMM-based profiles later.

### `org.freedesktop.systemd1.Manager` (client — we call this)

```text
StartUnit(name: String, mode: String) -> ObjectPath
```

- Service: `org.freedesktop.systemd1`
- Object path: `/org/freedesktop/systemd1`
- mode: `"replace"`

We only need `StartUnit`. Do not subscribe to `JobRemoved` or wait on the
returned job object — fire-and-forget is fine. systemd will journal any
failure, and our own log line on the StartUnit call is enough for
troubleshooting.

## Kernel ioctl details

From `lib/bluetooth/rfcomm.h:48-66`:

```c
#define RFCOMMCREATEDEV        _IOW('R', 200, int)

struct rfcomm_dev_req {
    int16_t  dev_id;    // in:  desired /dev/rfcommN number, or -1 for "any"
                        // out: returned device number (also via ioctl retval)
    uint32_t flags;     // bitmask of RFCOMM_* flags below
    bdaddr_t src;       // advisory with REUSE_DLC — kernel reads from socket
    bdaddr_t dst;       // advisory with REUSE_DLC — kernel reads from socket
    uint8_t  channel;   // advisory with REUSE_DLC — kernel reads from socket
};

#define RFCOMM_REUSE_DLC     0   /* bit 0: reuse this socket's DLC */
#define RFCOMM_RELEASE_ONHUP 1   /* bit 1: auto-destroy on peer hangup */
```

### ioctl request number

`_IOW('R', 200, int)` = `(1 << 30) | (4 << 16) | ('R' << 8) | 200` =
`0x400452c8`.

The kernel RFCOMM handler does not validate the size field, so even though
the macro encodes `sizeof(int)` the actual argument is a pointer to
`struct rfcomm_dev_req`. This is why we use the `_bad!` variant of the nix
ioctl macros — it skips nix's size check.

### Rust struct

```rust
#[repr(C)]
#[derive(Default)]
pub struct RfcommDevReq {
    pub dev_id: i16,       // +0
    // 2 bytes of padding here — u32 wants 4-byte alignment
    pub flags: u32,        // +4
    pub src: [u8; 6],      // +8
    pub dst: [u8; 6],      // +14
    pub channel: u8,       // +20
    // trailing padding to alignof=4 -> total size 24
}
```

`#[repr(C)]` produces the padding automatically; do not add explicit padding
fields. Sanity-check with
`assert_eq!(std::mem::size_of::<RfcommDevReq>(), 24)` in a unit test.

With `REUSE_DLC` the kernel sources `src`, `dst`, and `channel` from the
socket itself, so leaving them zero is correct and simpler than reading them
back via `getsockname`/`getpeername`.

### ioctl call

```rust
use nix::ioctl_write_ptr_bad;

ioctl_write_ptr_bad!(rfcomm_create_dev, 0x400452c8, RfcommDevReq);

let mut req = RfcommDevReq::default();
req.dev_id = -1;
req.flags = (1 << 0) | (1 << 1); // REUSE_DLC | RELEASE_ONHUP

let dev_num = unsafe { rfcomm_create_dev(fd.as_raw_fd(), &req) }?;
// dev_num is the returned /dev/rfcommN number.
```

The ioctl **must** be issued on the connected RFCOMM socket fd (the one
received from `NewConnection`), not on a separate control fd — the kernel
looks up the reusable DLC via the file's private data. See
`tools/rfcomm.c:499` for the canonical example.

## NewConnection handler — step by step

1. Log device path, fd number, and `Version`/`Features` from options at debug.
2. Call the ioctl. On error: log at error level (include `errno` via
   `nix::Error`), return `zbus::fdo::Error::Failed` with a short message.
   The `OwnedFd` drops at function exit and closes the socket; bluetoothd
   will report the failure to the peer.
3. Close the fd. With zbus's `OwnedFd` this happens automatically on drop;
   make this explicit by letting the function scope end, or by
   `drop(fd)` right after the ioctl for clarity. Do *not* leak the raw fd.
4. Device-unit visibility. For `BindsTo=dev-rfcommN.device` to work at
   all, udev must tag rfcomm tty devices with `TAG+="systemd"` — systemd's
   upstream `99-systemd.rules` only tags names starting with `tty`
   (`ttyS*`, `ttyUSB*`, ...), not `rfcomm*`. Without the tag,
   `dev-rfcommN.device` is never created and `serial-getty@rfcommN`
   times out with `Job dev-rfcommN.device/start timed out`. Ship a udev
   rules file alongside the daemon:
   ```
   SUBSYSTEM=="tty", KERNEL=="rfcomm[0-9]*", TAG+="systemd"
   ```
   Install at `/etc/udev/rules.d/99-bluetooth-getty.rules` and reload
   with `udevadm control --reload-rules`. Once tagged, the device unit
   appears on the uevent and `serial-getty@.service`'s
   `After=dev-%i.device` blocks `StartUnit` until it's ready — no
   userland race mitigation needed.
5. Build the unit name: `format!("serial-getty@rfcomm{}.service", dev_num)`.
   The `@` and `.service` are literal; do not escape — this instance name is
   always a valid systemd identifier because `dev_num` is a small integer.
6. Call `Manager.StartUnit(unit, "replace")`. Log the returned job path at
   info level. Ignore the return value beyond logging. On error: log, and
   manually release the tty with `RFCOMMRELEASEDEV` (`_IOW('R', 201, int)` =
   `0x400452c9`, same struct, with `req.dev_id = dev_num` and
   `flags = 1 << RFCOMM_HANGUP_NOW`) so we don't leak device nodes on a
   broken systemd.
7. Return `Ok(())`.

## Main / lifecycle

1. Parse CLI flags with clap:
   - `--name` (default `"Bluetooth getty"`)
   - `--uuid` (default SPP UUID)
   - `--channel` (default `1`)
   - `--object-path` (default `/jp/sorah/BluetoothGetty/spp`)
   - `--unit-template` (default `serial-getty@`) — the suffix `rfcommN.service`
     is always appended; this flag exists so ops can substitute a custom
     template later.
2. Init `tracing_subscriber` with env filter.
3. `zbus::connection::Builder::system()`
   - `.name("jp.sorah.BluetoothGetty")` (well-known name, optional but nice)
   - `.serve_at(object_path, Profile { ... })`
   - `.build().await`
4. Construct `ProfileManager1Proxy` against `org.bluez` and call
   `RegisterProfile`. On failure: log, exit non-zero.
5. `tokio::signal::unix` for SIGTERM and SIGINT; `select!` on both plus a
   future that waits forever. On signal:
   a. Call `UnregisterProfile`. Log any error and continue.
   b. Drop the connection, exit 0.

The `Profile` struct needs a handle to the zbus `Connection` so its
`NewConnection` method can create an ad-hoc `SystemdManagerProxy`. Pass it in
at construction time (`Arc<Connection>` or let zbus clone internally — zbus
connection handles are already cheap to clone).

## systemd unit for the daemon

`units/bluetooth-getty.service`:

```ini
[Unit]
Description=Bluetooth getty (SPP -> serial-getty@)
After=bluetooth.service
Requires=bluetooth.service
PartOf=bluetooth.service

[Service]
Type=dbus
BusName=jp.sorah.BluetoothGetty
ExecStart=/usr/local/bin/bluetooth-getty
Restart=on-failure
# No extra hardening in v1 — the daemon legitimately needs:
#   - system bus access to org.bluez and org.freedesktop.systemd1
#   - CAP_NET_BIND_SERVICE is not required (bluetoothd holds the socket)
#   - /dev/rfcomm* creation happens inside the kernel, not via mknod

[Install]
WantedBy=bluetooth.target
```

Install hint goes in the plan only — do not add an installer script.
Packaging is out of scope for v1.

## Edge cases and deliberate non-goals

- **Concurrent connections**: each `NewConnection` call gets its own
  `dev_num` and its own `serial-getty@rfcommN` instance. No shared state
  beyond the zbus connection; the handler is reentrant.
- **Non-root execution**: out of scope. `RegisterProfile` needs system-bus
  access to `org.bluez` and `StartUnit` needs to be allowed to start
  `serial-getty@*` — both are trivially satisfied when running as root under
  systemd, and non-trivial otherwise. Document as "must run as root" in the
  unit file comment and move on.
- **Polkit for StartUnit**: root bypasses polkit, so no rules file needed.
- **Peer-side authentication**: handled by `login` / PAM on the tty. The
  daemon does not authenticate peers itself. We deliberately do not set
  `RequireAuthentication` / `RequireAuthorization` on the profile — those
  would force BlueZ pairing before the RFCOMM channel opens, which is
  usually desirable but is an orthogonal policy call best left to the
  operator. Add them as CLI flags in v1 (default false) so users can opt in
  without code changes.
- **Custom SDP record**: not supported. If someone needs a custom
  `ServiceRecord`, they can fork or we add a `--service-record-file` flag
  later.
- **Multiple profile UUIDs in one daemon**: out of scope. One process, one
  profile. Run multiple instances if you need more.
- **Graceful shutdown of active sessions**: we do not kill live getty
  instances on daemon exit. They survive until the peer disconnects. This
  matches the behavior of `rfcomm watch` and is what operators expect.

## Testing (manual, because CI can't Bluetooth)

1. Build: `cargo build --release`.
2. Stop any running `bluetoothd --compat` and confirm the system
   `bluetooth.service` is running without `-C` / `--compat`.
3. Run the daemon interactively as root with
   `RUST_LOG=bluetooth_getty=debug,zbus=info ./target/release/bluetooth-getty`.
4. From another machine, run `sdptool browse <host-bdaddr>` (or on Linux,
   `bluetoothctl` → `info` after discovery) and verify the SPP record is
   advertised on the configured channel.
5. Connect with `rfcomm connect 0 <host-bdaddr> 1` on the peer, or with any
   Bluetooth serial terminal app.
6. Verify on the host:
   - `ls /dev/rfcomm*` shows the new node.
   - `systemctl status 'serial-getty@rfcomm*.service'` shows an active
     instance.
   - `journalctl -u 'serial-getty@rfcomm*.service'` shows agetty starting.
   - The peer sees a `login:` prompt and can authenticate.
7. Disconnect from the peer side. Verify:
   - `/dev/rfcommN` disappears.
   - `serial-getty@rfcommN.service` becomes inactive.
   - The daemon is still running and ready for the next connection.
8. Reconnect and confirm a second session works.
9. `kill -TERM` the daemon and verify the SDP record disappears
   (`sdptool browse` from the peer no longer lists it). Any live
   getty sessions should keep running until their peers disconnect.

## Out of scope for v1

- Config file (CLI flags are enough).
- systemd socket-activation style startup.
- Per-peer allowlists.
- Metrics, health endpoint, structured audit logs beyond `tracing` output.
- Windows / macOS builds.
- PSM / L2CAP-based profiles.
