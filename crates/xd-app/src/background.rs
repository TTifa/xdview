//! 媒体资源在后台创建、轮询及销毁；界面只持有可取消的代理和内存快照。
//! 代理丢弃即取消，迟到结果随独立接收端丢弃；关窗异步等待活动任务归零。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use xd_media::{CacheOptions, NetOptions};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub fn active() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

pub struct Task<T> {
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<T>,
}

impl<T: Send + 'static> Task<T> {
    pub fn start(
        name: &str,
        work: impl FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
    ) -> Result<Self, String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        let result = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                struct Completed;
                impl Drop for Completed {
                    fn drop(&mut self) {
                        ACTIVE.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _completed = Completed;
                let _ = tx.send(work(signal));
            });
        if let Err(e) = result {
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
            return Err(format!("无法启动后台任务：{e}；请释放系统资源后重试"));
        }
        Ok(Self { cancel, rx })
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn try_recv(&self) -> Result<T, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

type Work = Box<dyn FnOnce() -> Result<String, String> + Send>;

/// 配置、截图和资源回收串行执行，保证同一配置的写入顺序。
pub struct Background {
    tx: mpsc::Sender<Work>,
    rx: mpsc::Receiver<Result<String, String>>,
    /// 包括已完成但 UI 尚未消费的回执，不能仅用工作线程是否忙判定关窗。
    pending: AtomicUsize,
}

impl Background {
    pub fn new() -> Result<Self, String> {
        let (tx, jobs) = mpsc::channel::<Work>();
        let (results, rx) = mpsc::channel();
        let pending = AtomicUsize::new(0);
        // 启动阶段失败时不进入界面，此时还没有任何媒体资源需要回收。
        std::thread::Builder::new()
            .name("xdview-files".into())
            .spawn(move || {
                for job in jobs {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                        .unwrap_or_else(|_| Err("后台任务异常退出，请重试并检查日志".into()));
                    let _ = results.send(result);
                }
            })
            .map_err(|e| format!("无法启动文件与资源回收线程：{e}；请释放系统资源后重试"))?;
        Ok(Self { tx, rx, pending })
    }
    pub fn submit(&self, work: impl FnOnce() -> Result<String, String> + Send + 'static) -> bool {
        self.pending.fetch_add(1, Ordering::AcqRel);
        // worker 捕获每项任务的 panic，界面存活时接收端不会主动关闭。
        if self.tx.send(Box::new(work)).is_err() {
            self.pending.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    pub fn try_recv(&self) -> Option<Result<String, String>> {
        let result = self.rx.try_recv().ok()?;
        self.pending.fetch_sub(1, Ordering::Release);
        Some(result)
    }
}

pub struct SegmentCache {
    task: Task<Result<(), String>>,
    dir: PathBuf,
    usage: Arc<Mutex<xd_media::cache::CacheUsage>>,
    ready: Arc<AtomicBool>,
}

/// 等待旧缓存收尾时响应取消；返回的锁只能在后台任务内持有。
fn acquire_cache_owner<'a>(
    lock: &'a Mutex<()>,
    cancel: &AtomicBool,
) -> Result<Option<std::sync::MutexGuard<'a, ()>>, String> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        match lock.try_lock() {
            Ok(owner) => return Ok(Some(owner)),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("上次缓存任务异常退出，已取消启动；请重启程序后重试".into())
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl SegmentCache {
    pub fn start(opts: CacheOptions) -> Result<Self, String> {
        let dir = opts.dir.clone();
        let usage = Arc::new(Mutex::new(xd_media::cache::CacheUsage::default()));
        let snapshot = Arc::clone(&usage);
        let ready = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&ready);
        // 同目录的新旧缓存不能同时写入；等待旧代理完成收尾。
        static CACHE_LOCK: Mutex<()> = Mutex::new(());
        let task = Task::start("xdview-cache", move |cancel| {
            let Some(_owner) = acquire_cache_owner(&CACHE_LOCK, &cancel)? else {
                return Ok(());
            };
            if cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut cache = xd_media::SegmentCache::start(opts)?;
            started.store(true, Ordering::Release);
            let mut refreshed = Instant::now() - Duration::from_secs(1);
            let mut pruned = Instant::now();
            while !cancel.load(Ordering::Acquire) {
                if let Some(status) = cache.poll() {
                    return Err(format!("缓存进程退出：{status}"));
                }
                if refreshed.elapsed() >= Duration::from_secs(1) {
                    let value = cache.usage();
                    if let Ok(mut target) = snapshot.lock() {
                        *target = value;
                    }
                    refreshed = Instant::now();
                }
                if pruned.elapsed() >= Duration::from_secs(15) {
                    cache.prune();
                    pruned = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            cache.stop();
            Ok(())
        })?;
        Ok(Self {
            task,
            dir,
            usage,
            ready,
        })
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub fn usage(&self) -> xd_media::cache::CacheUsage {
        self.usage.try_lock().map(|v| *v).unwrap_or_default()
    }
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.task.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("缓存后台任务异常退出".into())),
        }
    }
    pub fn stop(self) {
        self.task.cancel();
    }
}

pub struct Recorder {
    device: String,
    task: Task<Result<PathBuf, String>>,
    started: Instant,
    stopping: bool,
}

