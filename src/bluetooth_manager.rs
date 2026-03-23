use btleplug::api::Manager as _;
use btleplug::platform::{Adapter, Manager};
use godot::classes::notify::NodeNotification;
use godot::classes::Object;
use godot::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::ble_device::BleDevice;
use crate::bluetooth_scanner::BluetoothScanner;
use crate::runtime::RuntimeManager;
use crate::types::{AdapterInfo, BleDeviceEvent, BleError, DeviceInfo, set_debug_mode, is_debug_mode};
use crate::{ble_debug, ble_info, ble_warn, ble_error};

/// BluetoothManager is the main entry point for BLE functionality in Godot
///
/// This node manages the Bluetooth adapter, runtime, and coordinates all BLE operations.
/// It provides methods for initialization, scanning, and device management.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct BluetoothManager {
    base: Base<Node>,

    /// The Bluetooth adapter instance
    adapter: Option<Arc<Adapter>>,

    /// Tokio runtime manager for async operations
    runtime: Option<Arc<RuntimeManager>>,

    /// Bluetooth scanner for device discovery
    scanner: Option<Arc<BluetoothScanner>>,

    /// Channel receiver for scan completion
    scan_complete_rx: Option<Arc<Mutex<mpsc::UnboundedReceiver<Result<(), String>>>>>,

    /// Channel receiver for discovered devices (real-time)
    device_rx: Option<Arc<Mutex<mpsc::UnboundedReceiver<DeviceInfo>>>>,

    /// Channel receiver for device events (thread-safe communication)
    device_event_rx: Option<Arc<Mutex<mpsc::UnboundedReceiver<BleDeviceEvent>>>>,

    /// Channel sender for device events (shared with all BleDevice instances)
    device_event_tx: Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>,

    /// Map of connected devices by address
    devices: Arc<Mutex<HashMap<String, Gd<BleDevice>>>>,

    /// Initialization state
    is_initialized: Arc<Mutex<bool>>,
}

#[godot_api]
impl INode for BluetoothManager {
    fn init(base: Base<Node>) -> Self {
        ble_info!("BluetoothManager: Initializing");

        Self {
            base,
            adapter: None,
            runtime: None,
            scanner: None,
            scan_complete_rx: None,
            device_rx: None,
            device_event_rx: None,
            device_event_tx: None,
            devices: Arc::new(Mutex::new(HashMap::new())),
            is_initialized: Arc::new(Mutex::new(false)),
        }
    }

    /// Called when the node enters the scene tree
    fn ready(&mut self) {
        ble_info!("BluetoothManager: Ready");
        self.base_mut().set_process(true);
    }

    /// Called every frame to process scan results and discovered devices
    fn process(&mut self, _delta: f64) {
        let mut devices_to_emit = Vec::new();
        if let Some(ref rx_arc) = self.device_rx {
            if let Ok(mut rx) = rx_arc.lock() {
                while let Ok(device_info) = rx.try_recv() {
                    devices_to_emit.push(device_info);
                }
            }
        }

        for device_info in devices_to_emit {
            let dict = device_info.to_dictionary();
            self.base_mut()
                .emit_signal("device_discovered", &[dict.to_variant()]);
        }

        let complete_opt = if let Some(ref rx_arc) = self.scan_complete_rx {
            if let Ok(mut rx) = rx_arc.lock() {
                rx.try_recv().ok()
            } else {
                None
            }
        } else {
            None
        };

        if let Some(result) = complete_opt {
            self.scan_complete_rx = None;
            self.device_rx = None;

            match result {
                Ok(()) => {
                    self.base_mut().emit_signal("scan_stopped", &[]);
                }
                Err(error_msg) => {
                    self.base_mut().emit_signal(
                        "error_occurred",
                        &[GString::from(error_msg.as_str()).to_variant()],
                    );
                    self.base_mut().emit_signal("scan_stopped", &[]);
                }
            }
        }

        let events = self.collect_device_events();
        for event in events {
            self.emit_event_deferred(event);
        }
    }

