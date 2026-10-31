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
