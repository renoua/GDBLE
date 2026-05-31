use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use godot::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ble_characteristic::{BleCharacteristicInfo, CharacteristicProperties};
use crate::ble_service::BleServiceInfo;
use crate::types::{BleDeviceEvent, BleError};
use crate::{ble_debug, ble_error, ble_info, ble_warn};

enum CharEventKind {
    Read,
    Write,
    Subscribe,
    Unsubscribe,
}

/// Recover from a poisoned Mutex by taking the inner value.
/// Logs the poisoning so it's visible in the output even without debug mode.
macro_rules! lock_or_recover {
    ($mutex:expr) => {
        $mutex.lock().unwrap_or_else(|e| {
            eprintln!("[BLE Error] Mutex poisoned, recovering guard: {}", e);
            e.into_inner()
        })
    };
}

/// BleDevice represents a single BLE peripheral device.
///
/// This class wraps a btleplug Peripheral and provides Godot-friendly
/// methods for connecting, disconnecting, and managing the device state.
///
/// Instances must be obtained via `BluetoothManager.connect_device()` —
/// direct GDScript instantiation (`BleDevice.new()`) will panic.
#[derive(GodotClass)]
#[class(base=RefCounted)]
pub struct BleDevice {
    base: Base<RefCounted>,

    /// The underlying btleplug peripheral
    peripheral: Arc<Peripheral>,

    /// Tokio runtime for executing async operations
    runtime: Arc<Runtime>,

    /// Device Bluetooth address
    address: GString,

    /// Device name (if available)
    name: GString,

    /// Connection state — shared with async tasks
    is_connected: Arc<Mutex<bool>>,

    /// Discovered services cache
    services: Arc<Mutex<Vec<BleServiceInfo>>>,

    /// Set of currently subscribed characteristic UUIDs (lowercase).
    /// Written from both the main thread (subscribe/unsubscribe) and the
    /// dispatcher task (on stream end, to clear all).
    subscribed_characteristics: Arc<Mutex<HashSet<String>>>,

    /// Single shared notification dispatcher task.
    /// Started on the first `subscribe_characteristic` call, aborted on disconnect.
    /// Only accessed from `#[func]` methods (main thread) — no Arc/Mutex needed.
    notification_dispatcher: Option<JoinHandle<()>>,

    /// Characteristic lookup cache populated after `discover_services`.
    /// Key: (service_uuid_lower, char_uuid_lower).
    char_cache: Arc<Mutex<HashMap<(String, String), Characteristic>>>,

    /// Event sender for thread-safe communication with BluetoothManager
    event_tx: Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>,
}

#[godot_api]
impl BleDevice {
    /// Signal emitted when device successfully connects
    #[signal]
    fn connected();

    /// Signal emitted when device disconnects (explicit or radio loss)
    #[signal]
    fn disconnected();

    /// Signal emitted when connection fails
    #[signal]
    fn connection_failed(error: GString);

    /// Signal emitted when services are discovered
    #[signal]
    fn services_discovered(services: Array<VarDictionary>);

    /// Signal emitted when a characteristic is read
    #[signal]
    fn characteristic_read(char_uuid: GString, data: PackedByteArray);

    /// Signal emitted when a characteristic is written
    #[signal]
    fn characteristic_written(char_uuid: GString);

    /// Signal emitted when a characteristic notification is received
    #[signal]
    fn characteristic_notified(char_uuid: GString, data: PackedByteArray);

    /// Signal emitted when an operation fails
    #[signal]
    fn operation_failed(operation: GString, error: GString);

    // -------------------------------------------------------------------------
    // Connection
    // -------------------------------------------------------------------------

    /// Asynchronously connect to the device
    #[func]
    fn connect_async(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Initiating connection to device: {}", address);

        let peripheral = self.peripheral.clone();
        let is_connected = self.is_connected.clone();
        let event_tx = self.event_tx.clone();

        self.runtime.spawn(async move {
            let result = match peripheral.connect().await {
                Ok(_) => {
                    *lock_or_recover!(is_connected) = true;
                    Ok(())
                }
                Err(e) => Err(BleError::ConnectionFailed(e.to_string())),
            };

            let event = match result {
                Ok(_) => BleDeviceEvent::ConnectSuccess {
                    device_address: address,
                },
                Err(error) => BleDeviceEvent::ConnectFailed {
                    device_address: address,
                    error: error.to_string(),
                },
            };
            Self::send_event_via(&event_tx, event);
        });
    }

