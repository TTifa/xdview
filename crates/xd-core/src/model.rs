//! 设备与能力模型。

/// 拉流协议。由 URL scheme 推导，决定 ffmpeg 的输入参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// 实时流（ffmpeg `-rtsp_transport tcp`）
    Rtsp,
    /// 云端/卡回看（小度同类实现里走 RTMP）
    Rtmp,
    /// HLS（分片播放列表输入）
    Hls,
    /// MJPEG over HTTP（低端 IPC 常见）
    Mjpeg,
    /// 其他，交给 ffmpeg 自行识别
    Other,
}

impl Transport {
    pub fn from_url(url: &str) -> Transport {
        let lower = url.trim().to_ascii_lowercase();
        if lower.starts_with("rtsp://") || lower.starts_with("rtsps://") {
            Transport::Rtsp
        } else if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") {
            Transport::Rtmp
        } else if lower.starts_with("http://") || lower.starts_with("https://") {
            if lower.contains(".m3u8") {
                Transport::Hls
            } else if lower.contains("mjpeg") || lower.contains("action=stream") {
                Transport::Mjpeg
            } else {
                Transport::Other
            }
        } else {
            Transport::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Rtsp => "RTSP",
            Transport::Rtmp => "RTMP",
            Transport::Hls => "HLS",
            Transport::Mjpeg => "MJPEG",
            Transport::Other => "FFmpeg",
        }
    }
}

/// 设备能力位。
///
/// 小度 App 通过下发的能力描述符（`duershow://settings/ai_camera/...`）动态渲染工具栏，
/// 这里用同样的思路：UI 只显示 [`Capabilities`] 允许的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub ptz: bool,
    pub zoom: bool,
    pub audio: bool,
    pub talk: bool,
    pub snapshot: bool,
    pub recording: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            ptz: true,
            zoom: true,
            audio: false,
            talk: false,
            snapshot: true,
            recording: true,
        }
    }
}

impl Capabilities {
    /// 全关，用于「只拉流、不带云台」的纯播放场景。
    pub fn viewer_only() -> Self {
        Self {
            ptz: false,
            zoom: false,
            audio: false,
            talk: false,
            snapshot: true,
            recording: true,
        }
    }
}

/// 一路摄像机。
#[derive(Debug, Clone)]
pub struct Camera {
    pub id: String,
    pub name: String,
    /// 主码流地址（RTSP/RTMP/HLS 均可）。
    pub stream_url: String,
    /// 子码流地址，用于弱网/移动端降清晰度。
    pub sub_stream_url: Option<String>,
    /// ONVIF 服务入口，例如 `http://192.168.1.10` 或 `http://192.168.1.10/onvif/ptz_service`。
    pub onvif_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub capabilities: Capabilities,
    /// 云台左右反向（有些设备开了「镜像/反向」，或吊装后方向天生相反）
    pub invert_pan: bool,
    /// 云台上下反向
    pub invert_tilt: bool,
}