    /// Called when the node receives a notification
    fn on_notification(&mut self, what: NodeNotification) {
        if what == NodeNotification::PREDELETE {
            ble_info!("BluetoothManager: Cleaning up resources");
            self.cleanup();
        }
    }
}

#[godot_api]
impl BluetoothManager {
    /// Signal emitted when adapter initialization completes
    ///
    /// # Parameters
    /// * `success` - Whether initialization succeeded
    /// * `error` - Error message if initialization failed
    #[signal]
    fn adapter_initialized(success: bool, error: GString);

    /// Signal emitted when a device is discovered during scanning
    ///
    /// # Parameters
    /// * `device_info` - VarDictionary containing device information
    #[signal]
    fn device_discovered(device_info: VarDictionary);

    /// Signal emitted when a device's information is updated
    ///
    /// # Parameters
    /// * `device_info` - VarDictionary containing updated device information
    #[signal]
    fn device_updated(device_info: VarDictionary);

    /// Signal emitted when scanning starts
    #[signal]
    fn scan_started();

    /// Signal emitted when scanning stops
    #[signal]
    fn scan_stopped();

    /// Signal emitted when an error occurs
    ///
    /// # Parameters
    /// * `error_message` - Description of the error
    #[signal]
    fn error_occurred(error_message: GString);

    /// Signal emitted when a device connection is initiated
    ///
    /// # Parameters
    /// * `address` - Device address
    #[signal]
    fn device_connecting(address: GString);

    /// Signal emitted when a device successfully connects
    ///
    /// # Parameters
    /// * `address` - Device address
    #[signal]
    fn device_connected(address: GString);

    /// Signal emitted when a device disconnects
    ///
    /// # Parameters
    /// * `address` - Device address
    #[signal]
    fn device_disconnected(address: GString);

    /// Enable or disable debug mode
    ///
    /// When debug mode is enabled, detailed operation logs are output to the console.
    ///
    /// # Parameters
    /// * `enabled` - true to enable debug mode, false to disable
    #[func]
    pub fn set_debug_mode(&self, enabled: bool) {
        set_debug_mode(enabled);
        if enabled {
            ble_info!("Debug mode enabled");
        } else {
            ble_info!("Debug mode disabled");
        }
    }

    /// Check if debug mode is enabled
    ///
    /// # Returns
    /// true if debug mode is enabled, false otherwise
    #[func]
    pub fn is_debug_mode(&self) -> bool {
        is_debug_mode()
    }

