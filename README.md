# irecovery

`irecovery` is a WIP pure Rust library for talking to Apple
recovery USB devices. It is inspired by
[`libirecovery`](https://github.com/libimobiledevice/libirecovery), but it is
not a binding to that project.

The crate uses [`nusb`](https://docs.rs/nusb/) for USB access and does not
depend on `libusb` or any C/C++ library.

## Status

This project is early and Linux first.

Implemented pieces include:

- Recovery/DFU/KIS-style Apple USB device discovery.
- Hotplug event watching.
- ECID-based opening with retries.
- iBoot descriptor parsing.
- Optional device metadata resolution.
- Basic recovery commands such as `setenv`, `saveenv`, and `reboot`.

Notable caveats:

- Linux is the primary tested target.
- macOS and Windows portability is intended through `nusb`, but not yet
  thoroughly verified.
- The command/upload surface is incomplete compared with `libirecovery`.
- Old legacy command paths are not fully implemented yet.

## Linux Permissions

On Linux, the process must be able to open the matching `/dev/bus/usb/*`
device node. For development, configure udev rules for Apple recovery devices
or run with permissions that can access the device.

## Optional Device Database

The core crate does not bundle the large device database by default. Enable the
`bundled-db` feature if you want built-in marketing/display-name resolution:

```bash
cargo run --features bundled-db --bin watch_recovery
```

Without this feature, device discovery still works, but unresolved names are
reported as `Unknown Device`.

## Example

Watch recovery-device events:

```bash
cargo run --bin watch_recovery
```

Exit recovery mode on the first detected device with an ECID:

```bash
cargo run --bin exit_recovery
```

Use the library:

```rust
use irecovery::{open_by_ecid, Result};

fn main() -> Result<()> {
    let client = open_by_ecid(0x1234_5678_9abc_def0, 10)?;
    client.setenv("auto-boot", "true")?;
    client.saveenv()?;
    client.reboot()?;
    Ok(())
}
```