    /// Disconnect from the device.
    /// Aborts the notification dispatcher, clears subscriptions, then calls
    /// the BLE disconnect and emits `Disconnected` via the event channel.
    #[func]
    fn disconnect(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Initiating disconnect for device: {}", address);

        // Abort the shared dispatcher task
        if let Some(handle) = self.notification_dispatcher.take() {
            handle.abort();
            ble_debug!("Aborted notification dispatcher for {}", address);
        }
        lock_or_recover!(self.subscribed_characteristics).clear();

        let peripheral = self.peripheral.clone();
        let is_connected = self.is_connected.clone();
        let event_tx = self.event_tx.clone();

        self.runtime.spawn(async move {
            match peripheral.disconnect().await {
                Ok(_) => {
                    ble_info!("Device {} disconnected", address);
                }
                Err(e) => {
                    let error = BleError::OperationFailed(format!("Disconnect failed: {}", e));
                    error.log_warning();
                }
            }
            *lock_or_recover!(is_connected) = false;
            Self::send_event_via(&event_tx, BleDeviceEvent::Disconnected { device_address: address });
        });
    }

    /// Check if the device is currently connected
    #[func]
    pub fn is_connected(&self) -> bool {
        *lock_or_recover!(self.is_connected)
    }

    /// Get the device's Bluetooth address
    #[func]
    fn get_address(&self) -> GString {
        self.address.clone()
    }

    /// Get the device's name
    #[func]
    fn get_name(&self) -> GString {
        self.name.clone()
    }

    // -------------------------------------------------------------------------
    // Service discovery
    // -------------------------------------------------------------------------

    /// Discover services on the connected device.
    /// On success emits `services_discovered` and populates the characteristic cache.
    #[func]
    fn discover_services(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Starting service discovery for device: {}", address);

        if !*lock_or_recover!(self.is_connected) {
            let error = BleError::NotConnected;
            error.log_error();
            self.base_mut().emit_signal(
                "operation_failed",
                &[
                    GString::from("discover_services").to_variant(),
                    error.to_gstring().to_variant(),
                ],
            );
            return;
        }

        let peripheral = self.peripheral.clone();
        let services_cache = self.services.clone();
        let char_cache = self.char_cache.clone();
        let event_tx = self.event_tx.clone();

        self.runtime.spawn(async move {
            ble_debug!("Calling peripheral.discover_services() for {}", address);

            let result = match peripheral.discover_services().await {
                Ok(_) => Ok(()),
                Err(e) => Err(BleError::ServiceDiscoveryFailed(e.to_string())),
            };

            match result {
                Ok(_) => {
                    ble_debug!("Service discovery successful for {}", address);

                    let services = peripheral.services();
                    let mut service_infos = Vec::new();

                    // Build char cache while iterating services
                    let mut new_cache: HashMap<(String, String), Characteristic> = HashMap::new();

                    for service in services {
                        let service_uuid_lower = service.uuid.to_string().to_lowercase();
                        ble_debug!(
                            "Found service {} with {} characteristics",
                            service.uuid,
                            service.characteristics.len()
                        );

                        let char_infos: Vec<BleCharacteristicInfo> = service
                            .characteristics
                            .iter()
                            .map(|c| {
                                // Populate cache
                                let char_uuid_lower = c.uuid.to_string().to_lowercase();
                                new_cache.insert(
                                    (service_uuid_lower.clone(), char_uuid_lower),
                                    c.clone(),
                                );
                                Self::convert_characteristic(c)
                            })
                            .collect();

                        let service_info =
                            BleServiceInfo::new(service.uuid.to_string(), char_infos);
                        service_infos.push(service_info);
                    }

                    ble_info!(
                        "Service discovery complete for {}: {} services, {} characteristics cached",
                        address,
                        service_infos.len(),
                        new_cache.len()
                    );

                    *lock_or_recover!(char_cache) = new_cache;
                    *lock_or_recover!(services_cache) = service_infos.clone();

                    Self::send_event_via(
                        &event_tx,
                        BleDeviceEvent::ServicesDiscovered {
                            device_address: address,
                            services: service_infos,
                        },
                    );
                }
                Err(error) => {
                    error.log_error();
                    Self::send_event_via(
                        &event_tx,
                        BleDeviceEvent::ServiceDiscoveryFailed {
                            device_address: address,
                            error: error.to_string(),
                        },
                    );
                }
            }
        });
    }