    /// Initialize the Bluetooth adapter
    ///
    /// This method must be called before any other BLE operations.
    /// It acquires the system's default Bluetooth adapter and sets up
    /// the async runtime. This is a blocking operation.
    #[func]
    pub fn initialize(&mut self) {
        ble_info!("Starting Bluetooth adapter initialization");
        ble_debug!("Checking initialization state");

        // Check if already initialized
        if self.check_already_initialized() {
            ble_warn!("Adapter already initialized, skipping initialization");
            self.base_mut().emit_signal(
                "adapter_initialized",
                &[true.to_variant(), GString::new().to_variant()],
            );
            return;
        }

        ble_debug!("Creating Tokio runtime manager");
        // Create runtime manager
        let runtime_manager = RuntimeManager::new();
        self.runtime = Some(Arc::new(runtime_manager));

        // Get adapter synchronously using block_on
        ble_debug!("Acquiring Bluetooth adapter");
        let result = if let Some(ref runtime_mgr) = self.runtime {
            runtime_mgr.block_on(Self::get_adapter_async())
        } else {
            let error = BleError::InitializationFailed("Runtime not created".to_string());
            error.log_error();
            Err(error)
        };

        match result {
            Ok(adapter) => {
                ble_info!("Bluetooth adapter acquired successfully");
                let adapter_arc = Arc::new(adapter);
                self.adapter = Some(adapter_arc.clone());

                // Create scanner
                ble_debug!("Creating Bluetooth scanner");
                if let Some(ref runtime_mgr) = self.runtime {
                    let scanner = BluetoothScanner::new(adapter_arc, runtime_mgr.runtime());
                    self.scanner = Some(Arc::new(scanner));
                    ble_debug!("Scanner created successfully");
                }

                // Create device event channel (shared across all devices)
                let (event_tx, event_rx) = mpsc::unbounded_channel::<BleDeviceEvent>();
                self.device_event_tx = Some(Arc::new(Mutex::new(event_tx)));
                self.device_event_rx = Some(Arc::new(Mutex::new(event_rx)));
                ble_debug!("Device event channel created");

                // Mark as initialized
                self.set_initialized(true);
                ble_info!("Bluetooth initialization complete");

                self.base_mut().emit_signal(
                    "adapter_initialized",
                    &[true.to_variant(), GString::new().to_variant()],
                );
            }
            Err(e) => {
                e.log_error();
                let error_msg = GString::from(e.to_string().as_str());
                self.base_mut().emit_signal(
                    "adapter_initialized",
                    &[false.to_variant(), error_msg.to_variant()],
                );
                self.base_mut().emit_signal(
                    "error_occurred",
                    &[error_msg.to_variant()],
                );
            }
        }
    }

    fn check_already_initialized(&self) -> bool {
        match self.is_initialized.lock() {
            Ok(guard) => *guard,
            Err(_) => {
                ble_error!("Failed to acquire initialization lock");
                false
            }
        }
    }

    fn set_initialized(&self, value: bool) {
        if let Ok(mut guard) = self.is_initialized.lock() {
            *guard = value;
        } else {
            ble_error!("Failed to update initialization state");
        }
    }

    /// Check if the adapter is initialized
    ///
    /// # Returns
    /// `true` if the adapter is ready for use, `false` otherwise
    #[func]
    pub fn is_initialized(&self) -> bool {
        if let Ok(initialized) = self.is_initialized.lock() {
            *initialized
        } else {
            false
        }
    }

    /// Get information about the Bluetooth adapter
    ///
    /// # Returns
    /// A VarDictionary containing adapter information (name, address)
    /// Returns an empty VarDictionary if not initialized
    #[func]
    pub fn get_adapter_info(&self) -> VarDictionary {
        if !self.is_initialized() {
            godot_warn!("BluetoothManager: Adapter not initialized");
            return VarDictionary::new();
        }

        if let Some(ref _adapter) = self.adapter {
            // Get adapter info
            let info = AdapterInfo::new(
                "System Bluetooth Adapter".to_string(),
                None, // btleplug doesn't provide adapter address easily
            );
            info.to_dictionary()
        } else {
            VarDictionary::new()
        }
    }

