use btleplug::api::{Central, CentralEvent, Peripheral as _, PeripheralProperties, ScanFilter};
use btleplug::platform::{Adapter, Peripheral};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::types::{BleError, DeviceInfo, ScanReport};
use crate::{ble_debug, ble_error, ble_warn};

pub(crate) fn canonical_address(address: &str) -> String {
    address.trim().to_ascii_uppercase()
}

// Mirrors the parsing in vendor/btleplug/src/winrtble/ble/watcher.rs. That crate has
// no logger wired to the game's ble_debug.log, so it can't report the effective
// value anywhere useful — this side re-reads the same env var purely for the scan
// report/logs. Trivial to keep in sync; not a behavioural dependency.
fn extended_advertisements_requested() -> bool {
    std::env::var("LJDC_BLE_EXTENDED_ADV")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// BluetoothScanner handles BLE device scanning operations
///
/// This struct manages the scanning state and discovered devices.
/// It is not a GodotClass and is used internally by BluetoothManager.
pub struct BluetoothScanner {
    /// The Bluetooth adapter used for scanning
    adapter: Arc<Adapter>,

    /// Tokio runtime for async operations
    runtime: Arc<Runtime>,

    /// Current scanning state
    is_scanning: Arc<Mutex<bool>>,

    /// Map of discovered devices by address
    discovered_devices: Arc<Mutex<HashMap<String, DeviceInfo>>>,

    /// Map of discovered peripherals by address, used for non-blocking connects
    discovered_peripherals: Arc<Mutex<HashMap<String, Peripheral>>>,

    /// Addresses already forwarded through `device_tx` during the current scan.
    /// Consulted by the post-scan pass so it publishes every device seen at least
    /// once, instead of the narrower "went from no services to some services" rule
    /// it used to apply.
    sent_addresses: Arc<Mutex<HashSet<String>>>,

    /// Total raw BLE events observed during the current/last scan (DeviceDiscovered
    /// + DeviceUpdated + ServicesAdvertisement). Tracked as an atomic — not just a
    /// local variable in `collect_devices` — because that future is usually dropped
    /// mid-poll by the outer `timeout()`, which would otherwise lose the count.
    last_event_count: Arc<AtomicU32>,

    /// Set to true by `stop_scan` to interrupt `collect_devices` early.
    /// Reset to false at the start of each new scan.
    stop_requested: Arc<AtomicBool>,
}

impl BluetoothScanner {
    /// Creates a new BluetoothScanner instance
    ///
    /// # Parameters
    /// * `adapter` - The Bluetooth adapter to use for scanning
    /// * `runtime` - The Tokio runtime for executing async operations
    ///
    /// # Returns
    /// A new BluetoothScanner instance
    pub fn new(adapter: Arc<Adapter>, runtime: Arc<Runtime>) -> Self {
        Self {
            adapter,
            runtime,
            is_scanning: Arc::new(Mutex::new(false)),
            discovered_devices: Arc::new(Mutex::new(HashMap::new())),
            discovered_peripherals: Arc::new(Mutex::new(HashMap::new())),
            sent_addresses: Arc::new(Mutex::new(HashSet::new())),
            last_event_count: Arc::new(AtomicU32::new(0)),
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts scanning for BLE devices
    ///
    /// This method initiates a BLE scan that will run for the specified duration.
    /// Discovered devices are sent through the provided channel as they are found.
    ///
    /// # Parameters
    /// * `scan_duration` - How long to scan for devices
    /// * `device_tx` - Channel sender for discovered devices
    ///
    /// # Returns
    /// Ok(()) if scanning started successfully, Err otherwise
    pub async fn start_scan(
        &self,
        scan_duration: Duration,
        device_tx: mpsc::UnboundedSender<DeviceInfo>,
    ) -> Result<(), BleError> {
        ble_debug!("Starting BLE scan for {:?}", scan_duration);

        // Check if already scanning
        {
            let mut scanning = self.is_scanning.lock().map_err(|e| {
                let error = BleError::InternalError(format!("Lock error: {}", e));
                error.log_error();
                error
            })?;

            if *scanning {
                let error = BleError::ScanFailed("Already scanning".to_string());
                error.log_warning();
                return Err(error);
            }

            *scanning = true;
        }

        // Reset early-stop flag for this scan cycle
        self.stop_requested.store(false, Ordering::Relaxed);
        self.last_event_count.store(0, Ordering::Relaxed);

        // Clear previous scan results
        {
            let mut devices = self.discovered_devices.lock().map_err(|e| {
                let error = BleError::InternalError(format!("Lock error: {}", e));
                error.log_error();
                error
            })?;
            let prev_count = devices.len();
            devices.clear();
            if prev_count > 0 {
                ble_debug!("Cleared {} previous scan results", prev_count);
            }
        }
        {
            let mut peripherals = self.discovered_peripherals.lock().map_err(|e| {
                let error = BleError::InternalError(format!("Lock error: {}", e));
                error.log_error();
                error
            })?;
            peripherals.clear();
        }
        {
            let mut sent = self.sent_addresses.lock().map_err(|e| {
                let error = BleError::InternalError(format!("Lock error: {}", e));
                error.log_error();
                error
            })?;
            sent.clear();
        }

        // Subscribe to the event stream BEFORE starting the scan. `events()` returns
        // a broadcast-style subscription (AdapterManager::event_stream) — any
        // advertisement the OS delivers between `start_scan()` and the moment
        // `collect_devices` first calls `events().await` used to be silently lost.
        // This was most visible on the very first scan of a session, where WinRT can
        // start delivering advertisements within a few milliseconds of Start().
        ble_debug!("start_scan: subscribing to event stream before adapter.start_scan()");
        let events = self.adapter.events().await.map_err(|e| {
            let error = BleError::ScanFailed(format!("Failed to get events: {}", e));
            error.log_error();
            error
        })?;

        // Start scanning
        ble_debug!("Initiating adapter scan");
        ble_debug!("Scan filter: {:?}", ScanFilter::default());
        ble_debug!(
            "start_scan: extended_advertisements_requested={} (LJDC_BLE_EXTENDED_ADV)",
            extended_advertisements_requested()
        );

        ble_debug!("start_scan: calling adapter.start_scan()");
        let scan_result = self.adapter.start_scan(ScanFilter::default()).await;
        match &scan_result {
            Ok(_) => ble_debug!("start_scan: adapter.start_scan() OK"),
            Err(e) => ble_debug!("start_scan: adapter.start_scan() ERROR: {}", e),
        }

        scan_result.map_err(|e| {
            let error = BleError::ScanFailed(e.to_string());
            error.log_error();
            error
        })?;

        let result = timeout(
            scan_duration,
            self.collect_devices(events, device_tx.clone(), self.stop_requested.clone()),
        )
        .await;
        ble_debug!("start_scan: collect_devices finished (timeout or done)");

        // Stop scanning
        let stop_result = self.adapter.stop_scan().await;
        ble_debug!(
            "start_scan: stop_scan result = {:?}",
            stop_result
                .as_ref()
                .map(|_| "Ok")
                .map_err(|e| e.to_string())
        );

        // Update scanning state
        {
            let mut scanning = self.is_scanning.lock().map_err(|e| {
                let error = BleError::InternalError(format!("Lock error: {}", e));
                error.log_error();
                error
            })?;
            *scanning = false;
        }

        // Check for errors
        stop_result.map_err(|e| {
            let error = BleError::ScanFailed(format!("Failed to stop scan: {}", e));
            error.log_error();
            error
        })?;

        // Post-scan pass: re-query all peripherals from the adapter cache.
        // btleplug 0.12 on Linux may fire ServicesAdvertisement/PropertiesChanged events whose
        // UUIDs are stored by BlueZ but not forwarded via DeviceUpdated to our event stream.
        // After stop_scan() the BlueZ/WinRT cache is authoritative — querying it gives the
        // final, fully-populated properties (name + service UUIDs) for every device seen
        // during scan. Every address not already forwarded through device_tx this scan is
        // published here, not just the ones whose services newly became non-empty — a
        // device seen exactly once during the window (common for a single advertisement
        // interval) was previously never published at all.
        match self.adapter.peripherals().await {
            Err(e) => ble_debug!("post-scan peripherals() ERROR: {}", e),
            Ok(peripherals) => {
                ble_debug!("post-scan peripherals() count: {}", peripherals.len());
                for peripheral in peripherals {
                    if let Ok(Some(properties)) = peripheral.properties().await {
                        let address = canonical_address(&peripheral.id().to_string());
                        ble_debug!(
                            "post-scan peripheral: addr={} name={:?} services={:?}",
                            address,
                            properties.local_name,
                            properties
                                .services
                                .iter()
                                .map(|u| u.to_string())
                                .collect::<Vec<_>>()
                        );
                        self.cache_peripheral(address.clone(), peripheral.clone());
                        let device_info = Self::create_device_info(address.clone(), properties);
                        if let Ok(mut devices) = self.discovered_devices.lock() {
                            devices.insert(address.clone(), device_info.clone());
                        }
                        let already_sent = self
                            .sent_addresses
                            .lock()
                            .map(|set| set.contains(&address))
                            .unwrap_or(false);
                        if !already_sent {
                            self.mark_sent(&address);
                            let _ = device_tx.send(device_info);
                        }
                    }
                }
            }
        }

        match result {
            Ok(Ok(())) | Err(_) => Ok(()),
            Ok(Err(e)) => {
                e.log_error();
                Err(e)
            }
        }
    }

    /// Stops an ongoing scan.
    ///
    /// Sets the `stop_requested` flag so that `collect_devices` exits on its next
    /// event loop iteration (latency ≤ one BLE event, typically ≪ 100 ms).
    /// Also spawns `adapter.stop_scan()` to close the BLE event stream,
    /// which causes `collect_devices` to exit via the `None` arm of `stream.next()`.
    pub fn stop_scan(&self) {
        if let Ok(mut scanning) = self.is_scanning.lock() {
            if !*scanning {
                return; // Not scanning
            }
            *scanning = false;
        }

        // Signal collect_devices to exit on next iteration
        self.stop_requested.store(true, Ordering::Relaxed);

        // Also stop the BLE adapter so the event stream closes
        let adapter = self.adapter.clone();
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            let _ = adapter.stop_scan().await;
        });
    }

    /// Collects devices during scanning.
    ///
    /// Listens for BLE discovery events and sends them through the channel immediately,
    /// while updating the `discovered_devices` map. Exits when:
    /// - `stop_requested` is set to true (by `stop_scan`), or
    /// - the event stream closes (after `adapter.stop_scan()`), or
    /// - the outer `timeout` in `start_scan` fires.
    async fn collect_devices(
        &self,
        mut events: std::pin::Pin<Box<dyn futures::Stream<Item = CentralEvent> + Send>>,
        device_tx: mpsc::UnboundedSender<DeviceInfo>,
        stop_requested: Arc<AtomicBool>,
    ) -> Result<(), BleError> {
        use btleplug::api::Peripheral as _;

        ble_debug!("collect_devices: events stream open, waiting for BLE events");
        let mut event_count: u32 = 0;
        let mut discovered_count = 0;

        while let Some(event) = events.next().await {
            // Check early-stop flag (set by stop_scan)
            if stop_requested.load(Ordering::Relaxed) {
                ble_debug!("collect_devices: stop requested, exiting early");
                break;
            }

            event_count += 1;
            self.last_event_count.store(event_count, Ordering::Relaxed);

            match event {
                CentralEvent::DeviceDiscovered(id) => {
                    discovered_count += 1;
                    ble_debug!("DeviceDiscovered: id={}", id);
                    if let Ok(peripheral) = self.adapter.peripheral(&id).await {
                        if let Ok(Some(properties)) = peripheral.properties().await {
                            let address = canonical_address(&id.to_string());
                            ble_debug!(
                                "  -> name={:?} services={:?}",
                                properties.local_name,
                                properties
                                    .services
                                    .iter()
                                    .map(|u| u.to_string())
                                    .collect::<Vec<_>>()
                            );
                            self.cache_peripheral(address.clone(), peripheral);
                            let device_info = Self::create_device_info(address.clone(), properties);

                            if let Ok(mut devices) = self.discovered_devices.lock() {
                                devices.insert(address.clone(), device_info.clone());
                            } else {
                                ble_error!("Failed to acquire device map lock");
                            }

                            self.mark_sent(&address);
                            if device_tx.send(device_info).is_err() {
                                ble_warn!("Failed to send device info through channel");
                            }
                        } else {
                            ble_debug!("  -> properties() returned None or Err");
                        }
                    } else {
                        ble_debug!("  -> peripheral() lookup failed");
                    }
                }
                CentralEvent::DeviceUpdated(id) => {
                    if let Ok(peripheral) = self.adapter.peripheral(&id).await {
                        if let Ok(Some(properties)) = peripheral.properties().await {
                            let address = canonical_address(&id.to_string());
                            self.cache_peripheral(address.clone(), peripheral);
                            let device_info = Self::create_device_info(address.clone(), properties);

                            ble_debug!(
                                "Updated device: {} ({}), RSSI: {:?}",
                                device_info.name.as_ref().unwrap_or(&"Unknown".to_string()),
                                address,
                                device_info.rssi
                            );

                            if let Ok(mut devices) = self.discovered_devices.lock() {
                                devices.insert(address.clone(), device_info.clone());
                            } else {
                                ble_error!("Failed to acquire device map lock");
                            }

                            // Always forward — the adapter's peripheral cache survives
                            // across scans (WinRT and BlueZ both keep it for the life of
                            // the adapter), so a device already known from an earlier
                            // scan in this session only ever emits DeviceUpdated, never
                            // DeviceDiscovered again. Previously this branch forwarded
                            // only if the address already existed in *this scan's*
                            // (freshly cleared) map, which made the first update of every
                            // such device silently disappear.
                            self.mark_sent(&address);
                            if device_tx.send(device_info).is_err() {
                                ble_warn!("Failed to send device update through channel");
                            }
                        }
                    }
                }
                // btleplug 0.12: service UUIDs can arrive via ServicesAdvertisement
                // instead of (or in addition to) DeviceUpdated.  Update the stored
                // device entry so the post-scan peripherals() pass has something to enrich.
                CentralEvent::ServicesAdvertisement { id, services } => {
                    if !services.is_empty() {
                        if let Ok(peripheral) = self.adapter.peripheral(&id).await {
                            if let Ok(Some(properties)) = peripheral.properties().await {
                                let address = canonical_address(&id.to_string());
                                self.cache_peripheral(address.clone(), peripheral);
                                let device_info =
                                    Self::create_device_info(address.clone(), properties);
                                if let Ok(mut devices) = self.discovered_devices.lock() {
                                    devices.insert(address.clone(), device_info.clone());
                                }
                                // See comment on the DeviceUpdated arm above — same reasoning.
                                self.mark_sent(&address);
                                let _ = device_tx.send(device_info);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        ble_debug!(
            "collect_devices done: {} total events, {} DeviceDiscovered",
            event_count,
            discovered_count
        );
        Ok(())
    }

    /// Gets all discovered devices
    ///
    /// # Returns
    /// A vector of DeviceInfo for all discovered devices
    pub fn get_devices(&self) -> Vec<DeviceInfo> {
        if let Ok(devices) = self.discovered_devices.lock() {
            devices.values().cloned().collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_cached_peripheral(&self, address: &str) -> Option<(Peripheral, Option<DeviceInfo>)> {
        let address = canonical_address(address);
        let peripheral = self
            .discovered_peripherals
            .lock()
            .ok()
            .and_then(|peripherals| peripherals.get(&address).cloned());
        let device_info = self
            .discovered_devices
            .lock()
            .ok()
            .and_then(|devices| devices.get(&address).cloned());
        peripheral.map(|p| (p, device_info))
    }

    pub fn is_device_cached(&self, address: &str) -> bool {
        let address = canonical_address(address);
        self.discovered_peripherals
            .lock()
            .map(|peripherals| peripherals.contains_key(&address))
            .unwrap_or(false)
    }

    /// Checks if currently scanning
    ///
    /// # Returns
    /// true if a scan is in progress, false otherwise
    pub fn is_scanning(&self) -> bool {
        if let Ok(scanning) = self.is_scanning.lock() {
            *scanning
        } else {
            false
        }
    }

    /// Builds a report of the last (or current) scan for `get_last_scan_report()`.
    /// `duration_s` is the timeout that was requested for that scan.
    pub async fn build_scan_report(&self, duration_s: f64) -> ScanReport {
        let adapter_state = match self.adapter.adapter_state().await {
            Ok(state) => format!("{:?}", state),
            Err(e) => {
                ble_debug!("build_scan_report: adapter_state() failed: {}", e);
                String::new()
            }
        };
        let unique_devices = self
            .discovered_devices
            .lock()
            .map(|d| d.len() as u32)
            .unwrap_or(0);
        ScanReport {
            adapter_state,
            raw_advertisements: self.last_event_count.load(Ordering::Relaxed),
            unique_devices,
            extended_adv_requested: extended_advertisements_requested(),
            duration_s,
        }
    }

    fn mark_sent(&self, address: &str) {
        if let Ok(mut sent) = self.sent_addresses.lock() {
            sent.insert(address.to_string());
        }
    }

    fn cache_peripheral(&self, address: String, peripheral: Peripheral) {
        if let Ok(mut peripherals) = self.discovered_peripherals.lock() {
            peripherals.insert(address, peripheral);
        } else {
            ble_error!("Failed to acquire peripheral map lock");
        }
    }

    fn create_device_info(address: String, properties: PeripheralProperties) -> DeviceInfo {
        let name = properties.local_name;
        let rssi = properties.rssi;
        let services: Vec<String> = properties
            .services
            .iter()
            .map(|uuid| uuid.to_string())
            .collect();
        let manufacturer_data = properties.manufacturer_data;
        let service_data: std::collections::HashMap<String, Vec<u8>> = properties
            .service_data
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let tx_power_level = properties.tx_power_level;

        DeviceInfo::with_unknown_pairing(
            address,
            name,
            rssi,
            services,
            manufacturer_data,
            service_data,
            tx_power_level,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::canonical_address;

    #[test]
    fn canonical_address_trims_and_normalizes_case() {
        assert_eq!(
            canonical_address(" de:0c:aa:bb:cc:dd "),
            "DE:0C:AA:BB:CC:DD"
        );
    }
}
