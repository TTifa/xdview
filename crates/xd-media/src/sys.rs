//! 让子进程随父进程一起结束（仅 Windows）。
//!
//! 起因：缓存/录像都是 `ffmpeg` 子进程。父进程被强杀（崩溃、任务管理器结束）
//! 时 `Drop` 不会执行，ffmpeg 会变成孤儿继续往磁盘写 —— 一个 24 小时运行的
//! 滚动缓存遇到这种情况会把盘写满。
//!
//! 办法是 Windows 的 **Job Object**：创建作业对象并设置
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，把子进程放进去。作业句柄由父进程持有，
//! 父进程无论怎么退出（正常/崩溃/被杀），句柄关闭时系统会连带杀掉作业里的进程。
//!
//! 没有第三方依赖，直接调 kernel32。共享一个进程级 Job；归属检查失败时终止并回收子进程。

use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// 按项目约定启动**生产**子进程：隐藏窗口（Windows）+ 挂到父进程的作业对象。
///
/// 所有 ffmpeg / ffprobe 入口都必须走这里。少任何一个都会留下隐患：
/// 不加 `CREATE_NO_WINDOW` 时 release 构建会闪黑框；不挂作业对象时父进程被强杀
/// （崩溃、任务管理器结束）会留下孤儿进程继续往磁盘写缓存。
pub fn spawn_managed(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    spawn_checked(cmd, |child| {
        kill_with_parent(child)?;
        #[cfg(windows)]
        resume_primary_thread(child).map_err(|e| job_failure("恢复子进程主线程", e))?;
        Ok(())
    })
}

/// 标准库不暴露 CreateProcess 返回的主线程句柄；在尚未执行的子进程中查找唯一主线程。
/// 先加入并检查 Job，恢复失败也走启动入口的 kill + wait，不能返回挂起进程。
#[cfg(windows)]
fn resume_primary_thread(child: &Child) -> std::io::Result<()> {
    use std::ffi::c_void;
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        thread_id: u32,
        process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    const _: () = assert!(std::mem::size_of::<ThreadEntry>() == 28);
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
        fn ResumeThread(thread: *mut c_void) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    unsafe {
        let raw = CreateToolhelp32Snapshot(0x0000_0004, 0); // TH32CS_SNAPTHREAD
        if raw == -1isize as *mut c_void {
            return Err(std::io::Error::last_os_error());
        }
        let snapshot = Handle(raw);
        let mut entry = ThreadEntry {
            size: std::mem::size_of::<ThreadEntry>() as u32,
            usage: 0,
            thread_id: 0,
            process_id: 0,
            base_priority: 0,
            delta_priority: 0,
            flags: 0,
        };
        let mut found = Thread32First(snapshot.0, &mut entry);
        while found != 0 {
            if entry.process_id == child.id() {
                let thread = OpenThread(0x0002, 0, entry.thread_id); // THREAD_SUSPEND_RESUME
                if thread.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let thread = Handle(thread);
                return match ResumeThread(thread.0) {
                    u32::MAX => Err(std::io::Error::last_os_error()),
                    1 => Ok(()),
                    _ => Err(std::io::Error::other("子进程主线程挂起状态异常")),
                };
            }
            entry.size = std::mem::size_of::<ThreadEntry>() as u32;
            found = Thread32Next(snapshot.0, &mut entry);
        }
        Err(std::io::Error::other("找不到子进程主线程，已取消启动"))
    }
}

fn spawn_checked(
    cmd: &mut Command,
    attach: impl FnOnce(&Child) -> std::io::Result<()>,
) -> std::io::Result<Child> {
    let mut child = cmd.spawn()?;
    if let Err(error) = attach(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

/// 等待子进程退出；超过 `timeout` 就杀掉它。
///
/// 返回 `None` 表示等待本身出错；超时时返回 kill 之后的退出状态（进程已结束）。
/// kill 之后仍要 `wait`，否则 Windows 上会留着进程句柄直到父进程退出。
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(windows)]
fn job_failure(stage: &str, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!(
        "Windows 媒体进程保护失败（{stage}）：{error}；已取消启动。请检查启动器或 IDE 的 Job 限制，尝试从资源管理器直接启动；若持续失败，请检查系统资源并重启程序"
    ))
}

