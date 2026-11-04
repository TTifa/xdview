//! 运行数据目录：与配置、源码和启动目录分离，统一约束各类输出路径。

use std::path::{Path, PathBuf};

/// 截图、录像、缓存和导出的共同根目录；构造后始终为绝对路径。
#[derive(Debug)]
pub struct StoragePaths {
    root: PathBuf,
}

impl StoragePaths {
    /// 空配置使用系统用户数据目录；相对配置以配置文件所在目录为基准。
    /// 不创建目录；缺少可用用户目录时返回错误，不静默回退到项目目录。
    pub fn resolve(configured: &str, config_path: &Path) -> Result<Self, String> {
        let root = if configured.trim().is_empty() {
            default_root_from(|name| std::env::var_os(name).map(PathBuf::from))
                .ok_or("找不到用户数据目录，请在 [settings] 中设置绝对路径 data_dir")?
        } else {
            let chosen = PathBuf::from(configured);
            if chosen.is_absolute() {
                chosen
            } else {
                if chosen.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::Prefix(_) | std::path::Component::RootDir
                    )
                }) {
                    return Err(
                        "data_dir 请使用完整绝对路径或普通相对路径，不能省略盘符或盘符后的根分隔符"
                            .into(),
                    );
                }
                let cwd = std::env::current_dir()
                    .map_err(|e| format!("无法定位配置目录：{e}；请将 data_dir 设为绝对路径"))?;
                configured_root(&chosen, config_path, &cwd)
            }
        };
        Ok(Self { root })
    }

    /// 根目录也存放含凭据的设备导出文件，不应公开分享整个目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 截图文件名只接受净化后的设备标识与程序生成的时间戳。
    pub fn snapshot(&self, camera_id: &str, stamp: &str) -> PathBuf {
        self.root.join("snapshots").join(format!(
            "{}_{}.bmp",
            safe_component(camera_id),
            safe_component(stamp)
        ))
    }

    pub fn recordings(&self) -> PathBuf {
        self.root.join("recordings")
    }

    pub fn exports(&self) -> PathBuf {
        self.root.join("exports")
    }

    /// 外部设备 ID 不能改变缓存目录层级。
    pub fn cache(&self, camera_id: &str) -> PathBuf {
        self.root.join("cache").join(safe_component(camera_id))
    }

    pub fn devices_export(&self) -> PathBuf {
        self.root.join("devices_export.toml")
    }
}

/// 窗口几何（逻辑像素；位置可为负——多屏副屏在左/上方时是合法坐标）。
/// 与含凭据和用户注释的 `config.toml` 分开存放，避免程序退出时全量覆写配置文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeomRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 主窗口 / 小窗两槽几何；`None` 表示尚未记忆，启动时由系统摆放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowGeom {
    pub win: Option<GeomRect>,
    pub mini: Option<GeomRect>,
}

/// 将逻辑坐标换算为物理坐标；完全离屏时返回 None，保留当前位置。
pub fn visible_position(rect: GeomRect, pixels_per_point: f32) -> Option<(f32, f32)> {
    visible_position_with(rect, pixels_per_point, monitor_intersects)
}

fn visible_position_with(
    rect: GeomRect,
    pixels_per_point: f32,
    intersects: impl FnOnce([i32; 4]) -> bool,
) -> Option<(f32, f32)> {
    if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return None;
    }
    let scale = f64::from(pixels_per_point);
    let physical = |v: i64| {
        (v as f64 * scale)
            .round()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32
    };
    let x = i64::from(rect.x);
    let y = i64::from(rect.y);
    let bounds = [
        physical(x),
        physical(y),
        physical(x + i64::from(rect.w)),
        physical(y + i64::from(rect.h)),
    ];
    intersects(bounds).then_some((rect.x as f32, rect.y as f32))
}

#[cfg(windows)]
fn monitor_intersects(bounds: [i32; 4]) -> bool {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromRect(rect: *const Rect, flags: u32) -> *mut std::ffi::c_void;
    }
    let rect = Rect {
        left: bounds[0],
        top: bounds[1],
        right: bounds[2],
        bottom: bounds[3],
    };
    // MONITOR_DEFAULTTONULL：完全不相交时返回空，负坐标本身不代表离屏。
    unsafe { !MonitorFromRect(&rect, 0).is_null() }
}

#[cfg(not(windows))]
fn monitor_intersects(_bounds: [i32; 4]) -> bool {
    // 非 Windows 平台暂无法查询显示器，保留原有位置恢复行为。
    true
}

impl WindowGeom {
    const FILE: &'static str = "window_geom.toml";

    pub fn path(root: &Path) -> PathBuf {
        root.join(Self::FILE)
    }