    /// Start scanning for BLE devices
    ///
    /// Initiates a BLE device scan that will run for the specified duration.
    /// Discovered devices are reported via the device_discovered signal.
    ///
    /// # Parameters
    /// * `timeout_seconds` - How long to scan for devices (default: 10.0 seconds)
    #[func]
    pub fn start_scan(&mut self, timeout_seconds: f64) {
        ble_debug!("start_scan called with timeout: {} seconds", timeout_seconds);

        if !self.is_initialized() {
            let error = BleError::InitializationFailed("Adapter not initialized".to_string());
            error.log_error();
            self.base_mut().emit_signal(
                "error_occurred",
                &[error.to_gstring().to_variant()],
            );
            return;
        }

        let scanner = match &self.scanner {
            Some(s) => s.clone(),
            None => {
                let error = BleError::InternalError("Scanner not available".to_string());
                error.log_error();
                self.base_mut().emit_signal(
                    "error_occurred",
                    &[error.to_gstring().to_variant()],
                );
                return;
            }
        };

        if scanner.is_scanning() {
            ble_warn!("Scan already in progress, ignoring request");
            return;
        }

        ble_info!("Starting BLE device scan for {} seconds", timeout_seconds);

        // Emit scan_started signal
        self.base_mut().emit_signal("scan_started", &[]);

        let duration = Duration::from_secs_f64(timeout_seconds);

        // Create channels for scan completion and device discovery
        let (complete_tx, complete_rx) = mpsc::unbounded_channel();
        let (device_tx, device_rx) = mpsc::unbounded_channel();
        
        self.scan_complete_rx = Some(Arc::new(Mutex::new(complete_rx)));
        self.device_rx = Some(Arc::new(Mutex::new(device_rx)));

        // Execute scan task asynchronously
        if let Some(ref runtime_mgr) = self.runtime {
            ble_debug!("Spawning scan task asynchronously");
            runtime_mgr.spawn(async move {
                ble_debug!("Scan task started");
                ble_debug!("About to call scanner.start_scan()");
                let scan_result = match scanner.start_scan(duration, device_tx).await {
                    Ok(()) => {
                        ble_debug!("scanner.start_scan() returned Ok");
                        let devices = scanner.get_devices();
                        ble_info!("Scan completed successfully, found {} devices", devices.len());
                        ble_debug!("Discovered devices: {:?}", devices);
                        Ok(())
                    }
                    Err(e) => {
                        ble_debug!("scanner.start_scan() returned Err: {}", e);
                        e.log_error();
                        Err(e.to_string())
                    }
                };
                
                ble_debug!("Scan task completed, sending completion signal through channel");
                // Send completion signal through channel
                if complete_tx.send(scan_result).is_err() {
                    ble_error!("Failed to send scan completion through channel");
                } else {
                    ble_debug!("Completion signal sent through channel successfully");
                }
            });
        } else {
            let error = BleError::InternalError("Runtime not available".to_string());
            error.log_error();
            self.base_mut().emit_signal(
                "error_occurred",
                &[error.to_gstring().to_variant()],
            );
        }
    }

    /// Stop an ongoing BLE device scan
    ///
    /// Stops the current scan if one is in progress.
    #[func]
    pub fn stop_scan(&mut self) {
        if !self.is_initialized() {
            godot_warn!("BluetoothManager: Cannot stop scan - adapter not initialized");
            return;
        }

        let Some(ref scanner) = self.scanner else {
            godot_warn!("BluetoothManager: Scanner not available");
            return;
        };

        if !scanner.is_scanning() {
            godot_warn!("BluetoothManager: Not currently scanning");
            return;
        }

        godot_print!("BluetoothManager: Stopping scan");
        scanner.stop_scan();

        // Emit scan_stopped signal
        self.base_mut().emit_signal("scan_stopped", &[]);
    }

    /// Get all discovered devices from the last scan
    ///
    /// # Returns
    /// An Array of VarDictionaries, each containing device information
    #[func]
    pub fn get_discovered_devices(&self) -> Array<VarDictionary> {
        if !self.is_initialized() {
            godot_warn!("BluetoothManager: Adapter not initialized");
            return Array::new();
        }

        let Some(ref scanner) = self.scanner else {
            godot_warn!("BluetoothManager: Scanner not available");
            return Array::new();
        };

        let devices = scanner.get_devices();
        devices
            .iter()
            .map(|device| device.to_dictionary())
            .collect()
    }

