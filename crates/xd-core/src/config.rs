//! 配置文件（TOML 子集）解析。
//!
//! 只支持本项目用到的语法：`[table]`、`[[array-of-tables]]`、`key = "str" | key = true | key = 12`、
//! `#` 注释。不支持多行字符串、内联表、数组、日期。
//!
//! ```toml
//! [settings]
//! ffmpeg = "ffmpeg"
//! max_width = 1280
//!
//! [[camera]]
//! id = "door"
//! name = "门口"
//! url = "rtsp://admin:pw@192.168.1.10:554/Streaming/Channels/101"
//! sub_url = "rtsp://admin:pw@192.168.1.10:554/Streaming/Channels/102"
//! onvif = "http://192.168.1.10"
//! username = "admin"
//! password = "pw"
//! caps = "ptz,zoom,snapshot,recording"
//! invert_pan = false   # 云台左右反向（设备开了镜像/反向时才需要）
//! invert_tilt = false  # 云台上下反向
//! ```

use crate::model::{Camera, Capabilities};

/// 配置文本上限（含设备导入），避免不受限的读取与解析分配。
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Settings {
    pub ffmpeg: String,
    /// 运行数据根目录；空字符串使用系统用户数据目录，相对路径基于配置文件目录。
    pub data_dir: String,
    pub max_width: u32,
    pub reconnect: bool,
    pub low_latency: bool,
    pub high_stream: bool,
    /// 拉流时清空 http_proxy 等环境变量（ffmpeg 会把裸 TCP 也塞进代理）
    pub no_proxy: bool,
    /// 默认关闭；仅对已确认的证书兼容问题临时使用 `-tls_verify 0`。
    pub insecure_tls: bool,
    /// RTSP 走硬件解码（`-hwaccel auto`）
    pub hwaccel: bool,
    /// 启动即开始滚动缓存（把实时流切片落盘，只留最近 N 分钟）
    pub cache_enabled: bool,
    /// 缓存保留时长（分钟）
    pub cache_minutes: u32,
    /// 缓存切片时长（秒）
    pub cache_segment_secs: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".to_string(),
            data_dir: String::new(),
            max_width: 1280,
            reconnect: true,
            low_latency: true,
            high_stream: true,
            no_proxy: true,
            insecure_tls: false,
            hwaccel: true,
            cache_enabled: false,
            cache_minutes: 30,
            cache_segment_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub cameras: Vec<Camera>,
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Bool(bool),
    Int(i64),
}

impl Value {
    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn as_u32(&self) -> Option<u32> {
        match self {
            Value::Int(i) if *i >= 0 && *i <= u32::MAX as i64 => Some(*i as u32),
            _ => None,
        }
    }
}

type Table = Vec<(String, Value)>;

