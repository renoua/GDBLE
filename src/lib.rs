use godot::prelude::*;

// Module declarations
mod ble_characteristic;
mod ble_device;
mod ble_service;
mod bluetooth_manager;
mod bluetooth_scanner;
mod runtime;
mod types;
mod windows_pairing;

// Re-export main classes for easier access
pub use ble_device::BleDevice;
pub use bluetooth_manager::BluetoothManager;

/// GDExtension entry point
///
/// This struct serves as the entry point for the Godot extension.
/// All classes marked with #[derive(GodotClass)] are automatically
/// registered when the extension is loaded.
struct GdBle;

#[gdextension]
unsafe impl ExtensionLibrary for GdBle {}