    /// Get the list of discovered services
    #[func]
    fn get_services(&self) -> Array<VarDictionary> {
        lock_or_recover!(self.services)
            .iter()
            .map(|s| s.to_dictionary())
            .collect()
    }

    // -------------------------------------------------------------------------
    // Read / Write
    // -------------------------------------------------------------------------

    /// Read a characteristic value
    #[func]
    fn read_characteristic(&mut self, service_uuid: GString, char_uuid: GString) {
        ble_debug!("Reading characteristic {} from service {}", char_uuid, service_uuid);

        if let Err(error) = self.check_connected() {
            error.log_error();
            self.base_mut().emit_signal(
                "operation_failed",
                &[
                    GString::from("read_characteristic").to_variant(),
                    error.to_gstring().to_variant(),
                ],
            );
            return;
        }

        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                let peripheral = self.peripheral.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();

                self.runtime.spawn(async move {
                    match peripheral.read(&char).await {
                        Ok(data) => {
                            ble_info!(
                                "Read {} bytes from characteristic {}",
                                data.len(),
                                char_uuid_for_async
                            );
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::CharacteristicRead {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                    data,
                                },
                            );
                        }
                        Err(e) => {
                            let error = BleError::ReadFailed(e.to_string());
                            error.log_error();
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::CharacteristicReadFailed {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                    error: error.to_string(),
                                },
                            );
                        }
                    }
                });
            }
            None => {
                Self::send_char_not_found_event(
                    &self.event_tx,
                    self.address.to_string(),
                    char_uuid_str,
                    &service_uuid_str,
                    CharEventKind::Read,
                );
            }
        }
    }

    /// Write data to a characteristic.
    /// Pass `with_response = true` for FTMS Control Point (0x2AD9) which requires
    /// WriteType::WithResponse and returns an indication.
    #[func]
    fn write_characteristic(
        &mut self,
        service_uuid: GString,
        char_uuid: GString,
        data: PackedByteArray,
        with_response: bool,
    ) {
        ble_debug!(
            "Writing {} bytes to characteristic {} ({}) from service {}",
            data.len(),
            char_uuid,
            if with_response { "with response" } else { "without response" },
            service_uuid
        );

        if let Err(error) = self.check_connected() {
            error.log_error();
            self.base_mut().emit_signal(
                "operation_failed",
                &[
                    GString::from("write_characteristic").to_variant(),
                    error.to_gstring().to_variant(),
                ],
            );
            return;
        }

        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                let peripheral = self.peripheral.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();
                let data_vec: Vec<u8> = data.to_vec();

                self.runtime.spawn(async move {
                    let write_type = if with_response {
                        WriteType::WithResponse
                    } else {
                        WriteType::WithoutResponse
                    };
                    match peripheral.write(&char, &data_vec, write_type).await {
                        Ok(_) => {
                            ble_info!(
                                "Wrote {} bytes to characteristic {}",
                                data_vec.len(),
                                char_uuid_for_async
                            );
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::CharacteristicWritten {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                },
                            );
                        }
                        Err(e) => {
                            let error = BleError::WriteFailed(e.to_string());
                            error.log_error();
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::CharacteristicWriteFailed {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                    error: error.to_string(),
                                },
                            );
                        }
                    }
                });
            }
            None => {
                Self::send_char_not_found_event(
                    &self.event_tx,
                    self.address.to_string(),
                    char_uuid_str,
                    &service_uuid_str,
                    CharEventKind::Write,
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // Notifications — single shared dispatcher
    // -------------------------------------------------------------------------

    /// Subscribe to characteristic notifications.
    ///
    /// A single dispatcher task is started on the first subscription and shared
    /// across all subscribed characteristics.  This avoids the N-handler / N-stream
    /// problem of the previous design, where each subscription opened a separate
    /// `peripheral.notifications()` stream that consumed all events.
    ///
    /// The UUID is inserted into `subscribed_characteristics` synchronously
    /// (before any async work) to prevent the TOCTOU race where two rapid calls
    /// both pass the "already subscribed?" check.
    #[func]
    fn subscribe_characteristic(&mut self, service_uuid: GString, char_uuid: GString) {
        ble_debug!(
            "Subscribing to characteristic {} from service {}",
            char_uuid,
            service_uuid
        );

        if let Err(error) = self.check_connected() {
            error.log_error();
            self.base_mut().emit_signal(
                "operation_failed",
                &[
                    GString::from("subscribe_characteristic").to_variant(),
                    error.to_gstring().to_variant(),
                ],
            );
            return;
        }

        let char_uuid_lower = char_uuid.to_string().to_lowercase();

        // ── TOCTOU fix: reserve the slot synchronously before any async work ──
        {
            let mut subs = lock_or_recover!(self.subscribed_characteristics);
            if subs.contains(&char_uuid_lower) {
                ble_warn!(
                    "Already subscribed to characteristic {}, ignoring",
                    char_uuid_lower
                );
                return;
            }
            subs.insert(char_uuid_lower.clone());
        }

        // ── Start the shared dispatcher if not already running ──
        // `notification_dispatcher` is `Option<JoinHandle<()>>`. We check
        // `is_finished()` to detect tasks that exited unexpectedly and restart them.
        let needs_dispatcher = self
            .notification_dispatcher
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true);

        if needs_dispatcher {
            if let Some(old) = self.notification_dispatcher.take() {
                old.abort();
            }

            let peripheral = self.peripheral.clone();
            let subscribed = self.subscribed_characteristics.clone();
            let event_tx = self.event_tx.clone();
            let is_connected = self.is_connected.clone();
            let address = self.address.to_string();
            let char_uuid_lower_for_cleanup = char_uuid_lower.clone();
            let subscribed_for_cleanup = self.subscribed_characteristics.clone();
            let event_tx_for_cleanup = self.event_tx.clone();

            let handle = self.runtime.spawn(async move {
                ble_debug!("Notification dispatcher started for {}", address);

                match peripheral.notifications().await {
                    Ok(mut stream) => {
                        while let Some(notification) = stream.next().await {
                            let uuid_lower = notification.uuid.to_string().to_lowercase();
                            let is_subscribed =
                                lock_or_recover!(subscribed).contains(&uuid_lower);
                            if is_subscribed {
                                ble_debug!(
                                    "Notification from {}: {} bytes",
                                    uuid_lower,
                                    notification.value.len()
                                );
                                Self::send_event_via(
                                    &event_tx,
                                    BleDeviceEvent::CharacteristicNotified {
                                        device_address: address.clone(),
                                        char_uuid: notification.uuid.to_string(),
                                        data: notification.value,
                                    },
                                );
                            }
                        }

                        // Stream ended — device disconnected (radio loss or explicit)
                        ble_info!(
                            "Notification stream ended for {} — emitting Disconnected",
                            address
                        );
                        *lock_or_recover!(is_connected) = false;
                        lock_or_recover!(subscribed_for_cleanup).clear();
                        Self::send_event_via(
                            &event_tx_for_cleanup,
                            BleDeviceEvent::Disconnected {
                                device_address: address.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        ble_error!(
                            "Failed to get notification stream for {}: {}",
                            address,
                            e
                        );
                        // Remove the reserved UUID so GDScript can retry
                        lock_or_recover!(subscribed_for_cleanup)
                            .remove(&char_uuid_lower_for_cleanup);
                        Self::send_event_via(
                            &event_tx_for_cleanup,
                            BleDeviceEvent::SubscribeFailed {
                                device_address: address,
                                char_uuid: char_uuid_lower_for_cleanup,
                                error: format!("notifications() failed: {}", e),
                            },
                        );
                    }
                }
            });

            self.notification_dispatcher = Some(handle);
        }

        // ── Spawn the actual BLE subscribe operation ──
        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                let peripheral = self.peripheral.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();
                let subscribed_on_fail = self.subscribed_characteristics.clone();
                let char_uuid_lower_on_fail = char_uuid_lower.clone();

                self.runtime.spawn(async move {
                    match peripheral.subscribe(&char).await {
                        Ok(_) => {
                            ble_info!("Subscribed to characteristic {}", char_uuid_for_async);
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::SubscribeSuccess {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                },
                            );
                        }
                        Err(e) => {
                            let error = BleError::SubscribeFailed(e.to_string());
                            error.log_error();
                            // Roll back the reservation so the caller can retry
                            lock_or_recover!(subscribed_on_fail)
                                .remove(&char_uuid_lower_on_fail);
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::SubscribeFailed {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                    error: error.to_string(),
                                },
                            );
                        }
                    }
                });
            }
            None => {
                // Roll back the reservation
                lock_or_recover!(self.subscribed_characteristics).remove(&char_uuid_lower);
                Self::send_char_not_found_event(
                    &self.event_tx,
                    self.address.to_string(),
                    char_uuid_str,
                    &service_uuid_str,
                    CharEventKind::Subscribe,
                );
            }
        }
    }

    /// Unsubscribe from characteristic notifications.
    ///
    /// The UUID is removed from `subscribed_characteristics` immediately so the
    /// dispatcher stops routing those notifications.  The actual BLE unsubscribe
    /// is done asynchronously; the dispatcher task keeps running for remaining subs.
    #[func]
    fn unsubscribe_characteristic(&mut self, service_uuid: GString, char_uuid: GString) {
        ble_debug!(
            "Unsubscribing from characteristic {} from service {}",
            char_uuid,
            service_uuid
        );

        if let Err(error) = self.check_connected() {
            error.log_error();
            self.base_mut().emit_signal(
                "operation_failed",
                &[
                    GString::from("unsubscribe_characteristic").to_variant(),
                    error.to_gstring().to_variant(),
                ],
            );
            return;
        }

        let char_uuid_lower = char_uuid.to_string().to_lowercase();

        // Remove from set immediately — dispatcher will stop routing this UUID
        lock_or_recover!(self.subscribed_characteristics).remove(&char_uuid_lower);

        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                let peripheral = self.peripheral.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();

                self.runtime.spawn(async move {
                    match peripheral.unsubscribe(&char).await {
                        Ok(_) => {
                            ble_info!(
                                "Unsubscribed from characteristic {}",
                                char_uuid_for_async
                            );
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::UnsubscribeSuccess {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                },
                            );
                        }
                        Err(e) => {
                            let error = BleError::UnsubscribeFailed(e.to_string());
                            error.log_error();
                            Self::send_event_via(
                                &event_tx,
                                BleDeviceEvent::UnsubscribeFailed {
                                    device_address: address,
                                    char_uuid: char_uuid_for_async,
                                    error: error.to_string(),
                                },
                            );
                        }
                    }
                });
            }
            None => {
                Self::send_char_not_found_event(
                    &self.event_tx,
                    self.address.to_string(),
                    char_uuid_str,
                    &service_uuid_str,
                    CharEventKind::Unsubscribe,
                );
            }
        }
    }
}

