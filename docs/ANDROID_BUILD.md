# Android Build Guide

This guide explains how to build and use GDBLE on Android platform.

## Prerequisites

1. **Rust toolchain** with Android targets:
   ```bash
   rustup target add aarch64-linux-android
   rustup target add x86_64-linux-android
   ```

2. **Android NDK** (r25c or later):
   - Download from: https://developer.android.com/ndk/downloads
   - Set environment variable: `export ANDROID_NDK_HOME=/path/to/ndk`

3. **Java Development Kit (JDK)** 11 or later

4. **Android SDK** with API level 31+ (Android 12+)

## Building for Android

### Option 1: Using GitHub Actions (Recommended)

The easiest way is to let GitHub Actions build for you. The workflow automatically builds for:
- Android ARM64 (aarch64-linux-android)
- Android x86_64 (x86_64-linux-android)

Android ARMv7 (32-bit) is not supported due to godot-ffi compatibility issues.

Simply push a tag or trigger the workflow manually:
```bash
git tag v1.0.0
git push origin v1.0.0
```

### Option 2: Manual Build

1. Install Android NDK and set environment variables:
   ```bash
   export ANDROID_NDK_HOME=/path/to/ndk
   export ANDROID_NDK=$ANDROID_NDK_HOME
   ```

2. Build for specific target:
   ```bash
   # ARM64 (most common for modern devices)
   cargo build --release --target aarch64-linux-android

   # x86_64 (emulators)
   cargo build --release --target x86_64-linux-android
   ```

3. Copy the compiled library:
   ```bash
   cp target/aarch64-linux-android/release/libgdble.so demo/addons/gdble/
   ```

## Using in Godot Android Project

### 1. Copy Files to Your Project

Copy the following files to your Godot project:
```
your_project/
├── res/
│   └── android/
│       ├── AndroidManifest.xml
│       └── gradle.properties
└── addons/
    └── gdble/
        ├── gdble.gdextension
        └── libgdble.so (for your target architecture)
```

### 2. Configure Export Template

1. In Godot Editor, go to **Project > Export**
2. Add an **Android** export preset
3. In the **Resources** tab, ensure:
   - `res://addons/gdble/gdble.gdextension` is included
   - `res://addons/gdble/libgdble.so` is included

4. In the **Permissions** tab, verify these permissions are set:
   - BLUETOOTH
   - BLUETOOTH_ADMIN
   - BLUETOOTH_SCAN (Android 12+)
   - BLUETOOTH_CONNECT (Android 12+)
   - ACCESS_FINE_LOCATION (Android 11 and below)
   - ACCESS_COARSE_LOCATION (Android 11 and below)

5. In the **Gradle Build** section, add ProGuard rules:
   - Copy `res://android/proguard-rules.pro` to your project's `res/android/` directory
   - The rules protect btleplug classes from being optimized out in release builds

### 2.1 ProGuard Configuration

For release builds with code shrinking enabled, add ProGuard rules to protect btleplug classes:

1. Create `res/android/proguard-rules.pro` in your Godot project:
```proguard
# btleplug resources - protect classes accessed by native code
-keep class com.nonpolynomial.** { *; }
-keep class io.github.gedgygedgy.** { *; }

# Keep native methods
-keepclasseswithmembernames class * {
    native <methods>;
}

# Keep classes that implement interfaces accessed by native code
-keep class * implements android.bluetooth.BluetoothAdapter$LeScanCallback { *; }
-keep class * implements android.bluetooth.BluetoothAdapter$DiscoveryListener { *; }
-keep class * implements android.bluetooth.BluetoothLeScanner$ScanCallback { *; }

# Keep Bluetooth-related classes
-keep class android.bluetooth.** { *; }
```

