use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use godot::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ble_characteristic::{BleCharacteristicInfo, CharacteristicProperties};
use crate::ble_service::BleServiceInfo;
use crate::types::{BleDeviceEvent, BleError};
use crate::{ble_debug, ble_info, ble_warn, ble_error};

enum CharEventKind {
    Read,
    Write,
    Subscribe,
    Unsubscribe,
}

/// BleDevice represents a single BLE peripheral device
///
/// This class wraps a btleplug Peripheral and provides Godot-friendly
/// methods for connecting, disconnecting, and managing the device state.
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

    /// Connection state
    is_connected: Arc<Mutex<bool>>,

    /// Discovered services
    services: Arc<Mutex<Vec<BleServiceInfo>>>,

    /// Set of subscribed characteristic UUIDs (stored as lowercase strings)
    subscribed_characteristics: Arc<Mutex<HashSet<String>>>,

    /// Notification task handles by characteristic UUID (for cleanup)
    notification_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,

    /// Event sender for thread-safe communication with BluetoothManager
    event_tx: Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>,
}

#[godot_api]
impl BleDevice {
    /// Signal emitted when device successfully connects
    #[signal]
    fn connected();

    /// Signal emitted when device disconnects
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

    /// Asynchronously connect to the device
    #[func]
    fn connect_async(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Initiating connection to device: {}", address);

        let peripheral = self.peripheral.clone();
        let runtime = self.runtime.clone();
        let is_connected = self.is_connected.clone();
        let event_tx = self.event_tx.clone();

        runtime.spawn(async move {
            let result = match peripheral.connect().await {
                Ok(_) => {
                    *is_connected.lock().unwrap() = true;
                    Ok(())
                }
                Err(e) => {
                    Err(BleError::ConnectionFailed(e.to_string()))
                }
            };

            if let Some(tx) = event_tx {
                let event = match result {
                    Ok(_) => BleDeviceEvent::ConnectSuccess {
                        device_address: address,
                    },
                    Err(error) => BleDeviceEvent::ConnectFailed {
                        device_address: address,
                        error: error.to_string(),
                    },
                };
                if let Ok(tx_guard) = tx.lock() {
                    let _ = tx_guard.send(event);
                }
            }
        });
    }

    /// Disconnect from the device
    #[func]
    fn disconnect(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Initiating disconnect for device: {}", address);

        // Abort all notification tasks
        let notification_tasks = self.notification_tasks.lock().unwrap();
        let task_count = notification_tasks.len();
        if task_count > 0 {
            ble_debug!("Aborting {} notification tasks", task_count);
            for (_, handle) in notification_tasks.iter() {
                handle.abort();
            }
        }
        drop(notification_tasks);
        self.notification_tasks.lock().unwrap().clear();

        let peripheral = self.peripheral.clone();
        let runtime = self.runtime.clone();
        let is_connected = self.is_connected.clone();
        let subscribed_chars = self.subscribed_characteristics.clone();
        let event_tx = self.event_tx.clone();

        runtime.spawn(async move {
            match peripheral.disconnect().await {
                Ok(_) => {
                    *is_connected.lock().unwrap() = false;
                    let sub_count = subscribed_chars.lock().unwrap().len();
                    subscribed_chars.lock().unwrap().clear();
                    if sub_count > 0 {
                        ble_debug!("Cleared {} subscriptions", sub_count);
                    }
                }
                Err(e) => {
                    let error = BleError::OperationFailed(format!("Disconnect failed: {}", e));
                    error.log_warning();
                    *is_connected.lock().unwrap() = false;
                    subscribed_chars.lock().unwrap().clear();
                }
            }

            if let Some(tx) = event_tx {
                let event = BleDeviceEvent::Disconnected {
                    device_address: address,
                };
                if let Ok(tx_guard) = tx.lock() {
                    let _ = tx_guard.send(event);
                }
            }
        });
    }