impl Camera {
    pub fn new(id: impl Into<String>, name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            stream_url: url.into(),
            sub_stream_url: None,
            onvif_url: None,
            username: None,
            password: None,
            capabilities: Capabilities::default(),
            invert_pan: false,
            invert_tilt: false,
        }
    }

    pub fn transport(&self) -> Transport {
        Transport::from_url(&self.stream_url)
    }

    /// 选择码流：`high == true` 用主码流。
    pub fn url_for(&self, high: bool) -> &str {
        if high {
            &self.stream_url
        } else {
            self.sub_stream_url.as_deref().unwrap_or(&self.stream_url)
        }
    }

    /// 实际交给 ffmpeg 的地址：配置里的 URL 没带账号时，用 `username`/`password` 补上。
    ///
    /// TP-Link / 海康这类设备的 RTSP 地址本身不含凭据（如
    /// `rtsp://192.168.1.50:554/stream1`），用户习惯把账号写在独立字段里，
    /// 若直接把裸地址交给 ffmpeg 会 401。小度走 ONVIF 那条路时是
    /// `inject_credentials` 补的，这里把同一逻辑套到手工配置的地址上。
    pub fn resolved_url(&self, high: bool) -> String {
        let url = self.url_for(high);
        match self.username.as_deref() {
            Some(user) if !user.is_empty() => {
                crate::onvif::inject_credentials(url, user, self.password.as_deref().unwrap_or(""))
            }
            _ => url.to_string(),
        }
    }

    /// 按设备实际方向校正云台速度。
    ///
    /// TP-Link TL-IPC 实测：下发 `pan=+0.8`（本应向右）位置反而减小，
    /// `tilt=+0.6`（本应向上）位置也减小 —— 双轴都反。这类差异只能按机型配，
    /// 所以做成 `invert_pan` / `invert_tilt` 两个开关。
    pub fn fix_velocity(&self, v: crate::onvif::Velocity) -> crate::onvif::Velocity {
        crate::onvif::Velocity {
            pan: if self.invert_pan { -v.pan } else { v.pan },
            tilt: if self.invert_tilt { -v.tilt } else { v.tilt },
            zoom: v.zoom,
        }
    }

    pub fn has_credentials(&self) -> bool {
        self.username
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    /// ONVIF 服务根地址（去掉已知的服务路径后缀）。
    pub fn onvif_base(&self) -> Option<String> {
        let raw = self.onvif_url.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let mut base = raw.trim_end_matches('/').to_string();
        for suffix in [
            "/onvif/ptz_service",
            "/onvif/media_service",
            "/onvif/device_service",
        ] {
            if let Some(stripped) = base.strip_suffix(suffix) {
                base = stripped.to_string();
                break;
            }
        }
        Some(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_detect() {
        assert_eq!(Transport::from_url("rtsp://a/b"), Transport::Rtsp);
        assert_eq!(Transport::from_url("RTMP://a/b"), Transport::Rtmp);
        assert_eq!(Transport::from_url("https://a/x.m3u8?y=1"), Transport::Hls);
        assert_eq!(Transport::from_url("http://a/mjpeg"), Transport::Mjpeg);
        assert_eq!(Transport::from_url("udp://a"), Transport::Other);
    }

    #[test]
    fn onvif_base_strips_service() {
        let mut cam = Camera::new("1", "门", "rtsp://a/s");
        cam.onvif_url = Some("http://192.168.1.9/onvif/ptz_service".into());
        assert_eq!(cam.onvif_base().as_deref(), Some("http://192.168.1.9"));

        cam.onvif_url = Some("http://192.168.1.9/".into());
        assert_eq!(cam.onvif_base().as_deref(), Some("http://192.168.1.9"));
    }

    #[test]
    fn velocity_inversion_is_per_camera() {
        let mut cam = Camera::new("t", "TP-Link", "rtsp://a/1");
        let v = crate::onvif::Velocity {
            pan: 0.7,
            tilt: -0.5,
            zoom: 0.2,
        };
        assert_eq!(cam.fix_velocity(v), v);
        cam.invert_pan = true;
        cam.invert_tilt = true;
        let fixed = cam.fix_velocity(v);
        assert_eq!(fixed.pan, -0.7);
        assert_eq!(fixed.tilt, 0.5);
        assert_eq!(fixed.zoom, 0.2, "缩放不应被翻");
    }

    #[test]
    fn resolved_url_injects_configured_account() {
        // TP-Link 的地址形如 rtsp://ip:554/stream1，账号在独立字段里
        let mut cam = Camera::new("p", "阳台", "rtsp://192.168.1.50:554/stream1");
        cam.username = Some("tapo".into());
        cam.password = Some("p@ss word".into());
        assert_eq!(
            cam.resolved_url(true),
            "rtsp://tapo:p%40ss%20word@192.168.1.50:554/stream1"
        );

        // 子码流同样处理
        cam.sub_stream_url = Some("rtsp://192.168.1.50:554/stream2".into());
        assert_eq!(
            cam.resolved_url(false),
            "rtsp://tapo:p%40ss%20word@192.168.1.50:554/stream2"
        );
    }

    #[test]
    fn resolved_url_keeps_inline_credentials() {
        let mut cam = Camera::new(
            "h",
            "门口",
            "rtsp://admin:pw@192.168.1.9/Streaming/Channels/101",
        );
        cam.username = Some("admin".into());
        cam.password = Some("pw".into());
        assert_eq!(
            cam.resolved_url(true),
            "rtsp://admin:pw@192.168.1.9/Streaming/Channels/101"
        );
    }

    #[test]
    fn resolved_url_untouched_without_account() {
        let cam = Camera::new("x", "无账号", "rtsp://192.168.1.9/live");
        assert_eq!(cam.resolved_url(true), "rtsp://192.168.1.9/live");
    }
}
