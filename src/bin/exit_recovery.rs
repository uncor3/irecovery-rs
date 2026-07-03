use std::error::Error;

use irecovery::{RecoveryDevice, open_by_ecid};

#[cfg(feature = "bundled-db")]
use irecovery::{db, list_recovery_devices_with_metadata};

#[cfg(not(feature = "bundled-db"))]
use irecovery::list_recovery_devices;

const OPEN_ATTEMPTS: usize = 10;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "bundled-db")]
    let devices = list_recovery_devices_with_metadata(&db::DEVICES)?;

    #[cfg(not(feature = "bundled-db"))]
    let devices = list_recovery_devices()?;

    let device = devices
        .iter()
        .find(|device| device.ecid.is_some())
        .ok_or("no recovery device with a parsed ECID was detected")?;
    let ecid = device.ecid.expect("device was filtered by ECID presence");

    print_target(device);

    let client = open_by_ecid(ecid, OPEN_ATTEMPTS)?;
    client.set_auto_boot_and_reboot()?;

    println!("sent auto-boot=true, saveenv, and reboot");
    Ok(())
}

fn print_target(device: &RecoveryDevice) {
    println!(
        "exiting recovery on id={:?} vid={:#06x} pid={:#06x} mode={:?} ecid={:#x} model={} name={}",
        device.id,
        device.vendor_id,
        device.product_id,
        device.mode,
        device.ecid.unwrap_or_default(),
        device.hardware_model().unwrap_or("unknown"),
        device.display_name(),
    );
}
