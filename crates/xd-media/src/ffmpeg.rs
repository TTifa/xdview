//! ffmpeg / ffprobe 命令行构造。

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 网络相关的行为开关。这两项都是**真机实测**逼出来的：
///
/// * `no_proxy`：ffmpeg 8 会读取 `http_proxy` 等环境变量，连裸 TCP（RTMP/RTSP）也会被塞进
///   HTTP 代理。实测本机代理把 `rtmps://…:1935` 导到 `127.0.0.1:3743` 后 TLS 必失败。
///   LAN 摄像头更不该走代理，所以默认清空这些变量。
/// * `insecure_tls`：默认关闭。2026-10-31 曾观测到小度 RTMPS 端点证书过期，
///   当时使用的 ffmpeg/GnuTLS 构建握手失败；该历史现象不代表当前证书状态或所有构建。
///   排查应先确认当前证书与工具版本，仅对已确认的兼容问题临时使用 `-tls_verify 0`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetOptions {
    pub no_proxy: bool,
    pub insecure_tls: bool,
    /// 给 RTSP 加 `-hwaccel auto`：4K HEVC 软解很吃 CPU，硬解能省一半以上
    pub hwaccel: bool,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            no_proxy: true,
            insecure_tls: false,
            hwaccel: true,
        }
    }
}

/// `-rw_timeout` 只能给非 TLS 的裸连接用。
///
/// 真机实测：给 `rtmps://` 加 `-rw_timeout 5000000` 后，gnutls 在握手阶段把读超时当成致命错误，
/// 直接 `IO error: Error number -138`；去掉该参数后同样的地址能正常完成 RTMP 握手。
pub fn rw_timeout_applicable(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    if !l.contains("://") {
        return false;
    }
    !(l.starts_with("rtmps://") || l.starts_with("rtsps://") || l.starts_with("https://"))
}

/// 会被 ffmpeg 读取的代理环境变量。
pub const PROXY_ENV: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub fps: Option<f32>,
}

/// 由 `ffmpeg` 路径推导 `ffprobe` 路径（同目录同名换前缀），推导不出就退回 PATH 里的 `ffprobe`。
pub fn ffprobe_bin(ffmpeg: &str) -> String {
    let p = Path::new(ffmpeg);
    match p.file_name().and_then(|s| s.to_str()) {
        Some(name) if name.to_ascii_lowercase().contains("ffmpeg") => {
            let replaced = name.replacen("ffmpeg", "ffprobe", 1);
            match p.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => {
                    dir.join(replaced).to_string_lossy().to_string()
                }
                _ => replaced,
            }
        }
        _ => "ffprobe".to_string(),
    }
}

/// 探测超时。真机测试时 `rtmps` 地址在代理劫持下会无限期挂着，没有上限会把调用线程拖死。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 探测第一路视频流的尺寸与编码。失败/超时返回 `None`（调用方走固定 1280x720 兜底）。
///
/// `cancel` 置位时立即放弃并杀掉 ffprobe —— 停止拉流时不该再等满 8 秒。
///
/// 超时/取消**一定会 kill + wait**：早期的实现只是给 `recv_timeout` 设 8 秒，
/// 后台线程仍阻塞在 `Command::output()` 上，挂住的 ffprobe 会随每次重连不断积累。
pub fn probe(ffmpeg: &str, url: &str, net: NetOptions, cancel: &AtomicBool) -> Option<ProbeInfo> {
    let ffprobe = ffprobe_bin(ffmpeg);
    let mut cmd = Command::new(&ffprobe);
    cmd.args(["-v", "error"]);
    let is_rtsp = url.trim().to_ascii_lowercase().starts_with("rtsp");
    if is_rtsp {
        cmd.args(["-timeout", "5000000"]);
    } else if rw_timeout_applicable(url) {
        cmd.args(["-rw_timeout", "5000000"]);
    }
    if net.insecure_tls {
        cmd.args(["-tls_verify", "0"]);
    }
    cmd.args([
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,codec_name,avg_frame_rate",
        "-of",
        "default=noprint_wrappers=1",
        url,
    ]);
    apply_net(&mut cmd, net);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // 子进程由本函数自己持有：这样超时/取消时能直接 kill + wait，
    // 而不是像 `Command::output()` 那样只能干等。
    let mut child = match crate::sys::spawn_managed(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[probe] 启动 ffprobe 失败（{ffprobe}）：{e}");
            return None;
        }
    };

    // 输出在另一条线程读：ffprobe 输出量很小，但管道写满会把子进程卡死，
    // 而本线程必须留着轮询 try_wait 才能响应取消。
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stdout.as_mut() {
            let _ = std::io::Read::read_to_string(s, &mut text);
        }
        text
    });

    let status = wait_cancellable(&mut child, PROBE_TIMEOUT, cancel);
    // 走到这里子进程一定已结束（正常退出或被 kill），读线程会读到 EOF 后返回
    let text = reader.join().unwrap_or_default();
    match status {
        Some(st) if st.success() => parse_probe_output(&text),
        _ => None,
    }
}