#[godot_api]
impl IRefCounted for BleDevice {
    /// Direct GDScript instantiation is not supported — use `BluetoothManager.connect_device()`.
    /// Godot-rust catches this panic at the FFI boundary (since 0.1.3), so the editor/game
    /// will not crash but the object will be null.
    fn init(_base: Base<RefCounted>) -> Self {
        panic!(
            "BleDevice cannot be instantiated directly from GDScript. \
             Use BluetoothManager.connect_device() to obtain a BleDevice instance."
        );
    }
}

impl Drop for BleDevice {
    fn drop(&mut self) {
        ble_info!("BleDevice: cleaning up resources for {}", self.address);
        self.cleanup();
    }
}

impl BleDevice {
    /// Internal constructor — called by BluetoothManager.
    pub fn new(
        peripheral: Peripheral,
        runtime: Arc<Runtime>,
        address: String,
        name: String,
        event_tx: Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>,
    ) -> Gd<Self> {
        Gd::from_init_fn(|base| Self {
            base,
            peripheral: Arc::new(peripheral),
            runtime,
            address: GString::from(&address),
            name: GString::from(&name),
            is_connected: Arc::new(Mutex::new(false)),
            services: Arc::new(Mutex::new(Vec::new())),
            subscribed_characteristics: Arc::new(Mutex::new(HashSet::new())),
            notification_dispatcher: None,
            char_cache: Arc::new(Mutex::new(HashMap::new())),
            event_tx: Some(event_tx),
        })
    }

