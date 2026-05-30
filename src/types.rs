use godot::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// 全局调试模式标志
static DEBUG_MODE: AtomicBool = AtomicBool::new(false);

/// 设置调试模式
pub fn set_debug_mode(enabled: bool) {
    DEBUG_MODE.store(enabled, Ordering::Relaxed);
}

/// 检查是否启用调试模式
pub fn is_debug_mode() -> bool {
    DEBUG_MODE.load(Ordering::Relaxed)
}

/// 调试日志宏 - 仅在调试模式下输出
/// 使用 eprintln! 确保线程安全，可以从任何线程调用
#[macro_export]
macro_rules! ble_debug {
    ($($arg:tt)*) => {
        if $crate::types::is_debug_mode() {
            eprintln!("[BLE Debug] {}", format!($($arg)*));
        }
    };
}

/// 信息日志宏 - 仅在调试模式下输出
/// 使用 eprintln! 确保线程安全，可以从任何线程调用
#[macro_export]
macro_rules! ble_info {
    ($($arg:tt)*) => {
        if $crate::types::is_debug_mode() {
            eprintln!("[BLE Info] {}", format!($($arg)*));
        }
    };
}

/// 警告日志宏 - 仅在调试模式下输出
/// 使用 eprintln! 确保线程安全，可以从任何线程调用
#[macro_export]
macro_rules! ble_warn {
    ($($arg:tt)*) => {
        if $crate::types::is_debug_mode() {
            eprintln!("[BLE Warning] {}", format!($($arg)*));
        }
    };
}

/// 错误日志宏 - 仅在调试模式下输出
/// 使用 eprintln! 确保线程安全，可以从任何线程调用
#[macro_export]
macro_rules! ble_error {
    ($($arg:tt)*) => {
        if $crate::types::is_debug_mode() {
            eprintln!("[BLE Error] {}", format!($($arg)*));
        }
    };
}

/// 设备信息结构
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub services: Vec<String>,
    pub manufacturer_data: std::collections::HashMap<u16, Vec<u8>>,
    pub service_data: std::collections::HashMap<String, Vec<u8>>,
    pub tx_power_level: Option<i16>,
    pub paired: bool,
    pub can_pair: bool,
    pub pairing_status: String,
}

impl DeviceInfo {
    /// 创建新的设备信息
    pub fn new(
        address: String,
        name: Option<String>,
        rssi: Option<i16>,
        services: Vec<String>,
        manufacturer_data: std::collections::HashMap<u16, Vec<u8>>,
        service_data: std::collections::HashMap<String, Vec<u8>>,
        tx_power_level: Option<i16>,
        paired: bool,
        can_pair: bool,
        pairing_status: String,
    ) -> Self {
        Self {
            address,
            name,
            rssi,
            services,
            manufacturer_data,
            service_data,
            tx_power_level,
            paired,
            can_pair,
            pairing_status,
        }
    }

    pub fn with_unknown_pairing(
        address: String,
        name: Option<String>,
        rssi: Option<i16>,
        services: Vec<String>,
        manufacturer_data: std::collections::HashMap<u16, Vec<u8>>,
        service_data: std::collections::HashMap<String, Vec<u8>>,
        tx_power_level: Option<i16>,
    ) -> Self {
        Self::new(
            address,
            name,
            rssi,
            services,
            manufacturer_data,
            service_data,
            tx_power_level,
            false,
            false,
            "unknown".to_string(),
        )
    }

    /// 转换为 Godot VarDictionary
    pub fn to_dictionary(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("address", self.address.clone());

        if let Some(ref name) = self.name {
            dict.set("name", name.clone());
        } else {
            dict.set("name", &Variant::nil());
        }

        if let Some(rssi) = self.rssi {
            dict.set("rssi", rssi);
        } else {
            dict.set("rssi", &Variant::nil());
        }

        // Services
        let mut services_array: Array<GString> = Array::new();
        for service in &self.services {
            services_array.push(&GString::from(service));
        }
        dict.set("services", &services_array);
        dict.set("service_uuids", &services_array);
        dict.set("uuids", &services_array);

        // Manufacturer Data
        let mut manuf_dict = VarDictionary::new();
        for (id, data) in &self.manufacturer_data {
            let mut byte_array = PackedByteArray::new();
            for byte in data {
                byte_array.push(*byte);
            }
            manuf_dict.set(*id, &byte_array);
        }
        dict.set("manufacturer_data", &manuf_dict);

        // Service Data
        let mut service_data_dict = VarDictionary::new();
        for (uuid_str, data) in &self.service_data {
            let mut byte_array = PackedByteArray::new();
            for byte in data {
                byte_array.push(*byte);
            }
            service_data_dict.set(uuid_str.as_str(), &byte_array);
        }
        dict.set("service_data", &service_data_dict);

        // TX Power Level
        if let Some(tx) = self.tx_power_level {
            dict.set("tx_power_level", tx);
        } else {
            dict.set("tx_power_level", &Variant::nil());
        }

        dict.set("paired", self.paired);
        dict.set("can_pair", self.can_pair);
        dict.set("pairing_status", self.pairing_status.clone());

        dict
    }
}