/// 等子进程退出，期间检查 `cancel`；超时或取消都 kill。
fn wait_cancellable(
    child: &mut std::process::Child,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            return child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// 解析 ffprobe 的 `default=noprint_wrappers=1` 输出。
fn parse_probe_output(text: &str) -> Option<ProbeInfo> {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut codec = String::from("unknown");
    let mut fps = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "width" => width = v.trim().parse().unwrap_or(0),
                "height" => height = v.trim().parse().unwrap_or(0),
                "codec_name" => codec = v.trim().to_string(),
                "avg_frame_rate" => fps = parse_ratio(v.trim()),
                _ => {}
            }
        }
    }
    if width == 0 || height == 0 {
        return None;
    }
    Some(ProbeInfo {
        width,
        height,
        codec,
        fps,
    })
}

/// 按 [`NetOptions`] 调整子进程环境。
pub fn apply_net(cmd: &mut Command, net: NetOptions) {
    if net.no_proxy {
        for var in PROXY_ENV {
            cmd.env_remove(var);
        }
    }
}

fn parse_ratio(s: &str) -> Option<f32> {
    let (a, b) = s.split_once('/')?;
    let a: f32 = a.parse().ok()?;
    let b: f32 = b.parse().ok()?;
    if b <= 0.0 || a <= 0.0 {
        return None;
    }
    Some(a / b)
}

/// 目标解码尺寸。`max_width == 0` 表示不缩放。
///
/// 返回 `(width, height, 是否需要 -vf scale)`；宽高都保证为偶数，避免 H.264 的
/// `yuv420p` 尺寸约束导致某些摄像头协商失败。
pub fn target_size(probe: Option<&ProbeInfo>, max_width: u32) -> (usize, usize, bool) {
    match probe {
        Some(p) => {
            let pw = p.width as usize;
            let ph = p.height as usize;
            if max_width == 0 || pw <= max_width as usize {
                (even(pw), even(ph), false)
            } else {
                let w = even(max_width as usize);
                let h = even(ph * w / pw.max(1));
                (w, h.max(2), true)
            }
        }
        None => (1280, 720, true),
    }
}

fn even(v: usize) -> usize {
    if v < 2 {
        2
    } else if v % 2 == 0 {
        v
    } else {
        v - 1
    }
}

/// 是否是"一发一收"的实时流。只有这类输入才该用激进低延迟参数。
///
/// 实测：`-fflags nobuffer` 作用在文件 / 需要回退式探测的容器（moov 在尾部）上，
/// 会导致 ffmpeg 一帧都读不出来（"Output file is empty, nothing was encoded"），
/// 因此绝不能对 HLS / 本地文件无脑加。
pub fn is_live_source(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("rtsp://")
        || l.starts_with("rtsps://")
        || l.starts_with("rtmp://")
        || l.starts_with("rtmps://")
        || l.starts_with("srt://")
        || l.starts_with("udp://")
        || l.starts_with("rtp://")
}

/// 构造 ffmpeg 输入参数。输出恒为 `rawvideo/rgba` 到 stdout。
pub fn input_args(url: &str, low_latency: bool, net: NetOptions) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "warning".into(),
    ];

    let lower = url.to_ascii_lowercase();
    if net.insecure_tls {
        args.push("-tls_verify".into());
        args.push("0".into());
    }
    let is_rtsp = lower.starts_with("rtsp://") || lower.starts_with("rtsps://");
    if is_rtsp {
        if net.hwaccel {
            args.push("-hwaccel".into());
            args.push("auto".into());
        }
        // IPC 的 UDP 丢包会让画面糊成马赛克，默认走 TCP 更稳
        args.push("-rtsp_transport".into());
        args.push("tcp".into());
        // RTSP 用 demuxer 自己的 -timeout（微秒）；这一层不认 -rw_timeout
        args.push("-timeout".into());
        args.push("5000000".into());
    } else if rw_timeout_applicable(url) {
        // 网络输入才设超时，本地文件不需要；TLS 连接尤其不能设（见 rw_timeout_applicable）
        args.push("-rw_timeout".into());
        args.push("5000000".into());
    }
    if low_latency && is_live_source(url) {
        args.push("-fflags".into());
        args.push("nobuffer".into());
        args.push("-flags".into());
        args.push("low_delay".into());
        args.push("-probesize".into());
        args.push("32768".into());
        args.push("-analyzeduration".into());
        args.push("0".into());
    }
    args
}