    /// 文件缺失、某行损坏或槽位不完整都按「未记忆」处理，不阻止启动。
    pub fn load(root: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path(root)) else {
            return Self::default();
        };
        let mut vals: [Option<i32>; 8] = [None; 8];
        for line in text.lines() {
            let Some((key, raw)) = line.split_once('=') else {
                continue;
            };
            let Ok(v) = raw.trim().parse::<i32>() else {
                continue;
            };
            let idx = match key.trim() {
                "win_x" => 0,
                "win_y" => 1,
                "win_w" => 2,
                "win_h" => 3,
                "mini_x" => 4,
                "mini_y" => 5,
                "mini_w" => 6,
                "mini_h" => 7,
                _ => continue,
            };
            vals[idx] = Some(v);
        }
        // 手改 / 损坏的文件里可能有超大尺寸，统一限幅，避免建出超屏窗口
        const MAX_SIDE: i32 = 8192;
        let rect =
            |x: Option<i32>, y: Option<i32>, w: Option<i32>, h: Option<i32>| match (x, y, w, h) {
                (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Some(GeomRect {
                    x,
                    y,
                    w: w.min(MAX_SIDE) as u32,
                    h: h.min(MAX_SIDE) as u32,
                }),
                _ => None,
            };
        Self {
            win: rect(vals[0], vals[1], vals[2], vals[3]),
            mini: rect(vals[4], vals[5], vals[6], vals[7]),
        }
    }

    /// 一次写完整个小文件；失败原因交给调用方决定如何提示。
    pub fn save(&self, root: &Path) -> Result<(), String> {
        let mut text = String::from("# xdview 窗口几何（程序写回；删除本文件可重置）\n");
        for (prefix, slot) in [("win", self.win), ("mini", self.mini)] {
            if let Some(r) = slot {
                text.push_str(&format!(
                    "{prefix}_x = {x}\n{prefix}_y = {y}\n{prefix}_w = {w}\n{prefix}_h = {h}\n",
                    x = r.x,
                    y = r.y,
                    w = r.w,
                    h = r.h
                ));
            }
        }
        // 先写临时文件再 rename：中途被杀只会留下 .tmp，正式文件仍是上一次的完整内容。
        // Windows 上 fs::rename 带 MOVEFILE_REPLACE_EXISTING，可覆盖已存在的目标。
        let path = Self::path(root);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("保存窗口几何失败：{e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("保存窗口几何失败：{e}")
        })
    }
}

fn configured_root(chosen: &Path, config_path: &Path, cwd: &Path) -> PathBuf {
    if chosen.is_absolute() {
        return chosen.to_path_buf();
    }
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        cwd.join(config_path)
    };
    config_path.parent().unwrap_or(cwd).join(chosen)
}