/// BLE 错误类型
#[derive(Debug, Clone)]
pub enum BleError {
    /// 未找到蓝牙适配器
    AdapterNotFound,
    /// 未找到设备
    DeviceNotFound(String),
    /// 连接失败
    ConnectionFailed(String),
    /// 操作失败
    OperationFailed(String),
    /// 设备未连接
    NotConnected,
    /// 无效的 UUID
    InvalidUuid(String),
    /// 服务未找到
    ServiceNotFound(String),
    /// 特征值未找到
    CharacteristicNotFound(String),
    /// 扫描失败
    ScanFailed(String),
    /// 初始化失败
    InitializationFailed(String),
    /// 读取失败
    ReadFailed(String),
    /// 写入失败
    WriteFailed(String),
    /// 订阅失败
    SubscribeFailed(String),
    /// 取消订阅失败
    UnsubscribeFailed(String),
    /// 服务发现失败
    ServiceDiscoveryFailed(String),
    /// 权限错误
    PermissionDenied(String),
    /// 超时错误
    Timeout(String),
    /// 内部错误
    InternalError(String),
}

impl BleError {
    /// 转换为 GString (用于 Godot 信号)
    pub fn to_gstring(&self) -> GString {
        GString::from(self.to_string().as_str())
    }

    /// 转换为字符串描述
    pub fn to_string(&self) -> String {
        match self {
            BleError::AdapterNotFound => "未找到蓝牙适配器，请确保系统蓝牙已启用".to_string(),
            BleError::DeviceNotFound(addr) => {
                format!("未找到指定的蓝牙设备: {}", addr)
            }
            BleError::ConnectionFailed(msg) => {
                format!("连接失败: {}", msg)
            }
            BleError::OperationFailed(msg) => {
                format!("操作失败: {}", msg)
            }
            BleError::NotConnected => "设备未连接，请先连接设备".to_string(),
            BleError::InvalidUuid(uuid) => {
                format!("无效的 UUID: {}", uuid)
            }
            BleError::ServiceNotFound(uuid) => {
                format!("未找到服务 UUID: {}", uuid)
            }
            BleError::CharacteristicNotFound(uuid) => {
                format!("未找到特征值 UUID: {}", uuid)
            }
            BleError::ScanFailed(msg) => {
                format!("扫描失败: {}", msg)
            }
            BleError::InitializationFailed(msg) => {
                format!("初始化失败: {}", msg)
            }
            BleError::ReadFailed(msg) => {
                format!("读取特征值失败: {}", msg)
            }
            BleError::WriteFailed(msg) => {
                format!("写入特征值失败: {}", msg)
            }
            BleError::SubscribeFailed(msg) => {
                format!("订阅通知失败: {}", msg)
            }
            BleError::UnsubscribeFailed(msg) => {
                format!("取消订阅失败: {}", msg)
            }
            BleError::ServiceDiscoveryFailed(msg) => {
                format!("服务发现失败: {}", msg)
            }
            BleError::PermissionDenied(msg) => {
                format!("权限被拒绝: {}", msg)
            }
            BleError::Timeout(msg) => {
                format!("操作超时: {}", msg)
            }
            BleError::InternalError(msg) => {
                format!("内部错误: {}", msg)
            }
        }
    }

