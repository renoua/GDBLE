use godot::prelude::*;

#[derive(Clone, Debug)]
pub struct PairingState {
    pub paired: bool,
    pub can_pair: bool,
    pub status: String,
}

impl PairingState {
    pub fn unavailable(status: &str) -> Self {
        Self {
            paired: false,
            can_pair: false,
            status: status.to_string(),
        }
    }

    pub fn to_dictionary(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("paired", self.paired);
        dict.set("can_pair", self.can_pair);
        dict.set("status", self.status.clone());
        dict
    }
}

#[derive(Clone, Debug)]
pub struct PairingResult {
    pub success: bool,
    pub status: String,
}

impl PairingResult {
    fn failed(status: &str) -> Self {
        Self {
            success: false,
            status: status.to_string(),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{PairingResult, PairingState};
    use std::future::Future;
    use windows::core::{Result as WinResult, HSTRING};
    use windows::Devices::Bluetooth::BluetoothLEDevice;
    use windows::Devices::Enumeration::{
        DeviceInformation, DevicePairingResultStatus, DeviceUnpairingResultStatus,
    };

    pub async fn get_pairing_state_async(address: &str) -> PairingState {
        match find_device_information(address).await {
            Ok(Some(info)) => match info.Pairing() {
                Ok(pairing) => PairingState {
                    paired: pairing.IsPaired().unwrap_or(false),
                    can_pair: pairing.CanPair().unwrap_or(false),
                    status: "ok".to_string(),
                },
                Err(err) => PairingState::unavailable(&format!("pairing_unavailable: {}", err)),
            },
            Ok(None) => PairingState::unavailable("device_not_found"),
            Err(err) => PairingState::unavailable(&format!("lookup_failed: {}", err)),
        }
    }

    pub fn get_pairing_state(address: &str) -> PairingState {
        block_on(get_pairing_state_async(address))
            .unwrap_or_else(|err| PairingState::unavailable(&err))
    }

    pub fn pair_device(address: &str) -> PairingResult {
        block_on(pair_device_async(address))
            .unwrap_or_else(|err| PairingResult::failed(err.as_str()))
    }

    pub fn unpair_device(address: &str) -> PairingResult {
        block_on(unpair_device_async(address))
            .unwrap_or_else(|err| PairingResult::failed(err.as_str()))
    }

    async fn pair_device_async(address: &str) -> PairingResult {
        let info = match find_device_information(address).await {
            Ok(Some(info)) => info,
            Ok(None) => return PairingResult::failed("device_not_found"),
            Err(err) => return PairingResult::failed(&format!("lookup_failed: {}", err)),
        };

        let pairing = match info.Pairing() {
            Ok(pairing) => pairing,
            Err(err) => return PairingResult::failed(&format!("pairing_unavailable: {}", err)),
        };

        if pairing.IsPaired().unwrap_or(false) {
            return PairingResult {
                success: true,
                status: "already_paired".to_string(),
            };
        }

        if !pairing.CanPair().unwrap_or(false) {
            return PairingResult::failed("cannot_pair");
        }

        match pairing.PairAsync() {
            Ok(operation) => match operation.await {
                Ok(result) => match result.Status() {
                    Ok(status) => PairingResult {
                        success: matches!(status, DevicePairingResultStatus::Paired),
                        status: format!("{:?}", status),
                    },
                    Err(err) => PairingResult::failed(&format!("pair_status_failed: {}", err)),
                },
                Err(err) => PairingResult::failed(&format!("pair_async_failed: {}", err)),
            },
            Err(err) => PairingResult::failed(&format!("pair_start_failed: {}", err)),
        }
    }

    async fn unpair_device_async(address: &str) -> PairingResult {
        let info = match find_device_information(address).await {
            Ok(Some(info)) => info,
            Ok(None) => return PairingResult::failed("device_not_found"),
            Err(err) => return PairingResult::failed(&format!("lookup_failed: {}", err)),
        };

        let pairing = match info.Pairing() {
            Ok(pairing) => pairing,
            Err(err) => return PairingResult::failed(&format!("pairing_unavailable: {}", err)),
        };

        if !pairing.IsPaired().unwrap_or(false) {
            return PairingResult {
                success: true,
                status: "already_unpaired".to_string(),
            };
        }

        match pairing.UnpairAsync() {
            Ok(operation) => match operation.await {
                Ok(result) => match result.Status() {
                    Ok(status) => PairingResult {
                        success: matches!(status, DeviceUnpairingResultStatus::Unpaired),
                        status: format!("{:?}", status),
                    },
                    Err(err) => PairingResult::failed(&format!("unpair_status_failed: {}", err)),
                },
                Err(err) => PairingResult::failed(&format!("unpair_async_failed: {}", err)),
            },
            Err(err) => PairingResult::failed(&format!("unpair_start_failed: {}", err)),
        }
    }

    async fn find_device_information(address: &str) -> WinResult<Option<DeviceInformation>> {
        if let Some(bluetooth_address) = parse_bluetooth_address(address) {
            let selector =
                BluetoothLEDevice::GetDeviceSelectorFromBluetoothAddress(bluetooth_address)?;
            let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)?.await?;
            if devices.Size()? > 0 {
                return Ok(Some(devices.GetAt(0)?));
            }
        }

        let id = HSTRING::from(address);
        match DeviceInformation::CreateFromIdAsync(&id) {
            Ok(operation) => operation.await.map(Some),
            Err(err) => Err(err),
        }
    }

    fn parse_bluetooth_address(address: &str) -> Option<u64> {
        let hex: String = address.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() != 12 {
            return None;
        }
        u64::from_str_radix(&hex, 16).ok()
    }

    fn block_on<F>(future: F) -> Result<F::Output, String>
    where
        F: Future,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("runtime_failed: {}", err))
            .map(|runtime| runtime.block_on(future))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{PairingResult, PairingState};

    pub async fn get_pairing_state_async(_address: &str) -> PairingState {
        PairingState::unavailable("unsupported_platform")
    }

    pub fn get_pairing_state(_address: &str) -> PairingState {
        PairingState::unavailable("unsupported_platform")
    }

    pub fn pair_device(_address: &str) -> PairingResult {
        PairingResult::failed("unsupported_platform")
    }

    pub fn unpair_device(_address: &str) -> PairingResult {
        PairingResult::failed("unsupported_platform")
    }
}

pub use platform::{get_pairing_state, get_pairing_state_async, pair_device, unpair_device};
