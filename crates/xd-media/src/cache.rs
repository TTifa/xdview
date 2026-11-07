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
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// 只在选择/登记及扫描/删除时持锁，ffmpeg 运行期间不阻塞其他目录的清理。
// 规范目录路径使相对路径与目录别名指向同一份保留记录；只协调本进程内的清理。
static EXPORT_PINS: Mutex<BTreeMap<PathBuf, usize>> = Mutex::new(BTreeMap::new());

struct PinnedSegments(Vec<PathBuf>);

impl PinnedSegments {
    fn select(dir: &Path, last_secs: u64) -> Result<Self, String> {
        let dir = std::fs::canonicalize(dir)
            .map_err(|e| format!("读取导出目录失败：{e}；请检查缓存目录后重试"))?;
        let mut pins = EXPORT_PINS
            .lock()
            .map_err(|_| "缓存保留状态异常，请重启程序后重试".to_string())?;
        let picked = segments_since(&list_segments(&dir), now_secs(), last_secs);
        for path in &picked {
            *pins.entry(path.clone()).or_default() += 1;
        }
        Ok(Self(picked))
    }
}

impl Drop for PinnedSegments {
    fn drop(&mut self) {
        // 中毒时保留旧记录并停止清理，不能冒险删除仍被使用的片段。
        if let Ok(mut pins) = EXPORT_PINS.lock() {
            for path in &self.0 {
                if let Some(count) = pins.get_mut(path) {
                    *count -= 1;
                    if *count == 0 {
                        pins.remove(path);
                    }
                }
            }
        }
    }
}

fn prune_directory(dir: &Path, keep_secs: Option<u64>) -> usize {
    let Ok(dir) = std::fs::canonicalize(dir) else {
        return 0;
    };
    let Ok(pins) = EXPORT_PINS.lock() else {
        return 0;
    };
    let all = scan_segments(&dir);
    let mut candidates: Vec<PathBuf> = all
        .iter()
        .filter(|(_, size, _)| *size == 0)
        .map(|(path, _, _)| path.clone())
        .collect();
    if let Some(keep_secs) = keep_secs {
        let files: Vec<_> = all.into_iter().filter(|(_, size, _)| *size > 0).collect();
        candidates.extend(expired(&files, now_secs(), keep_secs));
    }
    candidates
        .into_iter()
        .filter(|path| !pins.contains_key(path) && std::fs::remove_file(path).is_ok())
        .count()
}

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

    /// 删掉超过保留时长的片段，跳过本进程正在导出的片段，返回删除个数。
    /// 保留状态异常时停止清理；不保护其他进程或用户手动删除的文件。
    pub fn prune(&self) -> usize {
        prune_directory(&self.opts.dir, Some(self.opts.keep_secs))
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
        prune_directory(&self.opts.dir, None);
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

/// 挑出导出用的片段：mtime 不早于 `now - last_secs`；时间窗为空时不退回历史内容。
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
    export_cancellable(
        ffmpeg,
        dir,
        last_secs,
        out,
        net,
        &std::sync::atomic::AtomicBool::new(false),
        std::time::Duration::from_secs(120),
    )
}

