//! 本地滚动缓存（DVR）：把实时流按固定时长切片落盘，只保留最近 N 分钟。
//!
//! 和 [`crate::record::Recorder`] 的区别：
//!
//! | | `Recorder` | `SegmentCache` |
//! |---|---|---|
//! | 行为 | 点一下开录，产出**一个**文件 | 一直录，产出**很多片段**，自动清理旧的 |
//! | 用途 | 手动录一段 | 事后回看 / 出事再导出 |
//! | 编码 | `-c copy` | `-c copy`（同样不重编码） |
//!
//! 关键取舍：
//!
//! * 用 ffmpeg 的 `segment` 复用器切片，**不重新编码**，CPU 几乎为零；
//! * 容器用 MKV：切片时进程可能被杀，MKV 不会留下坏文件（MP4 要写 moov）；
//! * 清理靠文件 **mtime** 而不是文件名，避免时钟跳变导致误删；
//! * 永远保留最新 2 个片段 —— 否则流断掉、缓存刚重启时容易把仅有的内容删光。

use crate::ffmpeg::{apply_net, input_args, NetOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// 缓存参数。
#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub ffmpeg: String,
    pub url: String,
    /// 片段目录，通常 `cache/<设备 id>/`
    pub dir: PathBuf,
    /// 单片时长（秒）。太短会产生大量小文件，太长则导出粒度粗
    pub segment_secs: u64,
    /// 保留多久（秒）
    pub keep_secs: u64,
    pub net: NetOptions,
}

impl CacheOptions {
    /// 文件名模板：`<id>_20261031-153000.mkv`。
    ///
    /// 用 ffmpeg 的 `-strftime`，所以是**本地时间**；同目录内字典序即时间序，
    /// 导出时直接按文件名排序即可。
    pub fn pattern(&self) -> String {
        let id = sanitize_id(&self.dir);
        self.dir
            .join(format!("{id}_%Y%m%d-%H%M%S.mkv"))
            .to_string_lossy()
            .to_string()
    }
}

/// 目录名当设备 id 用；非法字符换成下划线。
fn sanitize_id(dir: &Path) -> String {
    let raw = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "cam".to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "cam".to_string()
    } else {
        cleaned
    }
}

