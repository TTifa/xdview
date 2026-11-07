//! 主界面状态与绘制。
//!
//! 视觉基准是 `designs/xdview/XDView 交互原型.html`：深色面板体系、
//! 三栏布局（设备 / 视频 / 控制，左右栏可拖拽调宽）、数字变焦、分级日志、右侧五个控制区块。

use crate::background::{Background, Recorder, SegmentCache, Task};
use crate::control::{Command, ControlLink, Feedback, Session, LEASE_RENEW};
use crate::font;
use crate::prefs::{PrefsWriter, Saved};
use crate::storage::{GeomRect, StoragePaths, WindowGeom};
use crate::theme::{Palette, Theme};
use eframe::egui;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use xd_core::config::AppConfig;
use xd_core::discovery::DiscoveredDevice;
use xd_core::model::Camera;
use xd_core::onvif::{Credentials, Preset, Profile, Velocity};
use xd_media::stream::{Event, Stream, StreamOptions};
use xd_media::{CacheOptions, Frame, NetOptions};

// 色板见 `theme.rs`：夜间/日间两套，字段与原型 CSS 变量一一对应。
// 绘制函数统一先 `let pal = self.pal;` 取一份副本，避免闭包里读字段和 `&mut self` 打架。

/// 专注态悬浮按钮的度量，对齐原型 `.focus-exit`：14px 外边距、12px 内边距、13px 图标。
const OVERLAY_TOP: f32 = 14.0;
const OVERLAY_RIGHT: f32 = 14.0;
const OVERLAY_BTN_H: f32 = 28.0;
const OVERLAY_PAD_X: f32 = 12.0;
const OVERLAY_ICON: f32 = 13.0;
const OVERLAY_GAP: f32 = 7.0;
/// 按钮行的淡入淡出时长（原型 `transition: opacity .4s`）
const OVERLAY_FADE: f32 = 0.35;

/// 小窗使用更紧凑的按钮与边距；覆盖区按最多三个按钮（含复位）预留。
const MINI_OVERLAY_MARGIN: f32 = 8.0;
const MINI_OVERLAY_BTN_H: f32 = 22.0;
const OVERLAY_BUTTONS_W: f32 = 176.0;

/// 正常关窗异步等待保存、媒体收尾和云台 Stop 的上限；再次关闭可立即退出。
const CLOSE_DEFER_MAX: Duration = Duration::from_secs(6);

/// 扫描弹窗正文的固定高度：扫描中 / 无结果 / 结果列表三种状态共用，
/// 弹窗高度就不会随设备条数或状态变化而跳动。
const SCAN_BODY_H: f32 = 250.0;

/// 界面形态：普通三栏 / 专注（只看视频）/ 小窗（专注 + 无边框置顶）。
/// 全屏由视口单独管理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Normal,
    Focus,
    Mini,
}

impl ViewMode {
    fn is_focus(self) -> bool {
        matches!(self, Self::Focus | Self::Mini)
    }

    fn is_mini(self) -> bool {
        matches!(self, Self::Mini)
    }

    fn after_enter_focus(self) -> Self {
        if self == Self::Normal {
            Self::Focus
        } else {
            self
        }
    }

    fn after_enter_mini(self) -> Self {
        Self::Mini
    }

    fn after_exit_mini(self) -> Self {
        Self::Focus
    }

    fn after_exit_focus(self) -> Self {
        Self::Normal
    }
}

/// 窗口几何槽位。
#[derive(Debug, Clone, Copy)]
enum GeomSlot {
    Win,
    Mini,
}

/// 只记录正常状态；窗口最大化或最小化后保留最近的正常几何。
fn remember_geom(geom: &mut WindowGeom, slot: GeomSlot, viewport: &egui::ViewportInfo) {
    if viewport.fullscreen.unwrap_or(false)
        || viewport.minimized == Some(true)
        || viewport.maximized == Some(true)
    {
        return;
    }
    let Some(inner) = viewport.inner_rect else {
        return;
    };
    let previous = match slot {
        GeomSlot::Win => geom.win,
        GeomSlot::Mini => geom.mini,
    };
    let Some(pos) = viewport
        .outer_rect
        .map(|r| r.min)
        .or_else(|| previous.map(|r| egui::pos2(r.x as f32, r.y as f32)))
    else {
        return;
    };
    let size = inner.size();
    if !size.x.is_finite() || !size.y.is_finite() || !pos.x.is_finite() || !pos.y.is_finite() {
        return;
    }
    let rect = GeomRect {
        x: pos.x.round() as i32,
        y: pos.y.round() as i32,
        w: size.x.round().max(1.0) as u32,
        h: size.y.round().max(1.0) as u32,
    };
    match slot {
        GeomSlot::Win => geom.win = Some(rect),
        GeomSlot::Mini => geom.mini = Some(rect),
    }
}

/// 视口命令异步生效，全屏状态以视口为准。
fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// 小窗默认尺寸：按视频比例给 240 高；比例异常/缺失回退 16:9，宽度限幅避免异常窗口。
pub(crate) fn mini_default_size(fw: f32, fh: f32) -> (f32, f32) {
    let (fw, fh) = if fw > 0.0 && fh > 0.0 && fw.is_finite() && fh.is_finite() {
        (fw, fh)
    } else {
        (16.0, 9.0)
    };
    let h = 240.0_f32;
    ((h * fw / fh).round().clamp(320.0, 1920.0), h)
}

/// 无记忆时使用默认尺寸，位置由系统决定。
fn restore_target(win: Option<GeomRect>) -> (egui::Vec2, Option<egui::Pos2>) {
    match win {
        Some(r) => (
            egui::vec2(r.w.max(400) as f32, r.h.max(300) as f32),
            Some(egui::pos2(r.x as f32, r.y as f32)),
        ),
        None => (egui::vec2(1440.0, 900.0), None),
    }
}

/// 拖动起点是否落在专注态右上悬浮按钮区：那里要留给按钮，不能触发拖窗。
fn drag_started_on_overlay(pos: Option<egui::Pos2>, rect: egui::Rect) -> bool {
    pos.is_some_and(|p| {
        p.x >= rect.max.x - OVERLAY_BUTTONS_W
            && p.y <= rect.min.y + MINI_OVERLAY_MARGIN + MINI_OVERLAY_BTN_H
    })
}

enum Conn {
    Idle,
    Connecting,
    Live {
        width: usize,
        height: usize,
        codec: String,
    },
    Failed(String),
}

impl Conn {
    /// 根据取流依赖处理 ONVIF 失败；媒体已启动时保留独立的媒体状态。
    fn on_onvif_failure(&mut self, resolve_stream: bool, media_started: bool, message: &str) {
        if resolve_stream && !media_started && matches!(self, Self::Connecting) {
            *self = Self::Failed(message.to_string());
        }
    }
}

/// 日志级别：与原型的 INFO / OK / WARN / ERR 四色对应。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Info,
    Ok,
    Warn,
    Err,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Err => "ERR",
        }
    }

    fn color(self, pal: &Palette) -> egui::Color32 {
        match self {
            Level::Info => pal.info,
            Level::Ok => pal.ok,
            Level::Warn => pal.warn,
            Level::Err => pal.bad,
        }
    }
}

struct LogLine {
    stamp: String,
    level: Level,
    msg: String,
}

#[derive(Default)]
struct Stats {
    frames: u64,
    bytes: u64,
    window: Option<Instant>,
    wframes: u64,
    wbytes: u64,
    fps: f32,
    /// 解码后写进管道的原始字节速率（MB/s）。注意这不是网络码率：
    /// rawvideo 是未压缩像素，1280x720x4x20fps ≈ 72 MB/s。
    mb_per_s: f32,
}

impl Stats {
    fn reset(&mut self) {
        *self = Stats::default();
    }

    fn on_frame(&mut self, f: &Frame) {
        let now = Instant::now();
        self.frames += 1;
        self.wframes += 1;
        self.bytes += f.rgba.len() as u64;
        self.wbytes += f.rgba.len() as u64;
        let w = *self.window.get_or_insert(now);
        let elapsed = now.duration_since(w).as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.wframes as f32 / elapsed;
            self.mb_per_s = self.wbytes as f32 / 1_048_576.0 / elapsed;
            self.window = Some(now);
            self.wframes = 0;
            self.wbytes = 0;
        }
    }
}

/// 显示中的预览帧。暂停只阻止帧替换，接收队列仍由 UI 正常消费，不积压历史视频。
#[derive(Default)]
struct Preview {
    tex: Option<egui::TextureHandle>,
    frame: Option<std::sync::Arc<Frame>>,
    paused: bool,
    /// 继续预览或媒体中断后，只有实际显示了新帧才能解除。
    waiting_for_frame: bool,
}

impl Preview {
    /// 显示帧是否已经可以作为实时画面展示（连接状态另由调用方判断）。
    fn is_live(&self) -> bool {
        self.frame.is_some() && !self.paused && !self.waiting_for_frame
    }

    fn invalidate(&mut self) {
        self.waiting_for_frame = true;
    }

    /// 冻结帧和连接状态共用的画面提示，解码会话建立不代表已经恢复出帧。
    fn notice(&self, conn: &Conn) -> Option<&'static str> {
        self.frame.as_ref()?;
        match conn {
            Conn::Connecting => Some("重新连接中 · 当前画面非实时"),
            Conn::Failed(_) => Some("连接失败 · 当前画面非实时"),
            Conn::Idle => Some("已断开 · 当前画面非实时"),
            Conn::Live { .. } if self.paused => Some("已暂停 · 当前画面已冻结"),
            Conn::Live { .. } if !self.is_live() => Some("等待新画面 · 当前画面非实时"),
            _ => None,
        }
    }

    fn receive(&mut self, ctx: &egui::Context, frame: Frame) {
        if self.paused {
            return;
        }
        let img =
            egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], frame.as_slice());
        // is_none_or 要 1.82，工作区 MSRV 是 1.81；这里等价于"没有纹理或尺寸变了"
        if self
            .tex
            .as_ref()
            .map_or(true, |t| t.size() != [frame.width, frame.height])
        {
            self.tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR));
        } else if let Some(t) = &mut self.tex {
            t.set(img, egui::TextureOptions::LINEAR);
        }
        self.frame = Some(std::sync::Arc::new(frame));
        self.waiting_for_frame = false;
    }

    fn toggle(&mut self) {
        if self.frame.is_some() {
            self.paused = !self.paused;
            if !self.paused {
                self.invalidate();
            }
        }
    }
}

pub struct XdView {
    cfg: AppConfig,
    cfg_path: PathBuf,
    selected: usize,
    stream: Option<Stream>,
    preview: Preview,
    stats: Stats,
    conn: Conn,
    log: VecDeque<LogLine>,
    control: ControlLink,
    profiles: Vec<Profile>,
    device_info: String,
    presets: Vec<Preset>,
    velocity: Velocity,
    recorder: Option<Recorder>,
    note: String,
    note_until: Option<Instant>,
    high_stream: bool,
    show_log: bool,
    storage: StoragePaths,
    discovered: Vec<DiscoveredDevice>,
    scanning: bool,
    /// 扫描弹窗是否打开（结果与进度都在弹窗里，侧栏不再内联）
    scan_open: bool,
    /// 上一次扫描的失败原因（WS-Discovery 异常等），成功或重新扫描时清空
    scan_error: Option<String>,
    /// 首帧时自动连接第一路（单机看护场景几乎总是希望一打开就出画面）
    autostart: bool,
    /// 每 2 秒把统计打到 stderr，便于无头/自动化验证（`--stats`）
    stats_stdout: bool,
    stats_tick: Option<Instant>,
    /// 滚动缓存：把实时流切片落盘，只保留最近 N 分钟
    cache: Option<SegmentCache>,
    /// 缓存进程意外退出后的重试时刻
    cache_retry_at: Option<Instant>,
    background: Background,
    config_source: std::sync::Arc<std::sync::Mutex<String>>,
    export_task: Option<Task<Result<String, String>>>,
    shutdown_at: Option<Instant>,
    shutdown_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 云台转速档：0=低速 1=中速 2=高速（对应原型的转动速度下拉）
    ptz_speed_idx: usize,
    /// 当前 ONVIF 会话标识。切换设备/断开时用 `ControlLink::revoke` 换新的，
    /// 用来丢弃上一台设备迟到的回执（否则 A 的流地址会被写进 B）。
    ptz_session: Option<Session>,
    /// 上次下发 ContinuousMove 的时刻。带租约的运动必须按 `LEASE_RENEW` 续租，
    /// 否则按住方向键 2 秒后设备侧租约到期就自己停了。
    ptz_lease_at: Option<Instant>,
    /// 新预置位名称输入框
    preset_name: String,
    /// 设备导入/导出弹窗
    ie_open: bool,
    /// 弹窗里的 TOML 文本（导出=展示当前设备；导入=粘贴后解析合并）
    ie_text: String,
    /// 导入/保存失败原因（空串=无错误）
    ie_error: String,
    /// 本地时区偏差（分钟，东为正）。启动时读一次，供时钟/日志/画面时间戳用。
    tz_bias_minutes: i32,
    /// 数字变焦倍率（1.0 = 适应窗口）。纯软件 UV 裁剪，不动摄像头。
    zoom: f32,
    /// 变焦窗口左上角的 UV（0..=1），与 `zoom` 配合实现平移。
    zoom_uv_min: egui::Pos2,
    /// 左栏（设备列表）宽度，由分界手柄拖拽驱动。
    left_w: f32,
    /// 右栏（实时统计/控制）宽度，由分界手柄拖拽驱动。
    right_w: f32,
    /// 界面形态（普通 / 专注 / 小窗）
    view_mode: ViewMode,
    /// 专注模式悬浮控件的自动隐藏时刻（指针/滚轮/按键一动就续期）
    hover_until: Option<Instant>,
    /// 主窗口 / 小窗两槽几何，存独立文件，退出时写回
    geom: WindowGeom,
    /// 退出专注后下一帧补发一次恢复命令（winit 的装饰/尺寸切换异步生效）
    pending_restore: Option<(egui::Vec2, Option<egui::Pos2>)>,
    /// 首帧取得 DPI 后再校验并恢复启动位置。
    initial_position_pending: bool,
    /// 「添加设备」弹窗（手填 ONVIF 地址；小度/TP-Link 不支持 WS-Discovery，这是正路）
    add_open: bool,
    add_name: String,
    add_url: String,
    add_user: String,
    add_pass: String,
    add_error: String,
    /// 当前主题（夜间 / 日间），与 `pal` 同步切换
    theme: Theme,
    /// 当前色板快照；所有绘制读这里，不在各处硬编码颜色
    pal: Palette,
    /// 主题偏好由后台线程写盘，UI 只投递
    prefs: PrefsWriter,
    /// 最后一次投递的写序号；`Some` 表示「可能还没落盘」，关窗暂缓据此判断。
    /// 写线程若意外退出，回执不会再来，`Some` 会一直留着——关窗仍有六秒上限兜底。
    pending_theme: Option<u64>,
    /// 关窗暂缓的截止时刻：用户点了关闭，但最后一次写还没回来
    close_state: CloseState,
}

/// 转速档 → ContinuousMove 速度。
const PTZ_SPEEDS: [f32; 3] = [0.35, 0.7, 1.0];
const PTZ_SPEED_NAMES: [&str; 3] = ["低速", "中速", "高速"];

/// 解码清晰度档位（max_width 上限，0 = 不缩放保持原始分辨率）。
/// 档位只影响解码显示，不影响录像（录像是 -c copy 原流）。
const RES_TIERS: [(&str, u32); 4] = [("原始", 0), ("720P", 1280), ("540P", 960), ("360P", 640)];

fn tier_label(max_width: u32) -> String {
    RES_TIERS
        .iter()
        .find(|(_, w)| *w == max_width)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| format!("{max_width}px"))
}