fn default_root_from(mut get: impl FnMut(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let mut absolute = |name| get(name).filter(|p| p.is_absolute());
    #[cfg(target_os = "windows")]
    let base = absolute("LOCALAPPDATA")
        .or_else(|| absolute("USERPROFILE").map(|p| p.join("AppData").join("Local")));
    #[cfg(target_os = "macos")]
    let base = absolute("HOME").map(|p| p.join("Library").join("Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = absolute("XDG_DATA_HOME")
        .or_else(|| absolute("HOME").map(|p| p.join(".local").join("share")));
    base.map(|p| p.join("xdview"))
}

fn safe_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .take(64)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        return "cam".into();
    }
    let upper = safe.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        format!("_{safe}")
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_position_is_checked_in_physical_pixels() {
        let rect = GeomRect {
            x: -1920,
            y: -100,
            w: 800,
            h: 600,
        };
        let displays = |b: [i32; 4]| {
            // 两块屏幕：左侧副屏和主屏，边界接触不算相交。
            [[-2880, -300, 0, 1320], [0, 0, 1920, 1080]]
                .iter()
                .any(|m| b[0] < m[2] && b[2] > m[0] && b[1] < m[3] && b[3] > m[1])
        };
        assert_eq!(
            visible_position_with(rect, 1.5, displays),
            Some((-1920.0, -100.0))
        );
        assert_eq!(
            visible_position_with(rect, 1.5, |b| b[2] > 0 && b[3] > 0),
            None
        );
        assert_eq!(visible_position_with(rect, f32::NAN, |_| true), None);
        let extreme = GeomRect {
            x: i32::MAX,
            y: i32::MIN,
            w: u32::MAX,
            h: u32::MAX,
        };
        assert!(visible_position_with(extreme, 2.0, |_| false).is_none());
    }

    fn absolute_base() -> PathBuf {
        std::env::temp_dir().join("xdview-path-tests")
    }

    #[test]
    fn default_directory_requires_absolute_user_location() {
        assert!(default_root_from(|_| None).is_none());
        assert!(default_root_from(|_| Some(PathBuf::from("relative"))).is_none());
        let base = absolute_base();
        let got = default_root_from(|_| Some(base.clone())).unwrap();
        assert!(got.is_absolute());
        assert!(got.ends_with("xdview"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_uses_local_appdata_and_falls_back_to_profile() {
        let base = absolute_base();
        let got = default_root_from(|name| (name == "LOCALAPPDATA").then(|| base.clone()));
        assert_eq!(got, Some(base.join("xdview")));
        let got = default_root_from(|name| (name == "USERPROFILE").then(|| base.clone()));
        assert_eq!(got, Some(base.join("AppData/Local/xdview")));
    }

    #[test]
    fn custom_relative_directory_follows_config_not_launch_directory() {
        let base = absolute_base();
        let cfg = base.join("config/config.toml");
        let chosen = Path::new("media");
        assert_eq!(
            configured_root(chosen, &cfg, &base.join("launch-a")),
            base.join("config/media")
        );
        assert_eq!(
            configured_root(chosen, &cfg, &base.join("launch-b")),
            base.join("config/media")
        );
        assert_eq!(
            configured_root(chosen, Path::new("config/config.toml"), &base),
            base.join("config/media")
        );
        let absolute = base.join("external");
        assert_eq!(configured_root(&absolute, &cfg, &base), absolute);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_ambiguous_windows_paths() {
        assert!(StoragePaths::resolve("C:media", Path::new("config.toml")).is_err());
        assert!(StoragePaths::resolve(r"\media", Path::new("config.toml")).is_err());
    }

    #[test]
    fn window_geom_roundtrip_keeps_negative_coords() {
        let root =
            std::env::temp_dir().join(format!("xdview-geom-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let geom = WindowGeom {
            win: Some(GeomRect {
                x: -1920,
                y: -80,
                w: 1600,
                h: 1000,
            }),
            mini: Some(GeomRect {
                x: 300,
                y: 200,
                w: 427,
                h: 240,
            }),
        };
        geom.save(&root).unwrap();
        assert_eq!(WindowGeom::load(&root), geom);
        // 再次保存必须整体覆盖（原子 rename 生效），不能残留上一版的 mini 行
        let shrunk = WindowGeom {
            win: Some(GeomRect {
                x: -267,
                y: -133,
                w: 1280,
                h: 720,
            }),
            mini: None,
        };
        shrunk.save(&root).unwrap();
        assert_eq!(WindowGeom::load(&root), shrunk);
        assert!(!WindowGeom::path(&root).with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn window_geom_tolerates_missing_and_broken_files() {
        let root = std::env::temp_dir().join(format!("xdview-geom-broken-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(WindowGeom::load(&root), WindowGeom::default());
        std::fs::write(
            WindowGeom::path(&root),
            "win_x = -30\nwin_y = nope\nwin_w = 800\nwin_h = 600\nmini_w = 427\n",
        )
        .unwrap();
        let geom = WindowGeom::load(&root);
        assert_eq!(geom.win, None, "缺 y 的槽位应视为未记忆");
        assert_eq!(geom.mini, None, "只有宽度的槽位应视为未记忆");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn window_geom_load_clamps_oversize_values() {
        let root = std::env::temp_dir().join(format!("xdview-geom-clamp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            WindowGeom::path(&root),
            "win_x = 10\nwin_y = 20\nwin_w = 999999\nwin_h = 600\n",
        )
        .unwrap();
        let geom = WindowGeom::load(&root);
        assert_eq!(geom.win.map(|r| (r.w, r.h)), Some((8192, 600)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hostile_camera_ids_cannot_escape_output_directories() {
        let root = absolute_base();
        let paths = StoragePaths { root: root.clone() };
        for id in [
            "../../outside",
            "C:\\outside",
            "/outside",
            "",
            "CON",
            "NUL",
            "门/口",
        ] {
            let shot = paths.snapshot(id, "2026-11-01");
            assert_eq!(shot.parent(), Some(root.join("snapshots").as_path()));
            assert_eq!(paths.cache(id).parent(), Some(root.join("cache").as_path()));
        }
        assert_eq!(paths.cache("CON").file_name().unwrap(), "_CON");
        assert_eq!(paths.recordings(), root.join("recordings"));
        assert_eq!(paths.exports(), root.join("exports"));
        assert_eq!(paths.devices_export(), root.join("devices_export.toml"));
        assert_eq!(
            paths.snapshot("door", "2026-11-01").file_name().unwrap(),
            "door_2026-11-01.bmp"
        );
    }
}
