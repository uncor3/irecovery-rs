//! Pure Rust recovery-mode discovery for Apple devices.
//!
//! This crate is a Linux-first, Rust-native reimplementation of the first
//! useful slice of `libirecovery`: enumerate Apple recovery-family USB
//! devices, watch hotplug events, open a device by ECID, and parse the iBoot
//! descriptor string into owned Rust types.
//!
//! USB access is provided by [`nusb`], which uses OS USB APIs directly and
//! does not depend on `libusb` or any C/C++ library. On Linux, users need
//! permission to open `/dev/bus/usb/*` nodes; install a udev rule scoped to
//! Apple's vendor ID (`05ac`) or the recovery product IDs used by your app.
//! macOS and Windows are API-compatible goals for this crate, but V1 is only
//! verified on Linux.

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroU8;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use futures_core::Stream;
use maybe_future::{Ready, blocking::Blocking};
use nusb::hotplug::{HotplugEvent, HotplugWatch};
use nusb::transfer::{ControlOut, ControlType, Recipient};
use nusb::{
    Device, DeviceId, DeviceInfo as UsbDeviceInfo, Interface, MaybeFuture as NusbMaybeFuture,
};
use thiserror::Error;

#[cfg(feature = "bundled-db")]
pub mod db;
mod maybe_future;
pub use maybe_future::MaybeFuture;

const APPLE_VENDOR_ID: u16 = 0x05ac;
const DEFAULT_INTERFACE: u8 = 0;
const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const ENGLISH_US: u16 = 0x0409;

/// Crate-local result type.
pub type Result<T> = std::result::Result<T, RecoveryError>;

/// Errors returned by recovery discovery and opening operations.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("USB operation failed: {0}")]
    Usb(#[from] nusb::Error),
    #[error("USB descriptor read failed: {0}")]
    Descriptor(#[from] nusb::GetDescriptorError),
    #[error("USB transfer failed: {0}")]
    Transfer(#[from] nusb::transfer::TransferError),
    #[error(
        "device is not an Apple recovery-family USB device: vid={vendor_id:#06x} pid={product_id:#06x}"
    )]
    NotRecoveryDevice { vendor_id: u16, product_id: u16 },
    #[error("no recovery device with ECID {ecid:#x} found after {attempts} attempt(s)")]
    NoMatchingDevice { ecid: u64, attempts: usize },
    #[error("device did not expose a parseable iBoot descriptor string")]
    MissingDeviceInfo,
    #[error("recovery command is too long: {length} bytes, maximum is 255")]
    CommandTooLong { length: usize },
    #[error("recovery command contains a NUL byte")]
    CommandContainsNul,
    #[error("environment variable name is empty or contains whitespace/NUL: {0}")]
    InvalidEnvironmentVariable(String),
    #[error("invalid descriptor field {field}: {value}")]
    InvalidDescriptorField { field: &'static str, value: String },
    #[error("unsupported platform behavior: {0}")]
    UnsupportedPlatform(&'static str),
}

/// Known Apple recovery-family USB modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryMode {
    Wtf,
    Dfu,
    Recovery,
    Restore,
    Kis,
    Unknown(u16),
}

impl RecoveryMode {
    /// Returns the known mode for an Apple recovery-family product ID.
    pub fn from_product_id(product_id: u16) -> Option<Self> {
        match product_id {
            0x1222 => Some(Self::Wtf),
            0x1227 => Some(Self::Dfu),
            0x1280 => Some(Self::Restore),
            0x1281 => Some(Self::Recovery),
            0x1881 => Some(Self::Kis),
            _ => None,
        }
    }

    /// Returns the product ID when this mode is known.
    pub fn product_id(self) -> Option<u16> {
        match self {
            Self::Wtf => Some(0x1222),
            Self::Dfu => Some(0x1227),
            Self::Restore => Some(0x1280),
            Self::Recovery => Some(0x1281),
            Self::Kis => Some(0x1881),
            Self::Unknown(_) => None,
        }
    }

    /// Returns true when the mode is one of the known V1 recovery-family modes.
    pub fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl From<u16> for RecoveryMode {
    fn from(product_id: u16) -> Self {
        Self::from_product_id(product_id).unwrap_or(Self::Unknown(product_id))
    }
}

/// Parsed iBoot/recovery descriptor information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryDeviceInfo {
    pub serial_string: String,
    pub pid: u16,
    pub cpid: Option<u32>,
    pub cprv: Option<u32>,
    pub cpfm: Option<u32>,
    pub scep: Option<u32>,
    pub bdid: Option<u32>,
    pub ecid: Option<u64>,
    pub ibfl: Option<u32>,
    pub srnm: Option<String>,
    pub imei: Option<String>,
    pub srtg: Option<String>,
}

impl RecoveryDeviceInfo {
    /// Parse an iBoot-style descriptor string.
    pub fn parse(serial_string: impl Into<String>, pid: u16) -> Result<Self> {
        let serial_string = serial_string.into();
        let mut info = Self {
            serial_string: serial_string.clone(),
            pid,
            ..Self::default()
        };

        info.cpid = parse_hex_u32(&serial_string, "CPID")?;
        info.cprv = parse_hex_u32(&serial_string, "CPRV")?;
        info.cpfm = parse_hex_u32(&serial_string, "CPFM")?;
        info.scep = parse_hex_u32(&serial_string, "SCEP")?;
        info.bdid = parse_hex_u64(&serial_string, "BDID")?.map(|value| value as u32);
        info.ecid = parse_hex_u64(&serial_string, "ECID")?;
        info.ibfl = parse_hex_u32(&serial_string, "IBFL")?;
        info.srnm = parse_bracketed(&serial_string, "SRNM");
        info.imei = parse_bracketed(&serial_string, "IMEI");
        info.srtg = parse_bracketed(&serial_string, "SRTG");

        Ok(info)
    }
}

/// Optional, application-provided metadata for a parsed recovery device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMetadata {
    pub model_identifier: &'static str,
    pub board: &'static str,
    pub marketing_name: &'static str,
    pub display_name: &'static str,
}