impl XdView {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: AppConfig,
        config_source: String,
        cfg_path: PathBuf,
        storage: StoragePaths,
        geom: WindowGeom,
        theme: Theme,
    ) -> Result<Self, String> {
        // 主题在 `main` 里（窗口创建前）读取并传进来，UI 线程不碰偏好文件。
        let background = Background::new()?;
        let pal = Palette::of(theme);
        apply_theme(&cc.egui_ctx, theme);
        let cjk = font::find_cjk_font();
        let cjk_note = match &cjk {
            Some(f) => format!("中文字体：{}", f.name),
            None => "未找到中文字体，中文可能显示为方块".to_string(),
        };
        if let Some(f) = cjk {
            let mut defs = egui::FontDefinitions::default();
            defs.font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(f.data).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                defs.families
                    .entry(family)
                    .or_default()
                    .push("cjk".to_owned());
            }
            cc.egui_ctx.set_fonts(defs);
        }

        let high_stream = cfg.settings.high_stream;
        // 写线程在窗口之前起好：首次切换主题时直接投递，不必临时建线程
        let prefs = PrefsWriter::spawn(storage.root().to_path_buf());

        let mut view = XdView {
            cfg,
            cfg_path,
            selected: 0,
            stream: None,
            preview: Preview::default(),
            stats: Stats::default(),
            conn: Conn::Idle,
            log: VecDeque::new(),
            control: ControlLink::new(),
            profiles: Vec::new(),
            device_info: String::new(),
            presets: Vec::new(),
            velocity: Velocity::default(),
            recorder: None,
            note: String::new(),
            note_until: None,
            high_stream,
            show_log: true,
            storage,
            discovered: Vec::new(),
            scanning: false,
            scan_open: false,
            scan_error: None,
            autostart: true,
            stats_stdout: false,
            stats_tick: None,
            cache: None,
            cache_retry_at: None,
            background,
            config_source: std::sync::Arc::new(std::sync::Mutex::new(config_source)),
            export_task: None,
            shutdown_at: None,
            shutdown_stop: None,
            ptz_speed_idx: 1,
            ptz_session: None,
            ptz_lease_at: None,
            preset_name: String::new(),
            ie_open: false,
            ie_text: String::new(),
            ie_error: String::new(),
            tz_bias_minutes: local_bias_minutes(),
            zoom: 1.0,
            zoom_uv_min: egui::pos2(0.0, 0.0),
            left_w: 246.0,
            right_w: 314.0,
            view_mode: ViewMode::Normal,
            hover_until: None,
            geom,
            pending_restore: None,
            initial_position_pending: true,
            add_open: false,
            add_name: String::new(),
            add_url: String::new(),
            add_user: String::new(),
            add_pass: String::new(),
            add_error: String::new(),
            theme,
            pal,
            prefs,
            pending_theme: None,
            close_state: CloseState::default(),
        };
        view.push_log(format!(
            "配置：{}（{} 路摄像机）",
            view.cfg_path.display(),
            view.cfg.cameras.len()
        ));
        view.push_log(format!("运行数据：{}", view.storage.root().display()));
        view.push_log(cjk_note);
        Ok(view)
    }

    fn push_log(&mut self, msg: impl Into<String>) {
        self.push_lv(Level::Info, msg);
    }

    fn push_lv(&mut self, level: Level, msg: impl Into<String>) {
        // 兜底：任何进日志面板（并可能被截图带出去）的文本都不能留真实凭据或签名。
        // 拉流侧的 ffmpeg stderr 已经脱过一次，这里再过一遍是幂等的。
        let msg = xd_core::onvif::mask_url_in_text(&msg.into());
        if self.log.len() >= 200 {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            stamp: self.stamp(),
            level,
            msg,
        });
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.note = xd_core::onvif::mask_url_in_text(&msg.into());
        self.note_until = Some(Instant::now() + Duration::from_secs(4));
    }

    /// 当前本地时间 `YYYY-MM-DD HH:MM:SS`。
    fn stamp(&self) -> String {
        fmt_stamp(unix_now() - i64::from(self.tz_bias_minutes) * 60)
    }

    /// 打开 `--stats`：周期性把帧率/分辨率打到 stderr。
    pub fn enable_stats(&mut self) {
        self.stats_stdout = true;
    }

    /// 启动时指定初始摄像机：按 id、1 起的序号、或名称片段匹配。
    ///
    /// 多路场景下（小度 + TP-Link）总从第一路开始很别扭，`--camera tplink` 可直接切。
    pub fn preselect(&mut self, want: &str) -> bool {
        let by_id = self.cfg.cameras.iter().position(|c| c.id == want);
        let by_index = want
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < self.cfg.cameras.len());
        let by_name = self.cfg.cameras.iter().position(|c| c.name.contains(want));
        match by_id.or(by_index).or(by_name) {
            Some(i) => {
                self.selected = i;
                self.reset_zoom();
                self.autostart = true;
                true
            }
            None => false,
        }
    }

    /// 每 2 秒打一行统计；帧数不增就标 `[stall]`，便于判断是卡住还是没收到流。
    fn emit_stats(&mut self, ctx: &egui::Context) {
        if !self.stats_stdout {
            return;
        }
        let now = Instant::now();
        let due = match self.stats_tick {
            None => true,
            Some(t) => now.duration_since(t).as_secs_f32() >= 2.0,
        };
        if !due {
            return;
        }
        self.stats_tick = Some(now);
        let size = self
            .preview
            .frame
            .as_ref()
            .map(|f| format!("{}x{}", f.width, f.height))
            .unwrap_or_else(|| "-".to_string());
        let cam = self.current().map(|c| c.name.clone()).unwrap_or_default();
        let tag = if self.stats.frames == 0 {
            " [stall]"
        } else {
            ""
        };
        eprintln!(
            "[stats] {} {} 帧={} {:.1}fps {:.1}MB/s{}",
            cam, size, self.stats.frames, self.stats.fps, self.stats.mb_per_s, tag
        );
        eprintln!(
            "[dbg] screen={:?} left_w={:.1} right_w={:.1} zoom={:.2} mode={:?} fs={} theme={:?}",
            ctx.screen_rect(),
            self.left_w,
            self.right_w,
            self.zoom,
            self.view_mode,
            is_fullscreen(ctx),
            self.theme
        );
    }

    fn net_options(&self) -> NetOptions {
        NetOptions {
            no_proxy: self.cfg.settings.no_proxy,
            insecure_tls: self.cfg.settings.insecure_tls,
            hwaccel: self.cfg.settings.hwaccel,
        }
    }

    fn current(&self) -> Option<&Camera> {
        self.cfg.cameras.get(self.selected)
    }

    /// 连接当前选中的摄像机：有 RTSP 就直接拉流，没 RTSP 就先找 ONVIF 要地址。
    fn connect_selected(&mut self) {
        let Some(cam) = self.current().cloned() else {
            return;
        };
        self.export_task = None;
        self.stop_recording_unless(Some(&cam.id));
        self.stop_cache(false);
        if let Some(mut s) = self.stream.take() {
            self.background.submit(move || {
                s.stop();
                Ok(String::new())
            });
        }
        // 换会话：上一台设备还在飞的 Connect/StreamUri 回执一律作废
        let session = self.control.revoke(cam.id.clone());
        self.ptz_session = Some(session.clone());
        self.ptz_lease_at = None;
        self.control.send(Command::Stop);
        self.control.send(Command::Disconnect);
        self.preview = Preview::default();
        self.stats.reset();
        self.velocity = Velocity::default();
        self.conn = Conn::Connecting;

        // 配置里可能把账号写在独立字段（TP-Link/海康常见），这里补进 URL
        let url = cam.resolved_url(self.high_stream);
        let has_url = !url.trim().is_empty();
        if has_url {
            self.start_media(&url);
            if self.cfg.settings.cache_enabled {
                self.start_cache();
            }
            self.push_log(format!(
                "连接 {} → {}",
                cam.name,
                xd_core::onvif::mask_credentials(&url)
            ));
        } else {
            self.push_lv(
                Level::Warn,
                format!("{} 没有流地址，改走 ONVIF GetStreamUri", cam.name),
            );
        }

        match cam.onvif_base() {
            Some(base) => {
                let credentials = if cam.has_credentials() {
                    Some(Credentials {
                        username: cam.username.clone().unwrap_or_default(),
                        password: cam.password.clone().unwrap_or_default(),
                    })
                } else {
                    None
                };
                self.control.send(Command::Connect {
                    session,
                    base,
                    credentials,
                    resolve_stream: !has_url,
                });
            }
            None => {
                self.profiles.clear();
                self.presets.clear();
                self.device_info.clear();
                if !has_url {
                    self.conn = Conn::Failed("既没有 RTSP 地址，也没有 ONVIF 入口".into());
                }
            }
        }
    }

    /// 缓存是否在跑。
    fn cache_running(&self) -> bool {
        self.cache.is_some()
    }

    /// 缓存目录：用户数据目录下的 cache/<设备 id>/。
    fn cache_dir(&self, cam_id: &str) -> PathBuf {
        self.storage.cache(cam_id)
    }

    fn start_cache(&mut self) {
        if self.cache.is_some() {
            return;
        }
        let Some(cam) = self.current().cloned() else {
            return;
        };
        let url = cam.resolved_url(self.high_stream);
        if url.trim().is_empty() {
            self.toast("没有流地址，无法缓存");
            return;
        }
        let opts = CacheOptions {
            ffmpeg: self.cfg.settings.ffmpeg.clone(),
            url,
            dir: self.cache_dir(&cam.id),
            segment_secs: self.cfg.settings.cache_segment_secs.max(5) as u64,
            keep_secs: (self.cfg.settings.cache_minutes.max(1) as u64) * 60,
            net: self.net_options(),
        };
        match SegmentCache::start(opts) {
            Ok(c) => {
                self.push_lv(
                    Level::Ok,
                    format!(
                        "已请求缓存 → {}（保留最近 {} 分钟，每片 {} 秒）",
                        c.dir().display(),
                        self.cfg.settings.cache_minutes,
                        self.cfg.settings.cache_segment_secs
                    ),
                );
                self.cache = Some(c);
            }
            Err(e) => {
                self.push_lv(Level::Err, format!("缓存启动失败：{e}"));
                self.toast("缓存启动失败，看日志");
            }
        }
    }

    fn stop_cache(&mut self, log: bool) {
        self.cache_retry_at = None;
        if let Some(c) = self.cache.take() {
            let dir = c.dir().to_path_buf();
            c.stop();
            if log {
                self.push_log(format!("已请求停止缓存（片段在 {}）", dir.display()));
            }
        }
    }

    fn toggle_cache(&mut self) {
        if self.cache_running() {
            self.stop_cache(true);
        } else {
            self.start_cache();
        }
    }

    fn poll_background(&mut self) -> bool {
        let mut failed = false;
        while let Some(result) = self.background.try_recv() {
            failed |= self.report_background(result);
        }
        if let Some(result) = self.recorder.as_ref().and_then(|rec| rec.poll()) {
            self.recorder = None;
            failed |= self
                .report_background(result.map(|path| format!("录像已保存 → {}", path.display())));
        }
        let result = self.export_task.as_ref().map(|task| task.try_recv());
        match result {
            Some(Ok(result)) => {
                self.export_task = None;
                failed |= self.report_background(result);
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.export_task = None;
                failed |= self.report_background(Err("导出后台任务异常退出".into()));
            }
            _ => {}
        }
        failed
    }

    fn report_background(&mut self, result: Result<String, String>) -> bool {
        let failed = result.is_err();
        match result {
            Ok(message) if !message.is_empty() => {
                self.push_lv(Level::Ok, message.clone());
                self.toast(message);
            }
            Err(error) => {
                self.push_lv(Level::Err, error.clone());
                self.toast(error);
            }
            _ => {}
        }
        failed
    }

    /// 轮询内存回执，目录统计和清理均在缓存线程执行。
    fn poll_cache(&mut self) {
        let result = self.cache.as_ref().and_then(|cache| cache.poll());
        if let Some(result) = result {
            self.cache = None;
            if let Err(e) = result {
                self.push_lv(Level::Err, e);
            }
            self.push_lv(Level::Warn, "缓存已退出，3 秒后重试");
            self.cache_retry_at = Some(Instant::now() + Duration::from_secs(3));
        } else if self.cache.is_none() && self.cache_retry_at.is_some_and(|at| Instant::now() >= at)
        {
            self.cache_retry_at = None;
            self.start_cache();
        }
    }

    /// 把最近 N 分钟的缓存拼成一个文件，存到 `exports/` 下。
    fn export_cache(&mut self, minutes: u64) {
        let Some(c) = &self.cache else {
            self.toast("缓存没在跑，先点「缓存」");
            return;
        };
        let dir = c.dir().to_path_buf();
        let cam_id = self.current().map(|c| c.id.clone()).unwrap_or_default();
        let out = xd_media::record::default_record_path(
            &self.storage.exports(),
            &format!("{cam_id}_last{minutes}m"),
        );
        if self.export_task.is_some() {
            self.toast("导出正在进行，请稍候");
            return;
        }
        let ffmpeg = self.cfg.settings.ffmpeg.clone();
        let net = self.net_options();
        match Task::start(
            "xdview-export",
            move |cancel| match xd_media::cache::export_cancellable(
                &ffmpeg,
                &dir,
                minutes.saturating_mul(60),
                &out,
                net,
                &cancel,
                Duration::from_secs(120),
            )? {
                0 => Err("指定时段没有可导出的片段，请等待缓存产生新片段".into()),
                n => Ok(format!("导出 {n} 个片段 → {}", out.display())),
            },
        ) {
            Ok(task) => {
                self.export_task = Some(task);
                self.toast("正在导出缓存…");
            }
            Err(e) => self.toast(e),
        }
    }

    /// 只启动媒体流（不动 ONVIF 会话），供 GetStreamUri 回来之后调用。
    fn start_media(&mut self, url: &str) {
        // 换码流/清晰度与重新连接都开始新预览，不能沿用上一会话的暂停帧。
        self.preview = Preview::default();
        if let Some(mut s) = self.stream.take() {
            self.background.submit(move || {
                s.stop();
                Ok(String::new())
            });
        }
        let opts = StreamOptions {
            url: url.to_string(),
            ffmpeg: self.cfg.settings.ffmpeg.clone(),
            max_width: self.cfg.settings.max_width,
            low_latency: self.cfg.settings.low_latency,
            reconnect: self.cfg.settings.reconnect,
            net: self.net_options(),
        };
        self.stream = Some(Stream::start(opts));
        self.conn = Conn::Connecting;
    }

    /// 主动向设备重新索取 RTSP 地址（换码流、换网络后很有用）。
    fn request_stream_uri(&mut self) {
        let Some(cam) = self.current().cloned() else {
            return;
        };
        let Some(base) = cam.onvif_base() else {
            self.toast("该设备没有 ONVIF 入口");
            return;
        };
        let credentials = if cam.has_credentials() {
            Some(Credentials {
                username: cam.username.clone().unwrap_or_default(),
                password: cam.password.clone().unwrap_or_default(),
            })
        } else {
            None
        };
        // 不换会话：这是对同一台设备重新取地址，沿用当前代次
        let session = self
            .ptz_session
            .clone()
            .unwrap_or_else(|| self.control.session(cam.id.clone()));
        self.control.send(Command::Connect {
            session,
            base,
            credentials,
            resolve_stream: true,
        });
        self.push_log("向设备索取 RTSP 地址…");
    }

    /// 把当前配置写回 config.toml（新增/导入设备、改设置后调用，重启不丢）。
    fn persist_config(&mut self) {
        let config = self.cfg.clone();
        let path = self.cfg_path.clone();
        let source = std::sync::Arc::clone(&self.config_source);
        self.background.submit(move || {
            let mut source = source
                .lock()
                .map_err(|_| "配置保存状态异常，请重启后重试".to_string())?;
            *source = xd_core::config::save_if_unchanged(&path, &config, &source)?;
            Ok(format!("配置已保存 → {}", path.display()))
        });
    }

    /// 手填 ONVIF 地址添加设备并立即连接。
    ///
    /// 小度/TP-Link 等设备不响应 WS-Discovery，扫描永远是空的——手填是正路。
    /// 地址可以只写 `http://ip:port`，服务路径由客户端按约定补（/onvif/device_service）。
    fn add_device_manual(&mut self) {
        let raw = self.add_url.trim().trim_end_matches('/').to_string();
        if raw.is_empty() {
            self.add_error = "请填写 ONVIF 地址，例如 http://192.168.1.64:8080".into();
            return;
        }
        let with_scheme = if raw.contains("://") {
            raw
        } else {
            format!("http://{raw}")
        };
        let Some((scheme, rest)) = with_scheme.split_once("://") else {
            self.add_error = "地址格式不对".into();
            return;
        };
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() {
            self.add_error = "地址里没有主机名".into();
            return;
        }
        let base = format!("{scheme}://{host}");
        if self
            .cfg
            .cameras
            .iter()
            .any(|c| c.onvif_base().as_deref() == Some(base.as_str()))
        {
            self.add_error = "该设备已在列表里".into();
            return;
        }
        let name = if self.add_name.trim().is_empty() {
            host.to_string()
        } else {
            self.add_name.trim().to_string()
        };
        let id = format!("onvif-{}", host.replace(':', "-"));
        let mut cam = Camera::new(id, name, String::new());
        cam.onvif_url = Some(base.clone());
        if !self.add_user.trim().is_empty() {
            cam.username = Some(self.add_user.trim().to_string());
            cam.password = Some(self.add_pass.clone());
        }
        self.cfg.cameras.push(cam);
        self.selected = self.cfg.cameras.len() - 1;
        self.reset_zoom();
        self.high_stream = true;
        self.add_open = false;
        self.add_name.clear();
        self.add_url.clear();
        self.add_user.clear();
        self.add_pass.clear();
        self.add_error.clear();
        self.push_lv(Level::Ok, format!("加入设备（{base}）"));
        self.persist_config();
        self.connect_selected();
    }

    /// 把一个扫描到的 ONVIF 设备加进列表并立即接入。
    /// 打开扫描弹窗；「没有结果 + 没在扫」时才真正发起一次探测。
    ///
    /// 结果保留到下一次探测：关掉弹窗再打开能看到上次扫到的设备，继续接入。
    fn open_scan(&mut self) {
        self.scan_open = true;
        if self.discovered.is_empty() && !self.scanning {
            self.start_scan();
        }
    }

    /// 发起一次探测（WS-Discovery + 网段探测），结果经 `Feedback::Devices` 回来。
    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        // 清掉上一轮，免得「扫描中」时还挂着旧结果或旧错误
        self.discovered.clear();
        self.scan_error = None;
        self.push_log("发送 WS-Discovery 探测…");
        self.control.send(Command::Discover { timeout_ms: 2000 });
    }

    /// 当前设备的连接状态短语（扫描弹窗卡片用；只对选中的那台准确）。
    fn short_status(&self) -> (&'static str, egui::Color32) {
        match &self.conn {
            Conn::Live { .. } => ("已连接", self.pal.ok),
            Conn::Connecting => ("连接中…", self.pal.warn),
            Conn::Failed(_) => ("连接失败", self.pal.bad),
            Conn::Idle => ("已加入", self.pal.dim),
        }
    }

    fn adopt(&mut self, dev: &DiscoveredDevice) {
        let Some(base) = dev.base_url() else {
            self.toast("设备地址无法解析");
            return;
        };
        if self
            .cfg
            .cameras
            .iter()
            .any(|c| c.onvif_base().as_deref() == Some(base.as_str()))
        {
            self.toast("该设备已在列表里");
            return;
        }
        let id = format!("onvif-{}", base.rsplit('/').next().unwrap_or("dev"));
        let mut cam = Camera::new(id, dev.display_name(), String::new());
        cam.onvif_url = Some(base.clone());
        self.cfg.cameras.push(cam);
        self.selected = self.cfg.cameras.len() - 1;
        self.reset_zoom();
        self.high_stream = true;
        self.push_lv(
            Level::Ok,
            format!("加入设备 {}（{base}）", dev.display_name()),
        );
        self.persist_config();
        self.connect_selected();
    }

    /// None 表示主动断开，Some 表示切换目标；同设备预览重连保留独立录像。
    fn stop_recording_unless(&mut self, device: Option<&str>) {
        if let Some(recorder) = &mut self.recorder {
            recorder.stop_unless_device(device);
        }
    }

    fn disconnect(&mut self, log: bool) {
        self.stop_recording_unless(None);
        self.export_task = None;
        self.preview = Preview::default();
        self.stop_cache(false);
        if let Some(mut s) = self.stream.take() {
            self.background.submit(move || {
                s.stop();
                Ok(String::new())
            });
        }
        // 递增代次：断开后迟到的回执不能再改配置或起播
        self.control.revoke(String::new());
        self.ptz_session = None;
        self.ptz_lease_at = None;
        self.control.send(Command::Stop);
        self.control.send(Command::Disconnect);
        self.profiles.clear();
        self.presets.clear();
        self.device_info.clear();
        self.velocity = Velocity::default();
        self.conn = Conn::Idle;
        if log {
            self.push_log("已断开");
        }
    }

    fn poll_stream(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(stream) = &self.stream {
            while let Some(ev) = stream.try_recv() {
                events.push(ev);
                if events.len() >= 8 {
                    break;
                }
            }
        }
        for ev in events {
            match ev {
                Event::Opened {
                    width,
                    height,
                    codec,
                } => {
                    self.preview.invalidate();
                    self.conn = Conn::Live {
                        width,
                        height,
                        codec: codec.clone(),
                    };
                    self.push_lv(
                        Level::Ok,
                        format!("已建立解码会话 {width}x{height} ({codec})"),
                    );
                }
                Event::Frame(f) => {
                    // 统计仍描述后台解码；暂停期间同样消费并丢弃新帧，不做纹理上传。
                    self.stats.on_frame(&f);
                    self.preview.receive(ctx, f);
                }
                Event::Status(msg) => {
                    self.push_log(msg);
                }
                Event::Interrupted { retrying } => {
                    self.preview.invalidate();
                    self.conn = if retrying {
                        Conn::Connecting
                    } else {
                        Conn::Idle
                    };
                    self.push_lv(
                        Level::Warn,
                        if retrying {
                            "流中断，重连中…"
                        } else {
                            "流已中断"
                        },
                    );
                }
                Event::Failed(msg) => {
                    self.preview.invalidate();
                    self.push_lv(Level::Err, format!("失败：{msg}"));
                    self.conn = Conn::Failed(msg);
                }
                Event::Closed => {
                    self.preview.invalidate();
                    if !matches!(self.conn, Conn::Failed(_)) {
                        self.conn = Conn::Idle;
                    }
                    self.push_log("拉流线程结束");
                }
            }
        }
    }

    fn poll_control(&mut self) {
        while let Some(fb) = self.control.try_recv() {
            // 丢弃过期回执：切设备/断开之后，上一台设备的 StreamUri、Presets、Connected
            // 不能再落到当前设备上（否则 B 会用 A 的地址起播）。
            if let Some(s) = fb.session() {
                if self.ptz_session.as_ref() != Some(s) {
                    continue;
                }
            }
            match fb {
                Feedback::Devices(list) => {
                    self.scanning = false;
                    self.push_log(format!("扫描到 {} 台 ONVIF 设备", list.len()));
                    if list.is_empty() {
                        self.push_lv(
                            Level::Warn,
                            "WS-Discovery 无应答。已知小度、TP-Link(Tapo) 等设备不支持 WS-Discovery，\
                             此类设备请直接手填 ONVIF 地址添加；其他设备确认同网段、ONVIF 已开启、\
                             防火墙放行 UDP 3702",
                        );
                        self.toast("没扫到设备：部分设备（小度/TP-Link）不支持 WS-Discovery，可手动填 ONVIF 地址");
                    }
                    self.scan_error = None;
                    self.discovered = list;
                }
                Feedback::StreamUri { uri, .. } => {
                    let (user, pass) = match self.cfg.cameras.get(self.selected) {
                        Some(c) => (
                            c.username.clone().unwrap_or_default(),
                            c.password.clone().unwrap_or_default(),
                        ),
                        None => (String::new(), String::new()),
                    };
                    let full = xd_core::onvif::inject_credentials(&uri, &user, &pass);
                    if let Some(cam) = self.cfg.cameras.get_mut(self.selected) {
                        cam.stream_url = full.clone();
                        if cam.capabilities.ptz || cam.onvif_url.is_some() {
                            cam.capabilities.ptz = true;
                            cam.capabilities.zoom = true;
                        }
                    }
                    self.push_lv(
                        Level::Ok,
                        format!(
                            "ONVIF 取流地址：{}",
                            xd_core::onvif::mask_credentials(&full)
                        ),
                    );
                    self.start_media(&full);
                    if self.cfg.settings.cache_enabled {
                        let mut cam = self.current().cloned();
                        if let Some(c) = &mut cam {
                            // start_cache 读的是 cam.resolved_url，这里把刚拿到的地址写回去
                            c.stream_url = full.clone();
                        }
                        if let Some(c) = cam {
                            if let Some(slot) = self.cfg.cameras.get_mut(self.selected) {
                                slot.stream_url = c.stream_url;
                            }
                        }
                        self.start_cache();
                    }
                }
                Feedback::Connected {
                    device_info,
                    profiles,
                    ..
                } => {
                    self.device_info = device_info.clone();
                    self.profiles = profiles;
                    self.push_lv(Level::Ok, format!("ONVIF 就绪：{device_info}"));
                    if let Some(s) = self.ptz_session.clone() {
                        self.control.send(Command::RefreshPresets(s));
                    }
                }
                Feedback::Presets(_, list) => {
                    self.presets = list;
                }
                Feedback::Notice(_, msg) => {
                    self.push_lv(Level::Ok, msg.clone());
                    self.toast(msg);
                }
                Feedback::ConnectFailed {
                    message,
                    resolve_stream,
                    ..
                } => {
                    // 错误携带原请求的取流依赖；已配置 RTSP 或媒体已启动时，ONVIF 只影响控制。
                    self.conn
                        .on_onvif_failure(resolve_stream, self.stream.is_some(), &message);
                    self.push_lv(Level::Warn, message);
                }
                Feedback::ScanFailed { message } => {
                    // 扫描失败要收尾：否则弹窗一直转圈、「重新扫描」还禁用
                    self.scanning = false;
                    self.discovered.clear();
                    self.scan_error = Some(message.clone());
                    self.push_lv(Level::Warn, message);
                }
                Feedback::Error { message, .. } => {
                    self.push_lv(Level::Warn, format!("云台：{message}"));
                }
            }
        }
    }

    /// 下发 `ContinuousMove` 并开始运动租约计时。
    fn ptz_move(&mut self, v: Velocity) {
        let Some(s) = self.ptz_session.clone() else {
            return;
        };
        self.control.send(Command::Move(s, v));
        self.ptz_lease_at = Some(Instant::now());
    }

    /// 停止云台并结束租约。
    ///
    /// Stop 不绑会话（见 [`Command::Stop`]）：即使会话已经作废也要发，
    /// 因为"停止"本身就是兜底动作。
    fn ptz_stop(&mut self) {
        if self.ptz_session.is_none() {
            return;
        }
        self.control.send(Command::Stop);
        self.ptz_lease_at = None;
    }

    /// 运动租约续租（每帧调用）。
    ///
    /// 云台指令带 `MOVE_LEASE`：设备侧到期会自己停，这是 Stop 丢失时的兜底。
    /// 代价是按住方向键时必须周期性重发，否则长按两秒后就"按着不动"了。
    fn ptz_renew_lease(&mut self) {
        if self.velocity == Velocity::default() {
            self.ptz_lease_at = None;
            return;
        }
        let due = match self.ptz_lease_at {
            None => true,
            Some(t) => t.elapsed() >= LEASE_RENEW,
        };
        if !due {
            return;
        }
        let cmd = match self.current() {
            Some(cam) => cam.fix_velocity(self.velocity),
            None => self.velocity,
        };
        self.ptz_move(cmd);
    }

    fn snapshot(&mut self) {
        let Some(frame) = &self.preview.frame else {
            self.toast("还没有画面可截图");
            return;
        };
        let id = self
            .current()
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "cam".into());
        let stamp = xd_core::onvif::iso8601_now()
            .replace([':', 'T'], "-")
            .replace('Z', "");
        let path = self.storage.snapshot(&id, &stamp);
        let frame = std::sync::Arc::clone(frame);
        self.background.submit(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("无法创建截图目录：{e}；请检查权限或修改 data_dir"))?;
            }
            xd_core::file_io::write_atomic(&path, &frame.to_bmp())?;
            Ok(format!("截图已保存 → {}", path.display()))
        });
    }

    fn toggle_record(&mut self) {
        if let Some(rec) = &mut self.recorder {
            rec.stop();
            self.toast("正在收尾录像…");
            return;
        }
        let Some(cam) = self.current().cloned() else {
            return;
        };
        let url = cam.resolved_url(self.high_stream);
        let out = xd_media::record::default_record_path(&self.storage.recordings(), &cam.id);
        match Recorder::start(
            cam.id,
            &self.cfg.settings.ffmpeg,
            &url,
            &out,
            self.cfg.settings.low_latency,
            self.net_options(),
        ) {
            Ok(r) => {
                self.push_lv(Level::Ok, format!("已请求录像 → {}", out.display()));
                self.recorder = Some(r);
            }
            Err(e) => self.toast(e),
        }
    }

    /// 只暂停预览显示，后台拉流、统计、录像和缓存继续工作。
    fn toggle_preview_pause(&mut self) {
        if self.preview.paused || matches!(self.conn, Conn::Live { .. }) {
            self.preview.toggle();
            self.push_log(if self.preview.paused {
                "预览已暂停（录像和滚动缓存继续运行）"
            } else {
                "预览已继续，等待最新画面"
            });
        }
    }

    fn draw_pause_button(&mut self, ui: &mut egui::Ui, on_stage: bool) {
        let pal = self.pal;
        let enabled = self.preview.frame.is_some()
            && (self.preview.paused || matches!(self.conn, Conn::Live { .. }));
        let text = if self.preview.paused {
            "继续预览"
        } else {
            "暂停"
        };
        let response = if self.view_mode != ViewMode::Focus {
            ui.add_enabled_ui(enabled, |ui| {
                pause_icon_button(ui, &pal, self.preview.paused, on_stage)
            })
            .inner
        } else {
            ui.add_enabled(
                enabled,
                egui::Button::new(egui::RichText::new(text).size(12.0).color(if on_stage {
                    pal.on_stage_text
                } else {
                    pal.text
                }))
                .fill(if on_stage {
                    egui::Color32::from_black_alpha(160)
                } else {
                    pal.panel2
                })
                .corner_radius(6.0_f32),
            )
        };
        if response
            .on_hover_text(if self.preview.paused {
                "继续预览（返回实时画面）"
            } else {
                "暂停预览（录像和缓存继续）"
            })
            .clicked()
        {
            self.toggle_preview_pause();
        }
    }

    fn status_of(&self) -> (String, egui::Color32) {
        let pal = &self.pal;
        match &self.conn {
            Conn::Idle => ("未连接".to_string(), pal.dim),
            Conn::Connecting => ("连接中…".to_string(), pal.warn),
            Conn::Live {
                width,
                height,
                codec,
            } => (format!("{width}x{height} {codec}"), pal.ok),
            Conn::Failed(e) => (format!("失败：{e}"), pal.bad),
        }
    }

    /// 切换主题：更新色板与 egui 视觉，并把偏好交给后台线程落盘。
    ///
    /// UI 线程不做文件 I/O（`data_dir` 可能在慢盘或网络位置）；这里只投递并记下序号，
    /// 回执由 [`Self::poll_prefs`] 处理。偏好只写 `ui_prefs.toml`，不碰 `config.toml`
    /// ——那里有明文密码和用户注释。
    fn set_theme(&mut self, ctx: &egui::Context, theme: Theme) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        self.pal = Palette::of(theme);
        apply_theme(ctx, theme);
        self.push_log(format!("主题 → {}", theme.label()));
        // 只投递不落盘：data_dir 在慢盘/网络位置时，写盘不能卡住绘制与云台续租
        self.pending_theme = Some(self.prefs.send(theme));
    }

    /// 轮询偏好写盘结果：失败要告诉用户，成功则解除「待落盘」状态。
    fn poll_prefs(&mut self) -> bool {
        let mut failed = false;
        while let Some(saved) = self.prefs.try_recv() {
            let Some(saved) = take_latest_save(&mut self.pending_theme, saved) else {
                continue;
            };
            if let Err(e) = saved.result {
                failed = true;
                let label = saved.theme.label();
                self.push_lv(Level::Warn, format!("主题偏好保存失败（{label}）：{e}"));
                self.toast("主题已切换，但偏好保存失败：下次启动可能恢复原主题");
            }
        }
        failed
    }

    /// 关窗先请求后台收尾，使用统一的有界暂缓状态机；不等待线程或网络。
    fn guard_close(&mut self, ctx: &egui::Context, save_failed: bool) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        let now = Instant::now();
        if requested && self.shutdown_at.is_none() {
            let ctx_copy = ctx.clone();
            match self.view_mode {
                ViewMode::Normal if self.pending_restore.is_none() => {
                    self.snapshot_geom(&ctx_copy, GeomSlot::Win)
                }
                ViewMode::Mini => self.snapshot_geom(&ctx_copy, GeomSlot::Mini),
                _ => {}
            }
            let geom = self.geom;
            let root = self.storage.root().to_path_buf();
            self.background.submit(move || {
                geom.save(&root)?;
                Ok(String::new())
            });
            self.shutdown_stop = Some(self.control.request_stop());
            self.export_task = None;
            self.stop_cache(false);
            if let Some(rec) = &mut self.recorder {
                rec.stop();
            }
            if let Some(mut stream) = self.stream.take() {
                self.background.submit(move || {
                    stream.stop();
                    Ok(String::new())
                });
            }
            self.shutdown_at = Some(now);
        }
        let pending = self.pending_theme.is_some()
            || (self.shutdown_at.is_some()
                && (self.background.pending()
                    || self.recorder.is_some()
                    || crate::background::active() != 0
                    || !self
                        .shutdown_stop
                        .as_ref()
                        .is_some_and(|done| done.load(std::sync::atomic::Ordering::Acquire))));
        match advance_close(&mut self.close_state, requested, pending, now, save_failed) {
            // 没有待落盘的写，或用户在暂缓期间再次点关闭：不拦
            CloseAction::Pass => {}
            CloseAction::Defer => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.request_repaint_after(Duration::from_millis(30));
            }
            CloseAction::Wait => ctx.request_repaint_after(Duration::from_millis(30)),
            CloseAction::ShowFailure => {
                if requested {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                ctx.request_repaint();
            }
            CloseAction::CloseNow => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// 切换主/子码流；连接中就地重连。
    fn set_stream(&mut self, high: bool) {
        if self.high_stream == high {
            return;
        }
        self.high_stream = high;
        self.push_log(format!(
            "切换到{}码流（rtsp 重协商，约 1 秒黑屏）",
            if high { "主" } else { "子" }
        ));
        if matches!(self.conn, Conn::Live { .. } | Conn::Connecting) {
            self.connect_selected();
        }
    }

    /// 切换解码清晰度档位（max_width 只在 ffmpeg 启动时生效，需重连）。
    fn apply_max_width_change(&mut self) {
        let w = self.cfg.settings.max_width;
        self.push_log(format!(
            "清晰度档位 → {}（max_width={w}，重连生效）",
            tier_label(w)
        ));
        if matches!(self.conn, Conn::Live { .. } | Conn::Connecting) {
            self.connect_selected();
        }
    }

    /// 打开设备导入/导出弹窗。export=true 时预填当前设备（导出场景）。
    fn open_ie(&mut self, export: bool) {
        self.ie_error.clear();
        if export {
            self.ie_text = xd_core::config::cameras_to_toml(&self.cfg.cameras);
        } else if self.ie_text.trim().is_empty() {
            self.ie_text = "# 把 [[camera]] 片段粘贴到这里，点「导入并合并」\n".to_string();
        }
        self.ie_open = true;
    }

    /// 解析弹窗文本并合并进设备列表（按 id / ONVIF 地址去重）。
    fn import_devices(&mut self) {
        match xd_core::config::parse_cameras(&self.ie_text) {
            Ok(list) => {
                let mut added = 0usize;
                let mut skipped = 0usize;
                for cam in list {
                    let dup = self.cfg.cameras.iter().any(|c| {
                        c.id == cam.id
                            || match (c.onvif_base(), cam.onvif_base()) {
                                (Some(a), Some(b)) => a == b,
                                _ => false,
                            }
                    });
                    if dup {
                        skipped += 1;
                    } else {
                        self.cfg.cameras.push(cam);
                        added += 1;
                    }
                }
                self.ie_error.clear();
                self.push_lv(
                    Level::Ok,
                    format!("导入设备：新增 {added} 台，跳过重复 {skipped} 台"),
                );
                self.toast(format!("导入完成：新增 {added} 台"));
                self.persist_config();
            }
            Err(e) => {
                self.ie_error = format!("导入失败：{e}");
            }
        }
    }

    /// 把当前设备写到 `devices_export.toml`（位于用户数据目录；含明文密码，不要公开）。
    fn save_devices_file(&mut self) {
        let text = xd_core::config::cameras_to_toml(&self.cfg.cameras);
        let path = self.storage.devices_export();
        self.ie_text = text.clone();
        self.background.submit(move || {
            xd_core::file_io::write_atomic(&path, text.as_bytes())?;
            Ok(format!("设备已导出 → {}", path.display()))
        });
    }
}