fn get<'a>(table: &'a Table, key: &str) -> Option<&'a Value> {
    table.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn get_str(table: &Table, key: &str) -> Option<String> {
    get(table, key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn parse(text: &str) -> Result<AppConfig, String> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err("配置超过 1 MiB，请减少设备数量或无关内容".into());
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut cameras: Vec<Table> = Vec::new();
    let mut settings_table: Table = Vec::new();
    let mut section = Section::Root;

    for (lineno, raw_line) in text.lines().enumerate() {
        let line = strip_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
            let name = inner.trim();
            if name.eq_ignore_ascii_case("camera") {
                cameras.push(Vec::new());
                section = Section::Camera(cameras.len() - 1);
            } else {
                return Err(format!("第 {} 行：未知的数组表 [[{}]]", lineno + 1, name));
            }
            continue;
        }
        if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = inner.trim();
            if name.eq_ignore_ascii_case("settings") {
                section = Section::Settings;
            } else {
                return Err(format!("第 {} 行：未知的表 [{}]", lineno + 1, name));
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("第 {} 行：缺少 '='", lineno + 1))?;
        let key = key.trim().to_string();
        let value = parse_value(value.trim())
            .map_err(|e| format!("第 {} 行 `{}`：{}", lineno + 1, key, e))?;
        match section {
            Section::Root => {
                return Err(format!(
                    "第 {} 行：`{}` 不在任何表内（请先写 [settings] 或 [[camera]]）",
                    lineno + 1,
                    key
                ))
            }
            Section::Settings => settings_table.push((key, value)),
            Section::Camera(i) => cameras[i].push((key, value)),
        }
    }

    let mut settings = Settings::default();
    if let Some(v) = get_str(&settings_table, "ffmpeg") {
        if !v.is_empty() {
            settings.ffmpeg = v;
        }
    }
    if let Some(value) = get(&settings_table, "data_dir") {
        settings.data_dir = value
            .as_str()
            .ok_or("[settings] data_dir 必须是字符串")?
            .to_string();
    }
    if let Some(v) = get(&settings_table, "max_width").and_then(|v| v.as_u32()) {
        settings.max_width = v;
    }
    if let Some(v) = get(&settings_table, "reconnect").and_then(|v| v.as_bool()) {
        settings.reconnect = v;
    }
    if let Some(v) = get(&settings_table, "low_latency").and_then(|v| v.as_bool()) {
        settings.low_latency = v;
    }
    if let Some(v) = get(&settings_table, "high_stream").and_then(|v| v.as_bool()) {
        settings.high_stream = v;
    }
    if let Some(v) = get(&settings_table, "no_proxy").and_then(|v| v.as_bool()) {
        settings.no_proxy = v;
    }
    if let Some(v) = get(&settings_table, "insecure_tls").and_then(|v| v.as_bool()) {
        settings.insecure_tls = v;
    }
    if let Some(v) = get(&settings_table, "hwaccel").and_then(|v| v.as_bool()) {
        settings.hwaccel = v;
    }
    if let Some(v) = get(&settings_table, "cache_enabled").and_then(|v| v.as_bool()) {
        settings.cache_enabled = v;
    }
    if let Some(v) = get(&settings_table, "cache_minutes").and_then(|v| v.as_int()) {
        settings.cache_minutes = v.clamp(1, 24 * 60) as u32;
    }
    if let Some(v) = get(&settings_table, "cache_segment_secs").and_then(|v| v.as_int()) {
        settings.cache_segment_secs = v.clamp(5, 600) as u32;
    }
    let mut out_cameras = Vec::new();
    for (i, table) in cameras.iter().enumerate() {
        let url =
            get_str(table, "url").ok_or_else(|| format!("第 {} 个 [[camera]] 缺少 url", i + 1))?;
        let id = get_str(table, "id").unwrap_or_else(|| format!("cam{}", i + 1));
        let name = get_str(table, "name").unwrap_or_else(|| id.clone());
        let mut cam = Camera::new(id, name, url);
        cam.sub_stream_url = get_str(table, "sub_url").filter(|s| !s.is_empty());
        cam.onvif_url = get_str(table, "onvif").filter(|s| !s.is_empty());
        cam.username = get_str(table, "username").filter(|s| !s.is_empty());
        cam.password = get_str(table, "password");
        cam.invert_pan = get(table, "invert_pan")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        cam.invert_tilt = get(table, "invert_tilt")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(caps) = get_str(table, "caps") {
            cam.capabilities = parse_caps(&caps);
        } else if cam.onvif_url.is_none() {
            // 没有 ONVIF 入口就不显示云台按钮
            cam.capabilities = Capabilities {
                ptz: false,
                zoom: false,
                ..cam.capabilities
            };
        }
        out_cameras.push(cam);
    }

    Ok(AppConfig {
        cameras: out_cameras,
        settings,
    })
}

enum Section {
    Root,
    Settings,
    Camera(usize),
}

