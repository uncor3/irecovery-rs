use std::error::Error;

use futures_lite::{StreamExt, future};
use irecovery::{MaybeFuture, RecoveryDevice, RecoveryEvent};

#[cfg(feature = "bundled-db")]
use irecovery::{db, watch_recovery_devices_with_metadata};

#[cfg(not(feature = "bundled-db"))]
use irecovery::watch_recovery_devices;

fn main() -> Result<(), Box<dyn Error>> {
    println!("Watching Apple recovery-family USB devices. Press Ctrl-C to stop.");

    #[cfg(feature = "bundled-db")]
    let mut events = Box::pin(watch_recovery_devices_with_metadata(&db::DEVICES).wait()?);

    #[cfg(not(feature = "bundled-db"))]
    let mut events = Box::pin(watch_recovery_devices().wait()?);

    loop {
        match future::block_on(events.as_mut().next()) {
            Some(Ok(RecoveryEvent::Connected(device))) => {
                print_device("connected", &device);
            }
            Some(Ok(RecoveryEvent::Disconnected(id))) => {
                println!("disconnected id={id:?}");
            }
            Some(Err(error)) => {
                eprintln!("watch error: {error}");
            }
            None => {
                println!("device watch ended");
                break;
            }
        }
    }

    Ok(())
}

fn print_device(label: &str, device: &RecoveryDevice) {
    println!(
        "{label} id={:?} vid={:#06x} pid={:#06x} mode={:?} ecid={} model={} name={}",
        device.id,
        device.vendor_id,
        device.product_id,
        device.mode,
        format_optional_hex(device.ecid),
        device.hardware_model().unwrap_or("unknown"),
        device.display_name(),
    );

    if let Some(serial) = &device.usb_serial_number {
        println!("  serial: {serial}");
    }
}

fn format_optional_hex(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| format!("{value:#x}"))
}