/// 构造 ffmpeg 参数。抽成纯函数便于断言。
pub fn cache_args(opts: &CacheOptions) -> Vec<String> {
    // 只做 copy，不需要解码，所以关掉 hwaccel（省得 ffmpeg 白初始化一次硬解）
    let net = NetOptions {
        hwaccel: false,
        ..opts.net
    };
    // 也不能带低延迟参数：`nobuffer` 会让我们自己丢关键帧，缓存反而残缺
    let mut args = input_args(&opts.url, false, net);
    args.extend(
        [
            "-i",
            &opts.url,
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "segment",
            "-segment_format",
            "matroska",
            "-segment_time",
            &opts.segment_secs.max(1).to_string(),
            "-reset_timestamps",
            "1",
            "-strftime",
            "1",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    // 去掉 `-nostdin`：缓存进程要能收 'q' 优雅收尾，父进程意外退出时也能靠 stdin EOF 自杀，
    // 否则会留下孤儿 ffmpeg 一直往磁盘写。
    args.retain(|a| a != "-nostdin");
    args.push(opts.pattern());
    args
}

pub struct SegmentCache {
    child: Child,
    opts: CacheOptions,
    started_ms: u128,
}

impl SegmentCache {
    pub fn start(opts: CacheOptions) -> Result<SegmentCache, String> {
        std::fs::create_dir_all(&opts.dir)
            .map_err(|e| format!("创建缓存目录 {} 失败: {e}", opts.dir.display()))?;
        let args = cache_args(&opts);
        let mut cmd = Command::new(&opts.ffmpeg);
        cmd.args(&args)
            // 留着 stdin：退出时给 ffmpeg 发 'q'，让它把当前片段写完（否则最后一片是残缺的）
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_net(&mut cmd, opts.net);
        // 隐藏窗口 + 挂作业对象：父进程被强杀时 Drop 不会跑，靠作业对象把 ffmpeg 一起带走
        let child = crate::sys::spawn_managed(&mut cmd)
            .map_err(|e| format!("启动缓存进程失败（{}）：{e}", opts.ffmpeg))?;
        Ok(SegmentCache {
            child,
            opts,
            started_ms: now_ms(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.opts.dir
    }

    pub fn elapsed_secs(&self) -> u64 {
        ((now_ms().saturating_sub(self.started_ms)) / 1000) as u64
    }

    /// `Some(status)` 表示进程已退出（流断了/参数不对），调用方负责重启。
    pub fn poll(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 统计当前缓存占用。
    pub fn usage(&self) -> CacheUsage {
        let files = list_segments(&self.opts.dir);
        CacheUsage {
            files: files.len(),
            bytes: files.iter().map(|(_, size, _)| *size).sum(),
            span_secs: match (files.first(), files.last()) {
                (Some((_, _, first)), Some((_, _, last))) => last.saturating_sub(*first),
                _ => 0,
            },
        }
    }

    /// 删掉超过保留时长的片段，返回删除个数。
    pub fn prune(&self) -> usize {
        let all = scan_segments(&self.opts.dir);
        let mut n = 0;
        // 0 字节残片：父进程被强杀时 ffmpeg 来不及写头，留在盘上纯属垃圾
        for (path, size, _) in &all {
            if *size == 0 && std::fs::remove_file(path).is_ok() {
                n += 1;
            }
        }
        let files: Vec<(PathBuf, u64, u64)> =
            all.into_iter().filter(|(_, sz, _)| *sz > 0).collect();
        for path in expired(&files, now_secs(), self.opts.keep_secs) {
            if std::fs::remove_file(&path).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// 停缓存：先请 ffmpeg 收尾（写完当前片段），2 秒内不退再强杀。
    pub fn stop(mut self) {
        self.quit();
        // 收尾时 ffmpeg 可能刚开了下一个片段就退出，留下 0 字节文件，顺手清掉
        self.prune();
    }

    fn quit(&mut self) {
        use std::io::Write;
        if let Some(mut si) = self.child.stdin.take() {
            let _ = si.write_all(b"q\n");
            let _ = si.flush();
        }
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SegmentCache {
    fn drop(&mut self) {
        self.quit();
        let empty: Vec<PathBuf> = scan_segments(&self.opts.dir)
            .into_iter()
            .filter(|(_, size, _)| *size == 0)
            .map(|(p, _, _)| p)
            .collect();
        for p in empty {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub files: usize,
    pub bytes: u64,
    /// 最旧到最新片段的跨度（近似为「缓存了多少秒」）
    pub span_secs: u64,
}

/// 列出目录里的片段（**跳过 0 字节残片**）：`(路径, 大小, mtime 秒)`，按文件名排序。
pub fn list_segments(dir: &Path) -> Vec<(PathBuf, u64, u64)> {
    scan_segments(dir)
        .into_iter()
        .filter(|(_, size, _)| *size > 0)
        .collect()
}

/// 目录里全部 `.mkv`（含 0 字节），供清理逻辑使用。
fn scan_segments(dir: &Path) -> Vec<(PathBuf, u64, u64)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, u64, u64)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x.eq_ignore_ascii_case("mkv"))
                .unwrap_or(false)
        })
        .map(|e| {
            let path = e.path();
            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (path, size, mtime)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 挑出应当删除的片段：早于 `now - keep_secs`，但**永远保留最新 2 个**。
pub fn expired(files: &[(PathBuf, u64, u64)], now: u64, keep_secs: u64) -> Vec<PathBuf> {
    if files.len() <= 2 {
        return Vec::new();
    }
    let cutoff = now.saturating_sub(keep_secs);
    let last_two = files.len() - 2;
    files[..last_two]
        .iter()
        .filter(|(_, _, mtime)| *mtime < cutoff)
        .map(|(p, _, _)| p.clone())
        .collect()
}

/// 挑出导出用的片段：mtime 不早于 `now - last_secs`；即使全都超时也至少给最新一个。
pub fn segments_since(files: &[(PathBuf, u64, u64)], now: u64, last_secs: u64) -> Vec<PathBuf> {
    if files.is_empty() {
        return Vec::new();
    }
    let cutoff = now.saturating_sub(last_secs);
    let picked: Vec<PathBuf> = files
        .iter()
        .filter(|(_, _, mtime)| *mtime >= cutoff)
        .map(|(p, _, _)| p.clone())
        .collect();
    if picked.is_empty() {
        return files
            .last()
            .map(|(p, _, _)| vec![p.clone()])
            .unwrap_or_default();
    }
    picked
}

/// 生成 ffmpeg concat 清单。路径统一用正斜杠 + 单引号，单引号本身按 ffmpeg 规则转义。
pub fn concat_list(paths: &[PathBuf]) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for p in paths {
        let abs = absolutize(p);
        let s = abs
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        out.push_str(&format!("file '{s}'\n"));
    }
    out
}

/// 把相对路径补成绝对路径。
///
/// ffmpeg 解析清单里的相对路径时是相对**清单文件所在目录**，而清单放在导出目录下，
/// 于是 `cache/cam/x.mkv` 会被解析成 `exports/cache/cam/x.mkv` —— 实测就是这里翻车的。
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// 把最近 `last_secs` 的片段拼成一个文件（`-c copy`，不重编码）。
///
/// 返回用到的片段数；`0` 表示目录里还没有内容。
pub fn export(
    ffmpeg: &str,
    dir: &Path,
    last_secs: u64,
    out: &Path,
    net: NetOptions,
) -> Result<usize, String> {
    let files = list_segments(dir);
    let picked = segments_since(&files, now_secs(), last_secs);
    if picked.is_empty() {
        return Ok(0);
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
        }
    }
    // 清单必须落在被拼接文件之外的地方没要求，但同盘的绝对路径最稳
    let list_path = out.with_extension("concat.txt");
    std::fs::write(&list_path, concat_list(&picked))
        .map_err(|e| format!("写 concat 清单失败: {e}"))?;

    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &list_path.to_string_lossy(),
        "-c",
        "copy",
        "-y",
        &out.to_string_lossy(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    apply_net(&mut cmd, net);
    // 同样走 spawn_managed：拼接进程也不能在父进程死后变孤儿
    let status = crate::sys::spawn_managed(&mut cmd).and_then(|c| c.wait_with_output());
    let _ = std::fs::remove_file(&list_path);
    match status {
        Ok(o) if o.status.success() => Ok(picked.len()),
        Ok(o) => Err(format!(
            "ffmpeg 拼接失败（退出码 {:?}）：{}",
            o.status.code(),
            xd_core::onvif::mask_url_in_text(String::from_utf8_lossy(&o.stderr).trim())
        )),
        Err(e) => Err(format!("运行 ffmpeg 失败: {e}")),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, mtime: u64) -> (PathBuf, u64, u64) {
        (PathBuf::from(name), 1024, mtime)
    }

    #[test]
    fn cache_args_copy_without_reencode() {
        let opts = CacheOptions {
            ffmpeg: "ffmpeg".into(),
            url: "rtsp://cam/stream1".into(),
            dir: PathBuf::from("cache/tplink"),
            segment_secs: 60,
            keep_secs: 1800,
            net: NetOptions::default(),
        };
        let a = cache_args(&opts);
        assert!(a.windows(2).any(|w| w[0] == "-c" && w[1] == "copy"));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "-segment_time" && w[1] == "60"));
        assert!(a.contains(&"segment".to_string()));
        // 不重编码就不该带 hwaccel / 低延迟激进参数
        assert!(!a.contains(&"-hwaccel".to_string()));
        assert!(!a.contains(&"-fflags".to_string()));
        assert!(
            !a.contains(&"-nostdin".to_string()),
            "缓存进程必须能读 stdin，否则收不到 'q'、父进程死了也不会退出"
        );
        // 模板带 strftime 占位符与设备前缀
        let last = a.last().unwrap();
        assert!(last.ends_with("tplink_%Y%m%d-%H%M%S.mkv"), "{last}");
        assert!(last.starts_with("cache/tplink/") || last.contains("cache"));
    }

    #[test]
    fn expired_keeps_newest_two() {
        let files = vec![
            f("a.mkv", 1000),
            f("b.mkv", 1100),
            f("c.mkv", 1200),
            f("d.mkv", 1300),
        ];
        // now=1400, keep=250 → cutoff=1150，a/b 过期；但 d/c 是最新两个，永不删
        let gone = expired(&files, 1400, 250);
        assert_eq!(gone, vec![PathBuf::from("a.mkv"), PathBuf::from("b.mkv")]);
        // 只剩两个时一个都不删
        assert!(expired(&files[2..], 99_999, 1).is_empty());
    }

    #[test]
    fn segments_since_picks_window_and_never_empty() {
        let files = vec![f("a.mkv", 1000), f("b.mkv", 1100), f("c.mkv", 1200)];
        let picked = segments_since(&files, 1250, 100);
        assert_eq!(picked, vec![PathBuf::from("c.mkv")]);
        // 全都过期时退回最新一个
        let picked = segments_since(&files, 99_999, 10);
        assert_eq!(picked, vec![PathBuf::from("c.mkv")]);
        assert!(segments_since(&[], 1, 1).is_empty());
    }

    #[test]
    fn concat_list_escapes_quotes_and_uses_forward_slashes() {
        let cwd = std::env::current_dir().unwrap();
        let abs = cwd.join("cache").join("cam").join("a.mkv");
        // 注意：`/tmp/it's.mkv` 在 Windows 上是「相对路径」（缺盘符），会被补成 cwd 下的路径，
        // 所以这里用 cwd.join 造一个真正的绝对路径
        let quoted = cwd.join("it's.mkv");
        let list = concat_list(&[abs, quoted]);
        assert!(list.starts_with("ffconcat version 1.0"));
        assert!(list.contains("cache/cam/a.mkv'"), "{list}");
        assert!(list.contains("it'\\''s.mkv'"), "单引号需转义: {list}");
    }

    #[test]
    fn concat_list_makes_relative_paths_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let list = concat_list(&[PathBuf::from("cache/cam/x.mkv")]);
        let cwd_fwd = cwd.to_string_lossy().replace('\\', "/");
        assert!(
            list.contains(&format!("{cwd_fwd}/cache/cam/x.mkv")),
            "相对路径必须绝对化，否则 ffmpeg 会相对清单目录去找: {list}"
        );
    }

    #[test]
    fn zero_byte_segments_are_not_listed() {
        let dir = std::env::temp_dir().join(format!("xdv_zero_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mkv"), b"data").unwrap();
        std::fs::write(dir.join("b.mkv"), b"").unwrap(); // 强杀留下的残片
        let listed = list_segments(&dir);
        assert_eq!(listed.len(), 1, "0 字节残片不应出现在列表里");
        assert!(listed[0].0.ends_with("a.mkv"));
        assert_eq!(scan_segments(&dir).len(), 2, "扫描层要能看到它，才能被清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_dir_name() {
        // 中文属于 Unicode alphanumeric，会被保留；分隔符与空格换成下划线
        assert_eq!(sanitize_id(Path::new("cache/门 口")), "门_口");
        assert_eq!(sanitize_id(Path::new("cache/tplink")), "tplink");
        assert_eq!(sanitize_id(Path::new("")), "cam");
    }
}
