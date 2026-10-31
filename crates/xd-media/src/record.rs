//! 录像：另起一个 ffmpeg 做 `-c copy`，不重新解码，CPU 占用极低。
//!
//! 容器固定用 MKV：进程随时被杀（点停止、崩溃、断电）都不会得到坏文件，
//! MP4 则需要写完 moov 才能播。

use crate::ffmpeg::{apply_net, record_args, NetOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub struct Recorder {
    child: Child,
    path: PathBuf,
    started_ms: u128,
}

impl Recorder {
    pub fn start(
        ffmpeg: &str,
        url: &str,
        out_path: &Path,
        low_latency: bool,
        net: NetOptions,
    ) -> Result<Recorder, String> {
        if let Some(dir) = out_path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
            }
        }
        let args = record_args(url, &out_path.to_string_lossy(), low_latency, net);
        let mut cmd = Command::new(ffmpeg);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_net(&mut cmd, net);
        // 隐藏窗口 + 挂作业对象（父进程被强杀时不留孤儿继续写盘）
        let child = crate::sys::spawn_managed(&mut cmd)
            .map_err(|e| format!("启动录制进程失败（{ffmpeg}）：{e}"))?;
        Ok(Recorder {
            child,
            path: out_path.to_path_buf(),
            started_ms: now_ms(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn elapsed_secs(&self) -> u64 {
        ((now_ms().saturating_sub(self.started_ms)) / 1000) as u64
    }

    /// 进程是否还在跑；`Some(status)` 表示已退出。
    pub fn poll(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 停止并收尾，返回输出文件路径。
    pub fn stop(mut self) -> PathBuf {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.path.clone()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `recordings/<cam>_20261031-114633.mkv`
pub fn default_record_path(dir: &Path, camera_id: &str) -> PathBuf {
    let stamp = xd_core::onvif::iso8601_now()
        .replace([':', 'T'], "-")
        .replace('Z', "");
    let safe_id: String = camera_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe_id}_{stamp}.mkv"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_path_is_sanitized() {
        let p = default_record_path(Path::new("recordings"), "门/口 01");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "路径分隔符必须被替换: {name}");
        assert!(!name.contains(' '), "空格必须被替换: {name}");
        assert!(name.ends_with(".mkv"));
        assert_eq!(p.parent().unwrap(), Path::new("recordings"));
    }

    #[test]
    fn record_path_keeps_safe_chars() {
        let p = default_record_path(Path::new("r"), "cam-01");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("cam-01_"));
    }
}
