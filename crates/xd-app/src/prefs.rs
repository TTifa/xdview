//! 界面偏好的后台写入。
//!
//! 主题切换只改内存态，真正落盘交给一条常驻线程：`data_dir` 可能被放在响应缓慢的
//! 网络位置，UI 线程不该为一次几十字节的写入停下来等系统 I/O 超时（那会让绘制、
//! 视频刷新和云台续租一起卡住）。
//!
//! 合并策略是「只保留最后一次选择」：一个槽位存待写主题，真正写之前再取一次最新值，
//! 写入期间到达的请求自然覆盖槽位。单写者按序执行，旧请求不可能晚完成再盖掉新选择。
//! 回执带单调序号，UI 可据此丢弃过期结果，也能判断「最后一次写是否还在路上」——
//! 关窗暂缓只需要这一个信息。

use crate::theme::Theme;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// 一次落盘的结果。`seq` 对应 [`PrefsWriter::send`] 的返回值。
#[derive(Debug)]
pub struct Saved {
    pub seq: u64,
    pub theme: Theme,
    pub result: Result<(), String>,
}

struct Job {
    seq: u64,
    theme: Theme,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Option<Job>>,
    cv: Condvar,
    next_seq: AtomicU64,
    stop: AtomicBool,
}

/// 写盘动作；抽成参数是为了让合并逻辑能在不碰真实文件的情况下测试。
type WriteFn = Arc<dyn Fn(&Path, Theme) -> Result<(), String> + Send + Sync>;

/// 界面偏好的写入器。
///
/// 用法：`send()` 投递（不阻塞），每帧 `try_recv()` 取回执，退出时 `shutdown()`。
/// `Drop` 会顺带停机；线程不 join——进程退出时它自然终止，最坏留下一个
/// `ui_prefs.toml.tmp`，而加载端只读正式文件，本来就是容错的。
pub struct PrefsWriter {
    shared: Arc<Shared>,
    tx: Sender<Saved>,
    rx: Receiver<Saved>,
    /// 后台线程不可用时返回失败回执，不在调用线程写盘。
    startup_error: Option<String>,
}

impl PrefsWriter {
    pub fn spawn(root: PathBuf) -> Self {
        Self::spawn_with(root, Arc::new(|root, theme| theme.save(root)))
    }

    fn spawn_with(root: PathBuf, write: WriteFn) -> Self {
        Self::spawn_using(root, write, |task| {
            thread::Builder::new()
                .name("xdview-prefs".into())
                .spawn(task)
                .map(drop)
        })
    }

    fn spawn_using(
        root: PathBuf,
        write: WriteFn,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread_tx = tx.clone();
        let thread_write = Arc::clone(&write);
        let startup_error = spawn(Box::new(move || {
            worker(root, thread_shared, thread_tx, thread_write)
        }))
        .err()
        .map(|e| format!("无法启动偏好写入线程：{e}"));
        Self {
            shared,
            tx,
            rx,
            startup_error,
        }
    }

    /// 投递一次写入，返回本次的序号；不等待、不阻塞。
    ///
    /// 槽位里只会留下最后一次选择：连续快速切换时中间态不会被写进文件。
    pub fn send(&self, theme: Theme) -> u64 {
        let seq = self.shared.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(error) = &self.startup_error {
            let result = Err(error.clone());
            let _ = self.tx.send(Saved { seq, theme, result });
            return seq;
        }
        {
            let mut slot = lock(&self.shared.slot);
            *slot = Some(Job { seq, theme });
        }
        self.shared.cv.notify_one();
        seq
    }

    /// 取一条回执；没有就返回 `None`（每帧调用，非阻塞）。
    pub fn try_recv(&self) -> Option<Saved> {
        self.rx.try_recv().ok()
    }

    /// 停机：已在槽位里的那次写仍会完成，之后线程退出。不 join。
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cv.notify_all();
    }
}