    fn check_connected(&self) -> Result<(), BleError> {
        if *lock_or_recover!(self.is_connected) {
            Ok(())
        } else {
            Err(BleError::NotConnected)
        }
    }

    /// Look up a characteristic, using the post-`discover_services` cache first.
    /// Falls back to a linear scan of `peripheral.characteristics()` if the cache
    /// is empty (e.g., before service discovery, or on cache miss).
    fn find_characteristic(&self, service_uuid: &str, char_uuid: &str) -> Option<Characteristic> {
        let service_lower = service_uuid.to_lowercase();
        let char_lower = char_uuid.to_lowercase();

        // Fast path: cache lookup — O(1) vs O(n) linear scan
        if let Ok(cache) = self.char_cache.lock() {
            if let Some(c) = cache.get(&(service_lower, char_lower)) {
                return Some(c.clone());
            }
        }

        // Slow path: linear scan (used before discover_services or on cache miss)
        self.peripheral
            .characteristics()
            .into_iter()
            .find(|c| {
                c.uuid.to_string().eq_ignore_ascii_case(char_uuid)
                    && c.service_uuid
                        .to_string()
                        .eq_ignore_ascii_case(service_uuid)
            })
    }

    fn send_event_via(
        tx: &Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>,
        event: BleDeviceEvent,
    ) {
        if let Some(tx) = tx {
            if let Ok(tx_guard) = tx.lock() {
                let _ = tx_guard.send(event);
            }
        }
    }