// ---------------------------------------------------------------------------- 绘制

impl XdView {
    fn draw_top(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(pal.panel)
                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 12,
                        bottom: 12,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("xdview")
                            .size(15.0)
                            .strong()
                            .color(pal.text),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("智能摄像机客户端")
                            .size(11.0)
                            .color(pal.faint),
                    );
                    ui.add_space(12.0);

                    chip(ui, &pal, "ONVIF", !self.profiles.is_empty());
                    let transport = self
                        .current()
                        .map(|c| c.transport().as_str().to_string())
                        .unwrap_or_else(|| "RTSP".into());
                    chip(ui, &pal, &format!("{transport} · TCP"), true);
                    chip(ui, &pal, "DVR 缓存", self.cache_running());

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if topbar_button(
                            ui,
                            &pal,
                            Some("专注"),
                            "只看视频（双击画面 / Esc 退出）",
                            |p, rect, fg| draw_overlay_icon(p, rect, OverlayIcon::Fullscreen, fg),
                        )
                        .clicked()
                        {
                            self.enter_focus(ctx, true);
                        }
                        ui.add_space(8.0);
                        // 主题切换（与原型顶栏同一个位置：专注按钮左侧）
                        if theme_button(ui, &pal, self.theme.is_light()).clicked() {
                            let next = self.theme.toggle();
                            self.set_theme(ctx, next);
                        }
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new(self.stamp())
                                .monospace()
                                .size(12.0)
                                .color(pal.dim),
                        );
                        // 分辨率与编码已在右侧统计区展示；顶栏仅保留连接异常/等待说明。
                        if !matches!(self.conn, Conn::Live { .. }) {
                            ui.add_space(14.0);
                            let (text, _) = self.status_of();
                            ui.label(egui::RichText::new(text).size(12.0).color(pal.dim));
                        }
                    });
                });
            });
    }

    fn draw_devices(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        egui::SidePanel::left("devices")
            .resizable(false)
            .exact_width(self.left_w)
            .frame(
                egui::Frame::new()
                    .fill(pal.panel)
                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 12,
                        bottom: 12,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("设备")
                            .size(12.0)
                            .strong()
                            .color(pal.dim),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let scanning = self.scanning;
                        // 扫描结果放独立弹窗（原型的做法）；侧栏只留设备卡，
                        // 免得同一台设备在上下两处各出现一次。
                        let label = if scanning { "扫描中…" } else { "扫描局域网" };
                        let btn = egui::Button::new(egui::RichText::new(label).size(12.0))
                            .fill(if scanning {
                                pal.warn_fill
                            } else {
                                egui::Color32::TRANSPARENT
                            })
                            .stroke(egui::Stroke::new(
                                1.0_f32,
                                if scanning {
                                    pal.warn_stroke
                                } else {
                                    pal.accent_stroke
                                },
                            ))
                            .corner_radius(6.0_f32);
                        if ui.add(btn).clicked() {
                            self.open_scan();
                        }
                    });
                });
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .id_salt("device_list")
                    .auto_shrink(egui::Vec2b::new(false, true))
                    .show(ui, |ui| {
                        let mut pick: Option<usize> = None;
                        for (i, cam) in self.cfg.cameras.iter().enumerate() {
                            let selected = i == self.selected;
                            let status = self.device_status(i);
                            let frame = egui::Frame::new()
                                .fill(if selected { pal.active_bg } else { pal.panel2 })
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    if selected {
                                        pal.accent_stroke
                                    } else {
                                        pal.border
                                    },
                                ))
                                .corner_radius(8.0_f32)
                                .inner_margin(egui::Margin {
                                    left: 12,
                                    right: 12,
                                    top: 10,
                                    bottom: 10,
                                });
                            let inner = frame.show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(cam.name.as_str())
                                            .size(13.0)
                                            .strong()
                                            .color(if selected { pal.text } else { pal.dim }),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(cam.transport().as_str())
                                                    .monospace()
                                                    .size(10.0)
                                                    .color(pal.faint),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(cam.id.as_str())
                                            .monospace()
                                            .size(10.0)
                                            .color(pal.faint),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            tag(ui, status.0, status.1);
                                        },
                                    );
                                });
                            });
                            let resp = ui.interact(
                                inner.response.rect,
                                egui::Id::new(("device_card", i)),
                                egui::Sense::click(),
                            );
                            if resp.clicked() {
                                pick = Some(i);
                            }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            ui.add_space(8.0);
                        }
                        if let Some(i) = pick {
                            if self.selected != i {
                                self.selected = i;
                                self.reset_zoom();
                            }
                            self.connect_selected();
                        }

                    });

                // 底部：添加设备（手填 ONVIF）/ 连接断开 / 导入导出 / 说明。
                // Layout::bottom_up 先加的排在最下面。
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    // 扫描不到设备时的正路：手填 ONVIF 地址添加（小度/TP-Link 不支持 WS-Discovery）
                    if ui
                        .add_sized(
                            [ui.available_width(), 28.0],
                            egui::Button::new(
                                egui::RichText::new("＋ 添加设备（手填 ONVIF）").size(12.0),
                            )
                            .fill(pal.accent_fill)
                            .stroke(egui::Stroke::new(1.0_f32, pal.accent_stroke))
                            .corner_radius(6.0_f32),
                        )
                        .clicked()
                    {
                        self.add_open = true;
                        self.add_error.clear();
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("连接").size(12.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                    .corner_radius(6.0_f32),
                            )
                            .clicked()
                        {
                            self.connect_selected();
                        }
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("断开").size(12.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.danger_stroke))
                                    .corner_radius(6.0_f32),
                            )
                            .clicked()
                        {
                            self.disconnect(true);
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("导入设备").size(12.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                    .corner_radius(6.0_f32),
                            )
                            .clicked()
                        {
                            self.open_ie(false);
                        }
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("导出设备").size(12.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                    .corner_radius(6.0_f32),
                            )
                            .clicked()
                        {
                            self.open_ie(true);
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "一次只连一路。断流的设备会按指数退避自动重连。\nONVIF 地址填到主机即可（如 http://192.168.1.64:8080），\n流地址经 GetStreamUri 自动获取。",
                        )
                        .size(11.0)
                        .color(pal.faint),
                    );
                    if !self.device_info.is_empty() {
                        ui.label(
                            egui::RichText::new(self.device_info.as_str())
                                .monospace()
                                .size(10.0)
                                .color(pal.faint),
                        );
                    }
                });
            });
    }

    /// 设备卡片右下角的状态标签。
    fn device_status(&self, i: usize) -> (&'static str, egui::Color32) {
        let pal = &self.pal;
        if i != self.selected {
            return ("未连接", pal.faint);
        }
        match &self.conn {
            Conn::Live { .. } => ("在线", pal.ok),
            Conn::Connecting => ("连接中", pal.warn),
            Conn::Failed(_) => ("失败", pal.bad),
            Conn::Idle => ("未连接", pal.faint),
        }
    }

    /// 数字变焦：以归一化锚点 `anchor`（0..=1，画面内）为不动点缩放 `factor` 倍。
    fn apply_zoom(&mut self, factor: f32, anchor: egui::Vec2) {
        let new_zoom = (self.zoom * factor).clamp(1.0, 8.0);
        let s_old = 1.0 / self.zoom;
        let s_new = 1.0 / new_zoom;
        self.zoom_uv_min.x += anchor.x * (s_old - s_new);
        self.zoom_uv_min.y += anchor.y * (s_old - s_new);
        self.zoom = new_zoom;
        self.clamp_zoom_uv();
    }

    /// 保证 UV 窗口整体落在 [0,1]² 内。
    fn clamp_zoom_uv(&mut self) {
        let s = 1.0 / self.zoom;
        self.zoom_uv_min.x = self.zoom_uv_min.x.clamp(0.0, (1.0 - s).max(0.0));
        self.zoom_uv_min.y = self.zoom_uv_min.y.clamp(0.0, (1.0 - s).max(0.0));
    }

    /// 复位数字变焦（适应窗口）。
    fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.zoom_uv_min = egui::pos2(0.0, 0.0);
    }

    // ---- 专注模式（只看视频）/ 小窗 / 全屏 ----

    fn snapshot_geom(&mut self, ctx: &egui::Context, slot: GeomSlot) {
        ctx.input(|i| remember_geom(&mut self.geom, slot, i.viewport()));
    }

    fn safe_position(&self, ctx: &egui::Context, rect: GeomRect) -> Option<egui::Pos2> {
        crate::storage::visible_position(rect, ctx.pixels_per_point())
            .map(|(x, y)| egui::pos2(x, y))
    }

    /// 启动即进专注模式时不记录 builder 尺寸，以免污染主窗口记忆。
    fn enter_focus(&mut self, ctx: &egui::Context, snapshot_win: bool) {
        if !self.view_mode.is_focus() {
            if snapshot_win {
                self.snapshot_geom(ctx, GeomSlot::Win);
            }
            self.view_mode = self.view_mode.after_enter_focus();
            self.ie_open = false;
            self.add_open = false;
            self.scan_open = false;
            self.add_error.clear();
            // 专注态没有侧栏，允许缩成小窗；普通态三栏需要 1100×700 起步
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                320.0, 200.0,
            )));
        }
        self.hover_until = Some(Instant::now() + Duration::from_secs(5));
    }

    /// 启动即进专注/小窗模式（`--focus` / `--mini`）。
    pub fn start_focus_mode(&mut self, ctx: &egui::Context, mini: bool) {
        self.enter_focus(ctx, false);
        if mini {
            self.enter_mini(ctx);
        }
    }

    /// 先退出全屏，再恢复窗口属性与几何，避免全屏时忽略尺寸命令。
    fn exit_focus(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        if self.view_mode.is_mini() {
            self.snapshot_geom(ctx, GeomSlot::Mini);
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::Normal,
            ));
        }
        self.view_mode = self.view_mode.after_exit_focus();
        // 先设置最小尺寸，再恢复几何，避免窗口撑大后位置改变。
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
            1100.0, 700.0,
        )));
        self.restore_win_geometry(ctx);
    }

    /// 小窗态：无边框 + 置顶。进入时按视频比例给默认尺寸（可记住自定义尺寸）。
    fn enter_mini(&mut self, ctx: &egui::Context) {
        // 专注态或启动小窗时的尺寸不代表主窗口几何。
        self.enter_focus(ctx, false);
        // 全屏态必须先退出全屏，否则 InnerSize 被 winit 忽略、窗口仍是全屏。
        if is_fullscreen(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        self.view_mode = self.view_mode.after_enter_mini();
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
        let (w, h) = self.mini_size();
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
        if !self.initial_position_pending {
            if let Some(pos) = self.geom.mini.and_then(|r| self.safe_position(ctx, r)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        }
        self.hover_until = Some(Instant::now() + Duration::from_secs(5));
    }

    /// 退出小窗：先记小窗几何（视口此刻仍是小窗尺寸），再还原装饰与主窗口几何。
    fn exit_mini(&mut self, ctx: &egui::Context) {
        self.snapshot_geom(ctx, GeomSlot::Mini);
        self.view_mode = self.view_mode.after_exit_mini();
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        self.restore_win_geometry(ctx);
    }

    /// 恢复普通态几何：即便 win 槽未记忆也给出默认尺寸；命令按「尺寸 → 位置」发，
    /// 并登记下一帧补发一次（winit 处理 Decorations / MinInnerSize 时会异步覆盖结果）。
    fn restore_win_geometry(&mut self, ctx: &egui::Context) {
        let (size, _) = restore_target(self.geom.win);
        let pos = self.geom.win.and_then(|r| self.safe_position(ctx, r));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        if let Some(p) = pos {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(p));
        }
        self.pending_restore = Some((size, pos));
    }

    fn toggle_mini(&mut self, ctx: &egui::Context) {
        if self.view_mode.is_mini() {
            self.exit_mini(ctx);
        } else {
            self.enter_mini(ctx);
        }
    }

    /// 小窗直接全屏可能不触发尺寸事件；先恢复窗口属性并设置显示器尺寸。
    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        if is_fullscreen(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            // 退出全屏后回到专注态的常规尺寸，避免停在整屏大小
            let (size, _) = restore_target(self.geom.win);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            return;
        }
        if self.view_mode.is_mini() {
            self.snapshot_geom(ctx, GeomSlot::Mini);
            self.view_mode = self.view_mode.after_exit_mini(); // Focus：全屏与小窗互斥
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::Normal,
            ));
        }
        // 不要让退出专注遗留的恢复命令在全屏时把窗口拉回小尺寸
        self.pending_restore = None;
        // monitor_size 是逻辑像素（egui-winit 已按 pixels_per_point 换算），可直接给 InnerSize
        if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(monitor));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
    }

    /// 小窗尺寸：优先用记忆值；否则按视频比例给 240 高，比例异常或缺失回退 16:9 并限幅。
    fn mini_size(&self) -> (f32, f32) {
        if let Some(r) = self.geom.mini.filter(|r| r.w >= 320 && r.h >= 200) {
            return (r.w.min(2000) as f32, r.h.min(2000) as f32);
        }
        let (fw, fh) = self
            .preview
            .frame
            .as_ref()
            .map(|f| (f.width as f32, f.height as f32))
            .unwrap_or((0.0, 0.0));
        mini_default_size(fw, fh)
    }

    /// 专注态悬浮按钮：原型 `.focus-exit` 的形态——半透明黑底、白色 28% 描边、
    /// 13px 图标 + 12px 文字、6px 圆角；小窗缩至 22px 高、11px 图标和 10px 文字。
    ///
    /// 自绘而不是用 `egui::Button`：图标要 painter 画（中文字体没有对应图形字符），
    /// 且原型要求图标与文字之间 7px 的行内间距，`Button` 只能放纯文本。
    fn overlay_button(
        ui: &mut egui::Ui,
        pal: &Palette,
        icon: OverlayIcon,
        text: &str,
        active: bool,
        alpha: f32,
        compact: bool,
    ) -> egui::Response {
        let (height, pad_x, icon_size, gap, font_size) = if compact {
            (MINI_OVERLAY_BTN_H, 7.0, 11.0, 4.0, 10.0)
        } else {
            (
                OVERLAY_BTN_H,
                OVERLAY_PAD_X,
                OVERLAY_ICON,
                OVERLAY_GAP,
                12.0,
            )
        };
        let color = if active {
            pal.stage_ok
        } else {
            pal.on_stage_text
        };
        let galley = ui.painter().layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(font_size),
            // PLACEHOLDER 让底色在绘制时由 fallback 决定，淡出时不用重建布局
            egui::Color32::PLACEHOLDER,
        );
        let width = pad_x * 2.0 + icon_size + gap + galley.size().x;
        // 原型 `.idle` 是 opacity:0 + pointer-events:none；淡到一半就不再接受点击
        let sense = if alpha >= 0.5 {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), sense);
        let fade = |c: egui::Color32| c.gamma_multiply(alpha);
        let p = ui.painter_at(rect);
        let bg = if resp.hovered() {
            egui::Color32::from_black_alpha(179) // rgba(0,0,0,.7)
        } else {
            egui::Color32::from_black_alpha(115) // rgba(0,0,0,.45)
        };
        p.rect_filled(rect, 6.0_f32, fade(bg));
        p.rect_stroke(
            rect,
            6.0_f32,
            egui::Stroke::new(
                1.0_f32,
                fade(if active {
                    pal.stage_ok
                } else {
                    egui::Color32::from_white_alpha(71) // rgba(255,255,255,.28)
                }),
            ),
            egui::StrokeKind::Inside,
        );
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + pad_x + icon_size / 2.0, rect.center().y),
            egui::vec2(icon_size, icon_size),
        );
        draw_overlay_icon(&p, icon_rect, icon, fade(color));
        p.galley(
            egui::pos2(
                icon_rect.right() + gap,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            fade(color),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        resp
    }

    /// 专注模式悬浮控件：小窗态顶部拖动区 + 右上按钮行。
    ///
    /// 样式对齐原型 `.focus-exit`（14px 边距、半透明黑底、白色 28% 描边、图标 + 12px 文字）；
    /// 原型的 `backdrop-filter: blur(4px)` 在 egui 里没有等价物，用同档不透明度代替。
    /// 指针/滚轮 5 秒不动开始淡出，动一下立即回来；淡到一半就不再接受点击（原型 `.idle`
    /// 的 `pointer-events: none`）。
    fn draw_focus_overlay(&mut self, ui: &mut egui::Ui, video_rect: egui::Rect) {
        let pal = self.pal;
        let compact = self.view_mode.is_mini();
        let (margin_top, margin_right, height) = if compact {
            (MINI_OVERLAY_MARGIN, MINI_OVERLAY_MARGIN, MINI_OVERLAY_BTN_H)
        } else {
            (OVERLAY_TOP, OVERLAY_RIGHT, OVERLAY_BTN_H)
        };
        let Some(until) = self.hover_until else {
            return;
        };
        let alpha = ui.ctx().animate_bool_with_time(
            egui::Id::new("focus_overlay"),
            Instant::now() < until,
            OVERLAY_FADE,
        );
        if alpha <= 0.01 {
            return;
        }
        // 小窗无边框，顶部铺一条拖动提示：只绘制不占事件，和按钮一起淡出。
        // 用上深下透的渐变而不是实心条，避免在 240px 高的窗口里割出一条硬边。
        if self.view_mode.is_mini() {
            let strip = egui::Rect::from_min_size(
                video_rect.min,
                egui::vec2(video_rect.width(), margin_top + height + 6.0),
            );
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(
                strip.left_top(),
                egui::Color32::from_black_alpha(96).gamma_multiply(alpha),
            );
            mesh.colored_vertex(
                strip.right_top(),
                egui::Color32::from_black_alpha(96).gamma_multiply(alpha),
            );
            mesh.colored_vertex(strip.right_bottom(), egui::Color32::TRANSPARENT);
            mesh.colored_vertex(strip.left_bottom(), egui::Color32::TRANSPARENT);
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            ui.painter_at(strip).add(egui::Shape::mesh(mesh));
        }
        let top = video_rect.min.y + margin_top;
        let row = egui::Rect::from_min_max(
            egui::pos2(video_rect.min.x, top),
            egui::pos2(video_rect.max.x - margin_right, top + height),
        );
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(row), |ui| {
            ui.spacing_mut().item_spacing.x = if compact { 4.0 } else { 6.0 };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ctx = ui.ctx().clone();
                let button = |ui: &mut egui::Ui, icon, text, active| {
                    Self::overlay_button(ui, &pal, icon, text, active, alpha, compact)
                };
                if button(ui, OverlayIcon::Fullscreen, "全屏", is_fullscreen(&ctx)).clicked() {
                    self.toggle_fullscreen(&ctx);
                }
                // 「小窗」只是入口：小窗态里它的作用是退出小窗（回到大窗专注），
                // 和「还原」都是"离开小窗"，摆两个出口只会让人分不清，所以小窗态不显示。
                if !self.view_mode.is_mini()
                    && button(ui, OverlayIcon::Mini, "小窗", false).clicked()
                {
                    self.toggle_mini(&ctx);
                }
                if button(ui, OverlayIcon::Restore, "还原", false).clicked() {
                    self.exit_focus(&ctx);
                }
                if self.zoom > 1.0 && button(ui, OverlayIcon::Reset, "复位", false).clicked() {
                    self.reset_zoom();
                }
            });
        });
    }

    fn draw_video(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(pal.bg))
            .show(ctx, |ui| {
                let avail = ui.available_size();
                let bar_h = if self.view_mode.is_focus() { 0.0 } else { 44.0 };
                let log_h = if self.show_log && !self.view_mode.is_focus() {
                    168.0
                } else {
                    0.0
                };
                let video_h = (avail.y - bar_h - log_h).max(80.0);
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(avail.x, video_h),
                    egui::Sense::click_and_drag(),
                );
                let painter = ui.painter_at(rect);

                // 画面层始终是深底：浅色主题只换外围面板，视频区保持原型 .stage 的观感
                painter.rect_filled(rect, 0.0_f32, pal.stage);

                match (&self.preview.tex, self.preview.frame.as_ref()) {
                    (Some(tex), Some(frame)) => {
                        // 数字变焦：按 zoom/zoom_uv_min 裁 UV（纯软件，不动摄像头）。
                        // 设备名/分辨率/码流等参数已在两侧面板展示，画面不再叠字。
                        let dst = fit_rect(rect, frame.width, frame.height);
                        let s = 1.0 / self.zoom;
                        let uv = egui::Rect::from_min_size(self.zoom_uv_min, egui::vec2(s, s));
                        painter.image(tex.id(), dst, uv, egui::Color32::WHITE);
                    }
                    _ => {
                        // 连接态遮罩
                        let (big, detail, color) = match &self.conn {
                            Conn::Connecting => {
                                let name =
                                    self.current().map(|c| c.name.clone()).unwrap_or_default();
                                (
                                    format!("正在连接 {name} …"),
                                    "ONVIF GetProfiles → GetStreamUri → RTSP DESCRIBE".to_string(),
                                    pal.warn,
                                )
                            }
                            Conn::Failed(e) => (
                                "连接失败".to_string(),
                                format!("{e}。将按指数退避自动重连。"),
                                pal.bad,
                            ),
                            // 解码会话已建立但还没到首帧（ffmpeg 预热中）
                            Conn::Live { .. } => (
                                "已连接，等待首帧…".to_string(),
                                "首个关键帧到达后自动显示画面".to_string(),
                                pal.warn,
                            ),
                            _ => (
                                "未连接".to_string(),
                                "从左侧选择设备开始预览".to_string(),
                                pal.stage_dim,
                            ),
                        };
                        if matches!(self.conn, Conn::Connecting) {
                            let spin_rect = egui::Rect::from_center_size(
                                rect.center() + egui::vec2(0.0, -34.0),
                                egui::vec2(28.0, 28.0),
                            );
                            ui.painter_at(rect).circle_stroke(
                                spin_rect.center(),
                                13.0_f32,
                                egui::Stroke::new(2.5_f32, pal.stage_border),
                            );
                        }
                        painter.text(
                            rect.center() + egui::vec2(0.0, 4.0),
                            egui::Align2::CENTER_CENTER,
                            big,
                            egui::FontId::proportional(15.0),
                            color,
                        );
                        painter.text(
                            rect.center() + egui::vec2(0.0, 28.0),
                            egui::Align2::CENTER_CENTER,
                            detail,
                            egui::FontId::monospace(11.0),
                            pal.stage_faint,
                        );
                    }
                }

                if self.preview.frame.is_some() {
                    let notice = self.preview.notice(&self.conn);
                    if let Some(text) = notice {
                        let badge = egui::Rect::from_min_size(
                            rect.min + egui::vec2(14.0, 14.0),
                            egui::vec2(208.0, 28.0),
                        );
                        painter.rect_filled(badge, 6.0_f32, egui::Color32::from_black_alpha(180));
                        painter.text(
                            badge.center(),
                            egui::Align2::CENTER_CENTER,
                            text,
                            egui::FontId::proportional(12.0),
                            pal.on_stage_text,
                        );
                    }
                }

                // 双击：普通模式进入专注模式（只看视频）；专注模式内双击复位变焦
                if resp.double_clicked() {
                    if self.view_mode.is_focus() {
                        self.reset_zoom();
                    } else {
                        self.enter_focus(ctx, true);
                    }
                }

                // 小窗未放大时复用视频响应拖动窗口，包括无画面时。
                if self.view_mode.is_mini() && self.zoom <= 1.0 {
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                    }
                    if resp.dragged() && !drag_started_on_overlay(resp.interact_pointer_pos(), rect)
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                }

                // ---- 数字变焦交互：滚轮缩放、拖动平移 ----
                if let Some((fw, fh)) = self.preview.frame.as_ref().map(|f| (f.width, f.height)) {
                    let dst = fit_rect(rect, fw, fh);
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if resp.hovered() && scroll.abs() > 0.0 {
                        let anchor = resp
                            .hover_pos()
                            .map(|p| {
                                egui::vec2(
                                    ((p.x - dst.min.x) / dst.width()).clamp(0.0, 1.0),
                                    ((p.y - dst.min.y) / dst.height()).clamp(0.0, 1.0),
                                )
                            })
                            .unwrap_or(egui::vec2(0.5, 0.5));
                        self.apply_zoom((scroll * 0.0022_f32).exp(), anchor);
                    }
                    if resp.dragged() && self.zoom > 1.0 {
                        let d = resp.drag_delta();
                        let s = 1.0 / self.zoom;
                        self.zoom_uv_min.x =
                            (self.zoom_uv_min.x - d.x / dst.width() * s).clamp(0.0, 1.0 - s);
                        self.zoom_uv_min.y =
                            (self.zoom_uv_min.y - d.y / dst.height() * s).clamp(0.0, 1.0 - s);
                    }
                }

                // ---- 分栏拖拽手柄：拖动左右分界线调节三栏宽度（专注模式无侧栏）----
                if !self.view_mode.is_focus() {
                    let full = ui.max_rect();
                    let hl = egui::Rect::from_min_max(
                        egui::pos2(full.min.x - 4.0, full.min.y),
                        egui::pos2(full.min.x + 4.0, full.max.y),
                    );
                    let hr = egui::Rect::from_min_max(
                        egui::pos2(full.max.x - 4.0, full.min.y),
                        egui::pos2(full.max.x + 4.0, full.max.y),
                    );
                    let rl = ui.interact(hl, egui::Id::new("split_l"), egui::Sense::drag());
                    let rr = ui.interact(hr, egui::Id::new("split_r"), egui::Sense::drag());
                    if rl.hovered() || rl.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        ui.painter_at(full).line_segment(
                            [
                                egui::pos2(hl.center().x, full.min.y),
                                egui::pos2(hl.center().x, full.max.y),
                            ],
                            egui::Stroke::new(2.0_f32, pal.link),
                        );
                    }
                    if rr.hovered() || rr.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        ui.painter_at(full).line_segment(
                            [
                                egui::pos2(hr.center().x, full.min.y),
                                egui::pos2(hr.center().x, full.max.y),
                            ],
                            egui::Stroke::new(2.0_f32, pal.link),
                        );
                    }
                    if rl.dragged() {
                        self.left_w = (self.left_w + rl.drag_delta().x).clamp(180.0, 440.0);
                    }
                    if rr.dragged() {
                        self.right_w = (self.right_w - rr.drag_delta().x).clamp(240.0, 560.0);
                    }
                }

                // ---- 视频下操作条（专注模式隐藏）----
                if !self.view_mode.is_focus() {
                    let can_stream_switch = self
                        .current()
                        .and_then(|c| c.sub_stream_url.as_ref())
                        .is_some();
                    let live = matches!(self.conn, Conn::Live { .. } | Conn::Connecting);
                    egui::Frame::new()
                        .fill(pal.panel)
                        .stroke(egui::Stroke::new(1.0_f32, pal.border))
                        .inner_margin(egui::Margin {
                            left: 18,
                            right: 18,
                            top: 0,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.set_height(bar_h);
                            ui.horizontal_centered(|ui| {
                                let (_st, color) = self.status_of();
                                let prefix = if matches!(self.conn, Conn::Live { .. })
                                    && self.preview.paused
                                {
                                    "预览已暂停"
                                } else if matches!(self.conn, Conn::Live { .. })
                                    && !self.preview.is_live()
                                {
                                    "等待新画面"
                                } else if matches!(self.conn, Conn::Live { .. }) {
                                    "实时预览"
                                } else {
                                    "连接状态"
                                };
                                ui.label(
                                    egui::RichText::new(prefix).size(12.0).strong().color(color),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        " · {} · {}",
                                        self.current().map(|c| c.name.as_str()).unwrap_or("—"),
                                        if self.high_stream {
                                            "主码流"
                                        } else {
                                            "子码流"
                                        }
                                    ))
                                    .size(12.0)
                                    .color(pal.dim),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        // 断开
                                        if ui
                                            .add_enabled(
                                                live,
                                                egui::Button::new(
                                                    egui::RichText::new("断开").size(12.0),
                                                )
                                                .fill(pal.panel2)
                                                .stroke(egui::Stroke::new(
                                                    1.0_f32,
                                                    pal.danger_stroke,
                                                ))
                                                .corner_radius(6.0_f32),
                                            )
                                            .clicked()
                                        {
                                            self.disconnect(true);
                                        }
                                        if can_stream_switch {
                                            ui.add_space(10.0);
                                            if seg_btn(ui, &pal, "子码流", !self.high_stream)
                                                .clicked()
                                            {
                                                self.set_stream(false);
                                            }
                                            if seg_btn(ui, &pal, "主码流", self.high_stream)
                                                .clicked()
                                            {
                                                self.set_stream(true);
                                            }
                                        }
                                        // 解码清晰度档位（max_width 上限）
                                        ui.add_space(10.0);
                                        let mw_before = self.cfg.settings.max_width;
                                        egui::ComboBox::from_id_salt("res_tier")
                                            .selected_text(
                                                egui::RichText::new(tier_label(mw_before))
                                                    .size(11.0),
                                            )
                                            .width(84.0)
                                            .show_ui(ui, |ui| {
                                                for (name, w) in RES_TIERS {
                                                    ui.selectable_value(
                                                        &mut self.cfg.settings.max_width,
                                                        w,
                                                        egui::RichText::new(name).size(11.0),
                                                    );
                                                }
                                            });
                                        if self.cfg.settings.max_width != mw_before {
                                            self.apply_max_width_change();
                                        }
                                        // 数字变焦（纯软件 UV 裁剪，不动摄像头）
                                        ui.add_space(10.0);
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("−").size(13.0),
                                                )
                                                .fill(pal.panel2)
                                                .corner_radius(6.0_f32),
                                            )
                                            .clicked()
                                        {
                                            self.apply_zoom(1.0 / 1.25, egui::vec2(0.5, 0.5));
                                        }
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{:.0}%",
                                                self.zoom * 100.0
                                            ))
                                            .monospace()
                                            .size(11.0)
                                            .color(pal.dim),
                                        )
                                        .on_hover_text("滚轮缩放 · 拖动平移 · 双击进入专注模式");
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("+").size(13.0),
                                                )
                                                .fill(pal.panel2)
                                                .corner_radius(6.0_f32),
                                            )
                                            .clicked()
                                        {
                                            self.apply_zoom(1.25, egui::vec2(0.5, 0.5));
                                        }
                                        if ui
                                            .add_enabled(
                                                self.zoom > 1.0,
                                                egui::Button::new(
                                                    egui::RichText::new("复位").size(11.0),
                                                )
                                                .fill(pal.panel2)
                                                .corner_radius(6.0_f32),
                                            )
                                            .clicked()
                                        {
                                            self.reset_zoom();
                                        }
                                    },
                                );
                            });
                        });
                }
                if self.show_log && !self.view_mode.is_focus() {
                    ui.add_space(1.0);
                    self.draw_log_strip(ui, log_h);
                }
                if self.view_mode.is_focus() {
                    self.draw_focus_overlay(ui, rect);
                }
                // 所有模式都在画面内提供暂停入口；前景层防止点击时触发视频拖动。
                if let Some(frame) = &self.preview.frame {
                    let pause_rect = if self.view_mode == ViewMode::Normal {
                        fit_rect(rect, frame.width, frame.height)
                    } else {
                        rect
                    };
                    egui::Area::new(egui::Id::new("preview_pause_control"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(if self.view_mode.is_mini() {
                            egui::pos2(
                                pause_rect.left() + MINI_OVERLAY_MARGIN,
                                pause_rect.bottom() - MINI_OVERLAY_MARGIN - MINI_OVERLAY_BTN_H,
                            )
                        } else {
                            egui::pos2(pause_rect.left() + 14.0, pause_rect.bottom() - 42.0)
                        })
                        .movable(false)
                        .show(ctx, |ui| self.draw_pause_button(ui, true));
                }
            });
    }

    /// 日志条（原型里位于中央列底部、视频操作条之下）。
    fn draw_log_strip(&mut self, ui: &mut egui::Ui, height: f32) {
        let pal = self.pal;
        egui::Frame::new()
            .fill(pal.inset)
            .stroke(egui::Stroke::new(1.0_f32, pal.border))
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 7,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.set_height(height);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("日志")
                            .size(11.0)
                            .strong()
                            .color(pal.dim),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("清空").size(11.0))
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(4.0_f32),
                            )
                            .clicked()
                        {
                            self.log.clear();
                            self.push_log("日志已清空");
                        }
                    });
                });
                ui.add_space(2.0);
                // 日期和时间共 19 字符，按实际等宽字体测量，避免固定 52px 截成「2026-1…」。
                let stamp_w = ui
                    .painter()
                    .layout_no_wrap(
                        "0000-00-00 00:00:00".to_string(),
                        egui::FontId::monospace(11.0),
                        pal.faint,
                    )
                    .size()
                    .x
                    .ceil()
                    + 2.0;
                egui::ScrollArea::vertical()
                    .id_salt("log_body")
                    .stick_to_bottom(true)
                    .auto_shrink(egui::Vec2b::new(false, false))
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                log_cell(
                                    ui,
                                    stamp_w,
                                    egui::Label::new(
                                        egui::RichText::new(&line.stamp)
                                            .monospace()
                                            .size(11.0)
                                            .color(pal.faint),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Extend)
                                    .halign(egui::Align::Min),
                                );
                                ui.add_space(10.0);
                                log_cell(
                                    ui,
                                    36.0,
                                    egui::Label::new(
                                        egui::RichText::new(line.level.as_str())
                                            .monospace()
                                            .size(11.0)
                                            .color(line.level.color(&pal)),
                                    )
                                    .truncate()
                                    .halign(egui::Align::Min),
                                );
                                ui.add_space(10.0);
                                let msg_w = (ui.available_width() - 2.0).max(40.0);
                                log_cell(
                                    ui,
                                    msg_w,
                                    egui::Label::new(
                                        egui::RichText::new(&line.msg).size(11.0).color(pal.msg),
                                    )
                                    .truncate()
                                    .halign(egui::Align::Min),
                                );
                            });
                        }
                    });
            });
    }

    fn draw_controls(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        egui::SidePanel::right("controls")
            .resizable(false)
            .exact_width(self.right_w)
            .frame(
                egui::Frame::new()
                    .fill(pal.panel)
                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                    .inner_margin(egui::Margin {
                        left: 0,
                        right: 0,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("ctl_scroll")
                    .auto_shrink(egui::Vec2b::new(false, true))
                    .show(ui, |ui| {
                        self.sect_stats(ui);
                        self.sect_ptz(ui);
                        self.sect_presets(ui);
                        self.sect_capture(ui);
                        self.sect_cache(ui);
                    });
            });
    }

    // ---- 右侧：实时统计 ----
    fn sect_stats(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        section(ui, &pal, "实时统计", |ui| {
            let (res, codec, stream) = match &self.conn {
                Conn::Live {
                    width,
                    height,
                    codec,
                } => (
                    format!("{width}x{height}"),
                    codec.clone(),
                    if self.high_stream {
                        "主码流"
                    } else {
                        "子码流"
                    },
                ),
                _ => ("—".into(), "—".into(), "—"),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let cw = ((ui.available_width() - 8.0) / 2.0).max(40.0);
                stat_cell(
                    ui,
                    &pal,
                    cw,
                    "帧率",
                    &format!("{:.1} fps", self.stats.fps),
                    pal.ok,
                );
                ui.add_space(8.0);
                stat_cell(ui, &pal, cw, "分辨率", &res, pal.text);
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let cw = ((ui.available_width() - 8.0) / 2.0).max(40.0);
                stat_cell(
                    ui,
                    &pal,
                    cw,
                    "解码吞吐",
                    &format!("{:.0} MB/s", self.stats.mb_per_s),
                    pal.text,
                );
                ui.add_space(8.0);
                stat_cell(
                    ui,
                    &pal,
                    cw,
                    "累计帧",
                    &format!("{}", self.stats.frames),
                    pal.text,
                );
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("{codec} · {stream} · rawvideo → GPU"))
                    .monospace()
                    .size(10.0)
                    .color(pal.faint),
            );
            if !self.device_info.is_empty() {
                ui.add_space(2.0);
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("重新取流地址").size(10.0))
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::NONE),
                    )
                    .clicked()
                {
                    self.request_stream_uri();
                }
            }
        });
    }

    // ---- 右侧：云台控制 ----
    fn sect_ptz(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        let caps = self.current().map(|c| c.capabilities);
        let has_onvif = !self.profiles.is_empty();
        let hint = if !caps.map(|c| c.ptz).unwrap_or(false) {
            "设备不支持"
        } else if has_onvif {
            "ONVIF PTZ"
        } else {
            "需连接"
        };
        section(ui, &pal, "云台控制", |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(hint)
                        .monospace()
                        .size(10.0)
                        .color(pal.faint),
                );
            });
            ui.add_space(6.0);

            let ptz_ok = caps.map(|c| c.ptz).unwrap_or(false) && has_onvif;
            if !ptz_ok {
                ui.label(
                    egui::RichText::new(if caps.map(|c| c.ptz).unwrap_or(false) {
                        "ONVIF 未就绪，连接后可用"
                    } else {
                        "该设备未启用云台"
                    })
                    .size(12.0)
                    .color(pal.dim),
                );
                return;
            }

            let speed = PTZ_SPEEDS[self.ptz_speed_idx];
            let mut want = Velocity::default();

            // 3x3 方向键（原型的 dpad）
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(42.0);
                        if pad(ui, &pal, "↑").is_pointer_button_down_on() {
                            want.tilt = speed;
                        }
                    });
                    ui.horizontal(|ui| {
                        if pad(ui, &pal, "←").is_pointer_button_down_on() {
                            want.pan = -speed;
                        }
                        if pad(ui, &pal, "归位").clicked() {
                            self.ptz_stop();
                        }
                        if pad(ui, &pal, "→").is_pointer_button_down_on() {
                            want.pan = speed;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(42.0);
                        if pad(ui, &pal, "↓").is_pointer_button_down_on() {
                            want.tilt = -speed;
                        }
                    });
                });

                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("变焦").size(10.0).color(pal.faint));
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized(
                                egui::vec2(28.0, 26.0),
                                egui::Button::new(egui::RichText::new("−").size(14.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                    .corner_radius(6.0_f32),
                            )
                            .is_pointer_button_down_on()
                        {
                            want.zoom = -speed;
                        }
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(48.0, 4.0), egui::Sense::hover());
                        let p = ui.painter_at(rect);
                        p.rect_filled(rect, 2.0_f32, pal.border);
                        let frac = (self.velocity.zoom.abs() / 1.0).clamp(0.0, 1.0);
                        if frac > 0.0 {
                            p.rect_filled(
                                egui::Rect::from_min_size(
                                    rect.min,
                                    egui::vec2(rect.width() * frac, 4.0),
                                ),
                                2.0_f32,
                                pal.ok,
                            );
                        }
                        if ui
                            .add_sized(
                                egui::vec2(28.0, 26.0),
                                egui::Button::new(egui::RichText::new("＋").size(14.0))
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                    .corner_radius(6.0_f32),
                            )
                            .is_pointer_button_down_on()
                        {
                            want.zoom = speed;
                        }
                    });

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("转动速度").size(10.0).color(pal.faint));
                    egui::ComboBox::from_id_salt("ptz_speed")
                        .selected_text(
                            egui::RichText::new(PTZ_SPEED_NAMES[self.ptz_speed_idx]).size(12.0),
                        )
                        .width(96.0)
                        .show_ui(ui, |ui| {
                            for (i, name) in PTZ_SPEED_NAMES.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.ptz_speed_idx,
                                    i,
                                    egui::RichText::new(*name).size(12.0),
                                );
                            }
                        });
                });
            });

            if want != Velocity::default() {
                if want != self.velocity {
                    // 按设备实际方向校正（TP-Link 这类双轴反向的机型需要）
                    let cmd = match self.current() {
                        Some(cam) => cam.fix_velocity(want),
                        None => want,
                    };
                    if want.zoom.abs() >= 0.01 {
                        self.push_log(format!(
                            "PTZ zoom{} · 速度 {:.2}",
                            if want.zoom > 0.0 { "+" } else { "-" },
                            want.zoom.abs()
                        ));
                    }
                    self.ptz_move(cmd);
                    self.velocity = want;
                }
            } else if self.velocity != Velocity::default() {
                self.ptz_stop();
                self.velocity = Velocity::default();
            }

            // 方向校正（按机型）
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("方向校正（按机型）")
                    .size(10.0)
                    .color(pal.faint),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (mut pan, mut tilt) = match self.current() {
                    Some(c) => (c.invert_pan, c.invert_tilt),
                    None => (false, false),
                };
                if toggle_pill(ui, &pal, "左右反向", pan, 88.0) {
                    pan = !pan;
                    if let Some(c) = self.cfg.cameras.get_mut(self.selected) {
                        c.invert_pan = pan;
                    }
                    self.push_lv(
                        Level::Warn,
                        format!("invert_pan = {pan}（方向校正立即生效）"),
                    );
                }
                ui.add_space(8.0);
                if toggle_pill(ui, &pal, "上下反向", tilt, 88.0) {
                    tilt = !tilt;
                    if let Some(c) = self.cfg.cameras.get_mut(self.selected) {
                        c.invert_tilt = tilt;
                    }
                    self.push_lv(
                        Level::Warn,
                        format!("invert_tilt = {tilt}（方向校正立即生效）"),
                    );
                }
            });
        });
    }

    // ---- 右侧：预置位 ----
    fn sect_presets(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        let hint = if self.presets.is_empty() {
            "—".to_string()
        } else {
            format!("{} 个", self.presets.len())
        };
        section(ui, &pal, "预置位", |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(hint)
                        .monospace()
                        .size(10.0)
                        .color(pal.faint),
                );
            });
            if !self.presets.is_empty() {
                ui.add_space(6.0);
                let presets = self.presets.clone();
                let mut goto: Option<String> = None;
                let mut remove: Option<String> = None;
                for (i, p) in presets.iter().enumerate() {
                    if i % 2 == 0 {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let pw = ((ui.available_width() - 6.0) / 2.0).max(40.0);
                            preset_cell(ui, &pal, pw, i, p, &mut goto, &mut remove);
                            ui.add_space(6.0);
                            if let Some(p2) = presets.get(i + 1) {
                                preset_cell(ui, &pal, pw, i + 1, p2, &mut goto, &mut remove);
                            }
                        });
                        ui.add_space(6.0);
                    }
                }
                if let (Some(tok), Some(s)) = (goto, self.ptz_session.clone()) {
                    self.control.send(Command::GotoPreset(s, tok));
                }
                if let (Some(tok), Some(s)) = (remove, self.ptz_session.clone()) {
                    self.control.send(Command::RemovePreset(s, tok));
                    self.push_log("删除预置位…（设备确认后列表会自动刷新）");
                }
            }

            // 新增：在当前位置存预置位
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let te_w = (ui.available_width() - 4.0 - 68.0).max(60.0);
                ui.add_sized(
                    egui::vec2(te_w, 26.0),
                    egui::TextEdit::singleline(&mut self.preset_name)
                        .hint_text("新预置位名称")
                        .desired_width(te_w),
                );
                ui.add_space(4.0);
                if ui
                    .add_sized(
                        egui::vec2(64.0, 26.0),
                        egui::Button::new(egui::RichText::new("存当前位置").size(11.0))
                            .fill(pal.panel2)
                            .stroke(egui::Stroke::new(1.0_f32, pal.border))
                            .corner_radius(6.0_f32),
                    )
                    .clicked()
                {
                    let name = self.preset_name.trim().to_string();
                    if name.is_empty() {
                        self.toast("先填写预置位名称");
                    } else if let Some(s) = self.ptz_session.clone() {
                        self.control.send(Command::SetPreset(s, name.clone()));
                        self.push_log(format!("保存预置位「{name}」到当前位置…"));
                        self.preset_name.clear();
                    } else {
                        self.toast("还没连上 ONVIF，无法保存预置位");
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("点击预置位跳转；右侧按钮删除；输入名称后可保存当前位置。")
                    .size(10.0)
                    .color(pal.faint),
            );
        });
    }

    // ---- 右侧：采集 ----
    fn sect_capture(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        section(ui, &pal, "采集", |ui| {
            let recording = self.recorder.is_some();
            ui.horizontal(|ui| {
                // item_spacing 会在控件间隐式加 8px，与 add_space 叠加会把行撑出面板被裁切
                ui.spacing_mut().item_spacing.x = 0.0;
                let cw = ((ui.available_width() - 8.0) / 2.0).max(40.0);
                if cap_btn(ui, &pal, cw, "截图", "BMP", false).clicked() {
                    self.snapshot();
                }
                ui.add_space(8.0);
                let label = if self.recorder.as_ref().is_some_and(|rec| rec.stopping()) {
                    "收尾中"
                } else if recording {
                    "停止"
                } else {
                    "录像"
                };
                if cap_btn(ui, &pal, cw, label, "MKV · 不重编码", recording).clicked() {
                    self.toggle_record();
                }
            });
            let record_secs = self.recorder.as_ref().map_or(0, |rec| rec.elapsed_secs());
            if self.recorder.is_some() {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("● REC  {}", fmt_duration(record_secs)))
                        .size(12.0)
                        .color(pal.bad),
                );
            }
        });
    }

    // ---- 右侧：滚动缓存 ----
    fn sect_cache(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        section(ui, &pal, "滚动缓存（DVR）", |ui| {
            let on = self.cache_running();
            let mut toggle_clicked = false;
            let row_w = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(row_w, 42.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // 为原型的 34×18 滑动开关留出固定位置，文字不得把开关挤出面板。
                    let left_w = (row_w - 8.0 - 34.0).max(0.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(left_w, 42.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.add_sized(
                                egui::vec2(left_w, 16.0),
                                egui::Label::new(
                                    egui::RichText::new(if on {
                                        if self.cache.as_ref().is_some_and(|cache| cache.ready()) {
                                            "运行中 · 切片落盘"
                                        } else {
                                            "正在启动…"
                                        }
                                    } else {
                                        "已关闭"
                                    })
                                    .size(12.0)
                                    .color(if on {
                                        pal.ok
                                    } else {
                                        pal.text
                                    }),
                                )
                                .truncate(),
                            );
                            let sub = if on {
                                if let Some(c) = &self.cache {
                                    let u = c.usage();
                                    format!(
                                        "保留最近 {} 分钟 · {} 片 / {:.0} MB",
                                        self.cfg.settings.cache_minutes,
                                        u.files,
                                        u.bytes as f32 / 1_048_576.0
                                    )
                                } else {
                                    format!("保留最近 {} 分钟", self.cfg.settings.cache_minutes)
                                }
                            } else {
                                "启动即切片落盘，只留最近 N 分钟".to_string()
                            };
                            ui.add_sized(
                                egui::vec2(left_w, 14.0),
                                egui::Label::new(
                                    egui::RichText::new(sub)
                                        .monospace()
                                        .size(10.0)
                                        .color(pal.faint),
                                )
                                .truncate(),
                            );
                        },
                    );
                    ui.add_space(8.0);
                    toggle_clicked = cache_switch(ui, &pal, on).clicked();
                },
            );
            if toggle_clicked {
                self.toggle_cache();
            }
            ui.add_space(6.0);
            let export = ui
                .scope(|ui| {
                    // 与原型 .cache-body.off 一致：整块淡化，避免禁用色与透明度叠加两次。
                    if !on {
                        ui.multiply_opacity(0.4);
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let field_w = ((ui.available_width() - 8.0) / 2.0).max(0.0);
                        cache_config_field(
                            ui,
                            &pal,
                            field_w,
                            "保留时长",
                            &format!("{} 分钟", self.cfg.settings.cache_minutes),
                        );
                        ui.add_space(8.0);
                        cache_config_field(
                            ui,
                            &pal,
                            field_w,
                            "切片粒度",
                            &format!("{} 秒", self.cfg.settings.cache_segment_secs),
                        );
                    });
                    ui.add_space(10.0);
                    // 使用整行宽度；关闭态只绘制，不注册点击，颜色由上面的整体透明度控制。
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 32.0),
                        if on {
                            egui::Sense::click()
                        } else {
                            egui::Sense::hover()
                        },
                    );
                    let p = ui.painter_at(rect);
                    p.rect_filled(
                        rect,
                        6.0,
                        if on && resp.hovered() {
                            pal.active_bg
                        } else {
                            pal.accent_fill
                        },
                    );
                    p.rect_stroke(
                        rect,
                        6.0,
                        egui::Stroke::new(1.0_f32, pal.accent_stroke),
                        egui::StrokeKind::Inside,
                    );
                    p.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "导出最近 10 分钟",
                        egui::FontId::proportional(12.0),
                        pal.ok,
                    );
                    resp.on_hover_text(if on {
                        "将最近 10 分钟的缓存片段导出为视频"
                    } else {
                        "开启滚动缓存后可导出"
                    })
                    .clicked()
                        && on
                })
                .inner;
            if export {
                self.export_cache(10);
            }
            if self.cache_retry_at.is_some() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("缓存进程已退出，重试中…")
                        .size(11.0)
                        .color(pal.warn),
                );
            }
        });
    }

    /// 设备导入/导出弹窗：文本即 TOML 片段，支持复制带走 / 存盘 / 粘贴合并。
    fn draw_ie_window(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        if !self.ie_open {
            return;
        }
        let mut open = true;
        let mut do_close = false;
        egui::Window::new("设备导入 / 导出")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "每台设备一段 [[camera]]（格式与 config.toml 相同）。导出内容含明文密码，注意保管。",
                    )
                    .size(11.0)
                    .color(pal.faint),
                );
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.ie_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(16)
                        .font(egui::TextStyle::Monospace),
                );
                if !self.ie_error.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(&self.ie_error).size(11.0).color(pal.bad));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("复制").clicked() {
                        ui.ctx().copy_text(self.ie_text.clone());
                        self.toast("已复制到剪贴板");
                    }
                    if ui.button("保存到文件").clicked() {
                        self.save_devices_file();
                    }
                    if ui.button("导入并合并").clicked() {
                        self.import_devices();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("关闭").clicked() {
                            do_close = true;
                        }
                    });
                });
            });
        if !open || do_close {
            self.ie_open = false;
        }
    }

    /// 「添加设备」弹窗：手填 ONVIF 地址/账号（扫描不可用时的正路）。
    /// 扫描弹窗：结果、进度、失败说明都在这里，侧栏只保留设备卡。
    ///
    /// 「接入」直接走 `adopt()`（GetProfiles → GetStreamUri → 写 config.toml），
    /// 成功后该条变「已接入」；点已接入的那条会关掉弹窗并切到该设备。
    fn draw_scan_window(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        if !self.scan_open {
            return;
        }
        let mut open = true;
        let mut restart = false;
        let mut manual = false;
        let mut close_clicked = false;
        let mut pick: Option<usize> = None;
        egui::Window::new("扫描局域网")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "WS-Discovery 组播 + 网段探测，只覆盖同一广播域；设备需已开启 ONVIF。",
                    )
                    .size(11.0)
                    .color(pal.faint),
                );
                ui.add_space(10.0);

                // 三种状态都画在同一块固定高度里，弹窗高度保持稳定
                ui.set_min_width(430.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), SCAN_BODY_H),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        // allocate_ui_with_layout 只限制不占位，要显式撑到固定高度
                        ui.set_min_height(SCAN_BODY_H);
                if self.scanning {
                    ui.add_space((SCAN_BODY_H - 64.0) * 0.5);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(
                            egui::RichText::new("正在探测局域网内的 ONVIF 设备…")
                                .size(12.0)
                                .color(pal.dim),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "小度、TP-Link(Tapo) 等不响应 WS-Discovery，扫不到属正常，可手动填写地址。",
                        )
                        .size(11.0)
                        .color(pal.faint),
                    );
                } else if self.discovered.is_empty() {
                    ui.add_space((SCAN_BODY_H - 96.0) * 0.5);
                    let (title, color) = match &self.scan_error {
                        Some(e) => (e.clone(), pal.bad),
                        None => ("没有发现设备。".to_string(), pal.warn),
                    };
                    ui.label(egui::RichText::new(title).size(12.0).color(color));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "确认设备已开启 ONVIF、与电脑同网段、防火墙放行 UDP 3702；\
                             不响应组播的设备请用「手动添加」。",
                        )
                        .size(11.0)
                        .color(pal.faint),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(format!(
                            "发现 {} 台 ONVIF 设备",
                            self.discovered.len()
                        ))
                        .size(11.0)
                        .strong()
                        .color(pal.dim),
                    );
                    ui.add_space(6.0);
                    let list = self.discovered.clone();
                    egui::ScrollArea::vertical()
                        .id_salt("scan_list")
                        .max_height(SCAN_BODY_H - 26.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for dev in &list {
                                let url = dev.base_url().unwrap_or_else(|| dev.endpoint.clone());
                                let name = dev.display_name();
                                // 和设备列表一样按 ONVIF 入口匹配：设备自报的名字和 config
                                // 里的 name 常常不同，标题用列表里的那个才对得上号。
                                let linked = self
                                    .cfg
                                    .cameras
                                    .iter()
                                    .position(|c| c.onvif_base().as_deref() == Some(url.as_str()));
                                let title = match linked {
                                    Some(i) => self.cfg.cameras[i].name.clone(),
                                    None if name == url => "ONVIF 设备".to_string(),
                                    None => name,
                                };
                                let card = egui::Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(egui::Stroke::new(
                                        1.0_f32,
                                        if linked.is_some() {
                                            pal.accent_stroke
                                        } else {
                                            pal.border
                                        },
                                    ))
                                    .corner_radius(8.0_f32)
                                    .inner_margin(egui::Margin {
                                        left: 13,
                                        right: 13,
                                        top: 11,
                                        bottom: 11,
                                    })
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.allocate_ui_with_layout(
                                                egui::vec2((ui.available_width() - 74.0).max(120.0), 18.0),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            egui::RichText::new(title)
                                                                .size(13.0)
                                                                .strong()
                                                                .color(pal.text),
                                                        )
                                                        .truncate(),
                                                    );
                                                },
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if let Some(i) = linked {
                                                        // "已接入" 只代表进了配置；连接是否成功要看 conn。
                                                        // 只有当前选中的设备有连接状态，其余如实显示「已加入」。
                                                        let (label, color) = if i == self.selected {
                                                            self.short_status()
                                                        } else {
                                                            ("已加入", pal.dim)
                                                        };
                                                        ui.label(
                                                            egui::RichText::new(label)
                                                                .size(11.0)
                                                                .color(color),
                                                        );
                                                    } else if ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new("接入")
                                                                    .size(11.0)
                                                                    .color(pal.ok),
                                                            )
                                                            .fill(pal.panel2)
                                                            .stroke(egui::Stroke::new(
                                                                1.0_f32,
                                                                pal.accent_stroke,
                                                            ))
                                                            .corner_radius(6.0_f32),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.adopt(dev);
                                                    }
                                                },
                                            );
                                        });
                                        ui.add_space(3.0);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(url.clone())
                                                    .monospace()
                                                    .size(10.0)
                                                    .color(pal.dim),
                                            )
                                            .truncate(),
                                        );
                                    });
                                if let Some(i) = linked {
                                    let hit = ui
                                        .interact(
                                            card.response.rect,
                                            ui.id().with(("scan", url.as_str())),
                                            egui::Sense::click(),
                                        )
                                        .on_hover_text("已接入 · 点击切到该设备");
                                    if hit.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        ui.painter_at(card.response.rect).rect_stroke(
                                            card.response.rect,
                                            8.0_f32,
                                            egui::Stroke::new(1.0_f32, pal.accent_stroke),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    if hit.clicked() {
                                        pick = Some(i);
                                    }
                                }
                                ui.add_space(8.0);
                            }
                        });
                }
                    },
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.scanning,
                            egui::Button::new(egui::RichText::new("重新扫描").size(12.0)),
                        )
                        .clicked()
                    {
                        restart = true;
                    }
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("手动添加").size(12.0),
                        ))
                        .clicked()
                    {
                        manual = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("关闭").size(12.0),
                            ))
                            .clicked()
                        {
                            close_clicked = true;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "接入 = 加入设备列表并尝试起播（写入 config.toml）；需要账号/鉴权的设备可能连接失败。",
                    )
                    .size(10.0)
                    .color(pal.faint),
                );
            });

        if restart {
            self.start_scan();
        }
        if manual {
            self.scan_open = false;
            self.add_open = true;
            self.add_error.clear();
        }
        if let Some(i) = pick {
            self.scan_open = false;
            // 已经在这台设备上播放/连接时才跳过；失败、空闲都要重连——
            // 拉流失败后 Stream 对象仍然存在，不能拿 stream.is_none() 当判据。
            let busy = matches!(self.conn, Conn::Live { .. } | Conn::Connecting);
            if self.selected != i || !busy {
                self.selected = i;
                self.reset_zoom();
                self.connect_selected();
            }
        }
        if close_clicked || !open {
            self.scan_open = false;
        }
    }

    fn draw_add_window(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        if !self.add_open {
            return;
        }
        let mut open = true;
        let mut do_close = false;
        egui::Window::new("添加设备")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(400.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "扫描找不到设备时手填（小度/TP-Link 等不支持 WS-Discovery）。地址可只写到主机。",
                    )
                    .size(11.0)
                    .color(pal.faint),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_sized([64.0, 20.0], egui::Label::new(egui::RichText::new("名称").size(12.0).color(pal.dim)));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_name)
                            .hint_text("可留空，默认用地址")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_sized([64.0, 20.0], egui::Label::new(egui::RichText::new("ONVIF 地址").size(12.0).color(pal.dim)));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_url)
                            .hint_text("http://192.168.1.64:8080")
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_sized([64.0, 20.0], egui::Label::new(egui::RichText::new("用户名").size(12.0).color(pal.dim)));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_user)
                            .desired_width(130.0),
                    );
                    ui.add_space(10.0);
                    ui.add_sized([32.0, 20.0], egui::Label::new(egui::RichText::new("密码").size(12.0).color(pal.dim)));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_pass)
                            .password(true)
                            .desired_width(130.0),
                    );
                });
                if !self.add_error.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(&self.add_error).size(11.0).color(pal.bad));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("添加并连接").size(12.0))
                                .fill(pal.accent_fill)
                                .stroke(egui::Stroke::new(1.0_f32, pal.accent_stroke))
                                .corner_radius(6.0_f32),
                        )
                        .clicked()
                    {
                        self.add_device_manual();
                        if self.add_error.is_empty() {
                            do_close = true;
                        }
                    }
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("取消").size(12.0))
                                .fill(pal.panel2)
                                .stroke(egui::Stroke::new(1.0_f32, pal.border))
                                .corner_radius(6.0_f32),
                        )
                        .clicked()
                    {
                        do_close = true;
                    }
                });
            });
        if !open || do_close {
            self.add_open = false;
        }
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        let pal = self.pal;
        let Some(until) = self.note_until else {
            return;
        };
        if Instant::now() >= until || self.note.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -196.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(pal.toast_fill)
                    .stroke(egui::Stroke::new(1.0_f32, pal.border_strong))
                    .corner_radius(7.0_f32)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        top: 9,
                        bottom: 9,
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.label(egui::RichText::new(&self.note).size(12.0).color(pal.text));
                    });
            });
    }
}