    /// Connect to a BLE device by address
    ///
    /// Creates a BleDevice instance and initiates connection. The device object
    /// is stored in the internal device map and can be retrieved later.
    ///
    /// # Parameters
    /// * `address` - The Bluetooth address of the device to connect to
    ///
    /// # Returns
    /// A BleDevice instance that can be used to interact with the device,
    /// or None if the device cannot be found or connection fails
    #[func]
    pub fn connect_device(&mut self, address: GString) -> Option<Gd<BleDevice>> {
        let address_str = address.to_string();
        ble_debug!("connect_device called for address: {}", address_str);

        if !self.is_initialized() {
            let error = BleError::InitializationFailed("Adapter not initialized".to_string());
            error.log_error();
            self.base_mut().emit_signal(
                "error_occurred",
                &[error.to_gstring().to_variant()],
            );
            return None;
        }

        // Check if device is already connected
        {
            let devices = self.devices.lock().unwrap();
            if let Some(existing_device) = devices.get(&address_str) {
                ble_info!("Device {} already connected, returning existing instance", address_str);
                return Some(existing_device.clone());
            }
        }

        let runtime = match &self.runtime {
            Some(r) => r.runtime(),
            None => {
                let error = BleError::InternalError("Runtime not available".to_string());
                error.log_error();
                self.base_mut().emit_signal(
                    "error_occurred",
                    &[error.to_gstring().to_variant()],
                );
                return None;
            }
        };

        // Find the peripheral from discovered devices
        let adapter = self.adapter.as_ref()?.clone();
        let address_clone = address_str.clone();

        ble_debug!("Searching for peripheral with address: {}", address_clone);
        // Use block_on to find the peripheral
        let peripheral_result = runtime.block_on(async move {
            use btleplug::api::{Central, Peripheral as _};
            
            // Get all peripherals
            let peripherals = adapter.peripherals().await.ok()?;
            ble_debug!("Found {} total peripherals", peripherals.len());
            
            // Find the one matching our address (UUID or MAC address)
            for peripheral in peripherals {
                let props = peripheral.properties().await.ok()??;
                
                // Try to match by UUID first (primary method for macOS)
                let peripheral_id = peripheral.id();
                let peripheral_uuid = peripheral_id.to_string();
                if peripheral_uuid.eq_ignore_ascii_case(&address_clone) {
                    ble_debug!("Found matching peripheral by UUID: {}", peripheral_uuid);
                    return Some(peripheral);
                }
                
                // Also try MAC address as fallback (avoid matching 00:00:00:00:00:00)
                let addr = props.address.to_string();
                if !addr.eq_ignore_ascii_case("00:00:00:00:00:00") {
                    if addr.eq_ignore_ascii_case(&address_clone) {
                        ble_debug!("Found matching peripheral by MAC: {}", addr);
                        return Some(peripheral);
                    }
                }
            }
            
            ble_debug!("No matching peripheral found for address: {}", address_clone);
            None
        });

        let peripheral = match peripheral_result {
            Some(p) => p,
            None => {
                let error = BleError::DeviceNotFound(address_str.clone());
                error.log_error();
                ble_warn!("Device {} not found. Make sure to scan first.", address_str);
                self.base_mut().emit_signal(
                    "error_occurred",
                    &[error.to_gstring().to_variant()],
                );
                return None;
            }
        };

        ble_debug!("Creating BleDevice instance for {}", address_str);
        
        let event_tx = match &self.device_event_tx {
            Some(tx) => tx.clone(),
            None => {
                let error = BleError::InternalError("Event channel not initialized".to_string());
                error.log_error();
                self.base_mut().emit_signal(
                    "error_occurred",
                    &[error.to_gstring().to_variant()],
                );
                return None;
            }
        };

        let device = BleDevice::new(peripheral, runtime.clone(), event_tx);

        {
            let mut devices = self.devices.lock().unwrap();
            devices.insert(address_str.clone(), device.clone());
        }

        ble_info!("Created BleDevice for {}", address_str);

        self.base_mut()
            .emit_signal("device_connecting", &[address.to_variant()]);

        Some(device)
    }