fn strip_comment(line: &str) -> &str {
    let mut in_quote = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if in_quote && escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            '#' if !in_quote => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_value(raw: &str) -> Result<Value, String> {
    if raw.starts_with('"') {
        if !raw.ends_with('"') || raw.len() < 2 {
            return Err("字符串缺少收尾引号".into());
        }
        let inner = &raw[1..raw.len() - 1];
        return Ok(Value::Str(unescape(inner)?));
    }
    if raw == "true" {
        return Ok(Value::Bool(true));
    }
    if raw == "false" {
        return Ok(Value::Bool(false));
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    Err("无法识别的值（字符串请加双引号）".into())
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(_) => return Err("不支持的字符串转义".into()),
                None => return Err("字符串末尾的反斜杠缺少转义字符".into()),
            }
        } else if c == '"' {
            return Err("字符串中含未转义的双引号".into());
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn parse_caps(raw: &str) -> Capabilities {
    let mut caps = Capabilities {
        ptz: false,
        zoom: false,
        audio: false,
        talk: false,
        snapshot: false,
        recording: false,
    };
    for item in raw.split(',') {
        match item.trim().to_ascii_lowercase().as_str() {
            "ptz" => caps.ptz = true,
            "zoom" => caps.zoom = true,
            "audio" => caps.audio = true,
            "talk" => caps.talk = true,
            "snapshot" => caps.snapshot = true,
            "recording" => caps.recording = true,
            "all" => {
                caps = Capabilities {
                    ptz: true,
                    zoom: true,
                    audio: true,
                    talk: true,
                    snapshot: true,
                    recording: true,
                }
            }
            _ => {}
        }
    }
    caps
}

impl Capabilities {
    pub fn to_spec(&self) -> String {
        let mut items = Vec::new();
        if self.ptz {
            items.push("ptz");
        }
        if self.zoom {
            items.push("zoom");
        }
        if self.audio {
            items.push("audio");
        }
        if self.talk {
            items.push("talk");
        }
        if self.snapshot {
            items.push("snapshot");
        }
        if self.recording {
            items.push("recording");
        }
        items.join(",")
    }
}

/// 旧版本把窗口几何写进 `config.toml`，这些键现已废弃（`parse` 会忽略）。
/// 只用于启动时给一句中文迁移提示，不参与解析。
pub fn has_legacy_window_keys(text: &str) -> bool {
    const KEYS: [&str; 8] = [
        "win_x", "win_y", "win_w", "win_h", "mini_x", "mini_y", "mini_w", "mini_h",
    ];
    text.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with('#') {
            return false;
        }
        line.split_once('=')
            .is_some_and(|(key, _)| KEYS.contains(&key.trim()))
    })
}

pub fn load(path: &std::path::Path) -> Result<AppConfig, String> {
    load_snapshot(path).map(|(config, _)| config)
}