/// 串行初始化，只有成功资源才进入缓存；失败后下一次调用可以重试。
#[cfg(windows)]
fn shared_job(
    owner: &std::sync::Mutex<Option<usize>>,
    create: impl FnOnce() -> std::io::Result<usize>,
) -> std::io::Result<usize> {
    let mut owner = owner
        .lock()
        .map_err(|_| std::io::Error::other("Job 初始化状态异常，请重启程序后重试"))?;
    if let Some(job) = *owner {
        return Ok(job);
    }
    let job = create()?;
    *owner = Some(job);
    Ok(job)
}

#[cfg(windows)]
fn create_job() -> std::io::Result<usize> {
    use std::ffi::c_void;

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        _pad0: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        _pad1: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    // JOBOBJECT_EXTENDED_LIMIT_INFORMATION 在 x64 上必须是 144 字节，
    // 之前少算多算都会让 SetInformationJobObject 返回 ERROR_BAD_LENGTH(24)
    const _: () = assert!(std::mem::size_of::<ExtendedLimitInformation>() == 144);
    const _: () = assert!(std::mem::size_of::<BasicLimitInformation>() == 64);

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *mut c_void,
            len: u32,
        ) -> i32;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return Err(job_failure("创建 Job", std::io::Error::last_os_error()));
        }
        let mut info = ExtendedLimitInformation {
            basic: BasicLimitInformation {
                per_process_user_time: 0,
                per_job_user_time: 0,
                limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                _pad0: 0,
                min_working_set: 0,
                max_working_set: 0,
                active_process_limit: 0,
                _pad1: 0,
                affinity: 0,
                priority_class: 0,
                scheduling_class: 0,
            },
            io: IoCounters {
                read_operation_count: 0,
                write_operation_count: 0,
                other_operation_count: 0,
                read_transfer_count: 0,
                write_transfer_count: 0,
                other_transfer_count: 0,
            },
            process_memory_limit: 0,
            job_memory_limit: 0,
            peak_process_memory_used: 0,
            peak_job_memory_used: 0,
        };
        let ok = SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            std::ptr::addr_of_mut!(info) as *mut c_void,
            std::mem::size_of::<ExtendedLimitInformation>() as u32,
        );
        if ok == 0 {
            let error = GetLastError();
            CloseHandle(job);
            return Err(job_failure(
                "设置 Job 退出回收策略",
                std::io::Error::from_raw_os_error(error as i32),
            ));
        }
        Ok(job as usize)
    }
}

#[cfg(windows)]
fn attach_job(job: usize, child: &Child) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
    }
    unsafe {
        let job = job as *mut c_void;
        if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
            return Err(job_failure("挂入 Job", std::io::Error::last_os_error()));
        }
        let mut assigned = 0;
        if IsProcessInJob(child.as_raw_handle(), job, &mut assigned) == 0 {
            return Err(job_failure(
                "检查 Job 归属",
                std::io::Error::last_os_error(),
            ));
        }
        if assigned == 0 {
            return Err(job_failure(
                "检查 Job 归属",
                std::io::Error::other("子进程未加入作业对象"),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
/// 把子进程加入进程级 Job；失败由调用者终止并回收，禁止继续无保护运行。
pub fn kill_with_parent(child: &Child) -> std::io::Result<()> {
    // 只缓存成功的句柄；操作系统在父进程退出时关闭它。
    static JOB: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);
    attach_job(shared_job(&JOB, create_job)?, child)
}

/// 导出专用进程树。Windows 在恢复前加入独立嵌套 Job，取消时只回收本次导出的后代。
/// Unix 使用独立进程组；其他平台拒绝启动，避免降级为无法取消的管道等待。
pub(crate) struct ProcessTree {
    pub(crate) child: Child,
    terminated: bool,
    #[cfg(windows)]
    job: TreeJob,
}

#[cfg(windows)]
struct TreeJob(usize);

#[cfg(windows)]
impl Drop for TreeJob {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" {
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            CloseHandle(self.0 as *mut std::ffi::c_void);
        }
    }
}