/// Resolves parsed recovery identifiers into user-facing device metadata.
pub trait DeviceMetadataResolver: Sync {
    fn resolve(&self, info: &RecoveryDeviceInfo) -> Option<DeviceMetadata>;
}

/// A discovered recovery-family USB device.
#[derive(Debug, Clone)]
pub struct RecoveryDevice {
    pub id: DeviceId,
    pub vendor_id: u16,
    pub product_id: u16,
    pub mode: RecoveryMode,
    pub ecid: Option<u64>,
    pub metadata: Option<DeviceMetadata>,
    pub usb_serial_number: Option<String>,
    pub device_info: Option<RecoveryDeviceInfo>,
}

impl RecoveryDevice {
    pub fn hardware_model(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.board)
            .or_else(|| self.device_info.as_ref()?.srtg.as_deref())
    }

    pub fn display_name(&self) -> &str {
        self.metadata
            .as_ref()
            .map_or("Unknown Device", |metadata| metadata.display_name)
    }
}

/// Hotplug events for recovery-family devices.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum RecoveryEvent {
    Connected(RecoveryDevice),
    Disconnected(DeviceId),
}

/// RAII handle for an opened recovery device and claimed interface.
#[derive(Debug)]
pub struct RecoveryClient {
    device: Device,
    interface: Interface,
    usb_info: UsbDeviceInfo,
    mode: RecoveryMode,
    device_info: RecoveryDeviceInfo,
    metadata: Option<DeviceMetadata>,
}

impl RecoveryClient {
    pub fn mode(&self) -> RecoveryMode {
        self.mode
    }

    pub fn device_info(&self) -> &RecoveryDeviceInfo {
        &self.device_info
    }

    pub fn metadata(&self) -> Option<&DeviceMetadata> {
        self.metadata.as_ref()
    }

