//! ONVIF(Profile T/S) 云台控制客户端。
//!
//! 小度 App 把云台/变焦放在**私有 RTC 会话内**下发（`IpcCameraGimbalControlView`、
//! `CameraRotateControl{Up,Down,Left,Right}Btn`、`IpcCameraZoomView`）。
//! PC 端要控制通用 IPC，等价物就是 ONVIF 的 `ContinuousMove` / `Stop` / `GotoPreset`。
//!
//! 鉴权支持两种，先 WS-Security 后 Digest：
//!
//! * WS-Security `UsernameToken`：`PasswordDigest = Base64( SHA1( nonce || created || password ) )`
//!   —— 小度摄像机、部分国产 IPC 走这条。
//! * HTTP `Digest`（RFC 2617）：设备先回 401 + `WWW-Authenticate`，再用 MD5 摘要重试
//!   —— TP-Link VIGI、海康、大华默认走这条。

use crate::http;
use crate::md5::hex_digest;
use crate::sha1::{base64, digest};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const NS_PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
pub const NS_MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
pub const NS_SCHEMA: &str = "http://www.onvif.org/ver10/schema";

/// 实际生效的鉴权方式，供 `--onvif` 之类诊断输出用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// 还没成功过（或设备不要求鉴权）
    Unknown,
    WsUsernameToken,
    Digest,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::Unknown => "未确定（无凭据或本次调用失败）",
            AuthKind::WsUsernameToken => "WS-UsernameToken",
            AuthKind::Digest => "HTTP Digest",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub token: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Preset {
    pub token: String,
    pub name: String,
}

/// 归一化后的云台速度，取值 `-1.0..=1.0`。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Velocity {
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
}

impl Velocity {
    pub fn clamp(mut self) -> Self {
        self.pan = self.pan.clamp(-1.0, 1.0);
        self.tilt = self.tilt.clamp(-1.0, 1.0);
        self.zoom = self.zoom.clamp(-1.0, 1.0);
        self
    }

    pub fn is_idle(&self) -> bool {
        self.pan.abs() < 0.01 && self.tilt.abs() < 0.01 && self.zoom.abs() < 0.01
    }
}

/// 云台当前姿态（ONVIF 归一化坐标，`-1.0..=1.0`）。
///
/// 这是判断「云台到底动没动」最可靠的依据：读的是电机位置，
/// 不受画面内容、也不受设备「智能追踪」自动转动的影响。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PtzStatus {
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
    /// 设备自报是否正在移动（`MoveStatus/PanTilt == MOVING`）
    pub moving: bool,
}

/// ONVIF 客户端。`base` 形如 `http://192.168.1.10` 或 `http://192.168.1.10:2020`。
#[derive(Debug, Clone)]
pub struct OnvifClient {
    base: String,
    credentials: Option<Credentials>,
    timeout: Duration,
    auth: Arc<Mutex<AuthKind>>,
    /// 设备自报的服务端点（`GetCapabilities/Capabilities/*/XAddr`）。
    ///
    /// 多数设备把 media/ptz 拆成 `/onvif/media_service`、`/onvif/ptz_service`，
    /// 但 TP-Link 这类 gSOAP 实现把所有服务挂在同一个 `/onvif/service` 上。
    /// 硬编码路径会「密码对了也调不通」，所以首次调用前先问设备要地址并缓存。
    service: Arc<Mutex<Option<String>>>,
}