impl ProcessTree {
    pub(crate) fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let job = TreeJob(create_job()?);
            cmd.creation_flags(0x0800_0000 | 0x0000_0004); // 隐藏、挂起
            let child = spawn_checked(cmd, |child| {
                kill_with_parent(child)?;
                attach_job(job.0, child)?;
                resume_primary_thread(child).map_err(|e| job_failure("恢复导出主线程", e))
            })?;
            Ok(Self {
                child,
                terminated: false,
                job,
            })
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            Ok(Self {
                child: spawn_managed(cmd)?,
                terminated: false,
            })
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = cmd;
            Err(std::io::Error::other("当前平台不支持可取消的导出进程树"))
        }
    }

    /// 同时结束根进程和仍持有继承管道的后代；随后调用方才能等待日志线程。
    pub(crate) fn terminate(&mut self) -> std::io::Result<()> {
        if self.terminated {
            return Ok(());
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            extern "system" {
                fn TerminateJobObject(job: *mut std::ffi::c_void, code: u32) -> i32;
            }
            if unsafe { TerminateJobObject(self.job.0 as *mut std::ffi::c_void, 1) } == 0 {
                return Err(job_failure(
                    "回收导出进程树",
                    std::io::Error::last_os_error(),
                ));
            }
        }
        #[cfg(unix)]
        {
            extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // SIGKILL；根进程可能已退出，后代仍属于其进程组。
            if unsafe { kill(-(self.child.id() as i32), 9) } != 0 {
                let error = std::io::Error::last_os_error();
                // ESRCH：进程组已退出。
                if error.raw_os_error() != Some(3) {
                    return Err(error);
                }
            }
        }
        let _ = self.child.kill();
        self.child.wait()?;
        self.terminated = true;
        Ok(())
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(not(windows))]
/// 非 Windows 平台暂不支持父进程异常退出联动。
pub fn kill_with_parent(_child: &std::process::Child) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn failed_job_initialization_can_retry_and_success_is_shared() {
        let owner = std::sync::Mutex::new(None);
        assert!(shared_job(&owner, || Err(std::io::Error::from_raw_os_error(8))).is_err());
        assert_eq!(shared_job(&owner, || Ok(7)).unwrap(), 7);
        assert_eq!(
            shared_job(&owner, || panic!("成功后不能重复创建 Job")).unwrap(),
            7
        );
        let message = job_failure("挂入 Job", std::io::Error::from_raw_os_error(5)).to_string();
        assert!(message.contains("挂入 Job") && message.contains("资源管理器"));
    }
    #[test]
    fn suspended_child_cannot_write_before_job_check_completes() {
        use std::os::windows::process::CommandExt;
        let path =
            std::env::temp_dir().join(format!("xdview-before-job-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut command = Command::new("cmd.exe");
        command
            .args(["/C", &format!("echo unexpected > \"{}\"", path.display())])
            .creation_flags(0x0800_0004);
        let result = spawn_checked(&mut command, |_| {
            std::thread::sleep(Duration::from_millis(100));
            assert!(!path.exists(), "Job 检查前不应执行子进程代码");
            Err(std::io::Error::other("模拟挂入失败"))
        });
        assert!(result.is_err());
        assert!(!path.exists());
    }
    #[test]
    fn assignment_failure_returns_error_after_reaping_child() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "exit", "0"]);
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
        let mut process = std::ptr::null_mut();
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, timeout: u32) -> u32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        let result = spawn_checked(&mut command, |child| {
            assert!(!child.as_raw_handle().is_null());
            process = unsafe { OpenProcess(0x0010_0000, 0, child.id()) };
            Err(std::io::Error::other("模拟 Job 归属失败"))
        });
        assert!(result.is_err());
        assert!(!process.is_null());
        unsafe {
            assert_eq!(
                WaitForSingleObject(process, 0),
                0,
                "返回错误前必须回收子进程"
            );
            CloseHandle(process);
        }
    }
}
