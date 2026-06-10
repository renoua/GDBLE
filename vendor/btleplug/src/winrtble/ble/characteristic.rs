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

use super::{super::utils::to_descriptor_value, descriptor::BLEDescriptor};
use crate::{
    Error, Result,
    api::{Characteristic, WriteType},
    winrtble::utils,
};

use log::{debug, trace, warn};
use std::{collections::HashMap, future::IntoFuture, time::Duration};
use uuid::Uuid;
use windows::core::Ref;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode,
        GenericAttributeProfile::{
            GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue,
            GattCommunicationStatus, GattValueChangedEventArgs, GattWriteOption,
        },
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter},
};

pub type NotifiyEventHandler = Box<dyn Fn(Vec<u8>) + Send>;

impl From<WriteType> for GattWriteOption {
    fn from(val: WriteType) -> Self {
        match val {
            WriteType::WithoutResponse => GattWriteOption::WriteWithoutResponse,
            WriteType::WithResponse => GattWriteOption::WriteWithResponse,
        }
    }
}

#[derive(Debug)]
pub struct BLECharacteristic {
    characteristic: GattCharacteristic,
    pub descriptors: HashMap<Uuid, BLEDescriptor>,
    notify_token: Option<i64>,
}

impl BLECharacteristic {
    pub fn new(
        characteristic: GattCharacteristic,
        descriptors: HashMap<Uuid, BLEDescriptor>,
    ) -> Self {
        BLECharacteristic {
            characteristic,
            descriptors,
            notify_token: None,
        }
    }

    pub async fn write_value(&self, data: &[u8], write_type: WriteType) -> Result<()> {
        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        let operation = self
            .characteristic
            .WriteValueWithOptionAsync(&writer.DetachBuffer()?, write_type.into())?;
        let result = operation.into_future().await?;
        if result == GattCommunicationStatus::Success {
            Ok(())
        } else {
            Err(Error::Other(
                format!("Windows UWP threw error on write: {:?}", result).into(),
            ))
        }
    }

