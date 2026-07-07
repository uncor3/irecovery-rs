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

## Examples

Watch recovery-device events from a Tokio runtime:

```toml
irecovery = { version = "0.2", features = ["tokio"] }
futures-lite = "2"
tokio = { version = "1", features = ["rt"] }
```

```rust
use std::{error::Error, sync::LazyLock};

use futures_lite::StreamExt;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build Tokio runtime")
});

fn main() -> Result<(), Box<dyn Error>> {
    RUNTIME.block_on(async move {
        let mut events = match irecovery::watch_recovery_devices().await {
            Ok(events) => Box::pin(events),
            Err(err) => {
                eprintln!("failed to watch recovery devices: {err}");
                return;
            }
        };

        while let Some(event) = events.as_mut().next().await {
            match event {
                Ok(irecovery::RecoveryEvent::Connected(device)) => {
                    eprintln!(
                        "recovery device connected: id={:?} mode={:?} ecid={:?} model={} name={}",
                        device.id,
                        device.mode,
                        device.ecid,
                        device.hardware_model().unwrap_or("unknown"),
                        device.display_name(),
                    );
                }
                Ok(irecovery::RecoveryEvent::Disconnected(id)) => {
                    eprintln!("recovery device disconnected: id={id:?}");
                }
                Err(err) => {
                    eprintln!("recovery device watch error: {err}");
                }
            }
        }
    });

    Ok(())
}
```

Watch recovery-device events without Tokio by blocking the current thread:

```rust
use std::error::Error;

use futures_lite::{future, StreamExt};
use irecovery::{MaybeFuture, RecoveryEvent};

fn main() -> Result<(), Box<dyn Error>> {
    let mut events = Box::pin(irecovery::watch_recovery_devices().wait()?);

    loop {
        match future::block_on(events.as_mut().next()) {
            Some(Ok(RecoveryEvent::Connected(device))) => {
                println!(
                    "recovery device connected: id={:?} mode={:?} ecid={:?} model={} name={}",
                    device.id,
                    device.mode,
                    device.ecid,
                    device.hardware_model().unwrap_or("unknown"),
                    device.display_name(),
                );
            }
            Some(Ok(RecoveryEvent::Disconnected(id))) => {
                println!("recovery device disconnected: id={id:?}");
            }
            Some(Err(err)) => {
                eprintln!("recovery device watch error: {err}");
            }
            None => break,
        }
    }

    Ok(())
}
```


```rust
use irecovery::{MaybeFuture, Result, open_by_ecid};

fn main() -> Result<()> {
    let client = open_by_ecid(0x1234_5678_9abc_def0, 10).wait()?;
    client.setenv("auto-boot", "true")?;
    client.saveenv()?;
    client.reboot()?;
    Ok(())
}
```

To watch recovery-device events, git clone the repo and run 

```bash
cargo run --bin watch_recovery
```

Exit recovery mode on the first detected device with an ECID:

```bash
cargo run --bin exit_recovery
```