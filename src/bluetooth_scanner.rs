use btleplug::api::{Central, CentralEvent, Peripheral as _, PeripheralProperties, ScanFilter};
use btleplug::platform::Adapter;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::types::{BleError, DeviceInfo};
use crate::{ble_debug, ble_error, ble_warn};

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

        // Start scanning
        ble_debug!("Initiating adapter scan");
        ble_debug!("Scan filter: {:?}", ScanFilter::default());

        let scan_result = self.adapter.start_scan(ScanFilter::default()).await;
        match &scan_result {
            Ok(_) => {} // ble_info!("Adapter start_scan() returned Ok"),
            Err(e) => ble_error!("Adapter start_scan() returned Err: {}", e),
        }

        scan_result.map_err(|e| {
            let error = BleError::ScanFailed(e.to_string());
            error.log_error();
            error
        })?;

        // ble_info!("BLE scan started successfully");

        // Wait for scan duration with additional debugging
        // ble_debug!("Waiting for scan duration: {:?}", scan_duration);
        let result = timeout(scan_duration, self.collect_devices(device_tx.clone())).await;

        // Stop scanning
        let stop_result = self.adapter.stop_scan().await;

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
        // After stop_scan() the BlueZ cache is authoritative — querying it gives the final,
        // fully-populated properties (name + service UUIDs) for every device seen during scan.
        if let Ok(peripherals) = self.adapter.peripherals().await {
            for peripheral in peripherals {
                if let Ok(Some(properties)) = peripheral.properties().await {
                    let address = peripheral.id().to_string();
                    let device_info = Self::create_device_info(address.clone(), properties).await;
                    let needs_update = if let Ok(mut devices) = self.discovered_devices.lock() {
                        let stale = match devices.get(&address) {
                            Some(old) => old.services.is_empty() && !device_info.services.is_empty(),
                            // Device not seen via events — add it (e.g. no DeviceDiscovered fired)
                            None => true,
                        };
                        devices.insert(address.clone(), device_info.clone());
                        stale
                    } else {
                        false
                    };
                    if needs_update {
                        let _ = device_tx.send(device_info);
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

    /// Stops an ongoing scan
    ///
    /// This method stops the current BLE scan if one is in progress.
    pub fn stop_scan(&self) {
        // Update scanning state
        if let Ok(mut scanning) = self.is_scanning.lock() {
            if !*scanning {
                return; // Not scanning
            }
            *scanning = false;
        }

        // Stop scan asynchronously
        let adapter = self.adapter.clone();
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let _ = adapter.stop_scan().await;
        });
    }

    /// Collects devices during scanning
    ///
    /// This internal method listens for device discovery events and sends them
    /// through the channel immediately, while also updating the discovered_devices map.
    async fn collect_devices(
        &self,
        device_tx: mpsc::UnboundedSender<DeviceInfo>,
    ) -> Result<(), BleError> {
        use btleplug::api::Peripheral as _;

        ble_debug!("Starting device collection");

        // Get events stream
        let mut events = self.adapter.events().await.map_err(|e| {
            let error = BleError::ScanFailed(format!("Failed to get events: {}", e));
            error.log_error();
            error
        })?;

        ble_debug!("Events stream created successfully");

        // Process events with a counter to avoid infinite loops
        let mut event_count = 0;
        let max_events = 1000; // Safety limit to prevent infinite loops

        while let Some(event) = events.next().await {
            event_count += 1;
            if event_count > max_events {
                ble_warn!("Event limit reached, stopping collection to prevent infinite loop");
                break;
            }

            // Only log every 10th event to reduce spam
            if event_count % 10 == 0 {
                ble_debug!("Processing event #{}: {:?}", event_count, event);
            }

            match event {
                CentralEvent::DeviceDiscovered(id) => {
                    if let Ok(peripheral) = self.adapter.peripheral(&id).await {
                        if let Ok(Some(properties)) = peripheral.properties().await {
                            let address = id.to_string();
                            let device_info =
                                Self::create_device_info(address.clone(), properties).await;

                            if let Ok(mut devices) = self.discovered_devices.lock() {
                                devices.insert(address.clone(), device_info.clone());
                            } else {
                                ble_error!("Failed to acquire device map lock");
                            }

                            if device_tx.send(device_info).is_err() {
                                ble_warn!("Failed to send device info through channel");
                            }
                        } else {
                            ble_debug!("Failed to get properties for device {:?}", id);
                        }
                    } else {
                        ble_debug!("Failed to get peripheral for device {:?}", id);
                    }
                }
                CentralEvent::DeviceUpdated(id) => {
                    if let Ok(peripheral) = self.adapter.peripheral(&id).await {
                        if let Ok(Some(properties)) = peripheral.properties().await {
                            let address = id.to_string();
                            let device_info =
                                Self::create_device_info(address.clone(), properties).await;

                            ble_debug!(
                                "Updated device: {} ({}), RSSI: {:?}",
                                device_info.name.as_ref().unwrap_or(&"Unknown".to_string()),
                                address,
                                device_info.rssi
                            );

                            let should_send =
                                if let Ok(mut devices) = self.discovered_devices.lock() {
                                    let exists = devices.contains_key(&address);
                                    devices.insert(address.clone(), device_info.clone());
                                    exists
                                } else {
                                    ble_error!("Failed to acquire device map lock");
                                    false
                                };

                            if should_send {
                                if device_tx.send(device_info).is_err() {
                                    ble_warn!("Failed to send device update through channel");
                                }
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
                                let address = id.to_string();
                                let device_info =
                                    Self::create_device_info(address.clone(), properties).await;
                                let should_send =
                                    if let Ok(mut devices) = self.discovered_devices.lock() {
                                        let exists = devices.contains_key(&address);
                                        devices.insert(address.clone(), device_info.clone());
                                        exists
                                    } else {
                                        false
                                    };
                                if should_send {
                                    let _ = device_tx.send(device_info);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // ble_info!("Device collection completed. Discovered {} unique devices from {} events", device_discovery_count, event_count);
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

    async fn create_device_info(address: String, properties: PeripheralProperties) -> DeviceInfo {
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
        let pairing_state = crate::windows_pairing::get_pairing_state_async(&address).await;

        DeviceInfo::new(
            address,
            name,
            rssi,
            services,
            manufacturer_data,
            service_data,
            tx_power_level,
            pairing_state.paired,
            pairing_state.can_pair,
            pairing_state.status,
        )
    }
}