impl OnvifClient {
    pub fn new(base: impl Into<String>, credentials: Option<Credentials>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base,
            credentials,
            timeout: Duration::from_secs(5),
            auth: Arc::new(Mutex::new(AuthKind::Unknown)),
            service: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 最近一次成功调用用到的鉴权方式。
    pub fn auth_kind(&self) -> AuthKind {
        *self.auth.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn device_url(&self) -> String {
        format!("{}/onvif/device_service", self.base)
    }

    fn ptz_url(&self) -> String {
        format!("{}/onvif/ptz_service", self.base)
    }

    fn media_url(&self) -> String {
        format!("{}/onvif/media_service", self.base)
    }

    /// 设备自报的服务端点（`GetCapabilities` 里的 `XAddr`）。
    ///
    /// 只作为 **404/405 时的回退**：多数设备把服务拆成 `/onvif/media_service`、
    /// `/onvif/ptz_service`，但也有一类 gSOAP 实现（如 TP-Link）把全部服务挂在
    /// 单一 `/onvif/service` 上。先按约定路径调，真被拒了再问设备要地址，
    /// 这样不会把原本能用的设备改坏。
    pub fn service_endpoint(&self) -> Option<String> {
        if let Some(cached) = self
            .service
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(cached);
        }
        let body = r#"<GetCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl"><Category>All</Category></GetCapabilities>"#;
        let resp = self.exchange(&self.device_url(), body).ok()?;
        if !resp.is_success() {
            return None;
        }
        let xml = resp.body_text();
        // 按 Media → PTZ → Device 顺序找，避免拿到 Analytics 的地址
        let xaddr = ["Media", "PTZ", "Device"]
            .iter()
            .filter_map(|tag| inner_text(&xml, tag).and_then(|inner| inner_text(&inner, "XAddr")))
            .next()?;
        let xaddr = xaddr.trim().trim_end_matches('/').to_string();
        *self.service.lock().unwrap_or_else(|e| e.into_inner()) = Some(xaddr.clone());
        Some(xaddr)
    }

    fn send(
        &self,
        url: &str,
        body: &str,
        security: &str,
        extra: &[(&str, &str)],
    ) -> Result<http::Response, String> {
        let env = soap_envelope(security, body);
        let mut headers: Vec<(&str, &str)> =
            vec![("Content-Type", "application/soap+xml; charset=utf-8")];
        headers.extend_from_slice(extra);
        http::post(url, &headers, env.as_bytes(), self.timeout)
    }

    /// 发一次 SOAP（含 Digest 回退），返回原始响应。
    fn exchange(&self, url: &str, body: &str) -> Result<http::Response, String> {
        let security = match &self.credentials {
            Some(c) => ws_username_token(&c.username, &c.password),
            None => String::new(),
        };
        let resp = self.send(url, body, &security, &[])?;

        // 401 + Digest 质询 → 换 HTTP Digest 再发一次。
        // 小度接受 WS-Security 所以不会走到这里；TP-Link VIGI / 海康 / 大华大多只认 Digest。
        if resp.status == 401 {
            if let (Some(creds), Some(challenge)) = (
                self.credentials.as_ref(),
                resp.header("www-authenticate")
                    .and_then(parse_digest_challenge),
            ) {
                let auth = digest_authorization(
                    &challenge,
                    &creds.username,
                    &creds.password,
                    "POST",
                    &path_of(url),
                    "xdview0c0ffee1",
                    "00000001",
                )?;
                let retry = self.send(url, body, "", &[("Authorization", auth.as_str())])?;
                if retry.is_success() {
                    *self.auth.lock().unwrap_or_else(|e| e.into_inner()) = AuthKind::Digest;
                }
                return Ok(retry);
            }
        }

        if resp.is_success() && self.credentials.is_some() {
            let mut guard = self.auth.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == AuthKind::Unknown {
                *guard = AuthKind::WsUsernameToken;
            }
        }
        Ok(resp)
    }

    fn call(&self, url: &str, body: &str) -> Result<String, String> {
        let resp = self.exchange(url, body)?;
        // 路径不对（404/405）时，改用设备自报的端点重试一次
        if matches!(resp.status, 404 | 405) {
            if let Some(alt) = self.service_endpoint() {
                if alt != url {
                    let retry = self.exchange(&alt, body)?;
                    return expect_ok(retry);
                }
            }
        }
        expect_ok(resp)
    }

    /// 取媒体配置档（ProfileToken），云台命令必须带它。
    pub fn get_profiles(&self) -> Result<Vec<Profile>, String> {
        let xml = self.call(
            &self.media_url(),
            &format!(r#"<trt:GetProfiles xmlns:trt="{NS_MEDIA}"/>"#),
        )?;
        let mut out = Vec::new();
        for el in elements(&xml, "Profiles") {
            if let Some(token) = attr(el.open, "token") {
                let name = inner_text(el.inner, "Name").unwrap_or_else(|| token.clone());
                out.push(Profile { token, name });
            }
        }
        if out.is_empty() {
            return Err("设备未返回任何 Profile".into());
        }
        Ok(out)
    }

    pub fn get_presets(&self, profile: &str) -> Result<Vec<Preset>, String> {
        let xml = self.call(
            &self.ptz_url(),
            &format!(
                r#"<trt:GetPresets xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken></trt:GetPresets>"#,
                escape(profile)
            ),
        )?;
        let mut out = Vec::new();
        for el in elements(&xml, "Preset") {
            if let Some(token) = attr(el.open, "token") {
                let name = inner_text(el.inner, "Name").unwrap_or_else(|| token.clone());
                out.push(Preset { token, name });
            }
        }
        Ok(out)
    }

    /// 持续移动；`timeout` 为设备侧自动停止时间，不传则由 [`Self::stop`] 收尾。
    pub fn continuous_move(
        &self,
        profile: &str,
        velocity: Velocity,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        let v = velocity.clamp();
        // 只发用到的轴：纯变焦时别带 PanTilt 0,0（部分机型会因此回 SOAP fault）
        let mut axes = String::new();
        if v.pan.abs() >= 0.01 || v.tilt.abs() >= 0.01 {
            axes.push_str(&format!(
                r#"<tt:PanTilt x="{:.3}" y="{:.3}" xmlns:tt="{NS_SCHEMA}"/>"#,
                v.pan, v.tilt
            ));
        }
        if v.zoom.abs() >= 0.01 {
            axes.push_str(&format!(
                r#"<tt:Zoom x="{:.3}" xmlns:tt="{NS_SCHEMA}"/>"#,
                v.zoom
            ));
        }
        let mut vel = String::new();
        if !axes.is_empty() {
            vel = format!(r#"<trt:Velocity>{axes}</trt:Velocity>"#);
        }
        let mut to = String::new();
        if let Some(d) = timeout {
            to = format!(r#"<trt:Timeout>{}</trt:Timeout>"#, iso8601_duration(d));
        }
        let body = format!(
            r#"<trt:ContinuousMove xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken>{vel}{to}</trt:ContinuousMove>"#,
            escape(profile)
        );
        self.call(&self.ptz_url(), &body).map(|_| ())
    }

    pub fn stop(&self, profile: &str) -> Result<(), String> {
        let body = format!(
            r#"<trt:Stop xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken><trt:PanTilt>true</trt:PanTilt><trt:Zoom>true</trt:Zoom></trt:Stop>"#,
            escape(profile)
        );
        self.call(&self.ptz_url(), &body).map(|_| ())
    }

    pub fn goto_preset(&self, profile: &str, preset: &str) -> Result<(), String> {
        let body = format!(
            r#"<trt:GotoPreset xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken><trt:PresetToken>{}</trt:PresetToken></trt:GotoPreset>"#,
            escape(profile),
            escape(preset)
        );
        self.call(&self.ptz_url(), &body).map(|_| ())
    }

    /// 在当前位置保存预置位（`token` 传 None 新建，传 Some 更新已有），返回 PresetToken。
    pub fn set_preset(
        &self,
        profile: &str,
        name: &str,
        token: Option<&str>,
    ) -> Result<String, String> {
        let tok = match token {
            Some(t) => format!("<trt:PresetToken>{}</trt:PresetToken>", escape(t)),
            None => String::new(),
        };
        let body = format!(
            r#"<trt:SetPreset xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken><trt:PresetName>{}</trt:PresetName>{tok}</trt:SetPreset>"#,
            escape(profile),
            escape(name)
        );
        let xml = self.call(&self.ptz_url(), &body)?;
        inner_text(&xml, "PresetToken").ok_or_else(|| "设备未返回 PresetToken".to_string())
    }

    /// 删除预置位。
    pub fn remove_preset(&self, profile: &str, token: &str) -> Result<(), String> {
        let body = format!(
            r#"<trt:RemovePreset xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken><trt:PresetToken>{}</trt:PresetToken></trt:RemovePreset>"#,
            escape(profile),
            escape(token)
        );
        self.call(&self.ptz_url(), &body).map(|_| ())
    }

    /// 读取云台当前姿态。
    pub fn get_status(&self, profile: &str) -> Result<PtzStatus, String> {
        let body = format!(
            r#"<trt:GetStatus xmlns:trt="{NS_PTZ}"><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStatus>"#,
            escape(profile)
        );
        let xml = self.call(&self.ptz_url(), &body)?;

        // Position 里的 PanTilt/Zoom 带 x 属性，MoveStatus 里的是纯文本，靠属性区分
        let pan_tilt = elements(&xml, "PanTilt")
            .into_iter()
            .find(|e| attr(e.open, "x").is_some())
            .ok_or_else(|| "设备未返回云台位置（可能不支持 GetStatus）".to_string())?;
        let zoom = elements(&xml, "Zoom")
            .into_iter()
            .find(|e| attr(e.open, "x").is_some());
        let moving = elements(&xml, "MoveStatus")
            .first()
            .map(|e| e.inner.contains("MOVING"))
            .unwrap_or(false);

        let num = |v: Option<String>| v.and_then(|x| x.parse::<f32>().ok()).unwrap_or(0.0);
        Ok(PtzStatus {
            pan: num(attr(pan_tilt.open, "x")),
            tilt: num(attr(pan_tilt.open, "y")),
            zoom: num(zoom.and_then(|e| attr(e.open, "x"))),
            moving,
        })
    }

    /// 取该 Profile 的 RTSP 取流地址（ONVIF Profile S 的 `GetStreamUri`）。
    ///
    /// 有了它就不必手抄 `rtsp://ip:554/Streaming/Channels/101` 这类厂商私有路径：
    /// 设备自己告诉我们地址。注意协议只写 `RTSP`，不要写 `RTSP+HTTP`。
    pub fn get_stream_uri(&self, profile: &str) -> Result<String, String> {
        let body = format!(
            r#"<trt:GetStreamUri xmlns:trt="{NS_MEDIA}"><trt:StreamSetup><tt:Stream xmlns:tt="{NS_SCHEMA}">RTP-Unicast</tt:Stream><tt:Transport xmlns:tt="{NS_SCHEMA}"><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>"#,
            escape(profile)
        );
        let xml = self.call(&self.media_url(), &body)?;
        let uri = inner_text(&xml, "Uri").ok_or_else(|| "设备未返回 MediaUri.Uri".to_string())?;
        Ok(uri)
    }

    /// 读取设备信息，兼作连通性探测。
    pub fn device_info(&self) -> Result<String, String> {
        let body = r#"<GetDeviceInformation xmlns="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let xml = self.call(&self.device_url(), body)?;
        let model = inner_text(&xml, "Model").unwrap_or_default();
        let firmware = inner_text(&xml, "FirmwareVersion").unwrap_or_default();
        Ok(format!("{model} / {firmware}"))
    }
}

/// 把凭据注入 RTSP URL。
///
/// ONVIF 返回的 `Uri` 常常不带账号密码（`rtsp://192.168.1.64:554/Streaming/Channels/101`），
/// 而 ffmpeg 需要 `rtsp://user:pass@host/...` 才能通过鉴权。
pub fn inject_credentials(uri: &str, user: &str, pass: &str) -> String {
    if user.is_empty() {
        return uri.to_string();
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return uri.to_string();
    };
    if !scheme.eq_ignore_ascii_case("rtsp") && !scheme.eq_ignore_ascii_case("rtsps") {
        return uri.to_string();
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest[..authority_end].contains('@') {
        return uri.to_string(); // 已带凭据
    }
    format!(
        "{}://{}:{}@{}",
        scheme,
        encode_credential(user),
        encode_credential(pass),
        rest
    )
}

/// 把 URL 里的密码换成 `***`，用于日志与界面显示。
///
/// 账号密码会进日志面板，还会被截图带出去，所以显示前一律脱敏。
pub fn mask_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split(':').next().unwrap_or("");
    format!("{scheme}://{user}:***@{host}{}", &rest[authority_end..])
}

/// 把**任意一行文本**里内嵌的 URL 全部脱敏（userinfo + 已知签名参数）。
///
/// ffmpeg 的 stderr 会整行回显地址（`Opening 'rtsp://user:pass@host/...' for reading`），
/// 只对完整 URL 调一次 [`mask_credentials`] 覆盖不到嵌在句子里的情况；带签名的地址
/// （`?...&secret=<32hex>`）同样属于敏感数据。日志、截图、诊断 CLI 输出前一律过一遍这里。
///
/// 没匹配到 URL 时原样返回；对已脱敏的文本重复调用是幂等的。
/// 无空白分隔的相邻／嵌套 URL 整组遮盖，避免歧义边界漏出内层凭据。
pub fn mask_url_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, end)) = next_url_range(rest) {
        out.push_str(&rest[..start]);
        let url = &rest[start..end];
        // 没有空白分隔的相邻／嵌套地址无法可靠区分查询值与日志标点。
        // 保守遮盖整个地址组，避免只处理外层 URL 而漏出内层凭据。
        let after_scheme = url.find("://").map_or(url.len(), |i| i + 3);
        if url[after_scheme..].contains("://") {
            out.push_str("***");
        } else {
            out.push_str(&mask_sensitive_query(&mask_credentials(url)));
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// 找出 `text` 里第一个 `scheme://…` 的字节范围；找不到返回 `None`。
///
/// 只按 ASCII 的 `://` 与边界字符切分，因此切片一定落在 UTF-8 字符边界上。
fn next_url_range(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 3 <= text.len() {
        if bytes[i] == b':' && bytes[i + 1] == b'/' && bytes[i + 2] == b'/' {
            let mut start = i;
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric()
                    || matches!(bytes[start - 1], b'+' | b'-' | b'.'))
            {
                start -= 1;
            }
            if start < i {
                let mut end = i + 3;
                while end < text.len() && !is_url_boundary(bytes[end]) {
                    end += 1;
                }
                // ffmpeg 常以单引号包裹 URL；userinfo 内的单引号/括号则属于合法密码字符。
                if start > 0 && bytes[start - 1] == b'\'' {
                    if let Some(close) = text[start..end].rfind('\'').map(|offset| start + offset) {
                        // 仅剥离包裹引号及其后的日志标点，不能把 userinfo 内的单引号当作结尾。
                        if close > i + 3
                            && bytes[close + 1..end]
                                .iter()
                                .all(|b| matches!(b, b':' | b',' | b';' | b')' | b']' | b'.'))
                        {
                            end = close;
                        }
                    }
                }
                if end > i + 3 {
                    return Some((start, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// URL 的终止字符：空白及不允许直接出现的字符；合法密码/查询值中的标点不能截断脱敏。
fn is_url_boundary(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'<' | b'>' | b'`' | b'{' | b'}' | b'|' | b'\\')
}

/// 需要遮盖的查询参数名（比较时忽略大小写与 `[]` 后缀）。
const SENSITIVE_KEYS: &[&str] = &[
    "secret",
    "token",
    "access_token",
    "access_key",
    "accesskey",
    "api_key",
    "apikey",
    "signature",
    "sig",
    "sign",
    "signkey",
    "auth",
    "auth_key",
    "key",
    "password",
    "passwd",
    "pwd",
    "credential",
    "credentials",
];

fn is_sensitive_key(name: &str) -> bool {
    let k = name
        .trim()
        .trim_end_matches(['[', ']'])
        .to_ascii_lowercase();
    SENSITIVE_KEYS.contains(&k.as_str())
}

/// 遮盖查询串里的敏感参数值，键名与非敏感参数原样保留。
///
/// 保留键名是为了让日志还能读出"哪类凭据被用了"；`timestamp` 这类过期时间不遮盖，
/// 否则没法从日志判断地址是否已失效。
fn mask_sensitive_query(url: &str) -> String {
    let Some(after_q) = url.find('?').map(|i| i + 1) else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..after_q]);
    let mut first = true;
    for pair in url[after_q..].split('&') {
        if !first {
            out.push('&');
        }
        first = false;
        match pair.split_once('=') {
            Some((k, _)) if is_sensitive_key(k) => {
                out.push_str(k);
                out.push_str("=***");
            }
            _ => out.push_str(pair),
        }
    }
    out
}

/// 只保留 URL userinfo 里的安全字符，其余百分号编码。
fn encode_credential(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------- 响应判定

/// 把 HTTP 响应收敛成「XML 字符串或人话错误」。
fn expect_ok(resp: http::Response) -> Result<String, String> {
    let text = resp.body_text();
    if resp.is_success() {
        return Ok(text);
    }
    let detail = fault_reason(&text);
    if resp.status == 401 || resp.status == 403 || text.contains("NotAuthorized") {
        return Err(format!(
            "ONVIF 鉴权失败（HTTP {} / {}）。请确认已在设备侧创建独立账号（如 TP-Link 的「摄像机账户」），\
             且该账号允许 ONVIF 访问",
            resp.status, detail
        ));
    }
    Err(format!("ONVIF 返回 HTTP {}：{}", resp.status, detail))
}

/// 从 SOAP Fault 里抽出 `Subcode` 与 `Reason/Text`。
///
/// 之前只取第一行，于是 TP-Link 这类设备的报错缩成 `<?xml version="1.0"...`，
/// 真正的 `ter:NotAuthorized` 反而看不到。
fn fault_reason(xml: &str) -> String {
    let subcode = inner_text(xml, "Subcode")
        .and_then(|s| inner_text(&s, "Value"))
        // Subcode 里可能还嵌套一层，稳妥点再扫一次全文档
        .or_else(|| {
            elements(xml, "Subcode")
                .first()
                .and_then(|e| inner_text(e.inner, "Value"))
        });
    let reason = inner_text(xml, "Text").or_else(|| inner_text(xml, "faultstring"));
    match (subcode, reason) {
        (Some(c), Some(r)) => format!("{c}：{r}"),
        (Some(c), None) => c,
        (None, Some(r)) => r,
        (None, None) => first_line(xml),
    }
}

fn path_of(url: &str) -> String {
    http::Url::parse(url)
        .map(|u| u.path)
        .unwrap_or_else(|_| "/".to_string())
}

// ---------------------------------------------------------------- HTTP Digest

/// `WWW-Authenticate: Digest ...` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
}

/// 解析 Digest 质询头；不是 Digest（比如只有 Basic）时返回 `None`。
pub fn parse_digest_challenge(header: &str) -> Option<DigestChallenge> {
    let lower = header.to_ascii_lowercase();
    if !lower.trim_start().starts_with("digest") {
        return None;
    }
    Some(DigestChallenge {
        realm: param(header, "realm")?,
        nonce: param(header, "nonce")?,
        qop: param(header, "qop"),
        opaque: param(header, "opaque"),
        algorithm: param(header, "algorithm"),
    })
}

/// 取 `name=value` 或 `name="value"` 里的 value。
///
/// 用非转义的朴素扫描：ONVIF 设备的质询头都是单行、无逗号转义，
/// 引号内的逗号（realm 里常见）靠「先看引号」来规避。
fn param(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(name) {
        let start = from + rel;
        // 前一个字符必须是分隔符，避免在 `xnonce` 里匹配到 `nonce`
        let prev = header[..start].chars().last();
        let boundary = match prev {
            None => true,
            Some(c) => c == ',' || c.is_whitespace(),
        };
        let after_name = lower[start + name.len()..].trim_start();
        if boundary && after_name.starts_with('=') {
            let value = after_name[1..].trim_start();
            // 索引换算回原串（to_ascii_lowercase 不改变 ASCII 字节长度）
            let offset = header.len() - value.len();
            let raw = &header[offset..];
            if let Some(stripped) = raw.strip_prefix('"') {
                return stripped.find('"').map(|end| stripped[..end].to_string());
            }
            let end = raw.find(',').unwrap_or(raw.len());
            return Some(raw[..end].trim().to_string());
        }
        from = start + name.len();
    }
    None
}

/// 生成 `Authorization: Digest ...` 头。
///
/// 参数 `nc`（nonce count）与 `cnonce` 由调用方给，便于按 RFC 2617 的例子做断言。
pub fn digest_authorization(
    challenge: &DigestChallenge,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    cnonce: &str,
    nc: &str,
) -> Result<String, String> {
    let algorithm = challenge.algorithm.as_deref().unwrap_or("MD5");
    let md5_sess = algorithm.eq_ignore_ascii_case("MD5-sess");
    if !algorithm.eq_ignore_ascii_case("MD5") && !md5_sess {
        return Err(format!(
            "设备要求 Digest 算法 {algorithm}，本程序只实现 MD5。\
             TP-Link VIGI 可在 Web UI「设置 → 网络设置 → 高级 → ONVIF → 摘要认证算法」改成 MD5"
        ));
    }

    let mut ha1 = hex_digest(format!("{username}:{}:{password}", challenge.realm).as_bytes());
    if md5_sess {
        ha1 = hex_digest(format!("{ha1}:{}:{cnonce}", challenge.nonce).as_bytes());
    }
    let ha2 = hex_digest(format!("{method}:{uri}").as_bytes());

    let qop_auth = challenge
        .qop
        .as_deref()
        .map(|q| q.split(',').any(|x| x.trim().eq_ignore_ascii_case("auth")))
        .unwrap_or(false);

    let response = if qop_auth {
        hex_digest(format!("{ha1}:{}:{nc}:{cnonce}:auth:{ha2}", challenge.nonce).as_bytes())
    } else {
        hex_digest(format!("{ha1}:{}:{ha2}", challenge.nonce).as_bytes())
    };

    let mut out = format!(
        r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{response}""#,
        quote(username),
        quote(&challenge.realm),
        quote(&challenge.nonce),
        quote(uri)
    );
    if qop_auth {
        out.push_str(&format!(
            ", qop=auth, nc={nc}, cnonce=\"{}\"",
            quote(cnonce)
        ));
    }
    if let Some(opaque) = &challenge.opaque {
        out.push_str(&format!(", opaque=\"{}\"", quote(opaque)));
    }
    out.push_str(&format!(", algorithm={algorithm}"));
    Ok(out)
}

/// 值里若出现 `"` 或 `\`，按 HTTP 规则转义，避免把请求头截断。
fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------- SOAP / WS-Security

pub fn soap_envelope(security: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">{security}<s:Body>{body}</s:Body></s:Envelope>"#
    )
}

/// WS-Security UsernameToken（PasswordDigest 形式）。
pub fn ws_username_token(username: &str, password: &str) -> String {
    let nonce = make_nonce();
    let created = iso8601_now();
    let mut material = Vec::with_capacity(nonce.len() + created.len() + password.len());
    material.extend_from_slice(&nonce);
    material.extend_from_slice(created.as_bytes());
    material.extend_from_slice(password.as_bytes());
    let password_digest = base64(&digest(&material));
    format!(
        r#"<s:Header><Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{}</Created></UsernameToken></Security></s:Header>"#,
        escape(username),
        password_digest,
        base64(&nonce),
        created
    )
}

fn make_nonce() -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = Vec::with_capacity(16);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&c.to_le_bytes());
    digest(&seed).to_vec()
}

// ---------------------------------------------------------------- 时间格式

pub fn iso8601_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso8601_from_unix(secs)
}

pub fn iso8601_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn iso8601_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs % 60 == 0 {
        format!("PT{}M", secs / 60)
    } else {
        format!("PT{}S", secs.max(1))
    }
}

// ---------------------------------------------------------------- 极简 XML 扫描

#[derive(Debug)]
pub struct Elem<'a> {
    /// 开标签原文（含属性），例如 `<trt:Profiles token="P1" fixed="true">`
    pub open: &'a str,
    /// 标签内的文本
    pub inner: &'a str,
}

/// 找到所有本地名等于 `tag` 的元素（自动忽略命名空间前缀）。
pub fn elements<'a>(xml: &'a str, tag: &str) -> Vec<Elem<'a>> {
    let bytes = xml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = xml[i..].find('<') {
        let start = i + rel;
        if start + 1 >= xml.len() {
            break;
        }
        let next = bytes[start + 1];
        if next == b'/' || next == b'?' || next == b'!' {
            i = start + 1;
            continue;
        }
        let open_end = match xml[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let open_tag = &xml[start..=open_end];
        let name = open_tag[1..]
            .split(|c: char| c == '>' || c == '/' || c.is_whitespace())
            .next()
            .unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tag {
            if open_tag.ends_with("/>") {
                out.push(Elem {
                    open: open_tag,
                    inner: "",
                });
                i = open_end + 1;
                continue;
            }
            let close = format!("</{name}>");
            if let Some(rel_close) = xml[open_end..].find(&close) {
                let inner_start = open_end + 1;
                let inner_end = open_end + rel_close;
                out.push(Elem {
                    open: open_tag,
                    inner: &xml[inner_start..inner_end],
                });
                i = inner_end + close.len();
                continue;
            }
            // 截断的目标元素不能作为有效数据返回。
        }
        i = open_end + 1;
    }
    out
}