    pub async fn read_value(&self) -> Result<Vec<u8>> {
        let result = self
            .characteristic
            .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)?
            .into_future()
            .await?;
        if result.Status()? == GattCommunicationStatus::Success {
            let value = result.Value()?;
            let reader = DataReader::FromBuffer(&value)?;
            let len = reader.UnconsumedBufferLength()? as usize;
            let mut input = vec![0u8; len];
            reader.ReadBytes(&mut input[0..len])?;
            Ok(input)
        } else {
            Err(Error::Other(
                format!("Windows UWP threw error on read: {:?}", result).into(),
            ))
        }
    }

    /// Synchronous part of subscribing: registers the `ValueChanged` handler and returns a clone
    /// of the `GattCharacteristic` (COM reference) together with the CCCD config value.
    ///
    /// The caller **must** call [`BLECharacteristic::write_cccd_with_retry`] with the returned
    /// values **without holding any DashMap guard** (Fix C — avoids blocking worker threads on a
    /// shared service shard while awaiting I/O).
    ///
    /// # Fix A1
    /// If `CharacteristicProperties()` reports neither Notify nor Indicate (can happen for
    /// non-bonded trainers whose property cache is stale on Windows), but a CCCD descriptor
    /// (0x2902) is present in `self.descriptors`, we assume `Notify` rather than failing
    /// immediately — matching bleak's behavior.
    ///
    /// On error the `notify_token` is cleaned up before returning.
    pub fn setup_notify(
        &mut self,
        on_value_changed: NotifiyEventHandler,
    ) -> Result<(GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue)> {
        let value_handler = TypedEventHandler::new(
            move |_: Ref<GattCharacteristic>, args: Ref<GattValueChangedEventArgs>| {
                if let Ok(args) = args.ok() {
                    let value = args.CharacteristicValue()?;
                    let reader = DataReader::FromBuffer(&value)?;
                    let len = reader.UnconsumedBufferLength()? as usize;
                    let mut input: Vec<u8> = vec![0u8; len];
                    reader.ReadBytes(&mut input[0..len])?;
                    trace!("changed {:?}", input);
                    on_value_changed(input);
                }
                Ok(())
            },
        );
        let token = self.characteristic.ValueChanged(&value_handler)?;
        self.notify_token = Some(token);

        let raw_config = to_descriptor_value(self.characteristic.CharacteristicProperties()?);

        // Fix A1: property cache may be stale for non-bonded trainers on Windows.
        // If no Notify/Indicate bit is set but a CCCD descriptor is present, assume Notify.
        let config = if raw_config == GattClientCharacteristicConfigurationDescriptorValue::None {
            const CCCD_UUID: Uuid = uuid::uuid!("00002902-0000-1000-8000-00805f9b34fb");
            if self.descriptors.contains_key(&CCCD_UUID) {
                warn!(
                    "CharacteristicProperties() reports no Notify/Indicate but CCCD descriptor \
                     present — assuming Notify (stale property cache on non-bonded device)"
                );
                GattClientCharacteristicConfigurationDescriptorValue::Notify
            } else {
                // Roll back token before surfacing the error.
                let _ = self.characteristic.RemoveValueChanged(token);
                self.notify_token = None;
                return Err(Error::NotSupported("Can not subscribe to attribute".into()));
            }
        } else {
            raw_config
        };

        debug!(
            "setup_notify {:?}: CCCD value chosen = {:?}",
            self.characteristic.Uuid(),
            config
        );

        Ok((self.characteristic.clone(), config))
    }

    /// Rolls back a `notify_token` previously set by [`setup_notify`]. Called when
    /// [`write_cccd_with_retry`] fails so the `ValueChanged` handler is unregistered.
    pub fn cleanup_notify_token(&mut self) {
        if let Some(token) = self.notify_token.take() {
            let _ = self.characteristic.RemoveValueChanged(token);
        }
    }

    /// Async part of subscribing: writes the CCCD descriptor with retry logic (Fix A2).
    ///
    /// This is an associated function (no `&self`) so it can be called after the DashMap guard
    /// has been dropped. On failure the caller should call [`cleanup_notify_token`] to unregister
    /// the `ValueChanged` handler.
    ///
    /// Retries up to 5 × 300 ms to absorb transient `Unreachable`/`AccessDenied` during LE link
    /// setup — consistent with the retry already present in `device.rs::get_characteristics`.
    pub async fn write_cccd_with_retry(
        gatt_char: GattCharacteristic,
        config: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> Result<()> {
        const MAX_RETRIES: u32 = 5;
        const RETRY_DELAY_MS: u64 = 300;

        for attempt in 0..MAX_RETRIES {
            let status = gatt_char
                .WriteClientCharacteristicConfigurationDescriptorAsync(config)?
                .into_future()
                .await?;
            trace!("subscribe attempt {} status {:?}", attempt, status);
            if status == GattCommunicationStatus::Success {
                return Ok(());
            }
            if attempt + 1 < MAX_RETRIES {
                warn!(
                    "CCCD write failed ({:?}), retrying ({}/{})",
                    status,
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            } else {
                return Err(Error::Other(
                    format!("Windows UWP threw error on subscribe: {:?}", status).into(),
                ));
            }
        }
        Ok(())
    }

    /// Synchronous part of unsubscribing: removes the `ValueChanged` token and returns a clone of
    /// the `GattCharacteristic` for the subsequent CCCD-None write.
    ///
    /// Like [`setup_notify`], the caller must call [`write_cccd_none`] outside any DashMap guard
    /// (Fix C).
    pub fn setup_unsubscribe(&mut self) -> Result<GattCharacteristic> {
        if let Some(token) = self.notify_token.take() {
            self.characteristic.RemoveValueChanged(token)?;
        }
        Ok(self.characteristic.clone())
    }

    /// Async part of unsubscribing: writes CCCD = None to stop notifications on the device.
    pub async fn write_cccd_none(gatt_char: GattCharacteristic) -> Result<()> {
        let config = GattClientCharacteristicConfigurationDescriptorValue::None;
        let status = gatt_char
            .WriteClientCharacteristicConfigurationDescriptorAsync(config)?
            .into_future()
            .await?;
        trace!("unsubscribe {:?}", status);
        if status == GattCommunicationStatus::Success {
            Ok(())
        } else {
            Err(Error::Other(
                format!("Windows UWP threw error on unsubscribe: {:?}", status).into(),
            ))
        }
    }

    pub fn uuid(&self) -> Uuid {
        utils::to_uuid(&self.characteristic.Uuid().unwrap())
    }

    pub fn to_characteristic(&self, service_uuid: Uuid) -> Characteristic {
        let uuid = self.uuid();
        let properties =
            utils::to_char_props(&self.characteristic.CharacteristicProperties().unwrap());
        let descriptors = self
            .descriptors
            .values()
            .map(|descriptor| descriptor.to_descriptor(service_uuid, uuid))
            .collect();
        Characteristic {
            uuid,
            service_uuid,
            descriptors,
            properties,
        }
    }
}

impl Drop for BLECharacteristic {
    fn drop(&mut self) {
        if let Some(token) = &self.notify_token {
            let result = self.characteristic.RemoveValueChanged(*token);
            if let Err(err) = result {
                debug!("Drop:remove_connection_status_changed {:?}", err);
            }
        }
    }
}