    fn send_char_not_found_event(
        tx: &Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>,
        device_address: String,
        char_uuid: String,
        service_uuid: &str,
        event_type: CharEventKind,
    ) {
        let error =
            BleError::CharacteristicNotFound(format!("{} in service {}", char_uuid, service_uuid));
        error.log_error();

        let event = match event_type {
            CharEventKind::Read => BleDeviceEvent::CharacteristicReadFailed {
                device_address,
                char_uuid,
                error: error.to_string(),
            },
            CharEventKind::Write => BleDeviceEvent::CharacteristicWriteFailed {
                device_address,
                char_uuid,
                error: error.to_string(),
            },
            CharEventKind::Subscribe => BleDeviceEvent::SubscribeFailed {
                device_address,
                char_uuid,
                error: error.to_string(),
            },
            CharEventKind::Unsubscribe => BleDeviceEvent::UnsubscribeFailed {
                device_address,
                char_uuid,
                error: error.to_string(),
            },
        };
        Self::send_event_via(tx, event);
    }

    fn convert_characteristic(characteristic: &Characteristic) -> BleCharacteristicInfo {
        let props = CharacteristicProperties {
            read: characteristic.properties.contains(CharPropFlags::READ),
            write: characteristic.properties.contains(CharPropFlags::WRITE),
            write_without_response: characteristic
                .properties
                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
            notify: characteristic.properties.contains(CharPropFlags::NOTIFY),
            indicate: characteristic.properties.contains(CharPropFlags::INDICATE),
        };
        BleCharacteristicInfo::new(characteristic.uuid.to_string(), props)
    }

    /// Abort the dispatcher and initiate a BLE disconnect if still connected.
    fn cleanup(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Cleanup for device: {}", address);

        // Abort the shared dispatcher task
        if let Some(handle) = self.notification_dispatcher.take() {
            handle.abort();
            ble_debug!("Aborted notification dispatcher during cleanup for {}", address);
        }

        let is_connected = *lock_or_recover!(self.is_connected);

        if is_connected {
            ble_debug!("Device {} is connected, disconnecting during cleanup", address);
            let peripheral = self.peripheral.clone();
            let is_connected_clone = self.is_connected.clone();
            let subscribed_chars = self.subscribed_characteristics.clone();

            self.runtime.spawn(async move {
                match peripheral.disconnect().await {
                    Ok(_) => ble_info!("Device {} disconnected during cleanup", address),
                    Err(e) => ble_warn!(
                        "Error disconnecting device {} during cleanup: {}",
                        address,
                        e
                    ),
                }
                *lock_or_recover!(is_connected_clone) = false;
                lock_or_recover!(subscribed_chars).clear();
            });
        }
    }
}