2. In your `export_presets.cfg`, ensure ProGuard is enabled for release builds:
```ini
[export_options]
application/config="res://export_presets.cfg"
application/config/use_custom_user_dir=true
application/config/name="Your App Name"
application/run/main_scene="res://main.tscn"
application/config/features=PackedStringArray("4.3")
application/config/icon="res://icon.png"
application/config/category="game"
application/permissions/android="BLUETOOTH,BLUETOOTH_ADMIN,BLUETOOTH_SCAN,BLUETOOTH_CONNECT,ACCESS_FINE_LOCATION,ACCESS_COARSE_LOCATION"
application/permissions/android/BLUETOOTH_SCAN/is_critical=true
application/permissions/android/BLUETOOTH_CONNECT/is_critical=true
application/permissions/android/ACCESS_FINE_LOCATION/is_critical=true
```

### 3. Runtime Permissions

Your GDScript code must request runtime permissions on Android 6.0+:

```gdscript
extends Node

var bluetooth_manager: BluetoothManager

func _ready():
    bluetooth_manager = BluetoothManager.new()
    add_child(bluetooth_manager)

    # Request permissions on Android
    if OS.get_name() == "Android":
        _request_android_permissions()
    else:
        _initialize_bluetooth()

func _request_android_permissions():
    var permissions = [
        "android.permission.BLUETOOTH_SCAN",
        "android.permission.BLUETOOTH_CONNECT",
        "android.permission.ACCESS_FINE_LOCATION"
    ]

    for permission in permissions:
        var status = OS.request_permission(permission)
        if status == false:
            print("Permission denied: ", permission)
            # Show error to user
            return

    # Wait a bit for permissions to be granted
    await get_tree().create_timer(1.0).timeout
    _initialize_bluetooth()

func _initialize_bluetooth():
    bluetooth_manager.adapter_initialized.connect(_on_initialized)
    bluetooth_manager.initialize()

func _on_initialized(success: bool, error: String):
    if success:
        print("Bluetooth initialized successfully")
        bluetooth_manager.start_scan(10.0)
    else:
        print("Failed to initialize Bluetooth: ", error)
```

## Android-Specific Considerations

### Permission Requirements

- **Android 12+ (API 31+)**: Requires `BLUETOOTH_SCAN` and `BLUETOOTH_CONNECT`
- **Android 11 and below**: Requires `BLUETOOTH`, `BLUETOOTH_ADMIN`, and location permissions
- **Location permissions**: Required for BLE scanning on Android 11 and below

### Background Scanning

Android has restrictions on background BLE scanning:
- **Android 12+**: Requires foreground service for continuous scanning
- **Android 11 and below**: Background scanning limited without location permission

### Device Compatibility

- **ARM64**: Most modern Android devices (recommended)
- **x86_64**: Intel-based devices and emulators
- **ARMv7**: Not supported due to godot-ffi compatibility issues

### Performance Tips

1. **Use appropriate scan timeout**: Don't scan indefinitely
2. **Stop scan when target found**: Save battery life
3. **Handle permission denials**: Provide clear user feedback
4. **Test on real devices**: Emulators may have limited BLE support

## Troubleshooting

### Build Errors

**Error: `linker not found`**
- Ensure Android NDK is installed and `ANDROID_NDK_HOME` is set

**Error: `target not found`**
- Install Android targets: `rustup target add aarch64-linux-android`

### Runtime Errors

**Error: `Permission denied`**
- Check AndroidManifest.xml has all required permissions
- Request runtime permissions in GDScript

**Error: `Bluetooth not available`**
- Ensure device has BLE hardware
- Check if Bluetooth is enabled in system settings

**Error: `Scan failed`**
- Verify location permissions are granted (Android 11 and below)
- Some devices require GPS to be enabled for BLE scanning

### Testing

1. **Use real devices**: Emulators often don't support BLE
2. **Test multiple Android versions**: Android 11, 12, 13+
3. **Test different architectures**: ARM64 and x86_64
4. **Use BLE debugging tools**: nRF Connect, LightBlue

## Additional Resources

- [Godot Android Export Documentation](https://docs.godotengine.org/en/stable/tutorials/export/android_export.html)
- [Android Bluetooth Permissions](https://developer.android.com/guide/topics/connectivity/bluetooth/permissions)
- [btleplug Android Support](https://github.com/deviceplug/btleplug)
- [Godot-Rust GDExtension](https://github.com/godot-rust/gdext)