impl eframe::App for XdView {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let background_failed = self.poll_background();
        let save_failed = self.poll_prefs() || background_failed;
        self.guard_close(ctx, save_failed);
        if self.shutdown_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("正在收尾录像、缓存和保存配置…");
                if !self.note.is_empty() {
                    ui.label(&self.note);
                }
                if ui.button("立即退出").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            return;
        }
        if self.autostart {
            self.autostart = false;
            if self.stream.is_none() && !self.cfg.cameras.is_empty() {
                self.connect_selected();
            }
        }
        // 首帧取得 DPI 后校验位置；后续正常帧记录实际几何。
        if self.initial_position_pending {
            self.initial_position_pending = false;
            let initial = if self.view_mode.is_mini() {
                self.geom.mini
            } else {
                self.geom.win
            };
            if let Some(pos) = initial.and_then(|r| self.safe_position(ctx, r)) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        } else if self.view_mode == ViewMode::Normal && self.pending_restore.is_none() {
            self.snapshot_geom(ctx, GeomSlot::Win);
        }
        // winit 的装饰切换与尺寸变更是异步的：退出专注的下一帧再钉一次几何，
        // 避免真机复现的「尺寸没恢复」与「位置漂移」。
        if let Some((size, pos)) = self.pending_restore.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            if let Some(p) = pos {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(p));
            }
        }
        self.poll_stream(ctx);
        self.poll_control();
        self.poll_cache();
        self.ptz_renew_lease();
        self.emit_stats(ctx);

        // 悬浮控件续期：指针/滚轮/按键一动就再显示 5 秒（滚轮缩放时要用「复位」）
        if ctx.input(|i| {
            i.pointer.delta() != egui::Vec2::ZERO
                || i.pointer.any_pressed()
                || i.pointer.any_click()
                || i.raw_scroll_delta != egui::Vec2::ZERO
                || i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Key { pressed: true, .. }))
        }) {
            self.hover_until = Some(Instant::now() + Duration::from_secs(5));
        }
        // Esc：先关弹窗，再退全屏，最后退专注（键盘优先级从最上层往下）
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.add_open {
                self.add_open = false;
                self.add_error.clear();
            } else if self.scan_open {
                self.scan_open = false;
            } else if self.ie_open {
                self.ie_open = false;
            } else if is_fullscreen(ctx) {
                self.toggle_fullscreen(ctx);
            } else if self.view_mode.is_focus() {
                self.exit_focus(ctx);
            }
        }

        if self.view_mode.is_focus() {
            self.draw_video(ctx);
        } else {
            self.draw_top(ctx);
            self.draw_devices(ctx);
            self.draw_controls(ctx);
            self.draw_video(ctx);
        }
        self.draw_ie_window(ctx);
        self.draw_scan_window(ctx);
        self.draw_add_window(ctx);
        self.draw_toast(ctx);

        if matches!(self.conn, Conn::Live { .. }) {
            ctx.request_repaint_after(Duration::from_millis(15));
        } else {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// 退出兜底只发取消；正常关窗的后台等待由 guard_close 完成。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.prefs.shutdown();
        // 正常关闭已经过异步收尾；强制关闭只发送取消，Job 负责最后的兜底。
        self.export_task = None;
        self.stop_cache(false);
        if let Some(rec) = &mut self.recorder {
            rec.stop();
        }
        self.disconnect(false);
    }
}