    /// Check if the device is currently connected
    #[func]
    pub fn is_connected(&self) -> bool {
        *self.is_connected.lock().unwrap()
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

    /// Discover services on the connected device
    #[func]
    fn discover_services(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Starting service discovery for device: {}", address);

        if !*self.is_connected.lock().unwrap() {
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
        let runtime = self.runtime.clone();
        let services_cache = self.services.clone();
        let event_tx = self.event_tx.clone();

        runtime.spawn(async move {
            ble_debug!("About to call peripheral.discover_services() for {}", address);
            
            let result = match peripheral.discover_services().await {
                Ok(_) => Ok(()),
                Err(e) => Err(BleError::ServiceDiscoveryFailed(e.to_string())),
            };

            match result {
                Ok(_) => {
                    ble_debug!("Service discovery successful for device: {}", address);
                    
                    let services = peripheral.services();
                    let mut service_infos = Vec::new();

                    for service in services {
                        let char_count = service.characteristics.len();
                        ble_debug!(
                            "Found service {} with {} characteristics",
                            service.uuid,
                            char_count
                        );

                        let char_infos: Vec<BleCharacteristicInfo> = service
                            .characteristics
                            .iter()
                            .map(|c| Self::convert_characteristic(c))
                            .collect();
                        let service_info =
                            BleServiceInfo::new(service.uuid.to_string(), char_infos);
                        service_infos.push(service_info);
                    }

                    ble_info!(
                        "Service discovery complete for device {}: found {} services",
                        address,
                        service_infos.len()
                    );

                    *services_cache.lock().unwrap() = service_infos.clone();

                    if let Some(tx) = event_tx {
                        let event = BleDeviceEvent::ServicesDiscovered {
                            device_address: address,
                            services: service_infos,
                        };
                        if let Ok(tx_guard) = tx.lock() {
                            let _ = tx_guard.send(event);
                        }
                    }
                }
                Err(error) => {
                    error.log_error();

                    if let Some(tx) = event_tx {
                        let event = BleDeviceEvent::ServiceDiscoveryFailed {
                            device_address: address,
                            error: error.to_string(),
                        };
                        if let Ok(tx_guard) = tx.lock() {
                            let _ = tx_guard.send(event);
                        }
                    }
                }
            }
        });
    }

    /// Get the list of discovered services
    #[func]
    fn get_services(&self) -> Array<VarDictionary> {
        let services = self.services.lock().unwrap();
        services.iter().map(|s| s.to_dictionary()).collect()
    }

    /// Read a characteristic value
    #[func]
    fn read_characteristic(&mut self, service_uuid: GString, char_uuid: GString) {
        ble_debug!(
            "Reading characteristic {} from service {}",
            char_uuid,
            service_uuid
        );

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
                ble_debug!("Found characteristic {}, reading value", char_uuid_str);
                
                let peripheral = self.peripheral.clone();
                let runtime = self.runtime.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();
                
                runtime.spawn(async move {
                    let result = peripheral.read(&char).await;

                    match result {
                        Ok(data) => {
                            ble_info!(
                                "Successfully read {} bytes from characteristic {}",
                                data.len(),
                                char_uuid_for_async
                            );
                            ble_debug!("Read data: {:?}", data);

                            let event = BleDeviceEvent::CharacteristicRead {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                                data,
                            };
                            Self::send_event_via(&event_tx, event);
                        }
                        Err(e) => {
                            let error = BleError::ReadFailed(e.to_string());
                            error.log_error();

                            let event = BleDeviceEvent::CharacteristicReadFailed {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                                error: error.to_string(),
                            };
                            Self::send_event_via(&event_tx, event);
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

    /// Write data to a characteristic
    #[func]
    fn write_characteristic(
        &mut self,
        service_uuid: GString,
        char_uuid: GString,
        data: PackedByteArray,
        with_response: bool,
    ) {
        let write_mode = if with_response { "with response" } else { "without response" };
        ble_debug!(
            "Writing {} bytes to characteristic {} ({}) from service {}",
            data.len(),
            char_uuid,
            write_mode,
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
                ble_debug!("Found characteristic {}, writing data", char_uuid_str);
                ble_debug!("Write data: {:?}", data.to_vec());

                let peripheral = self.peripheral.clone();
                let runtime = self.runtime.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let event_tx = self.event_tx.clone();
                let data_vec: Vec<u8> = data.to_vec();
                let data_len = data_vec.len();

                runtime.spawn(async move {
                    let result = if with_response {
                        peripheral
                            .write(&char, &data_vec, WriteType::WithResponse)
                            .await
                    } else {
                        peripheral
                            .write(&char, &data_vec, WriteType::WithoutResponse)
                            .await
                    };

                    match result {
                        Ok(_) => {
                            ble_info!(
                                "Successfully wrote {} bytes to characteristic {}",
                                data_len,
                                char_uuid_for_async
                            );

                            let event = BleDeviceEvent::CharacteristicWritten {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                            };
                            Self::send_event_via(&event_tx, event);
                        }
                        Err(e) => {
                            let error = BleError::WriteFailed(e.to_string());
                            error.log_error();

                            let event = BleDeviceEvent::CharacteristicWriteFailed {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                                error: error.to_string(),
                            };
                            Self::send_event_via(&event_tx, event);
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

    /// Subscribe to characteristic notifications
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

        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();
        let char_uuid_lower = char_uuid_str.to_lowercase();

        if self.subscribed_characteristics.lock().unwrap().contains(&char_uuid_lower) {
            ble_warn!("Already subscribed to characteristic {}, ignoring", char_uuid_str);
            return;
        }

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                ble_debug!("Found characteristic {}, subscribing", char_uuid_str);
                
                let peripheral = self.peripheral.clone();
                let runtime = self.runtime.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let char_uuid_lower_for_task = char_uuid_lower.clone();
                let event_tx = self.event_tx.clone();
                let subscribed_chars = self.subscribed_characteristics.clone();
                let notification_tasks = self.notification_tasks.clone();
                
                runtime.spawn(async move {
                    let result = peripheral.subscribe(&char).await;

                    match result {
                        Ok(_) => {
                            subscribed_chars
                                .lock()
                                .unwrap()
                                .insert(char_uuid_lower.clone());

                            let event = BleDeviceEvent::SubscribeSuccess {
                                device_address: address.clone(),
                                char_uuid: char_uuid_for_async.clone(),
                            };
                            Self::send_event_via(&event_tx, event);

                            let peripheral_clone = peripheral.clone();
                            let char_uuid_for_handler = char_uuid_for_async.clone();
                            let event_tx_for_handler = event_tx.clone();
                            let address_for_handler = address.clone();
                            let char_uuid_lower_for_insert = char_uuid_lower_for_task.clone();

                            let handle = tokio::spawn(async move {
                                ble_debug!("Starting notification handler for {}", char_uuid_for_handler);
                                let mut notification_stream = peripheral_clone.notifications().await;

                                if let Ok(stream) = notification_stream.as_mut() {
                                    use futures::StreamExt;
                                    while let Some(notification) = stream.next().await {
                                        if notification
                                            .uuid
                                            .to_string()
                                            .eq_ignore_ascii_case(&char_uuid_for_handler)
                                        {
                                            ble_debug!(
                                                "Received notification from {}: {} bytes",
                                                char_uuid_for_handler,
                                                notification.value.len()
                                            );

                                            let event = BleDeviceEvent::CharacteristicNotified {
                                                device_address: address_for_handler.clone(),
                                                char_uuid: char_uuid_for_handler.clone(),
                                                data: notification.value,
                                            };
                                            Self::send_event_via(&event_tx_for_handler, event);
                                        }
                                    }
                                    ble_debug!("Notification stream ended for {}", char_uuid_for_handler);
                                } else {
                                    ble_error!("Failed to get notification stream for {}", char_uuid_for_handler);
                                }
                            });

                            notification_tasks.lock().unwrap().insert(char_uuid_lower_for_insert, handle);
                        }
                        Err(e) => {
                            let error = BleError::SubscribeFailed(e.to_string());
                            error.log_error();

                            let event = BleDeviceEvent::SubscribeFailed {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                                error: error.to_string(),
                            };
                            Self::send_event_via(&event_tx, event);
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
                    CharEventKind::Subscribe,
                );
            }
        }
    }

    /// Unsubscribe from characteristic notifications
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

        // Abort notification task first
        let char_uuid_lower = char_uuid.to_string().to_lowercase();
        if let Some(handle) = self.notification_tasks.lock().unwrap().remove(&char_uuid_lower) {
            handle.abort();
            ble_debug!("Aborted notification task for {}", char_uuid_lower);
        }

        let service_uuid_str = service_uuid.to_string();
        let char_uuid_str = char_uuid.to_string();

        match self.find_characteristic(&service_uuid_str, &char_uuid_str) {
            Some(char) => {
                ble_debug!("Found characteristic {}, unsubscribing", char_uuid_str);
                
                let peripheral = self.peripheral.clone();
                let runtime = self.runtime.clone();
                let address = self.address.to_string();
                let char_uuid_for_async = char_uuid_str.clone();
                let char_uuid_lower_for_remove = char_uuid_lower.clone();
                let event_tx = self.event_tx.clone();
                let subscribed_chars = self.subscribed_characteristics.clone();
                
                runtime.spawn(async move {
                    let result = peripheral.unsubscribe(&char).await;

                    match result {
                        Ok(_) => {
                            subscribed_chars
                                .lock()
                                .unwrap()
                                .remove(&char_uuid_lower_for_remove);

                            ble_info!(
                                "Successfully unsubscribed from characteristic {}",
                                char_uuid_for_async
                            );

                            let event = BleDeviceEvent::UnsubscribeSuccess {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                            };
                            Self::send_event_via(&event_tx, event);
                        }
                        Err(e) => {
                            let error = BleError::UnsubscribeFailed(e.to_string());
                            error.log_error();

                            let event = BleDeviceEvent::UnsubscribeFailed {
                                device_address: address,
                                char_uuid: char_uuid_for_async,
                                error: error.to_string(),
                            };
                            Self::send_event_via(&event_tx, event);
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
    fn init(_base: Base<RefCounted>) -> Self {
        panic!("BleDevice::init called directly - use BleDevice::new() instead");
    }
}

impl Drop for BleDevice {
    fn drop(&mut self) {
        ble_info!("BleDevice: Cleaning up resources for device {}", self.address);
        self.cleanup();
    }
}

impl BleDevice {
    pub fn new(peripheral: Peripheral, runtime: Arc<Runtime>, event_tx: Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>) -> Gd<Self> {
        let address = peripheral.id().to_string();
        
        let properties = runtime.block_on(async { peripheral.properties().await });

        let name = match properties {
            Ok(Some(props)) => props.local_name.unwrap_or_default(),
            Ok(None) => String::new(),
            Err(e) => {
                ble_warn!("Failed to get device properties: {}", e);
                String::new()
            }
        };

        Gd::from_init_fn(|base| Self {
            base,
            peripheral: Arc::new(peripheral),
            runtime,
            address: GString::from(&address),
            name: GString::from(&name),
            is_connected: Arc::new(Mutex::new(false)),
            services: Arc::new(Mutex::new(Vec::new())),
            subscribed_characteristics: Arc::new(Mutex::new(HashSet::new())),
            notification_tasks: Arc::new(Mutex::new(HashMap::new())),
            event_tx: Some(event_tx),
        })
    }

    fn check_connected(&self) -> Result<(), BleError> {
        if *self.is_connected.lock().unwrap() {
            Ok(())
        } else {
            Err(BleError::NotConnected)
        }
    }

    fn find_characteristic(&self, service_uuid: &str, char_uuid: &str) -> Option<Characteristic> {
        let characteristics = self.peripheral.characteristics();
        characteristics.iter().find(|c| {
            c.uuid.to_string().eq_ignore_ascii_case(char_uuid)
                && c.service_uuid.to_string().eq_ignore_ascii_case(service_uuid)
        }).cloned()
    }

    fn send_event_via(tx: &Option<Arc<Mutex<mpsc::UnboundedSender<BleDeviceEvent>>>>, event: BleDeviceEvent) {
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
        let error = BleError::CharacteristicNotFound(format!(
            "{} in service {}",
            char_uuid, service_uuid
        ));
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

    fn cleanup(&mut self) {
        let address = self.address.to_string();
        ble_debug!("Starting cleanup for device: {}", address);

        // Abort all notification tasks
        let notification_tasks = self.notification_tasks.lock().unwrap();
        let task_count = notification_tasks.len();
        if task_count > 0 {
            ble_debug!("Aborting {} notification tasks", task_count);
            for (char_uuid, handle) in notification_tasks.iter() {
                handle.abort();
                ble_debug!("Aborted notification task for {}", char_uuid);
            }
        }
        drop(notification_tasks);
        self.notification_tasks.lock().unwrap().clear();

        let is_connected = *self.is_connected.lock().unwrap();
        
        if is_connected {
            ble_debug!("Device {} is connected, initiating disconnect", address);
            
            let sub_count = self.subscribed_characteristics.lock().unwrap().len();
            if sub_count > 0 {
                ble_debug!("Device has {} active subscriptions", sub_count);
            }

            let peripheral = self.peripheral.clone();
            let is_connected_clone = self.is_connected.clone();
            let subscribed_chars = self.subscribed_characteristics.clone();

            self.runtime.spawn(async move {
                match peripheral.disconnect().await {
                    Ok(_) => {
                        ble_info!("Device {} disconnected during cleanup", address);
                    }
                    Err(e) => {
                        ble_warn!("Error disconnecting device {} during cleanup: {}", address, e);
                    }
                }
                *is_connected_clone.lock().unwrap() = false;
                subscribed_chars.lock().unwrap().clear();
            });
        }
    }
}
