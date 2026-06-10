// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use std::time::Duration;

use crate::{Error, Result, api::BDAddr, winrtble::utils};
use log::{debug, trace, warn};
use tokio::time::timeout;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
        BluetoothLEPreferredConnectionParameters,
        GenericAttributeProfile::{
            GattCharacteristic, GattCommunicationStatus, GattDescriptor, GattDeviceService,
            GattDeviceServicesResult, GattSession,
        },
    },
    Foundation::TypedEventHandler,
};

/// Timeout for uncached GATT operations before falling back to cached mode.
/// Some Windows BLE drivers hang indefinitely on uncached requests (see #325).
const GATT_CACHE_TIMEOUT: Duration = Duration::from_secs(5);

pub type ConnectedEventHandler = Box<dyn Fn(bool) + Send>;
pub type MaxPduSizeChangedEventHandler = Box<dyn Fn(u16) + Send>;

pub struct BLEDevice {
    device: BluetoothLEDevice,
    gatt_session: GattSession,
    connection_token: i64,
    pdu_change_token: i64,
    services: Vec<GattDeviceService>,
}

impl BLEDevice {
    pub async fn new(
        address: BDAddr,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
    ) -> Result<Self> {
        let async_op = BluetoothLEDevice::FromBluetoothAddressAsync(address.into())
            .map_err(|_| Error::DeviceNotFound)?;
        let device = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        let async_op = GattSession::FromDeviceIdAsync(&device.BluetoothDeviceId()?)
            .map_err(|_| Error::DeviceNotFound)?;
        let gatt_session = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        // Keep the GATT link alive between operations.
        // Without this, WinRT drops the connection for non-bonded devices between discover_services()
        // and get_characteristics(), causing GetCharacteristicsWithCacheModeAsync to return a
        // non-Success status → empty characteristic list → chars=0 in Godot.
        // Wahoo KICKR, Tacx Flux and most smart trainers do not bond (CanPair==false),
        // so they are always affected on Windows when not previously paired.
        if let Err(e) = gatt_session.SetMaintainConnection(true) {
            debug!("SetMaintainConnection(true) failed (non-fatal): {:?}", e);
        }

        let connection_status_handler =
            TypedEventHandler::<BluetoothLEDevice, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    let is_connected = sender
                        .ConnectionStatus()
                        .ok()
                        .map_or(false, |v| v == BluetoothConnectionStatus::Connected);
                    connection_status_changed(is_connected);
                    trace!("state {:?}", sender.ConnectionStatus());
                }
                Ok(())
            });
        let connection_token = device
            .ConnectionStatusChanged(&connection_status_handler)
            .map_err(|_| Error::Other("Could not add connection status handler".into()))?;

        max_pdu_size_changed(gatt_session.MaxPduSize().unwrap());
        let max_pdu_size_changed_handler =
            TypedEventHandler::<GattSession, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    max_pdu_size_changed(sender.MaxPduSize().unwrap());
                }
                Ok(())
            });
        let pdu_change_token = gatt_session
            .MaxPduSizeChanged(&max_pdu_size_changed_handler)
            .map_err(|_| Error::Other("Could not add max pdu size changed handler".into()))?;

        Ok(BLEDevice {
            device,
            gatt_session,
            connection_token,
            pdu_change_token,
            services: vec![],
        })
    }

    async fn get_gatt_services(
        &self,
        cache_mode: BluetoothCacheMode,
    ) -> Result<GattDeviceServicesResult> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let async_op = self
            .device
            .GetGattServicesWithCacheModeAsync(cache_mode)
            .map_err(winrt_error)?;
        let service_result = async_op.await.map_err(winrt_error)?;
        Ok(service_result)
    }

    pub fn name(&self) -> windows::core::Result<windows::core::HSTRING> {
        self.device.Name()
    }

    pub async fn connect(&self) -> Result<()> {
        if self.is_connected().await? {
            return Ok(());
        }

        let service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        let status = service_result.Status().map_err(|_| Error::DeviceNotFound)?;
        utils::to_error(status)
    }

    async fn is_connected(&self) -> Result<bool> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let status = self.device.ConnectionStatus().map_err(winrt_error)?;

        Ok(status == BluetoothConnectionStatus::Connected)
    }

    pub async fn get_characteristics(
        service: &GattDeviceService,
    ) -> Result<Vec<GattCharacteristic>> {
        // Retry loop: even with SetMaintainConnection(true), the first GATT exchange after
        // connecting may fail with Unreachable while the LE link is being established.
        // 5 attempts × 300 ms covers slow trainers where link setup + encryption exceeds
        // the typical WinRT window.
        const MAX_RETRIES: u32 = 5;
        const RETRY_DELAY_MS: u64 = 300;

        for attempt in 0..MAX_RETRIES {
            let async_result = match timeout(
                GATT_CACHE_TIMEOUT,
                service
                    .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)?
                    .into_future(),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    warn!("Uncached characteristic discovery timed out, falling back to cached mode");
                    service
                        .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Cached)?
                        .await?
                }
            };

            match async_result.Status() {
                Ok(GattCommunicationStatus::Success) => {
                    let results = async_result.Characteristics()?;
                    debug!("characteristics {:?}", results.Size());
                    return Ok(results.into_iter().collect());
                }
                Ok(GattCommunicationStatus::ProtocolError) => {
                    return Err(Error::Other(
                        format!(
                            "get_characteristics for {:?} encountered a protocol error",
                            service
                        )
                        .into(),
                    ));
                }
                Ok(status) => {
                    // Unreachable / DeviceNotConnected can occur transiently while the LE link
                    // is being established (even with MaintainConnection set). Retry before
                    // giving up so non-bonded trainers (Wahoo, Tacx…) are given a fair chance.
                    if attempt + 1 < MAX_RETRIES {
                        warn!(
                            "get_characteristics status={:?}, retrying ({}/{})",
                            status,
                            attempt + 1,
                            MAX_RETRIES
                        );
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    }
                    // All retries exhausted — return an explicit error so callers can log it.
                    return Err(Error::Other(
                        format!(
                            "get_characteristics for {:?} failed after {} attempts: {:?}",
                            service, MAX_RETRIES, status
                        )
                        .into(),
                    ));
                }
                Err(e) => {
                    return Err(Error::Other(
                        format!("get_characteristics for {:?} failed: {:?}", service, e).into(),
                    ));
                }
            }
        }

        // Unreachable (loop always returns), but satisfies the compiler.
        Ok(vec![])
    }

    pub async fn get_characteristic_descriptors(
        characteristic: &GattCharacteristic,
    ) -> Result<Vec<GattDescriptor>> {
        let async_result = match timeout(
            GATT_CACHE_TIMEOUT,
            characteristic
                .GetDescriptorsWithCacheModeAsync(BluetoothCacheMode::Uncached)?
                .into_future(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                warn!("Uncached descriptor discovery timed out, falling back to cached mode");
                characteristic
                    .GetDescriptorsWithCacheModeAsync(BluetoothCacheMode::Cached)?
                    .await?
            }
        };
        let status = async_result.Status();
        if status == Ok(GattCommunicationStatus::Success) {
            let results = async_result.Descriptors()?;
            debug!("descriptors {:?}", results.Size());
            Ok(results.into_iter().collect())
        } else {
            Err(Error::Other(
                format!(
                    "get_characteristic_descriptors for {:?} failed: {:?}",
                    characteristic, status
                )
                .into(),
            ))
        }
    }

    pub fn get_connection_parameters(&self) -> Result<crate::api::ConnectionParameters> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let params = self.device.GetConnectionParameters().map_err(winrt_error)?;
        // ConnectionInterval is in units of 1.25ms, convert to microseconds
        let interval_us = (params.ConnectionInterval().map_err(winrt_error)? as u32) * 1250;
        let latency = params.ConnectionLatency().map_err(winrt_error)? as u16;
        // LinkTimeout is in units of 10ms, convert to microseconds
        let supervision_timeout_us = (params.LinkTimeout().map_err(winrt_error)? as u32) * 10_000;
        Ok(crate::api::ConnectionParameters {
            interval_us,
            latency,
            supervision_timeout_us,
        })
    }

    pub fn request_connection_parameters(
        &self,
        preset: crate::api::ConnectionParameterPreset,
    ) -> Result<()> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let params = match preset {
            crate::api::ConnectionParameterPreset::Balanced => {
                BluetoothLEPreferredConnectionParameters::Balanced()
            }
            crate::api::ConnectionParameterPreset::ThroughputOptimized => {
                BluetoothLEPreferredConnectionParameters::ThroughputOptimized()
            }
            crate::api::ConnectionParameterPreset::PowerOptimized => {
                BluetoothLEPreferredConnectionParameters::PowerOptimized()
            }
        }
        .map_err(winrt_error)?;
        let result = self
            .device
            .RequestPreferredConnectionParameters(&params)
            .map_err(winrt_error)?;
        let status = result.Status().map_err(winrt_error)?;
        // BluetoothLEPreferredConnectionParametersRequestStatus:
        //   Unspecified = 0, Success = 1, DeviceNotAvailable = 2, AccessDenied = 3
        match status.0 {
            1 => Ok(()),
            2 | 3 => Err(Error::NotSupported(format!(
                "request_connection_parameters not supported (status {:?})",
                status
            ))),
            _ => Err(Error::Other(
                format!(
                    "RequestPreferredConnectionParameters failed with status {:?}",
                    status
                )
                .into(),
            )),
        }
    }

    pub async fn discover_services(&mut self) -> Result<&[GattDeviceService]> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());

        // Always use Uncached, even for bonded devices: a Windows bonding can freeze a
        // stale GATT cache (services/characteristics/CCCD handles that no longer match
        // the device), which makes paired trainers fail persistently ("detected but no
        // data"). bleak effectively queries the live device too. Aligns with
        // get_characteristics, which already does Uncached → timeout → Cached fallback.
        debug!("discover_services: using Uncached mode (timeout fallback: Cached)");

        // Mirror the timeout+fallback from get_characteristics: some Windows BLE drivers
        // hang indefinitely on Uncached requests.
        let service_result = match timeout(
            GATT_CACHE_TIMEOUT,
            self.get_gatt_services(BluetoothCacheMode::Uncached),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                warn!("Uncached service discovery timed out, falling back to cached mode");
                self.get_gatt_services(BluetoothCacheMode::Cached).await?
            }
        };

        let status = service_result.Status().map_err(winrt_error)?;
        if status == GattCommunicationStatus::Success {
            // IVectorView is not Send — convert to Vec before any await point.
            let services: Vec<_> = service_result
                .Services()
                .map_err(winrt_error)?
                .into_iter()
                .collect();
            self.services = services;
            debug!("services {:?}", self.services.len());
        }
        Ok(self.services.as_slice())
    }
}

impl Drop for BLEDevice {
    fn drop(&mut self) {
        let result = self
            .gatt_session
            .RemoveMaxPduSizeChanged(self.pdu_change_token);
        if let Err(err) = result {
            debug!("Drop: remove_max_pdu_size_changed {:?}", err);
        }

        let result = self
            .device
            .RemoveConnectionStatusChanged(self.connection_token);
        if let Err(err) = result {
            debug!("Drop:remove_connection_status_changed {:?}", err);
        }

        self.services.iter().for_each(|service| {
            if let Err(err) = service.Close() {
                debug!("Drop:remove_gatt_Service {:?}", err);
            }
        });

        let result = self.device.Close();
        if let Err(err) = result {
            debug!("Drop:close {:?}", err);
        }
    }
}
