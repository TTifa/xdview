//! 小度式 RTC 寻址方案（**证据来源：`com.xiaoyu.open.uri.*` 与 `DuMiRtcUri` 字节码**）。
//!
//! 反汇编得到的构造逻辑：
//!
//! ```text
//! DuMiRtcUri.getUriForDevice(long):
//!     new RtcUri( deviceUid + "@" + DeviceType.HARD )      // → "1234567@HARD"
//!
//! RtcSipUri.getUri():
//!     RtcUri.getAppId() + ":" + RtcUri.uid + "@" + "SIP"   // → "appid:uid@SIP"
//! ```
//!
//! `DeviceType` 枚举全集（`com.xiaoyu.open.dumi.DuMiRtcExtend$DeviceType`）：
//! `HARD, SOFT, DESK, TVBOX, CONFERENCE, CONFNO, GROUP, H323, TEL, BROWSER, BRUCE, NEMONO, THIRD, UNKNOWN`。
//!
//! 本模块把这套 URI 语法**照搬**下来：既能作为文档，也能在接入自有 RTC 服务端时复用同一寻址规则。

use std::fmt;

/// URI 主体类型（对应小度 `DuMiRtcExtend.DeviceType` 里实际被用到的几类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriKind {
    /// 终端/账号
    Sip,
    /// 硬件设备（摄像机、音箱、手表）
    Hard,
    /// 会议号
    ConfNo,
    /// PSTN 电话
    Tel,
    /// 群组
    Group,
    /// 其他/未知
    Other,
}

impl UriKind {
    pub fn parse(tag: &str) -> UriKind {
        match tag.to_ascii_uppercase().as_str() {
            "SIP" => UriKind::Sip,
            "HARD" => UriKind::Hard,
            "CONFNO" => UriKind::ConfNo,
            "TEL" => UriKind::Tel,
            "GROUP" => UriKind::Group,
            _ => UriKind::Other,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            UriKind::Sip => "SIP",
            UriKind::Hard => "HARD",
            UriKind::ConfNo => "CONFNO",
            UriKind::Tel => "TEL",
            UriKind::Group => "GROUP",
            UriKind::Other => "UNKNOWN",
        }
    }
}

/// `[appId:]uid@KIND`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcUri {
    pub app_id: Option<String>,
    pub uid: String,
    pub kind: UriKind,
}

impl RtcUri {
    /// `<deviceUid>@HARD` —— 与小度摄像机完全一致的设备寻址。
    pub fn for_device(uid: impl Into<String>) -> Self {
        Self {
            app_id: None,
            uid: uid.into(),
            kind: UriKind::Hard,
        }
    }

    /// `<appId>:<uid>@SIP`
    pub fn for_endpoint(app_id: impl Into<String>, uid: impl Into<String>) -> Self {
        Self {
            app_id: Some(app_id.into()),
            uid: uid.into(),
            kind: UriKind::Sip,
        }
    }

    /// `<confNo>@CONFNO`
    pub fn for_conference(conf_no: impl Into<String>) -> Self {
        Self {
            app_id: None,
            uid: conf_no.into(),
            kind: UriKind::ConfNo,
        }
    }

    pub fn parse(raw: &str) -> Result<RtcUri, String> {
        let s = raw.trim();
        if s.is_empty() {
            return Err("空 URI".into());
        }
        let (head, kind) = match s.rsplit_once('@') {
            Some((h, k)) => (h, UriKind::parse(k)),
            None => return Err(format!("缺少 @KIND 后缀: {s}")),
        };
        let (app_id, uid) = match head.split_once(':') {
            Some((a, u)) => (Some(a.to_string()), u.to_string()),
            None => (None, head.to_string()),
        };
        if uid.is_empty() {
            return Err(format!("uid 为空: {s}"));
        }
        Ok(RtcUri { app_id, uid, kind })
    }

    pub fn host(&self) -> &str {
        self.uid.as_str()
    }
}

impl fmt::Display for RtcUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(app) = &self.app_id {
            write!(f, "{}:{}@{}", app, self.uid, self.kind.tag())
        } else {
            write!(f, "{}@{}", self.uid, self.kind.tag())
        }
    }
}

/// 呼叫模式。对应 `com.xiaoyu.open.call.RtcCallMode`：
/// `CallMode_AudioOnly / CallMode_AudioVideo / CallMode_Observed / CallMode_Observer / CallMode_Tel`。
///
/// 看摄像机用的是 **Observer**：观察方单向看，被观察方（摄像机）不显示画面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallMode {
    AudioOnly,
    AudioVideo,
    /// 被观察端（设备侧）
    Observed,
    /// 观察端（手机/PC 侧）—— 实时看摄像机的模式
    Observer,
    Tel,
}

impl CallMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CallMode::AudioOnly => "CallMode_AudioOnly",
            CallMode::AudioVideo => "CallMode_AudioVideo",
            CallMode::Observed => "CallMode_Observed",
            CallMode::Observer => "CallMode_Observer",
            CallMode::Tel => "CallMode_Tel",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_uri_roundtrip() {
        let u = RtcUri::for_device("8848");
        assert_eq!(u.to_string(), "8848@HARD");
        let p = RtcUri::parse("8848@HARD").unwrap();
        assert_eq!(p, u);
        assert_eq!(p.kind, UriKind::Hard);
    }

    #[test]
    fn sip_uri_keeps_app_id() {
        let p = RtcUri::parse("1024:abc@SIP").unwrap();
        assert_eq!(p.app_id.as_deref(), Some("1024"));
        assert_eq!(p.uid, "abc");
        assert_eq!(p.to_string(), "1024:abc@SIP");
    }

    #[test]
    fn rejects_garbage() {
        assert!(RtcUri::parse("no-suffix").is_err());
        assert!(RtcUri::parse("@HARD").is_err());
    }

    #[test]
    fn unknown_kind_is_preserved_as_other() {
        let p = RtcUri::parse("x@WATCH").unwrap();
        assert_eq!(p.kind, UriKind::Other);
        assert_eq!(p.to_string(), "x@UNKNOWN");
    }
}