    pub fn hardware_model(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.board)
            .or(self.device_info.srtg.as_deref())
    }

    pub fn display_name(&self) -> &str {
        self.metadata
            .as_ref()
            .map_or("Unknown Device", |metadata| metadata.display_name)
    }

    pub fn usb_info(&self) -> &UsbDeviceInfo {
        &self.usb_info
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn interface(&self) -> &Interface {
        &self.interface
    }

    /// Send a raw iBoot command with a custom USB request value.
    pub fn send_command_raw(&self, command: &str, b_request: u8) -> Result<()> {
        let payload = command_payload(command)?;
        if payload.is_empty() {
            return Ok(());
        }

        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: b_request,
                    value: 0,
                    index: 0,
                    data: &payload,
                },
                COMMAND_TIMEOUT,
            )
            .wait()?;

        Ok(())
    }

    /// Send a normal iBoot command using USB request `0`.
    pub fn send_command(&self, command: &str) -> Result<()> {
        self.send_command_raw(command, 0)
    }

    /// Set an iBoot environment variable.
    pub fn setenv(&self, variable: &str, value: &str) -> Result<()> {
        validate_env_variable(variable)?;
        if value.as_bytes().contains(&0) {
            return Err(RecoveryError::CommandContainsNul);
        }

        self.send_command(&format!("setenv {variable} {value}"))
    }

    /// Save iBoot environment variables.
    pub fn saveenv(&self) -> Result<()> {
        self.send_command("saveenv")
    }

    /// Ask the recovery device to reboot.
    pub fn reboot(&self) -> Result<()> {
        self.send_command("reboot")
    }

    /// Set `auto-boot=true`, save the environment, and reboot.
    pub fn set_auto_boot_and_reboot(&self) -> Result<()> {
        self.setenv("auto-boot", "true")?;
        self.saveenv()?;
        self.reboot()
    }
}

/// Convenient owned result equivalent to a successful initialization call.
#[derive(Debug, Clone)]
pub struct InitializedRecoveryDevice {
    pub mode: RecoveryMode,
    pub metadata: Option<DeviceMetadata>,
    pub device_info: RecoveryDeviceInfo,
}

impl InitializedRecoveryDevice {
    pub fn hardware_model(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.board)
            .or(self.device_info.srtg.as_deref())
    }

    pub fn display_name(&self) -> &str {
        self.metadata
            .as_ref()
            .map_or("Unknown Device", |metadata| metadata.display_name)
    }
}

/// Stream returned by [`watch_recovery_devices`].
pub struct RecoveryDeviceWatch<'a> {
    known_ids: HashSet<DeviceId>,
    metadata_resolver: Option<&'a dyn DeviceMetadataResolver>,
    pending: VecDeque<Result<RecoveryEvent>>,
    watch: Pin<Box<HotplugWatch>>,
}

