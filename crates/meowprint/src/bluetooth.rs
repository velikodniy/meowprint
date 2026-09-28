//! Find printers and open native Bluetooth connections.
//!
//! Use [`Bluetooth::scan`] to list devices or [`Bluetooth::find`] to find one
//! by name or identifier. [`Device::connect`] opens a [`Printer`] with your
//! selected [`Driver`]. All operations require a Tokio runtime.
//!
//! Scans can include devices other than printers. [`Device::is_printer_candidate`]
//! helps filter a device list, but does not prove compatibility with a driver.
//!
//! The host must permit Bluetooth access and provide a powered adapter for
//! discovery. The printer protocol does not require pairing. Await
//! [`Printer::disconnect`](crate::Printer::disconnect) when you finish using a connection.

use crate::{
    Driver, Error, Printer, Result, Transport, TransportEvent, WriteChannel, error::invalid,
};
use async_trait::async_trait;
use btleplug::{
    api::{
        Central, CentralEvent, CentralState, CharPropFlags, Characteristic, Manager as _,
        Peripheral as _, ScanFilter, Service, ValueNotification, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures_util::{Stream, StreamExt};
use std::{collections::HashMap, pin::Pin, time::Duration};
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{Instant, sleep, timeout, timeout_at},
};
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const NAME_SETTLE: Duration = Duration::from_millis(750);
/// The `AE30` service UUID used to recognize possible printers during discovery.
const SERVICE: Uuid = bluetooth_uuid(0xae30);
/// The alternative `AF30` service UUID used as a discovery hint.
const ADVERTISED_SERVICE: Uuid = bluetooth_uuid(0xaf30);

const fn bluetooth_uuid(short: u16) -> Uuid {
    Uuid::from_u128(((short as u128) << 96) | 0x0000_1000_8000_0080_5f9b_34fb)
}

fn ble(error: &btleplug::Error) -> Error {
    Error::Bluetooth(error.to_string())
}

fn write_limit(peripheral: &Peripheral) -> usize {
    usize::from(peripheral.mtu().saturating_sub(3)).clamp(20, 512)
}

/// Bluetooth services discovered by [`Device::inspect`].
///
/// This describes a temporary connection that inspection closes before returning.
/// The services can help identify a device, but do not prove driver compatibility.
#[derive(Debug)]
pub struct Inspection {
    /// All discovered services, including their characteristics and supported operations.
    pub services: Vec<Service>,
    /// Maximum bytes per write on the inspected connection.
    pub max_write_size: usize,
}

async fn bounded<T>(
    name: &'static str,
    future: impl Future<Output = btleplug::Result<T>>,
) -> Result<T> {
    timeout(CONNECT_TIMEOUT, future)
        .await
        .map_err(|_| Error::Timeout(name))?
        .map_err(|error| ble(&error))
}

/// A discovered Bluetooth device that can be inspected or connected.
///
/// Obtain devices from [`Bluetooth::scan`] or [`Bluetooth::find`]. Public fields
/// contain a snapshot of available advertisement data. They are not updated
/// automatically and do not prove that the device supports a printer driver.
///
/// [`Self::inspect`] reads services through a temporary connection.
/// [`Self::connect`] consumes this value and returns a connected [`Printer`].
#[derive(Debug)]
pub struct Device {
    /// Platform device identifier, accepted by [`Bluetooth::find`].
    ///
    /// The format depends on the host. It is not necessarily a Bluetooth address.
    pub id: String,
    /// Advertised name, when available.
    pub name: Option<String>,
    /// Received signal strength in dBm, a logarithmic power unit, when available.
    pub rssi: Option<i16>,
    /// Advertised service hints, without a compatibility guarantee.
    pub advertised_services: Vec<Uuid>,
    /// Raw manufacturer advertisement bytes, keyed by manufacturer identifier.
    pub manufacturer_data: HashMap<u16, Vec<u8>>,
    peripheral: Peripheral,
    adapter: Adapter,
}

impl Device {
    async fn from_peripheral(peripheral: Peripheral, adapter: Adapter) -> Result<Self> {
        let properties = bounded("device properties", peripheral.properties()).await?;
        Ok(Self {
            id: peripheral.id().to_string(),
            name: properties.as_ref().and_then(|p| {
                p.local_name
                    .clone()
                    .or_else(|| p.advertisement_name.clone())
            }),
            rssi: properties.as_ref().and_then(|p| p.rssi),
            advertised_services: properties
                .as_ref()
                .map(|p| p.services.clone())
                .unwrap_or_default(),
            manufacturer_data: properties.map(|p| p.manufacturer_data).unwrap_or_default(),
            peripheral,
            adapter,
        })
    }

    /// Return whether the advertised service or name resembles a known printer.
    ///
    /// Use this result to filter a device list. It does not select a driver,
    /// prove compatibility, or rule out printers that omit recognizable advertisements.
    #[must_use]
    pub fn is_printer_candidate(&self) -> bool {
        self.advertised_services
            .iter()
            .any(|s| *s == SERVICE || *s == ADVERTISED_SERVICE)
            || self.name.as_deref().is_some_and(|name| {
                [
                    "GT", "GB", "MX", "YT", "PD", "SC03", "X6", "JXM", "LY", "LP100",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            })
    }

    /// Connect and read services without sending any printer commands.
    ///
    /// The method attempts to disconnect before returning, including after a
    /// discovery failure. The result describes the temporary connection only.
    /// If the caller cancels inspection, cleanup continues on the Tokio runtime.
    /// Do not inspect a device while another session uses its connection.
    ///
    /// # Errors
    /// Return connection, discovery, or disconnect errors.
    pub async fn inspect(&self) -> Result<Inspection> {
        let mut cleanup = connection_cleanup(&self.peripheral, &self.adapter);
        let result = async {
            let peripheral = self.peripheral.clone();
            cleanup
                .start(async move { bounded("connection", peripheral.connect()).await })
                .await?;
            bounded("service discovery", self.peripheral.discover_services()).await?;
            Ok(Inspection {
                services: self.peripheral.services().into_iter().collect(),
                max_write_size: write_limit(&self.peripheral),
            })
        }
        .await;
        let disconnected = cleanup.finish().await;
        match result {
            Ok(services) => {
                disconnected?;
                Ok(services)
            }
            Err(error) => Err(error),
        }
    }

    /// Open a printer connection with the selected driver.
    ///
    /// This consumes the device. The returned printer accepts print jobs, queries,
    /// and paper movement. Choose a driver that matches the firmware. Setup
    /// inspects the Bluetooth services but does not test printer compatibility.
    /// If setup fails, this method attempts to disconnect before returning.
    /// If the caller cancels setup, cleanup continues on the Tokio runtime.
    ///
    /// # Errors
    /// Return connection, discovery, subscription, or timeout errors. Missing or
    /// ambiguous services required by the driver also cause an error.
    pub async fn connect<D: Driver>(self, driver: D) -> Result<Printer<BleTransport, D>> {
        let transport = self.connect_transport(driver).await?;
        Ok(Printer::new(transport, driver))
    }

    async fn connect_transport<D: Driver>(self, driver: D) -> Result<BleTransport> {
        let mut cleanup = connection_cleanup(&self.peripheral, &self.adapter);
        let mut result = async {
            let central_events = bounded("Bluetooth events", self.adapter.events()).await?;
            let peripheral = self.peripheral.clone();
            cleanup
                .start(async move { bounded("connection", peripheral.connect()).await })
                .await?;
            bounded("service discovery", self.peripheral.discover_services()).await?;
            let services = self.peripheral.services();
            let (write, raster, notify) = endpoints(services.iter(), driver)?;
            let notifications = bounded("notifications", self.peripheral.notifications()).await?;
            bounded(
                "notification subscription",
                self.peripheral.subscribe(&notify),
            )
            .await?;
            Ok(BleTransport {
                peripheral: self.peripheral.clone(),
                write,
                raster,
                notify,
                notifications,
                central_events,
                _adapter: self.adapter.clone(),
                cleanup: None,
            })
        }
        .await;
        match &mut result {
            Ok(transport) => transport.cleanup = Some(cleanup),
            Err(_) => {
                let _ = cleanup.finish().await;
            }
        }
        result
    }
}

/// Access to native Bluetooth adapters for printer discovery.
///
/// Create one with [`Self::new`], then use [`Self::scan`] to list devices or
/// [`Self::find`] to select a known name or identifier. Discovery uses all
/// available powered adapters. This type does not own an active printer session.
///
/// # Example
///
/// This example reads an MXW01 firmware version. Pass a device identifier as the
/// first command-line argument. Use it only with firmware supported by [`crate::Mxw01`].
#[doc = concat!(
    "\n```no_run\n",
    include_str!("../examples/query.rs"),
    "\n```\n",
)]
pub struct Bluetooth {
    adapters: Vec<Adapter>,
}