/// 完整的 ffmpeg 命令参数（不含可执行文件本身）。
pub fn decode_args(
    url: &str,
    low_latency: bool,
    width: usize,
    height: usize,
    need_scale: bool,
    net: NetOptions,
) -> Vec<String> {
    let mut args = input_args(url, low_latency, net);
    args.push("-i".into());
    args.push(url.to_string());
    if need_scale {
        args.push("-vf".into());
        args.push(format!("scale={width}:{height}"));
    }
    args.push("-an".into());
    args.push("-sn".into());
    args.push("-dn".into());
    args.push("-f".into());
    args.push("rawvideo".into());
    args.push("-pix_fmt".into());
    args.push("rgba".into());
    args.push("-".into());
    args
}

/// 录制参数：`-c copy` 直拷，容器用 mkv（进程被杀也不会坏文件头）。
pub fn record_args(url: &str, output: &str, low_latency: bool, net: NetOptions) -> Vec<String> {
    let mut args = input_args(url, low_latency, net);
    args.push("-i".into());
    args.push(url.to_string());
    args.push("-map".into());
    args.push("0".into());
    args.push("-c".into());
    args.push("copy".into());
    args.push("-y".into());
    args.push(output.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_derivation() {
        assert_eq!(ffprobe_bin("ffmpeg"), "ffprobe");
        assert_eq!(ffprobe_bin("ffmpeg.exe"), "ffprobe.exe");
        assert_eq!(
            ffprobe_bin(r"C:\ff\bin\ffmpeg.exe"),
            r"C:\ff\bin\ffprobe.exe"
        );
        // 分隔符跟随平台（Windows 上为反斜杠），因此用 Path::join 拼期望值
        assert_eq!(
            ffprobe_bin("/usr/bin/ffmpeg"),
            std::path::Path::new("/usr/bin")
                .join("ffprobe")
                .to_string_lossy()
        );
    }

    #[test]
    fn size_rules() {
        let p = ProbeInfo {
            width: 1920,
            height: 1080,
            codec: "h264".into(),
            fps: Some(25.0),
        };
        assert_eq!(target_size(Some(&p), 1280), (1280, 720, true));
        assert_eq!(target_size(Some(&p), 0), (1920, 1080, false));
        assert_eq!(target_size(Some(&p), 3840), (1920, 1080, false));
        assert_eq!(target_size(None, 1280), (1280, 720, true));
    }

    #[test]
    fn odd_dimensions_are_evened() {
        let p = ProbeInfo {
            width: 1921,
            height: 1081,
            codec: "h265".into(),
            fps: None,
        };
        assert_eq!(target_size(Some(&p), 0), (1920, 1080, false));
    }

    #[test]
    fn rtsp_gets_tcp_and_low_delay_flags() {
        let a = decode_args("rtsp://cam/1", true, 1280, 720, true, NetOptions::default());
        assert!(a.contains(&"-rtsp_transport".to_string()));
        assert!(a.contains(&"-fflags".to_string()));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-vf" && w[1] == "scale=1280:720"));
        // 最后必须是输出目标
        assert_eq!(a.last().unwrap(), "-");
        assert!(a.contains(&"rgba".to_string()));
    }

    #[test]
    fn hls_does_not_get_rtsp_flag() {
        let a = decode_args(
            "https://x/y.m3u8",
            false,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(!a.contains(&"-rtsp_transport".to_string()));
        assert!(!a.contains(&"-vf".to_string()));
    }

    #[test]
    fn low_latency_flags_only_for_live_sources() {
        // 实时流才有 nobuffer
        let live = decode_args(
            "rtsp://cam/1",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(live.contains(&"-fflags".to_string()));
        assert!(live.contains(&"nobuffer".to_string()));

        // HLS 不能加：需要缓冲才能探测分片
        let hls = decode_args(
            "https://x/y.m3u8",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(!hls.contains(&"nobuffer".to_string()));

        // 本地文件更不能加（实测会一帧都读不出来）
        let file = decode_args("D:/clip.mp4", true, 1280, 720, false, NetOptions::default());
        assert!(!file.contains(&"nobuffer".to_string()));
        assert!(!file.contains(&"-rw_timeout".to_string()));
    }

    #[test]
    fn insecure_tls_adds_flag() {
        let net = NetOptions {
            no_proxy: true,
            insecure_tls: true,
            hwaccel: true,
        };
        let a = decode_args(
            "rtmps://play.acmpy.xdrtc.com:1935/ipc/k",
            true,
            1280,
            720,
            false,
            net,
        );
        assert!(a.windows(2).any(|w| w[0] == "-tls_verify" && w[1] == "0"));
        let b = decode_args(
            "rtsp://cam/1",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(!b.contains(&"-tls_verify".to_string()));
    }

    #[test]
    fn proxy_env_removed_when_no_proxy() {
        use std::process::Command;
        let mut cmd = Command::new("ffmpeg");
        cmd.env("http_proxy", "http://127.0.0.1:3743");
        apply_net(
            &mut cmd,
            NetOptions {
                no_proxy: true,
                insecure_tls: false,
                hwaccel: true,
            },
        );
        // env_remove 后子进程看不到 http_proxy
        let out = cmd.args(["-version"]).output().expect("需要 ffmpeg");
        assert!(out.status.success());
        let mut probe = Command::new("cmd");
        probe.args(["/c", "set http_proxy"]);
        apply_net(
            &mut probe,
            NetOptions {
                no_proxy: true,
                insecure_tls: false,
                hwaccel: true,
            },
        );
        let text = String::from_utf8_lossy(&probe.output().unwrap().stdout).to_lowercase();
        assert!(
            !text.trim_end().ends_with("3743"),
            "http_proxy 应已被清除: {text}"
        );
    }

    #[test]
    fn hwaccel_only_for_rtsp() {
        let on = decode_args("rtsp://c/m", true, 1280, 720, false, NetOptions::default());
        assert!(on.windows(2).any(|w| w[0] == "-hwaccel" && w[1] == "auto"));
        let net = NetOptions {
            hwaccel: false,
            ..NetOptions::default()
        };
        let off = decode_args("rtsp://c/m", true, 1280, 720, false, net);
        assert!(!off.contains(&"-hwaccel".to_string()));
        let hls = decode_args(
            "https://x/y.m3u8",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(!hls.contains(&"-hwaccel".to_string()));
    }

    #[test]
    fn rtsp_uses_demuxer_timeout_not_rw_timeout() {
        let a = decode_args(
            "rtsp://cam/main",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(
            !a.contains(&"-rw_timeout".to_string()),
            "RTSP 不认 -rw_timeout"
        );
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-timeout" && w[1] == "5000000"));
    }

    #[test]
    fn rw_timeout_excluded_for_tls() {
        assert!(rw_timeout_applicable("rtsp://cam/1"));
        assert!(rw_timeout_applicable("rtmp://host/app"));
        assert!(!rw_timeout_applicable("rtmps://host:1935/ipc/k"));
        assert!(!rw_timeout_applicable("https://host/x.m3u8"));
        assert!(!rw_timeout_applicable("D:/clip.mp4"));
        // 关键回归：rtmps 不能出现 -rw_timeout
        let a = decode_args(
            "rtmps://h:1935/ipc/k",
            true,
            1280,
            720,
            false,
            NetOptions::default(),
        );
        assert!(!a.contains(&"-rw_timeout".to_string()));
    }

    #[test]
    fn live_source_detection() {
        assert!(is_live_source("rtsp://a/b"));
        assert!(is_live_source("SRT://a:9000"));
        assert!(is_live_source("rtmp://a/live"));
        assert!(!is_live_source("https://a/b.m3u8"));
        assert!(!is_live_source("D:/a.mp4"));
    }

    #[test]
    fn ratio_parse() {
        assert!((parse_ratio("25/1").unwrap() - 25.0).abs() < 0.001);
        assert!((parse_ratio("30000/1001").unwrap() - 29.97).abs() < 0.01);
        assert!(parse_ratio("0/0").is_none());
    }
}