// ---------------------------------------------------------------------------- 绘制小部件

/// 专注态悬浮按钮的图标；原型用内联 SVG，这里用 painter 画同样的形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayIcon {
    /// 全屏：四角向外撑开
    Fullscreen,
    /// 小窗：画中画（外框 + 右下小窗）
    Mini,
    /// 还原：四角向中心收，与原型 `.focus-exit` 的路径一致
    Restore,
    /// 复位：放大镜减号（数字变焦复位）
    Reset,
    /// 暂停预览：双竖条。
    Pause,
    /// 继续预览：播放三角。
    Play,
}

/// 普通窗口与小窗共用暂停／继续图标；面板与深色视频层分别选择可读配色。
fn pause_icon_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    paused: bool,
    on_stage: bool,
) -> egui::Response {
    let response = ui.add_sized(
        egui::vec2(MINI_OVERLAY_BTN_H, MINI_OVERLAY_BTN_H),
        egui::Button::new("")
            .fill(if on_stage {
                egui::Color32::from_black_alpha(115)
            } else {
                pal.panel2
            })
            .stroke(egui::Stroke::new(
                1.0_f32,
                if on_stage {
                    egui::Color32::from_white_alpha(71)
                } else {
                    pal.border
                },
            ))
            .corner_radius(5.0_f32),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            if paused {
                "继续预览"
            } else {
                "暂停预览"
            },
        )
    });
    draw_overlay_icon(
        &ui.painter_at(response.rect),
        response.rect.shrink(5.0),
        if paused {
            OverlayIcon::Play
        } else {
            OverlayIcon::Pause
        },
        if on_stage {
            pal.on_stage_text
        } else {
            pal.text
        },
    );
    response
}