impl Bluetooth {
    /// Open access to the host's Bluetooth adapters.
    ///
    /// This method does not scan or connect. Enable Bluetooth and allow your
    /// application to use it before scanning.
    ///
    /// # Errors
    /// Return an error if no adapter exists, a host operation fails, or initialization times out.
    pub async fn new() -> Result<Self> {
        let manager = bounded("Bluetooth initialization", Manager::new()).await?;
        let adapters = bounded("Bluetooth adapters", manager.adapters()).await?;
        if adapters.is_empty() {
            return Err(Error::Bluetooth(
                "No Bluetooth adapter is available. Enable Bluetooth and allow this terminal to use it.".into(),
            ));
        }
        Ok(Self { adapters })
    }

    /// Find one device by its advertised name or platform identifier.
    ///
    /// Name matching is exact and case-sensitive. Identifier matching ignores
    /// ASCII letter case. Use an identifier to distinguish devices with the same name.
    /// Matches can include devices other than printers.
    ///
    /// `duration` must be greater than zero and at most 60 seconds. Discovery can
    /// finish early when it finds a match. Host setup and cleanup take additional time.
    /// If the caller cancels discovery, scan cleanup continues on the Tokio runtime.
    ///
    /// # Errors
    /// Reject invalid durations, multiple matches, absent devices, unavailable
    /// powered adapters, or Bluetooth failures.
    pub async fn find(&self, selector: &str, duration: Duration) -> Result<Device> {
        scan_duration(duration)?;
        let adapters = powered_adapters(&self.adapters, |adapter| {
            bounded("Bluetooth state", adapter.adapter_state())
        })
        .await?;
        #[cfg(target_os = "macos")]
        if let Ok(uuid) = Uuid::parse_str(selector) {
            for &adapter in &adapters {
                let options = btleplug::api::RetrievePeripheralsOptions {
                    identifiers: Some(vec![uuid.into()]),
                    services: None,
                };
                for peripheral in
                    bounded("known device lookup", adapter.retrieve_peripherals(options)).await?
                {
                    if peripheral.id().to_string().eq_ignore_ascii_case(selector) {
                        return Device::from_peripheral(peripheral, adapter.clone()).await;
                    }
                }
            }
        }
        let mut scans = Vec::new();
        let result = async {
            let adapters = adapters.as_slice();
            let mut events = futures_util::stream::SelectAll::new();
            for (index, &adapter) in adapters.iter().enumerate() {
                let source = bounded("scan events", adapter.events()).await?;
                events.push(source.map(move |event| (index, event)).boxed());
                start_scan(adapter, &mut scans).await?;
            }
            let candidates = events
                .filter_map(|(index, event)| async move {
                    let id = match event {
                        CentralEvent::DeviceDiscovered(id)
                        | CentralEvent::DeviceUpdated(id)
                        | CentralEvent::ServicesAdvertisement { id, .. }
                        | CentralEvent::ManufacturerDataAdvertisement { id, .. }
                        | CentralEvent::ServiceDataAdvertisement { id, .. } => id,
                        CentralEvent::StateUpdate(state) if state != CentralState::PoweredOn => {
                            return Some(Err(invalid(
                                "Bluetooth became unavailable during discovery.",
                            )));
                        }
                        _ => return None,
                    };
                    let adapter = adapters[index];
                    let candidate = async {
                        let peripheral =
                            bounded("discovered device", adapter.peripheral(&id)).await?;
                        Device::from_peripheral(peripheral, adapter.clone()).await
                    }
                    .await;
                    match candidate {
                        Ok(device)
                            if device.id.eq_ignore_ascii_case(selector)
                                || device.name.as_deref() == Some(selector) =>
                        {
                            Some(Ok((device.id.clone(), device)))
                        }
                        Ok(_) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .boxed();
            first_unique_match(candidates, duration).await
        }
        .await;
        for cleanup in scans {
            let _ = cleanup.finish().await;
        }
        result
    }

    /// List devices after scanning all powered adapters for the requested duration.
    ///
    /// Results include other Bluetooth devices and devices cached by the host.
    /// The returned list is sorted and deduplicated by device identifier.
    /// Use [`Device::is_printer_candidate`] if your interface needs a printer filter.
    ///
    /// `duration` must be greater than zero and at most 60 seconds.
    /// It controls the scan wait. Adapter setup and device enumeration
    /// take additional time.
    /// If the caller cancels discovery, scan cleanup continues on the Tokio runtime.
    ///
    /// # Errors
    /// Reject zero durations, durations above 60 seconds, unavailable powered
    /// adapters, or Bluetooth failures.
    pub async fn scan(&self, duration: Duration) -> Result<Vec<Device>> {
        scan_duration(duration)?;
        let adapters = powered_adapters(&self.adapters, |adapter| {
            bounded("Bluetooth state", adapter.adapter_state())
        })
        .await?;
        let mut scans = Vec::new();
        let result = async {
            for &adapter in &adapters {
                start_scan(adapter, &mut scans).await?;
            }
            sleep(duration).await;
            let mut devices = Vec::new();
            for &adapter in &adapters {
                for peripheral in bounded("device list", adapter.peripherals()).await? {
                    devices.push(Device::from_peripheral(peripheral, adapter.clone()).await?);
                }
            }
            devices.sort_by(|a, b| a.id.cmp(&b.id));
            devices.dedup_by(|a, b| a.id == b.id);
            Ok(devices)
        }
        .await;
        for cleanup in scans {
            let _ = cleanup.finish().await;
        }
        result
    }
}

/// Keep host setup and cleanup alive when their caller cancels its future.
struct CleanupGuard {
    pending_start: Option<JoinHandle<Result<()>>>,
    cleanup: Option<Pin<Box<dyn Future<Output = Result<()>> + Send>>>,
    runtime: Handle,
}

impl CleanupGuard {
    fn new(cleanup: impl Future<Output = Result<()>> + Send + 'static) -> Self {
        Self {
            pending_start: None,
            cleanup: Some(Box::pin(cleanup)),
            runtime: Handle::current(),
        }
    }

    async fn start(
        &mut self,
        start: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        let task = self.pending_start.insert(self.runtime.spawn(start));
        let result = task.await;
        self.pending_start = None;
        result.map_err(|error| task_error(&error))?
    }

    fn disarm(mut self) {
        self.cleanup = None;
    }

    async fn finish(mut self) -> Result<()> {
        match self.schedule_cleanup() {
            Some(task) => task.await.map_err(|error| task_error(&error))?,
            None => Ok(()),
        }
    }

    fn schedule_cleanup(&mut self) -> Option<JoinHandle<Result<()>>> {
        let cleanup = self.cleanup.take()?;
        let pending_start = self.pending_start.take();
        Some(self.runtime.spawn(async move {
            // A canceled connect or scan start can still take effect. Finish it
            // before cleanup so that it cannot reopen the resource afterward.
            if let Some(start) = pending_start {
                let _ = start.await;
            }
            cleanup.await
        }))
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches its task. The captured runtime also
        // permits the guard itself to be dropped outside the runtime context.
        drop(self.schedule_cleanup());
    }
}

fn task_error(error: &tokio::task::JoinError) -> Error {
    Error::Bluetooth(format!("Bluetooth setup or cleanup task failed: {error}"))
}

fn connection_cleanup(peripheral: &Peripheral, adapter: &Adapter) -> CleanupGuard {
    let peripheral = peripheral.clone();
    let adapter = adapter.clone();
    CleanupGuard::new(async move {
        let _adapter = adapter;
        bounded("disconnect", peripheral.disconnect()).await
    })
}

async fn start_scan(adapter: &Adapter, scans: &mut Vec<CleanupGuard>) -> Result<()> {
    let stop_adapter = adapter.clone();
    let mut cleanup =
        CleanupGuard::new(async move { bounded("scan stop", stop_adapter.stop_scan()).await });
    let adapter = adapter.clone();
    let result = cleanup
        .start(
            async move { bounded("scan start", adapter.start_scan(ScanFilter::default())).await },
        )
        .await;
    scans.push(cleanup);
    result
}

async fn powered_adapters<'a, A, F, S>(adapters: &'a [A], mut state: F) -> Result<Vec<&'a A>>
where
    A: Sync,
    F: FnMut(&'a A) -> S + Send,
    S: Future<Output = Result<CentralState>> + Send,
{
    let mut powered = Vec::new();
    let mut failure = None;
    for adapter in adapters {
        match state(adapter).await {
            Ok(CentralState::PoweredOn) => powered.push(adapter),
            Ok(_) => {}
            Err(error) => failure = Some(error),
        }
    }
    if powered.is_empty() {
        return Err(failure.unwrap_or_else(|| Error::Bluetooth(
            "No powered Bluetooth adapter is available. Enable Bluetooth and allow this terminal to use it.".into(),
        )));
    }
    Ok(powered)
}

fn scan_duration(duration: Duration) -> Result<()> {
    if duration.is_zero() || duration > Duration::from_secs(60) {
        return Err(invalid(
            "Scan duration must be greater than zero and at most 60 seconds.",
        ));
    }
    Ok(())
}

async fn first_unique_match<T>(
    mut candidates: impl Stream<Item = Result<(String, T)>> + Unpin,
    duration: Duration,
) -> Result<T> {
    let mut deadline = Instant::now() + duration;
    let mut selected: Option<(String, T)> = None;
    while let Ok(Some(candidate)) = timeout_at(deadline, candidates.next()).await {
        let (id, device) = candidate?;
        if let Some((selected_id, _)) = &selected {
            if *selected_id != id {
                return Err(invalid(
                    "More than one device has this name. Select the identifier from meowprint scan.",
                ));
            }
        } else {
            deadline = deadline.min(Instant::now() + NAME_SETTLE);
        }
        selected = Some((id, device));
        // Repeated advertisements must not extend either deadline.
        if Instant::now() >= deadline {
            break;
        }
    }
    selected.map(|(_, device)| device).ok_or_else(|| invalid(
        "No matching device appeared. Turn on the printer, close its phone app, and run meowprint scan --all.",
    ))
}

fn endpoints<'a>(
    services: impl Iterator<Item = &'a Service>,
    driver: impl Driver,
) -> Result<(Characteristic, Characteristic, Characteristic)> {
    let required = driver.settings().endpoints;
    let mut pairs = services
        .filter(|s| {
            required
                .services
                .iter()
                .any(|&uuid| s.uuid == bluetooth_uuid(uuid))
        })
        .filter_map(|service| {
            let write = service.characteristics.iter().find(|c| {
                c.service_uuid == service.uuid
                    && c.uuid == bluetooth_uuid(required.control)
                    && c.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
            })?;
            let notify = service.characteristics.iter().find(|c| {
                c.service_uuid == service.uuid
                    && c.uuid == bluetooth_uuid(required.notify)
                    && c.properties.contains(CharPropFlags::NOTIFY)
            })?;
            let raster = service.characteristics.iter().find(|c| {
                c.service_uuid == service.uuid
                    && c.uuid == bluetooth_uuid(required.raster)
                    && c.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
            })?;
            Some((write.clone(), raster.clone(), notify.clone()))
        });
    let pair = pairs.next().ok_or_else(|| {
        invalid(
            "The selected driver requires compatible control, raster, and notification endpoints.",
        )
    })?;
    if pairs.next().is_some() {
        return Err(invalid(
            "Multiple compatible printer services were discovered. The device is ambiguous.",
        ));
    }
    Ok(pair)
}

/// A native Bluetooth connection implementing [`Transport`].
///
/// [`Device::connect`] returns a [`Printer`] that owns this transport.
/// Use the printer's methods to print images and query status.
///
/// Writes use the Bluetooth operation without a response. Success means local
/// acceptance, not printer acknowledgment. Close the connection explicitly through
/// [`Printer::disconnect`](crate::Printer::disconnect) or [`Transport::disconnect`].
/// If the transport is dropped, it attempts to disconnect in the background.
/// This fallback requires the Tokio runtime to remain alive.
pub struct BleTransport {
    peripheral: Peripheral,
    write: Characteristic,
    raster: Characteristic,
    notify: Characteristic,
    notifications: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    central_events: Pin<Box<dyn Stream<Item = CentralEvent> + Send>>,
    _adapter: Adapter,
    cleanup: Option<CleanupGuard>,
}

#[async_trait]
impl Transport for BleTransport {
    fn max_write_size(&self) -> usize {
        write_limit(&self.peripheral)
    }

    async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.max_write_size() {
            return Err(invalid(
                "A Bluetooth write cannot exceed the negotiated connection limit.",
            ));
        }
        if !self
            .peripheral
            .is_connected()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?
        {
            return Err(Error::Disconnected);
        }
        self.peripheral
            .write(
                match channel {
                    WriteChannel::Control => &self.write,
                    WriteChannel::Raster => &self.raster,
                },
                bytes,
                WriteType::WithoutResponse,
            )
            .await
            .map_err(|error| Error::Transport(error.to_string()))
    }

    async fn event(&mut self) -> Result<TransportEvent> {
        loop {
            tokio::select! {
                event = self.central_events.next() => match event {
                    Some(CentralEvent::DeviceDisconnected(id) | CentralEvent::DeviceServicesModified(id)) if id == self.peripheral.id() => return Ok(TransportEvent::Disconnected),
                    Some(CentralEvent::StateUpdate(state)) if state != CentralState::PoweredOn => return Ok(TransportEvent::Disconnected),
                    None => return Ok(TransportEvent::Disconnected),
                    _ => {},
                },
                event = self.notifications.next() => match event {
                    Some(value) if value.uuid == self.notify.uuid && value.service_uuid == self.notify.service_uuid => return Ok(TransportEvent::Notification(value.value)),
                    None => return Ok(TransportEvent::Disconnected),
                    _ => {},
                }
            }
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.peripheral
            .disconnect()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.disarm();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITE: Uuid = bluetooth_uuid(0xae01);
    const NOTIFY: Uuid = bluetooth_uuid(0xae02);

    fn service(uuid: Uuid, write_flags: CharPropFlags, notify_flags: CharPropFlags) -> Service {
        Service {
            uuid,
            primary: true,
            characteristics: [
                Characteristic {
                    uuid: WRITE,
                    service_uuid: uuid,
                    properties: write_flags,
                    descriptors: std::collections::BTreeSet::default(),
                },
                Characteristic {
                    uuid: NOTIFY,
                    service_uuid: uuid,
                    properties: notify_flags,
                    descriptors: std::collections::BTreeSet::default(),
                },
            ]
            .into(),
        }
    }

    #[test]
    fn mxw01_requires_a_separate_raster_endpoint() -> Result<()> {
        let mut service = service(
            SERVICE,
            CharPropFlags::WRITE_WITHOUT_RESPONSE,
            CharPropFlags::NOTIFY,
        );
        assert!(endpoints(std::iter::once(&service), crate::Mxw01::default()).is_err());
        service.characteristics.insert(Characteristic {
            uuid: bluetooth_uuid(0xae03),
            service_uuid: SERVICE,
            properties: CharPropFlags::WRITE_WITHOUT_RESPONSE,
            descriptors: std::collections::BTreeSet::default(),
        });
        let (control, raster, notify) =
            endpoints(std::iter::once(&service), crate::Mxw01::default())?;
        assert_eq!(control.uuid, WRITE);
        assert_eq!(raster.uuid, bluetooth_uuid(0xae03));
        assert_eq!(notify.uuid, NOTIFY);
        let (_, raster, _) = endpoints(std::iter::once(&service), crate::Gt01)?;
        assert_eq!(raster.uuid, WRITE);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn powered_adapter_filter_preserves_available_adapters_and_errors() -> Result<()> {
        let adapters = [0, 1, 2];
        let powered = powered_adapters(&adapters, |value| async move {
            Ok(if *value == 0 {
                CentralState::PoweredOff
            } else {
                CentralState::PoweredOn
            })
        })
        .await?;
        assert_eq!(powered, [&1, &2]);
        assert!(matches!(
            powered_adapters(&adapters, |_| async { Ok(CentralState::PoweredOff) }).await,
            Err(Error::Bluetooth(_))
        ));
        assert!(
            matches!(powered_adapters(&adapters, |_| async { Err(Error::Bluetooth("permission denied".into())) }).await, Err(Error::Bluetooth(message)) if message == "permission denied")
        );
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn duplicate_names_are_rejected_and_repeated_advertisements_do_not_extend_settle()
    -> Result<()> {
        let candidates = futures_util::stream::iter([Ok(("one".into(), 1)), Ok(("two".into(), 2))]);
        assert!(
            first_unique_match(candidates, Duration::from_secs(8))
                .await
                .is_err()
        );
        let candidates = futures_util::stream::unfold(0, |count| async move {
            if count != 0 {
                sleep(Duration::from_millis(100)).await;
            }
            Some((Ok(("one".to_owned(), count)), count + 1))
        });
        let start = Instant::now();
        let selected = first_unique_match(Box::pin(candidates), Duration::from_secs(8)).await?;
        assert!(selected > 0);
        assert_eq!(Instant::now() - start, NAME_SETTLE);
        Ok(())
    }
    #[test]
    fn ambiguous_services_and_missing_properties_are_rejected() {
        let first = service(
            SERVICE,
            CharPropFlags::WRITE_WITHOUT_RESPONSE,
            CharPropFlags::NOTIFY,
        );
        let second = service(
            ADVERTISED_SERVICE,
            CharPropFlags::WRITE_WITHOUT_RESPONSE,
            CharPropFlags::NOTIFY,
        );
        assert!(endpoints([&first, &second].into_iter(), crate::Gt01).is_err());
        let missing = service(SERVICE, CharPropFlags::WRITE, CharPropFlags::READ);
        assert!(endpoints(std::iter::once(&missing), crate::Gt01).is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn cancelled_setup_finishes_start_before_cleanup() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let started = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let starting = started.clone();
        let closing = closed.clone();
        let mut guard = CleanupGuard::new(async move {
            assert!(started.load(Ordering::SeqCst));
            closing.store(true, Ordering::SeqCst);
            Ok(())
        });
        let result = timeout(
            Duration::ZERO,
            guard.start(async move {
                sleep(Duration::from_secs(1)).await;
                starting.store(true, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await;
        assert!(result.is_err());
        drop(guard);
        sleep(Duration::from_secs(2)).await;
        assert!(closed.load(Ordering::SeqCst));
    }
    #[tokio::test(start_paused = true)]
    async fn dropped_cleanup_waiter_continues_and_successful_close_disarms_guard() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let closed = Arc::new(AtomicUsize::new(0));
        let closing = closed.clone();
        let guard = CleanupGuard::new(async move {
            sleep(Duration::from_secs(1)).await;
            closing.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(timeout(Duration::ZERO, guard.finish()).await.is_err());
        sleep(Duration::from_secs(2)).await;
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        let closing = closed.clone();
        let guard = CleanupGuard::new(async move {
            closing.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        guard.disarm();
        tokio::task::yield_now().await;
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn live_cleanup_uses_captured_runtime_outside_runtime_context() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let closed = Arc::new(AtomicBool::new(false));
        let closing = closed.clone();
        let guard = CleanupGuard::new(async move {
            closing.store(true, Ordering::SeqCst);
            Ok(())
        });
        assert!(std::thread::spawn(move || drop(guard)).join().is_ok());
        tokio::task::yield_now().await;
        assert!(closed.load(Ordering::SeqCst));
    }
    #[tokio::test(start_paused = true)]
    async fn cleanup_failures_and_deadlines_are_reported() {
        let guard = CleanupGuard::new(async { Err(Error::Bluetooth("cleanup failed".into())) });
        assert!(matches!(guard.finish().await, Err(Error::Bluetooth(_))));
        let guard = CleanupGuard::new(async {
            bounded("disconnect", std::future::pending::<btleplug::Result<()>>()).await
        });
        let start = Instant::now();
        assert!(matches!(
            guard.finish().await,
            Err(Error::Timeout("disconnect"))
        ));
        assert_eq!(Instant::now() - start, CONNECT_TIMEOUT);
    }
}
