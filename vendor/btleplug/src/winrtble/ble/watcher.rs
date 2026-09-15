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

use crate::{Error, Result, api::ScanFilter, winrtble::utils};
use std::sync::Mutex;
use windows::{
    Devices::Bluetooth::BluetoothError, Devices::Bluetooth::Advertisement::*,
    Foundation::TypedEventHandler, core::Ref,
};

pub type AdvertisementEventHandler =
    Box<dyn Fn(&BluetoothLEAdvertisementReceivedEventArgs) -> windows::core::Result<()> + Send>;

/// Whether `LJDC_BLE_EXTENDED_ADV=1` is set. `winrtble` is a private module of this
/// vendored crate, so this can't be reached from the gdble crate directly — the
/// caller in bluetooth_scanner.rs re-reads the same env var (trivial to keep in
/// sync) to log the effective value somewhere that actually ends up on disk, since
/// this crate has no logger of its own wired to the game's ble_debug.log.
fn extended_advertisements_requested() -> bool {
    std::env::var("LJDC_BLE_EXTENDED_ADV")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// LJDC: the WinRT `BluetoothLEAdvertisementWatcher` is now recreated on every
/// `start()` instead of being reused for the process lifetime. Reusing one watcher
/// across scans leaked a `Received` handler each time (never unregistered — N scans
/// meant N handlers firing per advertisement) and could hit `E_ILLEGAL_METHOD_CALL`
/// from `Start()` if the previous scan's `Stop()` was still winding down
/// asynchronously (WinRT leaves the watcher in `Stopping` for a moment, not
/// immediately `Stopped`). A `Start()` failure surfaced as a scan that completed
/// with zero devices, indistinguishable from "nothing found". bleak has the same
/// shape: a fresh `BleakScanner` per `discover()` call, never reused.
#[derive(Debug)]
pub struct BLEWatcher {
    watcher: Mutex<Option<BluetoothLEAdvertisementWatcher>>,
}

impl From<windows::core::Error> for Error {
    fn from(err: windows::core::Error) -> Error {
        Error::Other(format!("{:?}", err).into())
    }
}

fn lock_poisoned() -> Error {
    Error::Other("BLEWatcher lock poisoned".into())
}

impl BLEWatcher {
    pub fn new() -> Result<Self> {
        Ok(BLEWatcher {
            watcher: Mutex::new(None),
        })
    }

    pub fn start(&self, filter: ScanFilter, on_received: AdvertisementEventHandler) -> Result<()> {
        let ScanFilter { services } = filter;

        let mut guard = self.watcher.lock().map_err(|_| lock_poisoned())?;
        if let Some(old) = guard.take() {
            // A watcher left over from a scan that wasn't stopped cleanly (e.g. the
            // caller dropped straight into a new scan). Best-effort stop; errors are
            // discarded since we're replacing it unconditionally either way.
            let _ = old.Stop();
        }

        let ad = BluetoothLEAdvertisementFilter::new()?;
        let watcher = BluetoothLEAdvertisementWatcher::Create(&ad)?;

        // Clear any OS-level service UUID filter from a previous scan.
        // We intentionally do NOT set service UUIDs on the OS filter: on some
        // Windows BLE drivers the 128-bit UUID filter silently drops matching
        // advertisements. Software filtering in the handler is used instead.
        let filter_ad = watcher.AdvertisementFilter()?.Advertisement()?;
        filter_ad.ServiceUuids()?.Clear()?;

        watcher.SetScanningMode(BluetoothLEScanningMode::Active)?;

        // Extended advertisements are opt-in (default OFF). bleak never requests
        // this mode. Some Windows BLE controllers/drivers silently stop delivering
        // *legacy* advertisements (most FTMS trainers, incl. Elite/Tacx, are BT 4.x)
        // once extended mode is requested, or reject Start() outright — both of
        // which look identical to "trainer not found" from the GDScript side.
        // Set LJDC_BLE_EXTENDED_ADV=1 to opt back in for hardware that needs it.
        // (This crate has no logger of its own wired to the game's ble_debug.log —
        // the caller in bluetooth_scanner.rs re-reads the same env var to log the
        // effective value where it will actually end up on disk.)
        if extended_advertisements_requested() {
            if let Err(e) = watcher.SetAllowExtendedAdvertisements(true) {
                eprintln!("BLEWatcher: SetAllowExtendedAdvertisements failed: {}", e);
            }
        }

        // Pre-convert the filter UUIDs once so the handler closure is cheap.
        let filter_guids: Vec<windows::core::GUID> = services.iter().map(utils::to_guid).collect();

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Ok(args) = args.ok() {
                    // Software service-UUID filter.
                    if !filter_guids.is_empty() {
                        if let Ok(ad) = args.Advertisement() {
                            if let Ok(ad_uuids) = ad.ServiceUuids() {
                                let count = ad_uuids.Size().unwrap_or(0);
                                let advertised: Vec<windows::core::GUID> =
                                    (0..count).filter_map(|i| ad_uuids.GetAt(i).ok()).collect();
                                let all_present =
                                    filter_guids.iter().all(|g| advertised.contains(g));
                                if !all_present {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    on_received(args)?;
                }
                Ok(())
            },
        );

        // A watcher aborted by the OS/driver (radio disabled, unsupported profile,
        // policy…) used to fail completely silently: no error, no device, just an
        // empty result at the end of the scan window. Log whatever WinRT reports.
        let stopped_handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        > = TypedEventHandler::new(move |_sender, args: Ref<BluetoothLEAdvertisementWatcherStoppedEventArgs>| {
            if let Ok(args) = args.ok() {
                if let Ok(error) = args.Error() {
                    if error != BluetoothError::Success {
                        eprintln!("BLEWatcher: watcher stopped with error: {:?}", error);
                    }
                }
            }
            Ok(())
        });

        watcher.Received(&handler)?;
        watcher.Stopped(&stopped_handler)?;
        watcher.Start()?;

        *guard = Some(watcher);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut guard = self.watcher.lock().map_err(|_| lock_poisoned())?;
        if let Some(watcher) = guard.take() {
            watcher.Stop()?;
        }
        Ok(())
    }
}