/// 单次有界读取并返回原始文本快照，用于后续检测外部修改（含注释）。
pub fn load_snapshot(path: &std::path::Path) -> Result<(AppConfig, String), String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| {
            f.take((MAX_CONFIG_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| format!("读取配置 {} 失败: {e}", path.display()))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(format!("{}: 配置超过 1 MiB 上限", path.display()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{}: 配置必须是合法 UTF-8", path.display()))?;
    let config = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((config, text))
}

/// 写入前检查原始文本仍等于读取快照；外部修改时保留文件并提示重新载入。
/// 这是写前冲突检查，不能提供与不协作的外部写者之间的跨进程原子比较/替换。
pub fn save_if_unchanged(
    path: &std::path::Path,
    cfg: &AppConfig,
    expected: &str,
) -> Result<String, String> {
    let (_, current) = load_snapshot(path)?;
    if current != expected {
        return Err("配置已被外部修改，未覆盖文件；请重启载入最新配置后再修改设置".into());
    }
    save_atomic(path, cfg)?;
    Ok(app_config_to_toml(cfg))
}

/// 验证配置可被本解析器读回后原子替换；失败时不覆盖旧配置。
pub fn save_atomic(path: &std::path::Path, cfg: &AppConfig) -> Result<(), String> {
    let text = app_config_to_toml(cfg);
    let parsed = parse(&text)?;
    if app_config_to_toml(&parsed) != text {
        return Err("配置往返校验失败，未覆盖旧文件".into());
    }
    crate::file_io::write_atomic(path, text.as_bytes())
}

/// TOML 字符串转义（`parse_value` 的逆操作）。
fn escape_toml(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// 把单台设备序列化成一段 `[[camera]]`。
fn camera_block(cam: &Camera) -> String {
    let mut out = String::from("\n[[camera]]\n");
    out.push_str(&format!("id = \"{}\"\n", escape_toml(&cam.id)));
    out.push_str(&format!("name = \"{}\"\n", escape_toml(&cam.name)));
    out.push_str(&format!("url = \"{}\"\n", escape_toml(&cam.stream_url)));
    if let Some(sub) = &cam.sub_stream_url {
        if !sub.is_empty() {
            out.push_str(&format!("sub_url = \"{}\"\n", escape_toml(sub)));
        }
    }
    if let Some(onvif) = &cam.onvif_url {
        if !onvif.is_empty() {
            out.push_str(&format!("onvif = \"{}\"\n", escape_toml(onvif)));
        }
    }
    if let Some(user) = &cam.username {
        if !user.is_empty() {
            out.push_str(&format!("username = \"{}\"\n", escape_toml(user)));
        }
    }
    if let Some(pass) = &cam.password {
        out.push_str(&format!("password = \"{}\"\n", escape_toml(pass)));
    }
    let caps = cam.capabilities.to_spec();
    if !caps.is_empty() {
        out.push_str(&format!("caps = \"{caps}\"\n"));
    }
    out.push_str(&format!("invert_pan = {}\n", cam.invert_pan));
    out.push_str(&format!("invert_tilt = {}\n", cam.invert_tilt));
    out
}

/// 把设备列表序列化成 `[[camera]]` 片段（设备导入/导出用）。
///
/// 输出与 `parse` 的语法兼容，可直接粘进 config.toml 或导入窗口；
/// 含明文密码，导出文件不要入库。
pub fn cameras_to_toml(cameras: &[Camera]) -> String {
    let mut out = String::from("# xdview 设备导出（含明文密码，注意保管）\n");
    for cam in cameras {
        out.push_str(&camera_block(cam));
    }
    out
}

/// 把完整配置（settings + 设备）序列化回 config.toml，供程序写回磁盘。
///
/// 与 [`parse`] 对称：`parse` 认得的键这里都写。含明文密码（config.toml 本就 gitignored）。
pub fn app_config_to_toml(cfg: &AppConfig) -> String {
    let s = &cfg.settings;
    let mut out = String::from("# xdview 配置（由程序写回，手改也可）\n[settings]\n");
    out.push_str(&format!("ffmpeg = \"{}\"\n", escape_toml(&s.ffmpeg)));
    out.push_str(&format!("data_dir = \"{}\"\n", escape_toml(&s.data_dir)));
    out.push_str(&format!("max_width = {}\n", s.max_width));
    out.push_str(&format!("reconnect = {}\n", s.reconnect));
    out.push_str(&format!("low_latency = {}\n", s.low_latency));
    out.push_str(&format!("high_stream = {}\n", s.high_stream));
    out.push_str(&format!("no_proxy = {}\n", s.no_proxy));
    out.push_str(&format!("insecure_tls = {}\n", s.insecure_tls));
    out.push_str(&format!("hwaccel = {}\n", s.hwaccel));
    out.push_str(&format!("cache_enabled = {}\n", s.cache_enabled));
    out.push_str(&format!("cache_minutes = {}\n", s.cache_minutes));
    out.push_str(&format!("cache_segment_secs = {}\n", s.cache_segment_secs));
    for cam in &cfg.cameras {
        out.push_str(&camera_block(cam));
    }
    out
}

/// 解析设备片段（或完整 config.toml）里的 `[[camera]]` 列表，供导入用。
///
/// 有 `[settings]` 段也兼容（忽略设置，只取设备）；没有一台设备时报错。
pub fn parse_cameras(text: &str) -> Result<Vec<Camera>, String> {
    let cfg = parse(text)?;
    if cfg.cameras.is_empty() {
        return Err("文本里没有 [[camera]] 设备段".into());
    }
    Ok(cfg.cameras)
}

pub const EXAMPLE: &str = r#"# xdview 配置示例
[settings]
ffmpeg = "ffmpeg"      # ffmpeg 可执行文件（在 PATH 里就写 ffmpeg）
data_dir = ""         # 运行数据目录；空值使用系统用户数据目录
max_width = 1280       # 解码后最大宽度，0 表示保持原始分辨率
reconnect = true       # 断流自动重连
low_latency = true     # 低延迟参数（关掉缓冲）
high_stream = true     # 默认使用主码流
no_proxy = true        # 拉流时清空 http_proxy 等变量（ffmpeg 会把 RTMP/RTSP 也塞进代理）
insecure_tls = false   # 默认关闭；仅对已确认的证书兼容问题临时使用 -tls_verify 0
hwaccel = true         # RTSP 硬件解码（4K HEVC 软解很吃 CPU）
cache_enabled = false  # 启动即开始滚动缓存（切片落盘，只留最近 N 分钟）
cache_minutes = 30     # 缓存保留时长（分钟）
cache_segment_secs = 60 # 每个切片多长（秒）
[[camera]]
id = "door"
name = "门口"
url = "rtsp://admin:password@192.168.1.10:554/Streaming/Channels/101"
sub_url = "rtsp://admin:password@192.168.1.10:554/Streaming/Channels/102"
onvif = "http://192.168.1.10"
username = "admin"
password = "password"
caps = "ptz,zoom,snapshot,recording"

[[camera]]
id = "room"
name = "客厅"
url = "rtsp://192.168.1.11:8554/live"
caps = "snapshot,recording"

[[camera]]
id = "yard"
name = "院子"
# 只拉流、不带云台：caps 里不写 ptz 就不会显示方向键
url = ""
onvif = "http://192.168.1.12"
caps = "snapshot,recording"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_utf8_reports_size_limit_before_decoding() {
        let path = std::env::temp_dir().join(format!(
            "xdview-config-oversize-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, format!("{}中", "a".repeat(MAX_CONFIG_BYTES))).unwrap();
        let error = load_snapshot(&path).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("超过"), "{error}");
    }

    #[test]
    fn external_edit_is_not_overwritten_and_own_saves_advance_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("xdview-config-conflict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        let (mut config, source) = load_snapshot(&path).unwrap();
        config.cameras[0].name = "first".into();
        let source = save_if_unchanged(&path, &config, &source).unwrap();
        config.cameras[0].name = "second".into();
        let source = save_if_unchanged(&path, &config, &source).unwrap();
        let edited = format!("{source}\n# external editor\n");
        std::fs::write(&path, &edited).unwrap();
        config.cameras[0].name = "third".into();
        assert!(save_if_unchanged(&path, &config, &source)
            .unwrap_err()
            .contains("外部修改"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_save_validates_roundtrip_before_replacing_existing_configuration() {
        let dir = std::env::temp_dir().join(format!("xdview-config-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, b"original").unwrap();
        let mut config = parse(EXAMPLE).unwrap();
        config.cameras[0].name = "too-large".repeat(MAX_CONFIG_BYTES);
        assert!(save_atomic(&path, &config).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        config.cameras[0].name = "Gate \"A#1\"".into();
        save_atomic(&path, &config).unwrap();
        assert_eq!(load(&path).unwrap().cameras[0].name, "Gate \"A#1\"");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn escaped_quotes_and_hashes_survive_configuration_roundtrip() {
        let mut cfg = AppConfig::default();
        let mut camera = Camera::new("audit", "Gate \"A#1\"", "file.mp4");
        camera.password = Some("demo\"#\\secret".into());
        cfg.cameras.push(camera);
        let serialized = app_config_to_toml(&cfg);
        let parsed = parse(&serialized).expect("程序写出的配置必须能读取");
        assert_eq!(parsed.cameras[0].name, cfg.cameras[0].name);
        assert_eq!(parsed.cameras[0].password, cfg.cameras[0].password);
    }

    #[test]
    fn parses_example() {
        let cfg = parse(EXAMPLE).unwrap();
        assert_eq!(cfg.cameras.len(), 3);
        assert_eq!(cfg.cameras[0].id, "door");
        assert_eq!(cfg.cameras[0].name, "门口");
        assert!(cfg.cameras[0].capabilities.ptz);
        assert_eq!(
            cfg.cameras[0].onvif_base().as_deref(),
            Some("http://192.168.1.10")
        );
        assert_eq!(cfg.settings.max_width, 1280);
        // 第二路没有 onvif，自动关掉云台
        assert!(!cfg.cameras[1].capabilities.ptz);
        assert!(cfg.cameras[1].capabilities.snapshot);
        // 第三路只有 onvif、url 留空：等 GetStreamUri 填
        assert_eq!(cfg.cameras[2].stream_url, "");
        assert!(cfg.cameras[2].onvif_base().is_some());
    }

    #[test]
    fn comment_in_string_is_kept() {
        let cfg = parse("[[camera]]\nurl = \"rtsp://a/b#frag\"\n").unwrap();
        assert_eq!(cfg.cameras[0].stream_url, "rtsp://a/b#frag");
    }

    #[test]
    fn rejects_value_outside_table() {
        assert!(parse("url = \"x\"\n").is_err());
    }

    #[test]
    fn requires_url() {
        assert!(parse("[[camera]]\nname = \"x\"\n").is_err());
    }

    #[test]
    fn caps_roundtrip() {
        let caps = parse_caps("ptz, zoom ,snapshot");
        assert!(caps.ptz && caps.zoom && caps.snapshot && !caps.talk);
        assert_eq!(caps.to_spec(), "ptz,zoom,snapshot");
    }

    #[test]
    fn escaped_values() {
        let cfg = parse("[[camera]]\nurl = \"a\\\\b\\\"c\"\n").unwrap();
        assert_eq!(cfg.cameras[0].stream_url, "a\\b\"c");
    }

    #[test]
    fn detects_legacy_window_keys_but_ignores_comments() {
        assert!(has_legacy_window_keys("[settings]\nwin_x = 100\n"));
        assert!(has_legacy_window_keys("  mini_h = 240\n"));
        assert!(!has_legacy_window_keys("[settings]\n# win_x = 100\n"));
        assert!(!has_legacy_window_keys("[settings]\ncache_minutes = 30\n"));
    }

    #[test]
    fn app_config_roundtrip() {
        let mut cfg = parse(EXAMPLE).expect("EXAMPLE 应可解析");
        assert!(!cfg.cameras.is_empty());
        cfg.settings.data_dir = "D:/摄像机数据".into();
        cfg.settings.max_width = 540;
        cfg.settings.hwaccel = false;
        let text = app_config_to_toml(&cfg);
        let back = parse(&text).expect("写回的 TOML 应可解析");
        assert_eq!(back.settings.data_dir, cfg.settings.data_dir);
        assert_eq!(back.settings.max_width, 540);
        assert!(!back.settings.hwaccel);
        assert_eq!(back.settings.ffmpeg, cfg.settings.ffmpeg);
        assert_eq!(back.cameras.len(), cfg.cameras.len());
        for (a, b) in cfg.cameras.iter().zip(back.cameras.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.onvif_url, b.onvif_url);
            assert_eq!(a.username, b.username);
            assert_eq!(a.password, b.password);
            assert_eq!(a.invert_pan, b.invert_pan);
        }
    }

    #[test]
    fn cameras_toml_roundtrip() {
        let src = "[[camera]]\n\
                   id = \"door\"\n\
                   name = \"门口 \\\"一号\\\"\"\n\
                   url = \"rtsp://a:554/x\"\n\
                   sub_url = \"rtsp://a:554/y\"\n\
                   onvif = \"http://1.2.3.4\"\n\
                   username = \"admin\"\n\
                   password = \"p\\\\w\"\n\
                   caps = \"ptz,zoom\"\n\
                   invert_pan = true\n";
        let cams = parse_cameras(src).unwrap();
        let text = cameras_to_toml(&cams);
        let back = parse_cameras(&text).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "door");
        assert_eq!(back[0].name, "门口 \"一号\"");
        assert_eq!(back[0].stream_url, "rtsp://a:554/x");
        assert_eq!(back[0].sub_stream_url.as_deref(), Some("rtsp://a:554/y"));
        assert_eq!(back[0].onvif_url.as_deref(), Some("http://1.2.3.4"));
        assert_eq!(back[0].username.as_deref(), Some("admin"));
        assert_eq!(back[0].password.as_deref(), Some("p\\w"));
        assert!(back[0].capabilities.ptz && back[0].capabilities.zoom);
        assert!(back[0].invert_pan && !back[0].invert_tilt);
    }

    #[test]
    fn data_dir_defaults_and_type_validation() {
        assert!(parse(EXAMPLE).unwrap().settings.data_dir.is_empty());
        assert_eq!(
            parse("[settings]\ndata_dir = \"media\"\n")
                .unwrap()
                .settings
                .data_dir,
            "media"
        );
        assert!(parse("[settings]\ndata_dir = true\n").is_err());
    }

    #[test]
    fn parse_cameras_rejects_empty() {
        assert!(parse_cameras("[settings]\nmax_width = 640\n").is_err());
        assert!(parse_cameras("garbage").is_err());
    }

    #[test]
    fn empty_device_configuration_roundtrips_but_empty_import_is_rejected() {
        let mut config = parse(EXAMPLE).unwrap();
        config.cameras.clear();
        let saved = app_config_to_toml(&config);
        let reloaded = parse(&saved).unwrap();
        assert!(reloaded.cameras.is_empty());
        assert!(parse_cameras(&saved).is_err());
    }
}
