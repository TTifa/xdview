//! 录像：另起一个 ffmpeg 做 `-c copy`，不重新解码，CPU 占用极低。
//!
//! 容器固定用 MKV，正常停止必须让 ffmpeg 写完索引与时长。
//! 强杀、崩溃或断电仍可能留下不完整文件，不能报告正常保存成功。

use crate::ffmpeg::{apply_net, record_args, NetOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
            .stdin(Stdio::piped())
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

    /// 请求 ffmpeg 写完容器后退出，最多等两秒；超时强制回收并报告失败。
    /// 仅成功退出且输出非空时返回路径。必须在后台线程调用。
    pub fn stop(mut self) -> Result<PathBuf, String> {
        if let Some(mut input) = self.child.stdin.take() {
            let _ = input.write_all(b"q\n");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return Err(format!("录像进程异常退出：{status}；请检查输出文件"));
                    }
                    if std::fs::metadata(&self.path)
                        .map(|m| m.len() > 0)
                        .unwrap_or(false)
                    {
                        return Ok(self.path.clone());
                    }
                    return Err("录像未生成有效文件，请检查流地址和目录权限".into());
                }
                Err(e) => return Err(format!("等待录像退出失败：{e}")),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err("录像收尾超时，已终止进程；输出可能不完整，请检查文件".into());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
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