impl Drop for PrefsWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 锁中毒时继续用内部值：写偏好不值得让 UI 线程 panic。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn worker(root: PathBuf, shared: Arc<Shared>, tx: Sender<Saved>, write: WriteFn) {
    loop {
        let job = {
            let mut slot = lock(&shared.slot);
            while slot.is_none() && !shared.stop.load(Ordering::SeqCst) {
                slot = shared.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
            match slot.take() {
                Some(job) => job,
                // 停机且没有待写：直接退出
                None => return,
            }
        };
        // 写之前再取一次：排队期间被后来的选择顶掉的中间值不必落盘
        let job = lock(&shared.slot).take().unwrap_or(job);
        let result = write(&root, job.theme);
        if tx
            .send(Saved {
                seq: job.seq,
                theme: job.theme,
                result,
            })
            .is_err()
        {
            // 接收端已经销毁（窗口关了），线程可以收工
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn unavailable_writer_reports_failure_without_running_io_on_caller() {
        let writes = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&writes);
        let writer = PrefsWriter::spawn_using(
            PathBuf::from("unused"),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            |_| Err(std::io::Error::other("线程不可用")),
        );
        let seq = writer.send(Theme::Light);
        assert_eq!(writes.load(Ordering::SeqCst), 0, "调用线程不能执行写入");
        let saved = writer.try_recv().expect("线程不可用时应返回失败回执");
        assert_eq!(saved.seq, seq);
        assert!(saved.result.is_err());
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xdview-prefs-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// 等待指定序号的回执；期间到达的其他回执一并返回。
    fn wait_for(writer: &PrefsWriter, seq: u64) -> (Option<Saved>, Vec<Saved>) {
        let mut others = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            while let Some(saved) = writer.try_recv() {
                if saved.seq == seq {
                    return (Some(saved), others);
                }
                others.push(saved);
            }
            thread::sleep(Duration::from_millis(2));
        }
        (None, others)
    }

    /// 连续快速切换只写首尾两次：第一次正在写时，后续选择被合并成最后一次。
    #[test]
    fn rapid_switches_coalesce_to_first_and_last() {
        let root = temp_root("coalesce");
        let writes = Arc::new(Mutex::new(Vec::<Theme>::new()));
        let (started_tx, started_rx) = mpsc::channel();
        // Receiver 不是 Sync，包一层锁才能进 Arc<dyn Fn + Send + Sync>
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let started_cell = Mutex::new(Some(started_tx));
        let seen = Arc::clone(&writes);
        let writer = PrefsWriter::spawn_with(
            root.clone(),
            Arc::new(move |_root: &Path, theme: Theme| {
                let first = {
                    let mut log = seen.lock().unwrap();
                    log.push(theme);
                    log.len() == 1
                };
                if first {
                    if let Some(tx) = started_cell.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    // 卡住第一次写，模拟慢盘；期间 UI 会继续投递
                    let _ = release_rx.lock().unwrap().recv();
                }
                Ok(())
            }),
        );

        assert_eq!(writer.send(Theme::Light), 1);
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("后台线程应开始第一次写");
        writer.send(Theme::Dark);
        writer.send(Theme::Light);
        let last = writer.send(Theme::Dark);
        release_tx.send(()).unwrap();

        let (done, _) = wait_for(&writer, last);
        assert!(done.is_some(), "应收到最后一次写入的回执");
        let written = writes.lock().unwrap().clone();
        assert_eq!(written, vec![Theme::Light, Theme::Dark], "只应写首尾两次");
        writer.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 写失败要回报给 UI，且不能把线程打死——下一次切换仍能写入。
    #[test]
    fn failure_is_reported_and_writer_keeps_working() {
        let root = temp_root("failure");
        let attempts = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&attempts);
        let writer = PrefsWriter::spawn_with(
            root.clone(),
            Arc::new(move |_root: &Path, _theme: Theme| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("目标目录只读".to_string())
                } else {
                    Ok(())
                }
            }),
        );

        let first = writer.send(Theme::Light);
        let (failed, _) = wait_for(&writer, first);
        let failed = failed.expect("失败也要有回执，否则关窗会白等到超时");
        assert!(failed.result.is_err());
        assert_eq!(failed.theme, Theme::Light);

        let second = writer.send(Theme::Dark);
        let (ok, _) = wait_for(&writer, second);
        let ok = ok.expect("失败之后线程应继续工作");
        assert!(ok.result.is_ok());
        writer.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 走真实 `Theme::save`：投递后文件真的落到 `ui_prefs.toml`，且能读回来。
    #[test]
    fn real_writer_persists_the_theme_file() {
        let root = temp_root("real");
        let writer = PrefsWriter::spawn(root.clone());
        let seq = writer.send(Theme::Light);
        let (saved, _) = wait_for(&writer, seq);
        assert!(saved.expect("应有回执").result.is_ok());
        assert_eq!(Theme::load(&root), Theme::Light);
        let text = std::fs::read_to_string(root.join("ui_prefs.toml")).unwrap();
        assert!(text.contains("theme = \"light\""));
        assert!(!root.join("ui_prefs.toml.tmp").exists(), "不应留下临时文件");
        writer.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 停机前已投递的写仍会落盘，之后线程退出（不再处理新请求）。
    #[test]
    fn shutdown_finishes_the_queued_write() {
        let root = temp_root("shutdown");
        let writes = Arc::new(Mutex::new(Vec::<Theme>::new()));
        let seen = Arc::clone(&writes);
        let writer = PrefsWriter::spawn_with(
            root.clone(),
            Arc::new(move |_root: &Path, theme: Theme| {
                seen.lock().unwrap().push(theme);
                Ok(())
            }),
        );
        let seq = writer.send(Theme::Light);
        writer.shutdown();
        let (done, _) = wait_for(&writer, seq);
        assert!(done.is_some(), "停机不等于丢弃已投递的写");
        assert_eq!(*writes.lock().unwrap(), vec![Theme::Light]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