/// 在 `rect`（正方形）里画图标；坐标按 0..1 归一化，换尺寸不影响形状。
fn draw_overlay_icon(p: &egui::Painter, rect: egui::Rect, icon: OverlayIcon, color: egui::Color32) {
    let w = rect.width();
    let at = |x: f32, y: f32| egui::pos2(rect.min.x + x * w, rect.min.y + y * w);
    let stroke = egui::Stroke::new(1.4_f32, color);
    let poly = |pts: &[(f32, f32)]| {
        let points: Vec<egui::Pos2> = pts.iter().map(|(x, y)| at(*x, *y)).collect();
        p.add(egui::Shape::line(points, stroke));
    };
    match icon {
        // 原型 path：M8 3v3a2 2… / M21 8h-3… / M3 16h3… / M16 21v-3…（24 视图盒）
        OverlayIcon::Restore => {
            poly(&[(0.333, 0.125), (0.333, 0.25), (0.25, 0.333), (0.125, 0.333)]);
            poly(&[(0.875, 0.333), (0.75, 0.333), (0.667, 0.25), (0.667, 0.125)]);
            poly(&[(0.125, 0.667), (0.25, 0.667), (0.333, 0.75), (0.333, 0.875)]);
            poly(&[(0.667, 0.875), (0.667, 0.75), (0.75, 0.667), (0.875, 0.667)]);
        }
        OverlayIcon::Fullscreen => {
            poly(&[(0.125, 0.333), (0.125, 0.125), (0.333, 0.125)]);
            poly(&[(0.875, 0.333), (0.875, 0.125), (0.667, 0.125)]);
            poly(&[(0.125, 0.667), (0.125, 0.875), (0.333, 0.875)]);
            poly(&[(0.875, 0.667), (0.875, 0.875), (0.667, 0.875)]);
        }
        // 小窗：左下角一个窗口框 + 右下方向的箭头（「缩到这个窗口」）
        OverlayIcon::Mini => {
            p.rect_stroke(
                egui::Rect::from_min_max(at(0.42, 0.42), at(0.94, 0.94)),
                1.5_f32,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.line_segment([at(0.06, 0.06), at(0.36, 0.36)], stroke);
            p.line_segment([at(0.36, 0.36), at(0.16, 0.36)], stroke);
            p.line_segment([at(0.36, 0.36), at(0.36, 0.16)], stroke);
        }
        OverlayIcon::Reset => {
            p.circle_stroke(at(0.45, 0.45), w * 0.28, stroke);
            p.line_segment([at(0.66, 0.66), at(0.92, 0.92)], stroke);
            p.line_segment([at(0.33, 0.45), at(0.57, 0.45)], stroke);
        }
        OverlayIcon::Pause => {
            for (left, right) in [(0.18, 0.4), (0.6, 0.82)] {
                p.rect_filled(
                    egui::Rect::from_min_max(at(left, 0.12), at(right, 0.88)),
                    0.5_f32,
                    color,
                );
            }
        }
        OverlayIcon::Play => {
            p.add(egui::Shape::convex_polygon(
                vec![at(0.25, 0.1), at(0.9, 0.5), at(0.25, 0.9)],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
}

/// 关窗暂缓的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    /// 不干预，交给 eframe 正常关闭
    Pass,
    /// 取消这次关闭，等最后一次偏好写回来
    Defer,
    /// 已在暂缓中，继续等
    Wait,
    /// 主动关闭：写完了，或者已经等够 `CLOSE_DEFER_MAX`
    CloseNow,
    /// 写入失败时留在窗口，让用户看到提示。
    ShowFailure,
}

/// 过期回执不能更改待写状态或触发错误提示。
fn take_latest_save(pending: &mut Option<u64>, saved: Saved) -> Option<Saved> {
    if *pending != Some(saved.seq) {
        return None;
    }
    *pending = None;
    Some(saved)
}

/// 保存失败与普通等待分开记录，用户再次关闭必须直接放行。
#[derive(Debug, Default)]
struct CloseState {
    deadline: Option<Instant>,
    failed: bool,
}

/// 更新关窗状态；失败后保留窗口，后续的明确关闭优先于任何待处理任务。
fn advance_close(
    state: &mut CloseState,
    requested: bool,
    pending: bool,
    now: Instant,
    save_failed: bool,
) -> CloseAction {
    if requested && (state.failed || state.deadline.is_some()) {
        return CloseAction::Pass;
    }
    if state.failed {
        return CloseAction::Pass;
    }
    if save_failed && (requested || state.deadline.is_some()) {
        state.failed = true;
        state.deadline = None;
        return CloseAction::ShowFailure;
    }
    let action = close_action(requested, pending, state.deadline, now);
    if action == CloseAction::Defer {
        state.deadline = Some(now + CLOSE_DEFER_MAX);
    }
    action
}

/// 关闭请求的状态机（纯函数，便于确定性测试）。
///
/// 三条护栏：只在「确实有未落盘的写」时暂缓；暂缓期上限由调用方给的 `deadline` 决定；
/// 暂缓期间再次点关闭视为坚持要关，直接放行。超时后主动发 `Close`——因为 `CancelClose`
/// 已经把用户那次点击消费掉了，不主动关闭窗口就会一直开着。
fn close_action(
    close_requested: bool,
    pending: bool,
    deadline: Option<Instant>,
    now: Instant,
) -> CloseAction {
    if close_requested {
        return if pending && deadline.is_none() {
            CloseAction::Defer
        } else {
            CloseAction::Pass
        };
    }
    let Some(deadline) = deadline else {
        return CloseAction::Pass;
    };
    if !pending || now >= deadline {
        CloseAction::CloseNow
    } else {
        CloseAction::Wait
    }
}

/// 把主题写进 egui：两套样式都登记，再固定主题偏好。
///
/// egui 0.31 的 `set_visuals` 只写「当前」主题那一份样式，而当前主题默认跟随系统；
/// 系统运行中切到深色时，egui 会拿默认深色样式把我们的色板顶掉。
/// 两套都写 + 显式 `set_theme`，界面主题就只由这里的 `Theme` 决定。
fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_visuals_of(
        egui::Theme::Dark,
        crate::theme::build_visuals(&Palette::dark()),
    );
    ctx.set_visuals_of(
        egui::Theme::Light,
        crate::theme::build_visuals(&Palette::light()),
    );
    ctx.set_theme(if theme.is_light() {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
}

/// 主题切换按钮：原型顶栏那个 30×30 的方钮，夜间显示月亮、日间显示太阳。
///
/// 图标用 painter 画而不是贴字符：`☾`/`☀` 在中文字体里不一定有字形，
/// 缺字会变成方块。形状本身也很简单（圆 + 光芒 / 挖角圆）。
fn theme_button(ui: &mut egui::Ui, pal: &Palette, light: bool) -> egui::Response {
    let backdrop = pal.panel2;
    topbar_button(
        ui,
        pal,
        None,
        "切换日间 / 夜间模式",
        |p, rect, fg| {
            let c = rect.center();
            let w = rect.width();
            if light {
                // 日间：圆 + 八道光芒（原型 sun 图标：r=4、光芒 6→8，24 视图盒）
                p.circle_stroke(c, w * 0.167, egui::Stroke::new(w * 0.083, fg));
                for i in 0..8 {
                    let a = i as f32 * std::f32::consts::TAU / 8.0;
                    let dir = egui::vec2(a.cos(), a.sin());
                    p.line_segment(
                        [c + dir * w * 0.25, c + dir * w * 0.333],
                        egui::Stroke::new(w * 0.083, fg),
                    );
                }
            } else {
                // 夜间：实心圆挖掉偏右上的一块就是月牙（底是纯色 panel2，挖角可靠）
                p.circle_filled(c + egui::vec2(-w * 0.05, -w * 0.03), w * 0.46, fg);
                p.circle_filled(c + egui::vec2(w * 0.24, -w * 0.24), w * 0.41, backdrop);
            }
        },
    )
}

/// 顶栏按钮（原型 `.theme-btn` / `.theme-btn.wide`）：30 高、`panel2` 底、1px 描边、
/// 6px 圆角；悬停只换文字与描边颜色，底色不变。没给 `label` 时是 30×30 的方形图标钮，
/// 给了就按 `padding 0 10px` + `gap 6px` 排成宽钮（专注）。
fn topbar_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: Option<&str>,
    tip: &str,
    paint_icon: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
) -> egui::Response {
    const ICON: f32 = 15.0;
    const PAD: f32 = 10.0;
    const GAP: f32 = 6.0;
    let galley = label.map(|text| {
        ui.painter().layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(12.0),
            egui::Color32::PLACEHOLDER,
        )
    });
    let width = match &galley {
        Some(g) => PAD * 2.0 + ICON + GAP + g.size().x,
        None => 30.0,
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::click());
    let hovered = resp.hovered();
    let fg = if hovered { pal.text } else { pal.dim };
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0_f32, pal.panel2);
    p.rect_stroke(
        rect,
        6.0_f32,
        egui::Stroke::new(
            1.0_f32,
            if hovered {
                pal.border_strong
            } else {
                pal.border
            },
        ),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(
            if galley.is_some() {
                rect.left() + PAD + ICON / 2.0
            } else {
                rect.center().x
            },
            rect.center().y,
        ),
        egui::vec2(ICON, ICON),
    );
    paint_icon(&p, icon_rect, fg);
    if let Some(g) = galley {
        p.galley(
            egui::pos2(icon_rect.right() + GAP, rect.center().y - g.size().y / 2.0),
            g,
            fg,
        );
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(tip)
}

/// 原型的协议小圆片（ONVIF / RTSP · TCP / DVR 缓存）。`active` 为真时高亮。
fn chip(ui: &mut egui::Ui, pal: &Palette, text: &str, active: bool) {
    let (stroke, color) = if active {
        (egui::Stroke::new(1.0_f32, pal.accent_stroke), pal.dim)
    } else {
        (egui::Stroke::new(1.0_f32, pal.border), pal.faint)
    };
    egui::Frame::new()
        .fill(egui::Color32::TRANSPARENT)
        .stroke(stroke)
        .corner_radius(4.0_f32)
        .inner_margin(egui::Margin {
            left: 7,
            right: 7,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text)
                    .monospace()
                    .size(10.0)
                    .color(color),
            );
        });
}

/// 状态小标签（在线 / 离线 …）。
fn tag(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(3.0_f32)
        .inner_margin(egui::Margin {
            left: 6,
            right: 6,
            top: 1,
            bottom: 1,
        })
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(10.0).color(color));
        });
}