    /// Disconnect a BLE device by address
    #[func]
    pub fn disconnect_device(&mut self, address: GString) {
        let address_str = address.to_string();

        let device = {
            let devices = self.devices.lock().unwrap();
            devices.get(&address_str).cloned()
        };

        match device {
            Some(mut dev) => {
                ble_info!("BluetoothManager: Disconnecting device {}", address_str);
                dev.call("disconnect", &[]);
            }
            None => {
                ble_warn!(
                    "BluetoothManager: Device {} not found in connected devices",
                    address_str
                );
            }
        }
    }

    /// Get a connected device by address
    #[func]
    pub fn get_device(&self, address: GString) -> Option<Gd<BleDevice>> {
        let address_str = address.to_string();
        let devices = self.devices.lock().unwrap();
        devices.get(&address_str).cloned()
    }

    /// Get all connected devices
    ///
    /// # Returns
    /// An Array of BleDevice instances
    #[func]
    pub fn get_connected_devices(&self) -> Array<Gd<BleDevice>> {
        let devices = self.devices.lock().unwrap();
        devices.values().cloned().collect()
    }

    /// Async helper to get the Bluetooth adapter
    async fn get_adapter_async() -> Result<Adapter, BleError> {
        let manager = Manager::new()
            .await
            .map_err(|e| BleError::InitializationFailed(e.to_string()))?;

        let adapters = manager
            .adapters()
            .await
            .map_err(|e| BleError::InitializationFailed(e.to_string()))?;

        adapters.into_iter().next().ok_or(BleError::AdapterNotFound)
    }