/// 后台导出：进程运行与日志排空共用 timeout，取消或超时回收整个导出进程树。
/// 错误输出最多保留 64 KiB；文件系统调用本身的阻塞不由此期限中断。
/// 在同目录暂存，成功才替换目标；失败或取消保留已有目标并清理暂存文件。
/// 选择片段至进程树回收期间保留源文件，完成、失败或取消均释放；仅协调本进程清理。
pub fn export_cancellable(
    ffmpeg: &str,
    dir: &Path,
    last_secs: u64,
    out: &Path,
    net: NetOptions,
    cancel: &std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
) -> Result<usize, String> {
    use std::io::Read;
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    if cancel.load(Ordering::Relaxed) {
        return Err("导出已取消".into());
    }
    let retained = PinnedSegments::select(dir, last_secs)?;
    let picked = &retained.0;
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
    static EXPORT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = EXPORT_ID.fetch_add(1, Ordering::Relaxed);
    let name = out
        .file_stem()
        .ok_or("导出目标必须是文件")?
        .to_string_lossy();
    let ext = out
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mkv");
    let staged = out.with_file_name(format!(".{name}.{}.{id}.part.{ext}", std::process::id()));
    let list_path = staged.with_extension("concat.txt");
    std::fs::write(&list_path, concat_list(picked))
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
        &staged.to_string_lossy(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    apply_net(&mut cmd, net);
    // 独立进程树仍受父进程保护，且可以单独取消包装程序及持有 stderr 的后代。
    let result = (|| {
        let started = Instant::now();
        let interrupted = || {
            if cancel.load(Ordering::Acquire) {
                Some("导出已取消".to_string())
            } else if started.elapsed() >= timeout {
                Some("导出超时，已终止进程；请缩短导出时长后重试".to_string())
            } else {
                None
            }
        };
        let mut process =
            crate::sys::ProcessTree::spawn(&mut cmd).map_err(|e| format!("启动导出失败：{e}"))?;
        let stderr = process.child.stderr.take();
        let reader = std::thread::Builder::new()
            .name("export-stderr".into())
            .spawn(move || {
                let mut retained = Vec::new();
                if let Some(mut stderr) = stderr {
                    let mut buf = [0; 4096];
                    while let Ok(n) = stderr.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let keep = n.min(65536usize.saturating_sub(retained.len()));
                        retained.extend_from_slice(&buf[..keep]);
                    }
                }
                retained
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(e) => {
                return Err(format!("启动导出日志线程失败：{e}"));
            }
        };
        let mut status = None;
        let result = loop {
            if let Some(error) = interrupted() {
                break Err(error);
            }
            if status.is_none() {
                match process.child.try_wait() {
                    Ok(value) => status = value,
                    Err(e) => break Err(format!("等待导出失败：{e}")),
                }
            }
            if let Some(status) = status {
                if reader.is_finished() {
                    break Ok(status);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // 先关闭整棵进程树的管道写端，再 join；不能只 kill 已退出的包装进程。
        process
            .terminate()
            .map_err(|e| format!("回收导出失败：{e}"))?;
        let stderr = reader
            .join()
            .map_err(|_| "导出日志线程异常退出".to_string())?;
        let status = result?;
        if let Some(error) = interrupted() {
            return Err(error);
        }
        if status.success() {
            if !std::fs::metadata(&staged)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
            {
                return Err("导出未生成有效文件，请检查缓存片段".into());
            }
            std::fs::rename(&staged, out)
                .map_err(|e| format!("保存导出失败：{e}；已有文件保持不变"))?;
            Ok(picked.len())
        } else {
            Err(format!(
                "ffmpeg 拼接失败（{status}）：{}",
                xd_core::onvif::mask_url_in_text(String::from_utf8_lossy(&stderr).trim())
            ))
        }
    })();
    let _ = std::fs::remove_file(&list_path);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
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

    fn aged_segments(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xdview-pins-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.mkv", "b.mkv", "c.mkv", "d.mkv"] {
            let path = dir.join(name);
            std::fs::write(&path, b"synthetic").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(SystemTime::now() - std::time::Duration::from_secs(60)),
                )
                .unwrap();
        }
        dir
    }

    #[test]
    fn concurrent_exports_release_only_their_own_segment_pins() {
        let dir = aged_segments("shared");
        let other = aged_segments("other");
        let first = PinnedSegments::select(&dir, 120).unwrap();
        // 目录中包含 .. 的别名仍须命中相同的保留记录。
        let alias = dir.join("..").join(dir.file_name().unwrap());
        let second = PinnedSegments::select(&alias, 120).unwrap();
        assert_eq!(prune_directory(&dir, Some(1)), 0);
        assert_eq!(prune_directory(&other, Some(1)), 2, "其他目录正常清理");
        drop(first);
        let cleanup_dir = dir.clone();
        assert_eq!(
            std::thread::spawn(move || prune_directory(&cleanup_dir, Some(1)))
                .join()
                .unwrap(),
            0
        );
        drop(second);
        assert_eq!(prune_directory(&alias, Some(1)), 2);
        assert!(!dir.join("a.mkv").exists());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn failed_export_releases_pins_and_preserves_existing_output() {
        let dir = aged_segments("failure");
        let out = dir.join("existing.txt");
        std::fs::write(&out, b"original").unwrap();
        let result = export_cancellable(
            &dir.join("missing-ffmpeg.exe").to_string_lossy(),
            &dir,
            120,
            &out,
            NetOptions::default(),
            &std::sync::atomic::AtomicBool::new(false),
            std::time::Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            5,
            "暂存清单须回收"
        );
        assert_eq!(prune_directory(&dir, Some(1)), 2, "错误返回也必须释放保留");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_does_not_fall_back_to_expired_footage() {
        assert!(segments_since(&[f("old.mkv", 100)], 10_000, 600).is_empty());
    }

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
    fn segments_since_picks_only_requested_window() {
        let files = vec![f("a.mkv", 1000), f("b.mkv", 1100), f("c.mkv", 1200)];
        let picked = segments_since(&files, 1250, 100);
        assert_eq!(picked, vec![PathBuf::from("c.mkv")]);
        // 全都过期时如实报告为空
        let picked = segments_since(&files, 99_999, 10);
        assert!(picked.is_empty());
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