/// 日志单元格固定列宽并左起排列；add_sized 会居中子控件，仅改 Label::halign 不够。
fn log_cell(ui: &mut egui::Ui, width: f32, label: egui::Label) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 14.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(label);
        },
    );
}

/// 右侧区块：标题 + 分隔 + 内容。
fn section<R>(ui: &mut egui::Ui, pal: &Palette, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0_f32, pal.border))
        .inner_margin(egui::Margin {
            left: 16,
            right: 16,
            top: 13,
            bottom: 15,
        })
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(11.0)
                    .strong()
                    .color(pal.dim),
            );
            ui.add_space(10.0);
            add(ui);
        });
}

/// 统计小格子（原型的 stat-cell）。
fn stat_cell(
    ui: &mut egui::Ui,
    pal: &Palette,
    width: f32,
    key: &str,
    val: &str,
    color: egui::Color32,
) {
    egui::Frame::new()
        .fill(pal.panel2)
        .stroke(egui::Stroke::new(1.0_f32, pal.border))
        .corner_radius(6.0_f32)
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_min_width(width - 20.0);
            ui.label(egui::RichText::new(key).size(10.0).color(pal.faint));
            ui.add_space(2.0);
            ui.label(egui::RichText::new(val).monospace().size(15.0).color(color));
        });
}

/// 云台方向键。
fn pad(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
    ui.add_sized(
        egui::vec2(38.0, 28.0),
        egui::Button::new(egui::RichText::new(label).size(13.0))
            .fill(pal.panel2)
            .stroke(egui::Stroke::new(1.0_f32, pal.border))
            .corner_radius(6.0_f32),
    )
}

/// 缓存滑动开关：对齐原型的 34×18 轨道和 14px 圆点，不在开关内挤入文字。
fn cache_switch(ui: &mut egui::Ui, pal: &Palette, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(34.0, 18.0), egui::Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(
        rect,
        9.0,
        if on {
            pal.accent_stroke
        } else {
            pal.border_strong
        },
    );
    p.circle_filled(
        egui::pos2(rect.left() + if on { 25.0 } else { 9.0 }, rect.center().y),
        7.0,
        if on { pal.ok } else { pal.dim },
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(if on {
        "关闭滚动缓存"
    } else {
        "开启滚动缓存"
    })
}

/// 参数展示框沿用原型下拉框的形态。本轮只读展示配置，不提供菜单或修改运行参数。
fn cache_config_field(ui: &mut egui::Ui, pal: &Palette, width: f32, label: &str, value: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 44.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(egui::RichText::new(label).size(10.0).color(pal.faint));
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 6.0, pal.panel2);
            p.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0_f32, pal.border),
                egui::StrokeKind::Inside,
            );
            // 右侧预留箭头宽度，非默认配置的数值也不能压住箭头。
            let text_rect = rect.shrink2(egui::vec2(10.0, 0.0));
            p.with_clip_rect(egui::Rect::from_min_max(
                text_rect.min,
                egui::pos2(text_rect.right() - 12.0, text_rect.bottom()),
            ))
            .text(
                text_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                value,
                egui::FontId::proportional(12.0),
                pal.dim,
            );
            let c = egui::pos2(rect.right() - 10.0, rect.center().y);
            p.add(egui::Shape::line(
                vec![
                    c + egui::vec2(-3.0, -1.5),
                    c + egui::vec2(0.0, 1.5),
                    c + egui::vec2(3.0, -1.5),
                ],
                egui::Stroke::new(1.0_f32, pal.dim),
            ));
            resp.on_hover_text("当前配置（只读）；可在配置文件中调整");
        },
    );
}

/// 开关小药丸（左右反向）。
fn toggle_pill(ui: &mut egui::Ui, pal: &Palette, label: &str, on: bool, width: f32) -> bool {
    let bg = if on { pal.accent_fill } else { pal.panel2 };
    let stroke = if on {
        egui::Stroke::new(1.0_f32, pal.accent_stroke)
    } else {
        egui::Stroke::new(1.0_f32, pal.border)
    };
    let resp = ui.add_sized(
        egui::vec2(width, 26.0),
        egui::Button::new(
            egui::RichText::new(format!("{}  {}", if on { "●" } else { "○" }, label))
                .size(11.0)
                .color(if on { pal.ok } else { pal.dim }),
        )
        .fill(bg)
        .stroke(stroke)
        .corner_radius(6.0_f32),
    );
    resp.clicked()
}

/// 预置位格子：跳转按钮 + 删除按钮（等宽排布）。
fn preset_cell(
    ui: &mut egui::Ui,
    pal: &Palette,
    width: f32,
    idx: usize,
    p: &Preset,
    goto: &mut Option<String>,
    remove: &mut Option<String>,
) {
    let label = format!("{:02}  {}", idx + 1, p.name);
    if ui
        .add_sized(
            egui::vec2(width - 30.0, 28.0),
            egui::Button::new(egui::RichText::new(label).size(11.0))
                .fill(pal.panel2)
                .stroke(egui::Stroke::new(1.0_f32, pal.border))
                .corner_radius(6.0_f32),
        )
        .clicked()
    {
        *goto = Some(p.token.clone());
    }
    ui.add_space(4.0);
    // 字体不一定包含 ✕，直接画叉号，按钮与说明都不再依赖该字形。
    let delete = ui.add_sized(
        egui::vec2(26.0, 28.0),
        egui::Button::new("")
            .fill(pal.panel2)
            .stroke(egui::Stroke::new(1.0_f32, pal.border))
            .corner_radius(6.0_f32),
    );
    delete.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "删除预置位")
    });
    let center = delete.rect.center();
    let painter = ui.painter_at(delete.rect);
    let stroke = egui::Stroke::new(1.4_f32, if delete.hovered() { pal.bad } else { pal.dim });
    painter.line_segment(
        [
            center + egui::vec2(-4.0, -4.0),
            center + egui::vec2(4.0, 4.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(-4.0, 4.0),
            center + egui::vec2(4.0, -4.0),
        ],
        stroke,
    );
    if delete
        .on_hover_text(format!("删除预置位「{}」", p.name))
        .clicked()
    {
        *remove = Some(p.token.clone());
    }
}

