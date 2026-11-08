package com.lesjeuxducycle.ble;

import android.Manifest;
import android.app.Activity;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.location.LocationManager;
import android.os.Build;
import android.provider.Settings;
import android.util.Log;

import androidx.annotation.NonNull;

import org.godotengine.godot.Godot;
import org.godotengine.godot.plugin.GodotPlugin;
import org.godotengine.godot.plugin.SignalInfo;
import org.godotengine.godot.plugin.UsedByGodot;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * Pont Android du BLE natif (crate Rust gdble_ljdc / btleplug).
 *
 * - charge la bibliothèque native puis lui fournit un JNIEnv (btleplug en a besoin) ;
 * - gère permissions Bluetooth, activation du Bluetooth et service de localisation.
 *
 * Singleton Godot : Engine.get_singleton("LjdcBle").
 */
public class LjdcBlePlugin extends GodotPlugin {
    private static final String TAG = "LjdcBle";
    private static final int REQ_PERMISSIONS = 7701;
    private static final int REQ_ENABLE_BT = 7702;

    private static final SignalInfo SIGNAL_PERMISSIONS =
            new SignalInfo("ble_permissions_result", Boolean.class);
    private static final SignalInfo SIGNAL_ENABLE_BT =
            new SignalInfo("bluetooth_enable_result", Boolean.class);

    private boolean libraryLoaded = false;
    private boolean nativeReady = false;

    private static native boolean nativeInit();

    public LjdcBlePlugin(Godot godot) {
        super(godot);
        try {
            System.loadLibrary("gdble_ljdc");
            libraryLoaded = true;
            nativeReady = nativeInit();
            Log.i(TAG, "native init: " + nativeReady);
        } catch (Throwable t) {
            Log.e(TAG, "native init failed", t);
        }
    }

    @NonNull
    @Override
    public String getPluginName() {
        return "LjdcBle";
    }

    @NonNull
    @Override
    public Set<SignalInfo> getPluginSignals() {
        Set<SignalInfo> signals = new HashSet<>();
        signals.add(SIGNAL_PERMISSIONS);
        signals.add(SIGNAL_ENABLE_BT);
        return signals;
    }

    private Context context() {
        Activity a = getActivity();
        return a != null ? a : getGodot().getContext();
    }

    private static String[] requiredPermissions() {
        if (Build.VERSION.SDK_INT >= 31) {
            return new String[] {
                Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT
            };
        }
        return new String[] {Manifest.permission.ACCESS_FINE_LOCATION};
    }

    @UsedByGodot
    public boolean isInitialized() {
        return libraryLoaded && nativeReady;
    }

    @UsedByGodot
    public int sdkInt() {
        return Build.VERSION.SDK_INT;
    }

    @UsedByGodot
    public boolean hasBlePermissions() {
        Context c = context();
        for (String p : requiredPermissions()) {
            if (c.checkSelfPermission(p) != PackageManager.PERMISSION_GRANTED) {
                return false;
            }
        }
        return true;
    }

    /** Résultat asynchrone : signal ble_permissions_result(granted). */
    @UsedByGodot
    public void requestBlePermissions() {
        Activity a = getActivity();
        if (a == null) {
            emitSignal(SIGNAL_PERMISSIONS.getName(), false);
            return;
        }
        if (hasBlePermissions()) {
            emitSignal(SIGNAL_PERMISSIONS.getName(), true);
            return;
        }
        a.requestPermissions(requiredPermissions(), REQ_PERMISSIONS);
    }

    @Override
    public void onMainRequestPermissionsResult(
            int requestCode, String[] permissions, int[] grantResults) {
        super.onMainRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode != REQ_PERMISSIONS) {
            return;
        }
        emitSignal(SIGNAL_PERMISSIONS.getName(), hasBlePermissions());
    }

    @UsedByGodot
    public boolean isBluetoothEnabled() {
        BluetoothManager bm =
                (BluetoothManager) context().getSystemService(Context.BLUETOOTH_SERVICE);
        BluetoothAdapter adapter = bm != null ? bm.getAdapter() : null;
        return adapter != null && adapter.isEnabled();
    }

    @UsedByGodot
    public boolean hasBluetoothAdapter() {
        BluetoothManager bm =
                (BluetoothManager) context().getSystemService(Context.BLUETOOTH_SERVICE);
        return bm != null && bm.getAdapter() != null;
    }

    /** Résultat asynchrone : signal bluetooth_enable_result(enabled). */
    @UsedByGodot
    public void requestEnableBluetooth() {
        Activity a = getActivity();
        if (a == null) {
            emitSignal(SIGNAL_ENABLE_BT.getName(), false);
            return;
        }
        try {
            a.startActivityForResult(
                    new Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE), REQ_ENABLE_BT);
        } catch (Throwable t) {
            Log.e(TAG, "requestEnableBluetooth failed", t);
            emitSignal(SIGNAL_ENABLE_BT.getName(), false);
        }
    }

    @Override
    public void onMainActivityResult(int requestCode, int resultCode, Intent data) {
        super.onMainActivityResult(requestCode, resultCode, data);
        if (requestCode == REQ_ENABLE_BT) {
            emitSignal(SIGNAL_ENABLE_BT.getName(), isBluetoothEnabled());
        }
    }

    /** Le scan BLE exige la localisation activée sur Android 11 et moins. */
    @UsedByGodot
    public boolean isLocationEnabled() {
        Context c = context();
        if (Build.VERSION.SDK_INT >= 28) {
            LocationManager lm = (LocationManager) c.getSystemService(Context.LOCATION_SERVICE);
            return lm != null && lm.isLocationEnabled();
        }
        try {
            return Settings.Secure.getInt(
                            c.getContentResolver(), Settings.Secure.LOCATION_MODE)
                    != Settings.Secure.LOCATION_MODE_OFF;
        } catch (Settings.SettingNotFoundException e) {
            return false;
        }
    }

    @UsedByGodot
    public boolean locationRequiredForScan() {
        return Build.VERSION.SDK_INT < 31;
    }
}
