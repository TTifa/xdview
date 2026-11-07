//! xd-core —— 与摄像机交互的纯逻辑层，**不依赖任何第三方 crate**。
//!
//! 设备控制使用标准 ONVIF；配置、能力模型、发现与协议辅助按职责分模块。
//! `rtc_uri` 仅保留历史 URI 语法模型，不提供厂商云端信令。
//!
//! 通用 IPC（TP-Link / 海康 / 大华）还会用到 HTTP Digest 鉴权，见 [`onvif::digest_authorization`]
//! 与 [`md5`]。

pub mod config;
pub mod discovery;
pub mod file_io;
pub mod http;
pub mod md5;
pub mod model;
pub mod onvif;
pub mod rtc_uri;
pub mod sha1;

pub use model::{Camera, Capabilities, Transport};

/// 诊断 stdout 的统一脱敏边界，格式化设备回包与错误后再过滤 URL。
#[macro_export]
macro_rules! diagnostic_println {
    () => { std::println!() };
    ($($arg:tt)*) => { std::println!("{}", $crate::onvif::mask_url_in_text(&format!($($arg)*))) };
}

/// 诊断 stderr 的统一脱敏边界。
#[macro_export]
macro_rules! diagnostic_eprintln {
    () => { std::eprintln!() };
    ($($arg:tt)*) => { std::eprintln!("{}", $crate::onvif::mask_url_in_text(&format!($($arg)*))) };
}

#[cfg(test)]
mod diagnostic_tests {
    #[test]
    fn empty_diagnostic_output_is_supported() {
        crate::diagnostic_eprintln!();
    }
}