    /// 获取错误代码
    pub fn error_code(&self) -> &str {
        match self {
            BleError::AdapterNotFound => "ADAPTER_NOT_FOUND",
            BleError::DeviceNotFound(_) => "DEVICE_NOT_FOUND",
            BleError::ConnectionFailed(_) => "CONNECTION_FAILED",
            BleError::OperationFailed(_) => "OPERATION_FAILED",
            BleError::NotConnected => "NOT_CONNECTED",
            BleError::InvalidUuid(_) => "INVALID_UUID",
            BleError::ServiceNotFound(_) => "SERVICE_NOT_FOUND",
            BleError::CharacteristicNotFound(_) => "CHARACTERISTIC_NOT_FOUND",
            BleError::ScanFailed(_) => "SCAN_FAILED",
            BleError::InitializationFailed(_) => "INITIALIZATION_FAILED",
            BleError::ReadFailed(_) => "READ_FAILED",
            BleError::WriteFailed(_) => "WRITE_FAILED",
            BleError::SubscribeFailed(_) => "SUBSCRIBE_FAILED",
            BleError::UnsubscribeFailed(_) => "UNSUBSCRIBE_FAILED",
            BleError::ServiceDiscoveryFailed(_) => "SERVICE_DISCOVERY_FAILED",
            BleError::PermissionDenied(_) => "PERMISSION_DENIED",
            BleError::Timeout(_) => "TIMEOUT",
            BleError::InternalError(_) => "INTERNAL_ERROR",
        }
    }

    /// 判断错误是否可重试
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            BleError::Timeout(_) | BleError::ConnectionFailed(_) | BleError::OperationFailed(_)
        )
    }

    /// 记录错误到控制台（线程安全）
    pub fn log_error(&self) {
        eprintln!("[BLE Error] {}: {}", self.error_code(), self.to_string());
    }

    /// 记录警告到控制台（线程安全）
    pub fn log_warning(&self) {
        eprintln!("[BLE Warning] {}: {}", self.error_code(), self.to_string());
    }
}

impl std::fmt::Display for BleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl std::error::Error for BleError {}

/// 设备事件枚举 - 用于从后台线程发送事件到主线程
#[derive(Clone, Debug)]
pub enum BleDeviceEvent {
    /// 连接成功
    ConnectSuccess { device_address: String },
    /// 连接失败
    ConnectFailed {
        device_address: String,
        error: String,
    },
    /// 断开连接
    Disconnected { device_address: String },
    /// 服务发现完成
    ServicesDiscovered {
        device_address: String,
        services: Vec<BleServiceInfo>,
    },
    /// 服务发现失败
    ServiceDiscoveryFailed {
        device_address: String,
        error: String,
    },
    /// 特征值读取完成
    CharacteristicRead {
        device_address: String,
        char_uuid: String,
        data: Vec<u8>,
    },
    /// 特征值读取失败
    CharacteristicReadFailed {
        device_address: String,
        char_uuid: String,
        error: String,
    },
    /// 特征值写入完成
    CharacteristicWritten {
        device_address: String,
        char_uuid: String,
    },
    /// 特征值写入失败
    CharacteristicWriteFailed {
        device_address: String,
        char_uuid: String,
        error: String,
    },
    /// 特征值通知
    CharacteristicNotified {
        device_address: String,
        char_uuid: String,
        data: Vec<u8>,
    },
    /// 订阅成功
    SubscribeSuccess {
        device_address: String,
        char_uuid: String,
    },
    /// 订阅失败
    SubscribeFailed {
        device_address: String,
        char_uuid: String,
        error: String,
    },
    /// 取消订阅成功
    UnsubscribeSuccess {
        device_address: String,
        char_uuid: String,
    },
    /// 取消订阅失败
    UnsubscribeFailed {
        device_address: String,
        char_uuid: String,
        error: String,
    },
}

/// 适配器信息结构
#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub name: String,
    pub address: Option<String>,
}

impl AdapterInfo {
    /// 创建新的适配器信息
    pub fn new(name: String, address: Option<String>) -> Self {
        Self { name, address }
    }

    /// 转换为 Godot VarDictionary
    pub fn to_dictionary(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("name", self.name.clone());

        if let Some(ref address) = self.address {
            dict.set("address", address.clone());
        } else {
            dict.set("address", &Variant::nil());
        }

        dict
    }
}

// Re-export BLE service and characteristic types from their modules
// These are used by other modules but not directly in this file
#[allow(unused_imports)]
pub use crate::ble_characteristic::{BleCharacteristicInfo, CharacteristicProperties};
#[allow(unused_imports)]
pub use crate::ble_service::BleServiceInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn device_info_unknown_pairing_defaults_are_non_blocking_placeholders() {
        let info = DeviceInfo::with_unknown_pairing(
            "AA:BB:CC:DD:EE:FF".to_string(),
            Some("Trainer".to_string()),
            Some(-42),
            vec!["1826".to_string()],
            HashMap::new(),
            HashMap::new(),
            None,
        );

        assert!(!info.paired);
        assert!(!info.can_pair);
        assert_eq!(info.pairing_status, "unknown");
    }
}