impl Stream for RecoveryDeviceWatch<'_> {
    type Item = Result<RecoveryEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        loop {
            match self.watch.as_mut().poll_next(cx) {
                Poll::Ready(Some(HotplugEvent::Connected(info))) => {
                    if !is_recovery_usb_device(&info) {
                        continue;
                    }
                    return Poll::Ready(Some(
                        recovery_device_from_usb_info(info, self.metadata_resolver).map(|device| {
                            self.known_ids.insert(device.id);
                            RecoveryEvent::Connected(device)
                        }),
                    ));
                }
                Poll::Ready(Some(HotplugEvent::Disconnected(id))) => {
                    if self.known_ids.remove(&id) {
                        return Poll::Ready(Some(Ok(RecoveryEvent::Disconnected(id))));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// List currently connected Apple recovery-family USB devices.
pub fn list_recovery_devices() -> impl MaybeFuture<Output = Result<Vec<RecoveryDevice>>> {
    Blocking::new(|| list_recovery_devices_with_optional_metadata_blocking(None))
}

/// List currently connected Apple recovery-family USB devices with metadata.
pub fn list_recovery_devices_with_metadata(
    metadata_resolver: &dyn DeviceMetadataResolver,
) -> impl MaybeFuture<Output = Result<Vec<RecoveryDevice>>> + '_ {
    Ready(list_recovery_devices_with_optional_metadata_blocking(Some(
        metadata_resolver,
    )))
}

fn list_recovery_devices_with_optional_metadata_blocking(
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<Vec<RecoveryDevice>> {
    let devices = nusb::list_devices().wait()?;
    let mut recovery_devices = Vec::new();

    for usb_info in devices {
        if is_recovery_usb_device(&usb_info) {
            recovery_devices.push(recovery_device_from_usb_info(usb_info, metadata_resolver)?);
        }
    }

    Ok(recovery_devices)
}

/// Watch recovery-family device connect/disconnect events.
///
/// The watch is created before the initial device list is seeded, matching
/// nusb's recommended pattern for avoiding missed connect events.
pub fn watch_recovery_devices() -> impl MaybeFuture<Output = Result<RecoveryDeviceWatch<'static>>> {
    Blocking::new(|| watch_recovery_devices_with_optional_metadata_blocking(None))
}

/// Watch recovery-family device connect/disconnect events with metadata.
pub fn watch_recovery_devices_with_metadata(
    metadata_resolver: &dyn DeviceMetadataResolver,
) -> impl MaybeFuture<Output = Result<RecoveryDeviceWatch<'_>>> + '_ {
    Ready(watch_recovery_devices_with_optional_metadata_blocking(
        Some(metadata_resolver),
    ))
}

fn watch_recovery_devices_with_optional_metadata_blocking(
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<RecoveryDeviceWatch<'_>> {
    let watch = nusb::watch_devices()?;
    let devices = list_recovery_devices_with_optional_metadata_blocking(metadata_resolver)?;
    let known_ids = devices.iter().map(|device| device.id).collect();
    let pending = devices
        .into_iter()
        .map(|device| Ok(RecoveryEvent::Connected(device)))
        .collect();

    Ok(RecoveryDeviceWatch {
        known_ids,
        metadata_resolver,
        pending,
        watch: Box::pin(watch),
    })
}

/// Open the recovery device with the matching ECID, retrying enumeration.
pub fn open_by_ecid(
    ecid: u64,
    attempts: usize,
) -> impl MaybeFuture<Output = Result<RecoveryClient>> {
    Blocking::new(move || open_by_ecid_with_optional_metadata_blocking(ecid, attempts, None))
}

/// Open the recovery device with the matching ECID and optional metadata.
pub fn open_by_ecid_with_metadata(
    ecid: u64,
    attempts: usize,
    metadata_resolver: &dyn DeviceMetadataResolver,
) -> impl MaybeFuture<Output = Result<RecoveryClient>> + '_ {
    Ready(open_by_ecid_with_optional_metadata_blocking(
        ecid,
        attempts,
        Some(metadata_resolver),
    ))
}

fn open_by_ecid_with_optional_metadata_blocking(
    ecid: u64,
    attempts: usize,
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<RecoveryClient> {
    let attempts = attempts.max(1);

    for attempt in 0..attempts {
        let devices = nusb::list_devices().wait()?;
        for usb_info in devices {
            if !is_recovery_usb_device(&usb_info) {
                continue;
            }

            match open_recovery_client(usb_info, metadata_resolver) {
                Ok(client) if client.device_info.ecid == Some(ecid) => return Ok(client),
                Ok(_) => {}
                Err(_) => {}
            }
        }

        if attempt + 1 < attempts {
            thread::sleep(RETRY_DELAY);
        }
    }

    Err(RecoveryError::NoMatchingDevice { ecid, attempts })
}

/// Open and summarize a recovery device by ECID.
pub fn init_recovery_device(
    ecid: u64,
    attempts: usize,
) -> impl MaybeFuture<Output = Result<InitializedRecoveryDevice>> {
    Blocking::new(move || {
        init_recovery_device_with_optional_metadata_blocking(ecid, attempts, None)
    })
}

/// Open and summarize a recovery device by ECID with optional metadata.
pub fn init_recovery_device_with_metadata(
    ecid: u64,
    attempts: usize,
    metadata_resolver: &dyn DeviceMetadataResolver,
) -> impl MaybeFuture<Output = Result<InitializedRecoveryDevice>> + '_ {
    Ready(init_recovery_device_with_optional_metadata_blocking(
        ecid,
        attempts,
        Some(metadata_resolver),
    ))
}

/// Open a recovery device by ECID, set `auto-boot=true`, save env, and reboot.
pub fn set_auto_boot_and_reboot(
    ecid: u64,
    attempts: usize,
) -> impl MaybeFuture<Output = Result<()>> {
    Blocking::new(move || set_auto_boot_and_reboot_blocking(ecid, attempts))
}

fn set_auto_boot_and_reboot_blocking(ecid: u64, attempts: usize) -> Result<()> {
    let client = open_by_ecid_with_optional_metadata_blocking(ecid, attempts, None)?;
    client.set_auto_boot_and_reboot()
}

fn init_recovery_device_with_optional_metadata_blocking(
    ecid: u64,
    attempts: usize,
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<InitializedRecoveryDevice> {
    let client = open_by_ecid_with_optional_metadata_blocking(ecid, attempts, metadata_resolver)?;
    Ok(InitializedRecoveryDevice {
        mode: client.mode,
        metadata: client.metadata.clone(),
        device_info: client.device_info.clone(),
    })
}

fn is_recovery_usb_device(info: &UsbDeviceInfo) -> bool {
    info.vendor_id() == APPLE_VENDOR_ID
        && RecoveryMode::from_product_id(info.product_id()).is_some()
}

fn recovery_device_from_usb_info(
    usb_info: UsbDeviceInfo,
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<RecoveryDevice> {
    let vendor_id = usb_info.vendor_id();
    let product_id = usb_info.product_id();
    let mode =
        RecoveryMode::from_product_id(product_id).ok_or(RecoveryError::NotRecoveryDevice {
            vendor_id,
            product_id,
        })?;
    let usb_serial_number = usb_info.serial_number().map(ToOwned::to_owned);

    let device_info = usb_serial_number
        .as_deref()
        .and_then(|serial| RecoveryDeviceInfo::parse(serial, product_id).ok());
    let metadata = device_info
        .as_ref()
        .and_then(|info| metadata_resolver.and_then(|resolver| resolver.resolve(info)));

    Ok(RecoveryDevice {
        id: usb_info.id(),
        vendor_id,
        product_id,
        mode,
        ecid: device_info.as_ref().and_then(|info| info.ecid),
        metadata,
        usb_serial_number,
        device_info,
    })
}

fn open_recovery_client(
    usb_info: UsbDeviceInfo,
    metadata_resolver: Option<&dyn DeviceMetadataResolver>,
) -> Result<RecoveryClient> {
    let vendor_id = usb_info.vendor_id();
    let product_id = usb_info.product_id();
    let mode =
        RecoveryMode::from_product_id(product_id).ok_or(RecoveryError::NotRecoveryDevice {
            vendor_id,
            product_id,
        })?;

    let device = usb_info.open().wait()?;
    let descriptor_string = read_iboot_descriptor_string(&device, usb_info.serial_number())?;
    let device_info = RecoveryDeviceInfo::parse(descriptor_string, product_id)?;

    let interface = claim_recovery_interface(&device)?;
    let metadata = metadata_resolver.and_then(|resolver| resolver.resolve(&device_info));

    Ok(RecoveryClient {
        device,
        interface,
        usb_info,
        mode,
        device_info,
        metadata,
    })
}

fn claim_recovery_interface(device: &Device) -> Result<Interface> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        device
            .detach_and_claim_interface(DEFAULT_INTERFACE)
            .wait()
            .or_else(|_| device.claim_interface(DEFAULT_INTERFACE).wait())
            .map_err(RecoveryError::from)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        device
            .claim_interface(DEFAULT_INTERFACE)
            .wait()
            .map_err(RecoveryError::from)
    }
}

fn read_iboot_descriptor_string(
    device: &Device,
    enumerated_serial: Option<&str>,
) -> Result<String> {
    if let Some(serial) = enumerated_serial {
        if looks_like_iboot_descriptor(serial) {
            return Ok(serial.to_owned());
        }
    }

    for index in [3, 1, 2] {
        let Some(index) = NonZeroU8::new(index) else {
            continue;
        };
        match device
            .get_string_descriptor(index, ENGLISH_US, DESCRIPTOR_TIMEOUT)
            .wait()
        {
            Ok(value) if looks_like_iboot_descriptor(&value) => return Ok(value),
            Ok(_) => {}
            Err(_) => {}
        }
    }

    enumerated_serial
        .filter(|serial| !serial.is_empty())
        .map(ToOwned::to_owned)
        .ok_or(RecoveryError::MissingDeviceInfo)
}

fn looks_like_iboot_descriptor(value: &str) -> bool {
    value.contains("ECID:") || value.contains("CPID:") || value.contains("BDID:")
}

fn parse_hex_u32(input: &str, key: &'static str) -> Result<Option<u32>> {
    parse_hex_u64(input, key).map(|value| value.map(|value| value as u32))
}

fn parse_hex_u64(input: &str, key: &'static str) -> Result<Option<u64>> {
    let Some(value) = parse_token(input, key) else {
        return Ok(None);
    };

    let value = value.trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(value, 16)
        .map(Some)
        .map_err(|_| RecoveryError::InvalidDescriptorField {
            field: key,
            value: value.to_owned(),
        })
}

fn parse_token<'a>(input: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}:");
    let start = input.find(&prefix)? + prefix.len();
    let rest = &input[start..];
    rest.split_whitespace().next()
}

fn parse_bracketed(input: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:[");
    let start = input.find(&prefix)? + prefix.len();
    let rest = &input[start..];
    let end = rest.find(']')?;
    Some(rest[..end].to_owned())
}

fn command_payload(command: &str) -> Result<Vec<u8>> {
    let length = command.len();
    if length >= 0x100 {
        return Err(RecoveryError::CommandTooLong { length });
    }
    if command.as_bytes().contains(&0) {
        return Err(RecoveryError::CommandContainsNul);
    }
    if command.is_empty() {
        return Ok(Vec::new());
    }

    let mut payload = Vec::with_capacity(length + 1);
    payload.extend_from_slice(command.as_bytes());
    payload.push(0);
    Ok(payload)
}

fn validate_env_variable(variable: &str) -> Result<()> {
    if variable.is_empty()
        || variable
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(RecoveryError::InvalidEnvironmentVariable(
            variable.to_owned(),
        ));
    }

    Ok(())
}

#[cfg(feature = "bundled-db")]
impl DeviceMetadataResolver for &[db::DeviceDatabaseInfo] {
    fn resolve(&self, info: &RecoveryDeviceInfo) -> Option<DeviceMetadata> {
        self.iter()
            .find(|entry| {
                info.srtg
                    .as_deref()
                    .is_some_and(|srtg| srtg.eq_ignore_ascii_case(entry.board))
            })
            .or_else(|| {
                self.iter().find(|entry| {
                    info.bdid == Some(u32::from(entry.cpid))
                        && info.cpid == Some(u32::from(entry.bdid))
                })
            })
            .map(|entry| DeviceMetadata {
                model_identifier: entry.model_identifier,
                board: entry.board,
                marketing_name: entry.marketing_name,
                display_name: entry.display_name,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_DESCRIPTOR: &str = "CPID:8015 CPRV:11 CPFM:03 SCEP:01 BDID:06 ECID:0011223344556677 IBFL:1C SRNM:[F2LTEST12345] IMEI:[356000000000000] SRTG:[d22ap]";

    #[test]
    fn parses_full_iboot_descriptor() {
        let info = RecoveryDeviceInfo::parse(FULL_DESCRIPTOR, 0x1281).unwrap();

        assert_eq!(info.pid, 0x1281);
        assert_eq!(info.cpid, Some(0x8015));
        assert_eq!(info.cprv, Some(0x11));
        assert_eq!(info.cpfm, Some(0x03));
        assert_eq!(info.scep, Some(0x01));
        assert_eq!(info.bdid, Some(0x06));
        assert_eq!(info.ecid, Some(0x0011_2233_4455_6677));
        assert_eq!(info.ibfl, Some(0x1c));
        assert_eq!(info.srnm.as_deref(), Some("F2LTEST12345"));
        assert_eq!(info.imei.as_deref(), Some("356000000000000"));
        assert_eq!(info.srtg.as_deref(), Some("d22ap"));
    }

    #[test]
    fn parses_missing_fields_as_none() {
        let info = RecoveryDeviceInfo::parse("CPID:8015 ECID:abc", 0x1281).unwrap();

        assert_eq!(info.cpid, Some(0x8015));
        assert_eq!(info.ecid, Some(0xabc));
        assert_eq!(info.bdid, None);
        assert_eq!(info.srnm, None);
    }

    #[test]
    fn rejects_malformed_hex() {
        let error = RecoveryDeviceInfo::parse("CPID:not-hex ECID:1234", 0x1281).unwrap_err();

        assert!(matches!(
            error,
            RecoveryError::InvalidDescriptorField { field: "CPID", .. }
        ));
    }

    #[test]
    fn parses_lowercase_and_prefixed_hex() {
        let info =
            RecoveryDeviceInfo::parse("CPID:0x8015 BDID:0x0a ECID:deadbeef", 0x1281).unwrap();

        assert_eq!(info.cpid, Some(0x8015));
        assert_eq!(info.bdid, Some(0x0a));
        assert_eq!(info.ecid, Some(0xdead_beef));
    }

    #[test]
    fn maps_known_recovery_modes() {
        assert_eq!(
            RecoveryMode::from_product_id(0x1222),
            Some(RecoveryMode::Wtf)
        );
        assert_eq!(
            RecoveryMode::from_product_id(0x1227),
            Some(RecoveryMode::Dfu)
        );
        assert_eq!(
            RecoveryMode::from_product_id(0x1281),
            Some(RecoveryMode::Recovery)
        );
        assert_eq!(
            RecoveryMode::from_product_id(0x1881),
            Some(RecoveryMode::Kis)
        );
        assert_eq!(RecoveryMode::from_product_id(0x9999), None);
        assert_eq!(RecoveryMode::from(0x9999), RecoveryMode::Unknown(0x9999));
    }

    #[test]
    fn resolver_can_match_by_hardware_model() {
        struct FakeResolver;

        impl DeviceMetadataResolver for FakeResolver {
            fn resolve(&self, info: &RecoveryDeviceInfo) -> Option<DeviceMetadata> {
                (info.srtg.as_deref() == Some("d22ap")).then_some(DeviceMetadata {
                    model_identifier: "iPhone10,3",
                    board: "d22ap",
                    marketing_name: "iPhone X",
                    display_name: "iPhone X",
                })
            }
        }

        let info = RecoveryDeviceInfo::parse(FULL_DESCRIPTOR, 0x1281).unwrap();
        let metadata = FakeResolver.resolve(&info).unwrap();

        assert_eq!(metadata.board, "d22ap");
        assert_eq!(metadata.display_name, "iPhone X");
    }

    #[test]
    fn device_uses_metadata_convenience_methods() {
        let device_info = RecoveryDeviceInfo::parse(FULL_DESCRIPTOR, 0x1281).unwrap();
        let device = InitializedRecoveryDevice {
            mode: RecoveryMode::Recovery,
            metadata: Some(DeviceMetadata {
                model_identifier: "iPhone10,3",
                board: "d22ap",
                marketing_name: "iPhone X",
                display_name: "iPhone X",
            }),
            device_info,
        };

        assert_eq!(device.hardware_model(), Some("d22ap"));
        assert_eq!(device.display_name(), "iPhone X");
    }

    #[test]
    fn parse_bracketed_stops_at_closing_bracket() {
        let info =
            RecoveryDeviceInfo::parse("ECID:1 SRNM:[ABC 123] SRTG:[d22ap] EXTRA:ignored", 0x1281)
                .unwrap();

        assert_eq!(info.srnm.as_deref(), Some("ABC 123"));
        assert_eq!(info.srtg.as_deref(), Some("d22ap"));
    }

    #[test]
    fn command_payload_is_nul_terminated() {
        assert_eq!(command_payload("reboot").unwrap(), b"reboot\0");
        assert_eq!(command_payload("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn command_payload_rejects_long_commands() {
        let error = command_payload(&"x".repeat(0x100)).unwrap_err();

        assert!(matches!(
            error,
            RecoveryError::CommandTooLong { length: 256 }
        ));
    }

    #[test]
    fn command_payload_rejects_nul_bytes() {
        let error = command_payload("setenv x\0y").unwrap_err();

        assert!(matches!(error, RecoveryError::CommandContainsNul));
    }

    #[test]
    fn env_variable_validation_rejects_bad_names() {
        assert!(validate_env_variable("auto-boot").is_ok());
        assert!(matches!(
            validate_env_variable(""),
            Err(RecoveryError::InvalidEnvironmentVariable(_))
        ));
        assert!(matches!(
            validate_env_variable("auto boot"),
            Err(RecoveryError::InvalidEnvironmentVariable(_))
        ));
        assert!(matches!(
            validate_env_variable("auto\0boot"),
            Err(RecoveryError::InvalidEnvironmentVariable(_))
        ));
    }

    #[cfg(feature = "bundled-db")]
    #[test]
    fn bundled_db_resolver_matches_by_board() {
        let info = RecoveryDeviceInfo::parse(FULL_DESCRIPTOR, 0x1281).unwrap();
        let metadata = (&db::DEVICES).resolve(&info).unwrap();

        assert_eq!(metadata.model_identifier, "iPhone10,3");
        assert_eq!(metadata.board, "d22ap");
        assert_eq!(metadata.display_name, "iPhone X");
    }

    #[cfg(feature = "bundled-db")]
    #[test]
    fn bundled_db_resolver_falls_back_to_adapted_cpid_bdid() {
        let info = RecoveryDeviceInfo::parse("CPID:8015 BDID:06 ECID:1", 0x1281).unwrap();
        let metadata = (&db::DEVICES).resolve(&info).unwrap();

        assert_eq!(metadata.board, "d22ap");
    }

    #[cfg(feature = "bundled-db")]
    #[test]
    fn bundled_db_resolver_returns_none_for_unknown_device() {
        let info =
            RecoveryDeviceInfo::parse("CPID:ffff BDID:ff ECID:1 SRTG:[nopeap]", 0x1281).unwrap();

        assert_eq!((&db::DEVICES).resolve(&info), None);
    }
}