/// 第一个匹配元素的文本（去空白、去 CDATA）。
pub fn inner_text(xml: &str, tag: &str) -> Option<String> {
    let els = elements(xml, tag);
    let raw = els.first()?.inner.trim();
    if raw.is_empty() {
        return None;
    }
    let cleaned = raw
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .trim();
    Some(cleaned.to_string())
}

/// 在开标签里读属性值。
pub fn attr(open_tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let idx = open_tag.find(&needle)?;
    let rest = &open_tag[idx + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_and_nested_urls_never_expose_credentials() {
        for urls in [
            "http://first.invalid/path,rtsp://demo:synthetic-password@second.invalid/live",
            "'http://first.invalid/path','rtsp://demo:synthetic-password@second.invalid/live'",
            "http://first.invalid/?redirect=rtsp://demo:synthetic-password@second.invalid/live?token=synthetic-token",
            "http://first.invalid/?token=http://synthetic-token.invalid/path",
        ] {
            let masked = mask_url_in_text(&format!("input {urls} end"));
            assert!(!masked.contains("synthetic-password"), "{masked}");
            assert!(!masked.contains("synthetic-token"), "{masked}");
            assert!(masked.starts_with("input ") && masked.ends_with(" end"));
            assert_eq!(mask_url_in_text(&masked), masked);
        }
    }

    #[test]
    fn quoted_sensitive_url_preserves_ffmpeg_log_punctuation() {
        let output = mask_url_in_text(
            "from 'rtsp://demo:test(pass)'word@camera.invalid/live?token=test(secret)':",
        );
        assert_eq!(
            output,
            "from 'rtsp://demo:***@camera.invalid/live?token=***':"
        );
    }

    #[test]
    fn unclosed_target_elements_are_not_returned_as_valid_text() {
        assert!(elements("<Uri>rtsp://camera.invalid/live", "Uri").is_empty());
        assert!(inner_text("<Name>truncated", "Name").is_none());
        assert!(elements("<Profiles token=\"test\"><Name>ok</Name>", "Profiles").is_empty());
        assert_eq!(
            inner_text("<Uri>rtsp://camera.invalid/live</Uri>", "Uri").as_deref(),
            Some("rtsp://camera.invalid/live")
        );
    }

    #[test]
    fn masks_legal_punctuation_in_url_passwords_and_signed_values() {
        for password in ["demo(pass)", "demo,pass", "demo;pass", "demo'pass"] {
            let text =
                format!("input rtsp://audit:{password}@camera.invalid/live?token=test(secret)");
            let masked = mask_url_in_text(&text);
            assert!(!masked.contains(password), "必须遮盖整个密码：{masked}");
            assert!(
                !masked.contains("test(secret)"),
                "必须遮盖整个签名：{masked}"
            );
        }
    }

    #[test]
    fn compares_against_known_base64_digest_shape() {
        // 只验证结构：包含 UsernameToken/Password/Nonce/Created 四段
        let header = ws_username_token("admin", "pass123");
        assert!(header.contains("<Username>admin</Username>"));
        assert!(header.contains("#PasswordDigest"));
        assert!(header.contains("#Base64Binary"));
        assert!(header.contains("</Created>"));
    }

    #[test]
    fn iso8601_epoch() {
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn finds_profiles_with_token_and_name() {
        let xml = r#"<trt:GetProfilesResponse xmlns:trt="x">
            <trt:Profiles token="Profile_1" fixed="true"><tt:Name>mainStream</tt:Name></trt:Profiles>
            <trt:Profiles token="Profile_2"><tt:Name>subStream</tt:Name></trt:Profiles>
        </trt:GetProfilesResponse>"#;
        let els = elements(xml, "Profiles");
        assert_eq!(els.len(), 2);
        assert_eq!(attr(els[0].open, "token").as_deref(), Some("Profile_1"));
        assert_eq!(
            inner_text(els[1].inner, "Name").as_deref(),
            Some("subStream")
        );
    }

    #[test]
    fn ignores_self_closing_and_other_tags() {
        let xml = r#"<a><b/><Profiles token="X"/></a>"#;
        let els = elements(xml, "Profiles");
        assert_eq!(els.len(), 1);
        assert_eq!(els[0].inner, "");
    }

    #[test]
    fn duration_format() {
        assert_eq!(iso8601_duration(Duration::from_secs(30)), "PT30S");
        assert_eq!(iso8601_duration(Duration::from_secs(120)), "PT2M");
    }

    #[test]
    fn finds_stream_uri() {
        let xml = r#"<trt:GetStreamUriResponse xmlns:trt="x"><trt:MediaUri>
            <tt:Uri xmlns:tt="y">rtsp://192.168.1.64:554/Streaming/Channels/101?transportmode=unicast</tt:Uri>
            <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
        </trt:MediaUri></trt:GetStreamUriResponse>"#;
        assert_eq!(
            inner_text(xml, "Uri").as_deref(),
            Some("rtsp://192.168.1.64:554/Streaming/Channels/101?transportmode=unicast")
        );
    }

    #[test]
    fn injects_credentials_into_bare_rtsp() {
        assert_eq!(
            inject_credentials("rtsp://192.168.1.64:554/live", "admin", "p@ss:1"),
            "rtsp://admin:p%40ss%3A1@192.168.1.64:554/live"
        );
    }

    #[test]
    fn keeps_existing_credentials() {
        let u = "rtsp://bob:pw@10.0.0.9/ch1";
        assert_eq!(inject_credentials(u, "admin", "x"), u);
    }

    #[test]
    fn ignores_non_rtsp_and_empty_user() {
        assert_eq!(inject_credentials("http://a/b", "admin", "x"), "http://a/b");
        assert_eq!(inject_credentials("rtsp://a/b", "", "x"), "rtsp://a/b");
    }

    #[test]
    fn masks_password_for_display() {
        assert_eq!(
            mask_credentials("rtsp://tapo:p%40ss@192.168.1.50:554/stream1"),
            "rtsp://tapo:***@192.168.1.50:554/stream1"
        );
        assert_eq!(
            mask_credentials("rtsp://192.168.1.50:554/stream1"),
            "rtsp://192.168.1.50:554/stream1"
        );
        assert_eq!(mask_credentials("not a url"), "not a url");
    }

    #[test]
    fn masks_url_embedded_in_ffmpeg_stderr() {
        let line =
            "[tcp @ 0x1f9a] Opening 'rtsp://admin:pw123@192.168.1.10:554/stream1' for reading";
        let out = mask_url_in_text(line);
        assert!(
            out.contains("rtsp://admin:***@192.168.1.10:554/stream1"),
            "{out}"
        );
        assert!(!out.contains("pw123"), "密码不能留在日志里: {out}");
        // 上下文要留着，否则日志没法用来排错
        assert!(out.contains("for reading"));
    }

    #[test]
    fn masks_signed_query_parameters() {
        let line =
            "rtmps://play.xdrtc.com:1935/ipc/k?timestamp=1700000000&secret=abcdef0123456789abcdef";
        let out = mask_url_in_text(line);
        assert!(out.contains("secret=***"), "{out}");
        assert!(!out.contains("abcdef0123456789abcdef"), "{out}");
        // 过期时间不遮盖：日志要能判断地址是否还有效
        assert!(out.contains("timestamp=1700000000"), "{out}");
    }

    #[test]
    fn masking_is_idempotent_and_keeps_plain_text() {
        let once = mask_url_in_text("rtsp://u:p@h/x");
        assert_eq!(mask_url_in_text(&once), once, "重复脱敏必须幂等");
        assert_eq!(
            mask_url_in_text("没有 URL 的一行日志"),
            "没有 URL 的一行日志"
        );
        assert_eq!(
            mask_url_in_text("本地路径 D:/a/b.mkv 不是 URL"),
            "本地路径 D:/a/b.mkv 不是 URL"
        );
    }

    #[test]
    fn parses_realistic_digest_challenge() {
        // 典型国产 IPC 的质询头：realm 里带逗号和空格
        let header = r#"Digest realm="IP Camera, Inc.", qop="auth", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", algorithm=MD5"#;
        let ch = parse_digest_challenge(header).expect("应能解析");
        assert_eq!(ch.realm, "IP Camera, Inc.");
        assert_eq!(ch.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(ch.qop.as_deref(), Some("auth"));
        assert_eq!(ch.algorithm.as_deref(), Some("MD5"));
    }

    #[test]
    fn rejects_basic_and_missing_nonce() {
        assert!(parse_digest_challenge("Basic realm=\"x\"").is_none());
        assert!(parse_digest_challenge("Digest realm=\"x\"").is_none());
    }

    #[test]
    fn digest_matches_rfc2617_example() {
        // RFC 2617 §3.5 官方例子，response 必须等于 6629fae49393a05397450978507c4ef1
        let ch = DigestChallenge {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            qop: Some("auth".into()),
            opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".into()),
            algorithm: None,
        };
        let header = digest_authorization(
            &ch,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "0a4f113b",
            "00000001",
        )
        .unwrap();
        assert!(
            header.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{header}"
        );
        assert!(header.contains("qop=auth"));
        assert!(header.contains("nc=00000001"));
        assert!(header.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
        assert!(header.starts_with("Digest username=\"Mufasa\""));
    }

    #[test]
    fn digest_without_qop_uses_rfc2069_form() {
        let ch = DigestChallenge {
            realm: "r".into(),
            nonce: "n".into(),
            qop: None,
            opaque: None,
            algorithm: None,
        };
        let header = digest_authorization(&ch, "u", "p", "POST", "/s", "c", "00000001").unwrap();
        assert!(!header.contains("qop"), "{header}");
        // 无 qop 时不带 nc/cnonce
        assert!(!header.contains("nc="));
    }

    #[test]
    fn digest_sha256_tells_user_what_to_change() {
        let ch = DigestChallenge {
            realm: "r".into(),
            nonce: "n".into(),
            qop: Some("auth".into()),
            opaque: None,
            algorithm: Some("SHA-256".into()),
        };
        let err = digest_authorization(&ch, "u", "p", "POST", "/s", "c", "1").unwrap_err();
        assert!(err.contains("SHA-256"));
        assert!(err.contains("VIGI"));
    }

    #[test]
    fn parses_ptz_status_position() {
        let xml = r#"<?xml version="1.0"?><tptz:GetStatusResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
<tptz:PTZStatus><tt:Position xmlns:tt="http://www.onvif.org/ver10/schema">
<tt:PanTilt x="-0.4123" y="0.1250" space="urn:onvif:ptz:pos"/><tt:Zoom x="0.0000" space="urn:onvif:ptz:zoom"/>
</tt:Position><tt:MoveStatus><tt:PanTilt>MOVING</tt:PanTilt><tt:Zoom>IDLE</tt:Zoom></tt:MoveStatus>
</tptz:PTZStatus></tptz:GetStatusResponse>"#;
        let pan_tilt = elements(xml, "PanTilt")
            .into_iter()
            .find(|e| attr(e.open, "x").is_some())
            .expect("应找到带位置的 PanTilt");
        assert_eq!(attr(pan_tilt.open, "x").as_deref(), Some("-0.4123"));
        assert_eq!(attr(pan_tilt.open, "y").as_deref(), Some("0.1250"));
        let move_status = elements(xml, "MoveStatus");
        assert!(move_status.first().unwrap().inner.contains("MOVING"));
    }

    #[test]
    fn fault_reason_surfaces_not_authorized() {
        // TP-Link / gSOAP 设备对无凭据请求回 400 + 这段 Fault（真机实测）
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:ter="http://www.onvif.org/ver10/error">
<SOAP-ENV:Body><SOAP-ENV:Fault>
<SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:Sender</SOAP-ENV:Value>
<SOAP-ENV:Subcode><SOAP-ENV:Value>ter:NotAuthorized</SOAP-ENV:Value></SOAP-ENV:Subcode></SOAP-ENV:Code>
<SOAP-ENV:Reason><SOAP-ENV:Text xml:lang="en">Authority failure</SOAP-ENV:Text></SOAP-ENV:Reason>
</SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;
        assert_eq!(fault_reason(xml), "ter:NotAuthorized：Authority failure");
    }

    #[test]
    fn fault_reason_falls_back_to_first_line() {
        assert_eq!(fault_reason("<html>bad</html>"), "<html>bad</html>");
    }

    #[test]
    fn path_extraction_for_digest() {
        assert_eq!(
            path_of("http://192.168.1.9:2020/onvif/media_service"),
            "/onvif/media_service"
        );
        assert_eq!(path_of("http://192.168.1.9"), "/");
    }
}