impl Recorder {
    pub fn start(
        device: String,
        ffmpeg: &str,
        url: &str,
        out: &Path,
        low_latency: bool,
        net: NetOptions,
    ) -> Result<Self, String> {
        let (ffmpeg, url, out) = (ffmpeg.to_owned(), url.to_owned(), out.to_path_buf());
        let task = Task::start("xdview-record", move |cancel| {
            if cancel.load(Ordering::Acquire) {
                return Err("录像已取消".into());
            }
            let mut recorder = xd_media::Recorder::start(&ffmpeg, &url, &out, low_latency, net)?;
            while !cancel.load(Ordering::Acquire) {
                if let Some(status) = recorder.poll() {
                    return Err(format!("录像提前退出：{status}；请检查流地址和输出文件"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            recorder.stop()
        })?;
        Ok(Self {
            device,
            task,
            started: Instant::now(),
            stopping: false,
        })
    }
    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
    pub fn stopping(&self) -> bool {
        self.stopping
    }
    /// 主动断开或切换设备时取消；同设备切换码流/清晰度不影响录像。
    pub fn stop_unless_device(&mut self, device: Option<&str>) {
        if device != Some(self.device.as_str()) {
            self.stop();
        }
    }
    pub fn stop(&mut self) {
        self.stopping = true;
        self.task.cancel();
    }
    pub fn poll(&self) -> Option<Result<PathBuf, String>> {
        match self.task.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("录像后台任务异常退出".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_failure_stays_pending_until_its_receipt_is_consumed() {
        let background = Background::new().unwrap();
        let (release, wait) = mpsc::channel();
        let (completed, done) = mpsc::channel();
        background.submit(move || {
            wait.recv().unwrap();
            Err("模拟保存失败".into())
        });
        // 串行队列执行到下一项，说明前项的失败回执已经发送。
        background.submit(move || {
            completed.send(()).unwrap();
            Ok(String::new())
        });
        assert!(background.try_recv().is_none());
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(background.pending(), "UI 尚未处理失败，不能放行关闭");
        assert!(matches!(background.try_recv(), Some(Err(_))));
        assert!(background.pending(), "下一项回执仍未处理");
        let deadline = Instant::now() + Duration::from_secs(1);
        while background.try_recv().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!background.pending());
    }

    #[test]
    fn poisoned_cache_owner_reports_failure_instead_of_waiting_for_cancel() {
        let lock = Arc::new(Mutex::new(()));
        let poison = Arc::clone(&lock);
        assert!(std::thread::spawn(move || {
            let _owner = poison.lock().unwrap();
            panic!("模拟缓存线程失败");
        })
        .join()
        .is_err());
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Release);
        });
        let result = acquire_cache_owner(&lock, &cancel);
        timer.join().unwrap();
        assert!(result.is_err(), "锁已中毒应立即报告原因");
    }

    #[test]
    fn recorder_cancels_on_disconnect_or_other_device_but_keeps_same_device() {
        for target in [None, Some("camera-b")] {
            let (finished, observed) = mpsc::channel();
            let task = Task::start("record-owner-test", move |cancel| {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                finished.send(()).unwrap();
                Ok(PathBuf::from("synthetic.mkv"))
            })
            .unwrap();
            let mut recorder = Recorder {
                device: "camera-a".into(),
                task,
                started: Instant::now(),
                stopping: false,
            };
            recorder.stop_unless_device(Some("camera-a"));
            assert!(!recorder.stopping());
            assert!(!recorder.task.cancel.load(Ordering::Acquire));
            recorder.stop_unless_device(target);
            assert!(recorder.stopping());
            observed.recv_timeout(Duration::from_secs(1)).unwrap();
        }
    }
    #[test]
    fn dropping_proxy_cancels_without_waiting_for_worker() {
        let (started, ready) = mpsc::channel();
        let (done, stopped) = mpsc::channel();
        let task = Task::start("cancel-test", move |cancel| {
            started.send(()).unwrap();
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            done.send(()).unwrap();
        })
        .unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(task);
        stopped.recv_timeout(Duration::from_secs(1)).unwrap();
    }
    #[test]
    fn queued_writes_keep_order_and_report_failure() {
        let background = Background::new().unwrap();
        let (tx, rx) = mpsc::channel();
        for n in 0..3 {
            let tx = tx.clone();
            background.submit(move || {
                tx.send(n).unwrap();
                if n == 1 {
                    Err("模拟写入失败".into())
                } else {
                    Ok(String::new())
                }
            });
        }
        for n in 0..3 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), n);
        }
    }

    #[test]
    fn late_start_result_is_disposed_on_worker_after_receiver_is_dropped() {
        let (ready, started) = mpsc::channel();
        let (finish, wait) = mpsc::channel();
        let (disposed, observed) = mpsc::channel();
        struct Resource(mpsc::Sender<std::thread::ThreadId>);
        impl Drop for Resource {
            fn drop(&mut self) {
                let _ = self.0.send(std::thread::current().id());
            }
        }
        let task = Task::start("late-start-test", move |_| {
            ready.send(()).unwrap();
            wait.recv().unwrap();
            Resource(disposed)
        })
        .unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(task);
        finish.send(()).unwrap();
        let dropped_on = observed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(
            dropped_on,
            std::thread::current().id(),
            "迟到资源不能回到 UI 析构"
        );
    }
}