    fn collect_device_events(&mut self) -> Vec<BleDeviceEvent> {
        let mut events = Vec::new();
        if let Some(ref rx_arc) = self.device_event_rx {
            if let Ok(mut rx) = rx_arc.lock() {
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }
            }
        }
        events
    }

    fn emit_event_deferred(&mut self, event: BleDeviceEvent) {
        match event {
            BleDeviceEvent::ConnectSuccess { device_address } => {
                ble_info!("Device {} connected successfully", device_address);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred("emit_signal", &["connected".to_variant()]);
                }
                self.base_mut().call_deferred(
                    "emit_signal",
                    &["device_connected".to_variant(), GString::from(&device_address).to_variant()],
                );
            }
            BleDeviceEvent::ConnectFailed { device_address, error } => {
                ble_error!("Device {} connection failed: {}", device_address, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &["connection_failed".to_variant(), GString::from(&error).to_variant()],
                    );
                }
            }
            BleDeviceEvent::Disconnected { device_address } => {
                ble_info!("Device {} disconnected", device_address);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred("emit_signal", &["disconnected".to_variant()]);
                }
                {
                    let mut devices = self.devices.lock().unwrap();
                    devices.remove(&device_address);
                }
                self.base_mut().call_deferred(
                    "emit_signal",
                    &["device_disconnected".to_variant(), GString::from(&device_address).to_variant()],
                );
            }
            BleDeviceEvent::ServicesDiscovered { device_address, services } => {
                ble_info!("Device {} discovered {} services", device_address, services.len());
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let services_array: Array<VarDictionary> =
                        services.iter().map(|s| s.to_dictionary()).collect();
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &["services_discovered".to_variant(), services_array.to_variant()],
                    );
                }
            }
            BleDeviceEvent::ServiceDiscoveryFailed { device_address, error } => {
                ble_error!("Device {} service discovery failed: {}", device_address, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "operation_failed".to_variant(),
                            GString::from("discover_services").to_variant(),
                            GString::from(&error).to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::CharacteristicRead { device_address, char_uuid, data } => {
                ble_debug!("Device {} read {} bytes from {}", device_address, data.len(), char_uuid);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let packed_data = PackedByteArray::from(&data[..]);
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "characteristic_read".to_variant(),
                            GString::from(&char_uuid).to_variant(),
                            packed_data.to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::CharacteristicReadFailed { device_address, char_uuid, error } => {
                ble_error!("Device {} failed to read {}: {}", device_address, char_uuid, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "operation_failed".to_variant(),
                            GString::from("read_characteristic").to_variant(),
                            GString::from(&error).to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::CharacteristicWritten { device_address, char_uuid } => {
                ble_debug!("Device {} wrote to {}", device_address, char_uuid);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &["characteristic_written".to_variant(), GString::from(&char_uuid).to_variant()],
                    );
                }
            }
            BleDeviceEvent::CharacteristicWriteFailed { device_address, char_uuid, error } => {
                ble_error!("Device {} failed to write {}: {}", device_address, char_uuid, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "operation_failed".to_variant(),
                            GString::from("write_characteristic").to_variant(),
                            GString::from(&error).to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::CharacteristicNotified { device_address, char_uuid, data } => {
                ble_debug!("Device {} notification from {}: {} bytes", device_address, char_uuid, data.len());
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let packed_data = PackedByteArray::from(&data[..]);
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "characteristic_notified".to_variant(),
                            GString::from(&char_uuid).to_variant(),
                            packed_data.to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::SubscribeSuccess { device_address, char_uuid } => {
                ble_info!("Device {} subscribed to {}", device_address, char_uuid);
            }
            BleDeviceEvent::SubscribeFailed { device_address, char_uuid, error } => {
                ble_error!("Device {} failed to subscribe {}: {}", device_address, char_uuid, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "operation_failed".to_variant(),
                            GString::from("subscribe_characteristic").to_variant(),
                            GString::from(&error).to_variant(),
                        ],
                    );
                }
            }
            BleDeviceEvent::UnsubscribeSuccess { device_address, char_uuid } => {
                ble_info!("Device {} unsubscribed from {}", device_address, char_uuid);
            }
            BleDeviceEvent::UnsubscribeFailed { device_address, char_uuid, error } => {
                ble_error!("Device {} failed to unsubscribe {}: {}", device_address, char_uuid, error);
                if let Some(device) = self.get_device_by_address(&device_address) {
                    let mut obj: Gd<Object> = device.upcast();
                    obj.call_deferred(
                        "emit_signal",
                        &[
                            "operation_failed".to_variant(),
                            GString::from("unsubscribe_characteristic").to_variant(),
                            GString::from(&error).to_variant(),
                        ],
                    );
                }
            }
        }
    }

    fn get_device_by_address(&self, address: &str) -> Option<Gd<BleDevice>> {
        let devices = self.devices.lock().unwrap();
        devices.get(address).cloned()
    }

    /// Clean up resources when the node is destroyed
    fn cleanup(&mut self) {
        ble_info!("Performing cleanup of Bluetooth resources");

        // Stop any ongoing scan
        if let Some(ref scanner) = self.scanner {
            if scanner.is_scanning() {
                ble_debug!("Stopping active scan during cleanup");
                scanner.stop_scan();
            }
        }

        // Clone device list first to avoid holding lock while calling disconnect
        let devices_to_disconnect: Vec<Gd<BleDevice>> = {
            let devices = self.devices.lock().unwrap();
            if !devices.is_empty() {
                ble_debug!("Disconnecting {} devices during cleanup", devices.len());
            }
            devices.values().cloned().collect()
        };

        // Disconnect devices without holding the lock
        for device in devices_to_disconnect {
            let mut device = device;
            ble_debug!("Disconnecting device");
            device.call("disconnect", &[]);
        }

        // Clear devices map
        if let Ok(mut devices) = self.devices.lock() {
            devices.clear();
            ble_debug!("Cleared device map");
        } else {
            ble_error!("Failed to acquire device map lock during cleanup");
        }

        // Mark as not initialized
        if let Ok(mut initialized) = self.is_initialized.lock() {
            *initialized = false;
            ble_debug!("Reset initialization state");
        } else {
            ble_error!("Failed to acquire initialization lock during cleanup");
        }

        // Drop scanner, adapter and runtime
        self.scanner = None;
        self.adapter = None;
        self.runtime = None;
        
        ble_info!("Bluetooth cleanup complete");
    }
}