/// 采集大按钮（截图 / 录像）。
///
/// 用 allocate_exact_size 固定宽度：此前用 Frame + vertical_centered 会被
/// 居中布局的 min_rect 撑满整行，拖宽右侧面板（宽度反馈回路）。
fn cap_btn(
    ui: &mut egui::Ui,
    pal: &Palette,
    width: f32,
    label: &str,
    sub: &str,
    hot: bool,
) -> egui::Response {
    let (fill, stroke, color) = if hot {
        (
            pal.danger_fill,
            egui::Stroke::new(1.0_f32, pal.danger_stroke),
            pal.bad,
        )
    } else {
        (pal.panel2, egui::Stroke::new(1.0_f32, pal.border), pal.dim)
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 8.0_f32, fill);
    p.rect_stroke(rect, 8.0_f32, stroke, egui::StrokeKind::Inside);
    let c = rect.center();
    p.text(
        c + egui::vec2(0.0, -11.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        color,
    );
    p.text(
        c + egui::vec2(0.0, 10.0),
        egui::Align2::CENTER_CENTER,
        sub,
        egui::FontId::monospace(9.0),
        pal.faint,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// 分段按钮（主码流 / 子码流）。
fn seg_btn(ui: &mut egui::Ui, pal: &Palette, label: &str, on: bool) -> egui::Response {
    let (fill, stroke, color) = if on {
        (
            pal.accent_fill,
            egui::Stroke::new(1.0_f32, pal.accent_stroke),
            pal.ok,
        )
    } else {
        (pal.panel2, egui::Stroke::new(1.0_f32, pal.border), pal.dim)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(11.0).color(color))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(6.0_f32),
    )
}

/// 在 `outer` 内按原始宽高比居中放置画面。
fn fit_rect(outer: egui::Rect, w: usize, h: usize) -> egui::Rect {
    if w == 0 || h == 0 {
        return outer;
    }
    let ar = w as f32 / h as f32;
    let max = outer.size();
    let mut size = egui::vec2(max.x, max.x / ar);
    if size.y > max.y {
        size.y = max.y;
        size.x = max.y * ar;
    }
    egui::Rect::from_center_size(outer.center(), size)
}

fn fmt_duration(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// 把「已按本地偏差平移过的 unix 秒」格式化成 `YYYY-MM-DD HH:MM:SS`。
fn fmt_stamp(local_secs: i64) -> String {
    let s = xd_core::onvif::iso8601_from_unix(local_secs);
    s.replace('T', " ").replace('Z', "")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 本地时区偏差（分钟，东为正）。
///
/// 不引第三方时间库：Windows 直接问 kernel32，其余平台退回 UTC。
/// Bias 语义是「UTC 以西的分钟数」，所以取负号。
#[cfg(windows)]
fn local_bias_minutes() -> i32 {
    #[repr(C)]
    struct SystemTimeRec {
        _year: u16,
        _month: u16,
        _day_of_week: u16,
        _day: u16,
        _hour: u16,
        _minute: u16,
        _second: u16,
        _millis: u16,
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        _standard_name: [u16; 32],
        _standard_date: SystemTimeRec,
        standard_bias: i32,
        _daylight_name: [u16; 32],
        _daylight_date: SystemTimeRec,
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }
    let mut tzi = TimeZoneInformation {
        bias: 0,
        _standard_name: [0; 32],
        _standard_date: SystemTimeRec {
            _year: 0,
            _month: 0,
            _day_of_week: 0,
            _day: 0,
            _hour: 0,
            _minute: 0,
            _second: 0,
            _millis: 0,
        },
        standard_bias: 0,
        _daylight_name: [0; 32],
        _daylight_date: SystemTimeRec {
            _year: 0,
            _month: 0,
            _day_of_week: 0,
            _day: 0,
            _hour: 0,
            _minute: 0,
            _second: 0,
            _millis: 0,
        },
        daylight_bias: 0,
    };
    let code = unsafe { GetTimeZoneInformation(&mut tzi) };
    // 2 = TIME_ZONE_ID_DAYLIGHT；此时还要叠加 DaylightBias（一般是 -60）
    if code == 2 {
        -(tzi.bias + tzi.daylight_bias)
    } else {
        -tzi.bias
    }
}

#[cfg(not(windows))]
fn local_bias_minutes() -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_log_message_starts_at_left_edge_of_its_column() {
        let ctx = egui::Context::default();
        let mut expected_x = 0.0;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 100.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.add_space(180.0);
                        expected_x = ui.next_widget_position().x;
                        log_cell(
                            ui,
                            300.0,
                            egui::Label::new("short log message")
                                .truncate()
                                .halign(egui::Align::Min),
                        );
                    });
                });
            },
        );
        let text = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == "short log message" => {
                    Some(text)
                }
                _ => None,
            })
            .expect("日志正文应被绘制");
        assert!(
            (text.pos.x - expected_x).abs() < 1.0,
            "短正文不能在固定宽度列中居中：actual={} expected={expected_x}",
            text.pos.x
        );
    }

    fn preview_test_frame(seq: u64, width: usize) -> Frame {
        Frame {
            width,
            height: 1,
            rgba: vec![seq as u8; width * 4],
            seq,
            captured_ms: seq as u128,
        }
    }

    #[test]
    fn paused_preview_keeps_displayed_pixels_and_resume_skips_discarded_frames() {
        let ctx = egui::Context::default();
        let mut preview = Preview::default();
        preview.receive(&ctx, preview_test_frame(1, 1));
        preview.toggle();
        let frozen_texture = preview.tex.as_ref().unwrap().id();
        // 模拟持续消费队列：分辨率和像素都改变，也不能替换暂停帧。
        for seq in 2..20 {
            preview.receive(&ctx, preview_test_frame(seq, 2));
        }
        assert_eq!(preview.frame.as_ref().unwrap().seq, 1);
        assert_eq!(preview.frame.as_ref().unwrap().rgba, vec![1; 4]);
        assert_eq!(preview.tex.as_ref().unwrap().id(), frozen_texture);
        assert_eq!(preview.tex.as_ref().unwrap().size(), [1, 1]);
        preview.toggle();
        preview.receive(&ctx, preview_test_frame(20, 2));
        assert_eq!(preview.frame.as_ref().unwrap().seq, 20);
        assert_eq!(preview.tex.as_ref().unwrap().size(), [2, 1]);
    }

    #[test]
    fn resume_waits_for_an_actual_new_frame_before_marking_live() {
        let ctx = egui::Context::default();
        let mut preview = Preview::default();
        preview.receive(&ctx, preview_test_frame(1, 1));
        assert!(preview.is_live());
        preview.toggle();
        preview.receive(&ctx, preview_test_frame(2, 1));
        preview.toggle();
        assert!(!preview.is_live(), "继续按钮不能把冻结帧立即标为实时");
        assert_eq!(preview.frame.as_ref().unwrap().seq, 1);
        preview.receive(&ctx, preview_test_frame(3, 1));
        assert!(preview.is_live());
        assert_eq!(preview.frame.as_ref().unwrap().seq, 3);
    }

    #[test]
    fn optional_onvif_failure_does_not_fail_connecting_rtsp() {
        let mut conn = Conn::Connecting;
        conn.on_onvif_failure(false, true, "ONVIF 鉴权失败");
        assert!(
            matches!(conn, Conn::Connecting),
            "RTSP 仍在连接，不能误报预览失败"
        );
    }

    #[test]
    fn reconnecting_and_opened_without_frames_keep_old_preview_non_live() {
        let ctx = egui::Context::default();
        let mut preview = Preview::default();
        let live = Conn::Live {
            width: 1,
            height: 1,
            codec: "test".into(),
        };
        preview.receive(&ctx, preview_test_frame(1, 1));
        preview.toggle();
        // Interrupted 后保留冻结帧，继续按钮不能解除非实时提示。
        preview.invalidate();
        preview.toggle();
        assert_eq!(
            preview.notice(&Conn::Connecting),
            Some("重新连接中 · 当前画面非实时")
        );
        // Opened 只说明进程启动，尚未有恢复后的第一帧。
        preview.invalidate();
        assert!(!preview.is_live());
        assert_eq!(preview.notice(&live), Some("等待新画面 · 当前画面非实时"));
        assert_eq!(preview.frame.as_ref().unwrap().seq, 1);
        preview.receive(&ctx, preview_test_frame(3, 2));
        assert!(preview.is_live());
        assert_eq!(preview.notice(&live), None);
        preview.invalidate();
        assert_eq!(
            preview.notice(&Conn::Failed("断流".into())),
            Some("连接失败 · 当前画面非实时")
        );
        assert_eq!(preview.notice(&Conn::Idle), Some("已断开 · 当前画面非实时"));
    }

    #[test]
    fn reconnect_does_not_unpause_preview_or_accept_background_frames() {
        let ctx = egui::Context::default();
        let mut preview = Preview::default();
        preview.receive(&ctx, preview_test_frame(1, 1));
        preview.toggle();
        preview.invalidate();
        preview.receive(&ctx, preview_test_frame(2, 2));
        assert!(preview.paused);
        assert!(!preview.is_live());
        assert_eq!(preview.frame.as_ref().unwrap().seq, 1);
        preview.toggle();
        assert!(!preview.is_live());
        preview.receive(&ctx, preview_test_frame(3, 2));
        assert!(preview.is_live());
    }

    #[test]
    fn required_onvif_failure_only_fails_preview_before_media_starts() {
        let mut conn = Conn::Connecting;
        conn.on_onvif_failure(true, false, "无法获取流地址");
        assert!(matches!(&conn, Conn::Failed(e) if e == "无法获取流地址"));
        let mut conn = Conn::Connecting;
        conn.on_onvif_failure(true, true, "迟到的取流失败");
        assert!(matches!(conn, Conn::Connecting));
        let mut conn = Conn::Live {
            width: 1,
            height: 1,
            codec: "test".into(),
        };
        conn.on_onvif_failure(false, true, "云台不可用");
        assert!(matches!(conn, Conn::Live { .. }));
    }

    #[test]
    fn new_preview_session_clears_pause_and_old_device_frame() {
        let ctx = egui::Context::default();
        let mut preview = Preview::default();
        preview.toggle();
        assert!(!preview.paused, "首帧到来前不可暂停");
        preview.receive(&ctx, preview_test_frame(1, 1));
        preview.toggle();
        assert!(preview.paused);
        preview = Preview::default();
        assert!(!preview.paused);
        assert!(preview.frame.is_none());
        assert!(preview.tex.is_none());
        preview.receive(&ctx, preview_test_frame(2, 2));
        assert_eq!(preview.frame.as_ref().unwrap().seq, 2);
    }

    #[test]
    fn timeout_close_request_is_not_deferred_again() {
        let start = Instant::now();
        let mut deadline = CloseState::default();
        assert_eq!(
            advance_close(&mut deadline, true, true, start, false),
            CloseAction::Defer
        );
        let expired = start + CLOSE_DEFER_MAX;
        assert_eq!(
            advance_close(&mut deadline, false, true, expired, false),
            CloseAction::CloseNow
        );
        // egui 将 Close 转换成下一帧的关闭请求，写入仍可能阻塞。
        assert_eq!(
            advance_close(&mut deadline, true, true, expired, false),
            CloseAction::Pass
        );
    }

    #[test]
    fn retry_close_after_save_failure_passes_even_while_media_is_pending() {
        let now = Instant::now();
        let mut state = CloseState::default();
        assert_eq!(
            advance_close(&mut state, true, true, now, false),
            CloseAction::Defer
        );
        assert_eq!(
            advance_close(&mut state, false, true, now, true),
            CloseAction::ShowFailure
        );
        assert_eq!(
            advance_close(&mut state, true, true, now, false),
            CloseAction::Pass
        );
    }

    #[test]
    fn save_result_completes_or_cancels_close_wait() {
        let start = Instant::now();
        let mut deadline = CloseState::default();
        assert_eq!(
            advance_close(&mut deadline, true, true, start, false),
            CloseAction::Defer
        );
        assert_eq!(
            advance_close(&mut deadline, false, false, start, false),
            CloseAction::CloseNow
        );
        assert_eq!(
            advance_close(&mut deadline, true, false, start, false),
            CloseAction::Pass
        );

        let mut deadline = CloseState::default();
        assert_eq!(
            advance_close(&mut deadline, true, true, start, false),
            CloseAction::Defer
        );
        assert_eq!(
            advance_close(&mut deadline, false, false, start, true),
            CloseAction::ShowFailure
        );
        assert_eq!(deadline.deadline, None);
        assert_eq!(
            advance_close(&mut deadline, true, false, start, false),
            CloseAction::Pass
        );
    }

    #[test]
    fn outdated_save_failure_does_not_override_latest_success() {
        let mut pending = Some(2);
        let stale = Saved {
            seq: 1,
            theme: Theme::Light,
            result: Err("旧写入失败".into()),
        };
        assert!(take_latest_save(&mut pending, stale).is_none());
        assert_eq!(pending, Some(2));
        let current = Saved {
            seq: 2,
            theme: Theme::Dark,
            result: Ok(()),
        };
        assert!(take_latest_save(&mut pending, current)
            .unwrap()
            .result
            .is_ok());
        assert_eq!(pending, None);
    }

    #[test]
    fn failed_save_keeps_closing_window_open_until_user_retries() {
        let now = Instant::now();
        let mut state = CloseState::default();
        assert_eq!(
            advance_close(&mut state, true, true, now, false),
            CloseAction::Defer
        );
        assert_eq!(
            advance_close(&mut state, false, true, now, true),
            CloseAction::ShowFailure
        );
        assert_eq!(
            advance_close(&mut state, false, false, now + CLOSE_DEFER_MAX, false),
            CloseAction::Pass
        );
        assert_eq!(
            advance_close(&mut state, true, true, now, true),
            CloseAction::Pass
        );
    }

    #[test]
    fn normal_geometry_survives_maximize_and_minimize() {
        let mut geom = WindowGeom::default();
        let mut viewport = egui::ViewportInfo {
            outer_rect: Some(egui::Rect::from_min_size(
                egui::pos2(-300.0, 50.0),
                egui::vec2(1300.0, 830.0),
            )),
            inner_rect: Some(egui::Rect::from_min_size(
                egui::pos2(-292.0, 80.0),
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        remember_geom(&mut geom, GeomSlot::Win, &viewport);
        let normal = geom.win;
        assert_eq!(
            normal,
            Some(GeomRect {
                x: -300,
                y: 50,
                w: 1280,
                h: 800
            })
        );
        viewport.maximized = Some(true);
        viewport.inner_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1920.0, 1080.0),
        ));
        remember_geom(&mut geom, GeomSlot::Win, &viewport);
        assert_eq!(geom.win, normal);
        viewport.maximized = Some(false);
        viewport.minimized = Some(true);
        viewport.inner_rect = None;
        remember_geom(&mut geom, GeomSlot::Win, &viewport);
        assert_eq!(geom.win, normal);
    }

    #[test]
    fn close_guard_only_defers_a_really_pending_write() {
        let t0 = Instant::now();
        let deadline = t0 + CLOSE_DEFER_MAX;
        // 没有写要等：任何情况下都正常关闭
        assert_eq!(close_action(false, false, None, t0), CloseAction::Pass);
        assert_eq!(close_action(true, false, None, t0), CloseAction::Pass);
        // 有写没落盘：暂缓
        assert_eq!(close_action(true, true, None, t0), CloseAction::Defer);
        // 暂缓期间再点一次关闭：放行，不能让人以为窗口关不掉
        assert_eq!(
            close_action(true, true, Some(deadline), t0),
            CloseAction::Pass
        );
        // 等待中：还没写完就继续等，写完立刻关
        assert_eq!(
            close_action(false, true, Some(deadline), t0),
            CloseAction::Wait
        );
        assert_eq!(
            close_action(false, false, Some(deadline), t0),
            CloseAction::CloseNow
        );
        // 超时：无论写没写回来都关
        assert_eq!(
            close_action(false, true, Some(deadline), deadline),
            CloseAction::CloseNow
        );
        assert_eq!(
            close_action(
                false,
                true,
                Some(deadline),
                deadline + Duration::from_millis(1)
            ),
            CloseAction::CloseNow
        );
        // 没有暂缓在前，后续帧不该主动发 Close
        assert_eq!(close_action(false, true, None, t0), CloseAction::Pass);
    }

    #[test]
    fn mini_default_size_falls_back_and_limits_bad_aspect() {
        assert_eq!(mini_default_size(1920.0, 1080.0), (427.0, 240.0));
        assert_eq!(mini_default_size(0.0, 0.0), (427.0, 240.0));
        assert_eq!(mini_default_size(1920.0, 0.0), (427.0, 240.0));
        assert_eq!(mini_default_size(f32::NAN, 9.0), (427.0, 240.0));
        assert_eq!(mini_default_size(7680.0, 1.0), (1920.0, 240.0));
    }

    #[test]
    fn restore_target_always_has_size_and_keeps_negative_position() {
        // 真机复现：--mini 启动时 win 槽未记忆，旧实现完全不恢复尺寸
        let (size, pos) = restore_target(None);
        assert_eq!(size, egui::vec2(1440.0, 900.0));
        assert!(pos.is_none(), "无记忆时不该把窗口挪到 (0,0)");
        let (size, pos) = restore_target(Some(GeomRect {
            x: -267,
            y: -133,
            w: 1280,
            h: 720,
        }));
        assert_eq!(size, egui::vec2(1280.0, 720.0));
        assert_eq!(pos, Some(egui::pos2(-267.0, -133.0)));
    }

    #[test]
    fn view_mode_transitions_cover_full_sequence() {
        // Normal → Focus → Mini → Focus → Normal
        let mut mode = ViewMode::Normal;
        mode = mode.after_enter_focus();
        assert_eq!(mode, ViewMode::Focus);
        mode = mode.after_enter_mini();
        assert_eq!(mode, ViewMode::Mini);
        mode = mode.after_exit_mini();
        assert_eq!(mode, ViewMode::Focus);
        mode = mode.after_exit_focus();
        assert_eq!(mode, ViewMode::Normal);
        // 已在专注/小窗时重复进入不改变形态；小窗可直接退出专注
        assert_eq!(ViewMode::Focus.after_enter_focus(), ViewMode::Focus);
        assert_eq!(ViewMode::Mini.after_enter_focus(), ViewMode::Mini);
        assert_eq!(ViewMode::Mini.after_exit_focus(), ViewMode::Normal);
    }

    #[test]
    fn view_mode_has_no_invalid_combination() {
        assert!(!ViewMode::Normal.is_focus() && !ViewMode::Normal.is_mini());
        assert!(ViewMode::Focus.is_focus() && !ViewMode::Focus.is_mini());
        assert!(ViewMode::Mini.is_focus() && ViewMode::Mini.is_mini());
    }

    #[test]
    fn fit_keeps_aspect() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 500.0));
        let r = fit_rect(outer, 1920, 1080);
        assert!((r.width() / r.height() - 16.0 / 9.0).abs() < 0.01);
        assert!(r.width() <= 1000.0 && r.height() <= 500.0);
    }

    #[test]
    fn fit_portrait_source() {
        let outer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 800.0));
        let r = fit_rect(outer, 720, 1280);
        assert!(r.height() > r.width());
    }

    #[test]
    fn duration_format() {
        assert_eq!(fmt_duration(0), "00:00");
        assert_eq!(fmt_duration(125), "02:05");
    }

    #[test]
    fn stamp_shape() {
        // 2026-10-31 19:00:00 对应的 unix 秒附近
        let s = fmt_stamp(1_793_000_000);
        assert_eq!(s.len(), 19);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
    }

    #[test]
    fn level_labels() {
        assert_eq!(Level::Info.as_str(), "INFO");
        assert_eq!(Level::Err.as_str(), "ERR");
    }
}
