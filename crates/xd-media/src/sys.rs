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
//! 没有第三方依赖，直接调 kernel32。失败不致命：拿不到句柄就退回原来的行为。

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
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn()?;
    kill_with_parent(&child);
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
pub fn kill_with_parent(child: &std::process::Child) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

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
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            eprintln!("[job] CreateJobObjectW 失败: {}", GetLastError());
            return;
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
            eprintln!("[job] SetInformationJobObject 失败: {}", GetLastError());
            return;
        }
        // 句柄故意不关闭：父进程活着它就得活着，父进程一死系统自动关句柄并杀子进程
        let assigned = AssignProcessToJobObject(job, child.as_raw_handle());
        if assigned == 0 {
            eprintln!("[job] AssignProcessToJobObject 失败: {}", GetLastError());
        }
    }
}

#[cfg(not(windows))]
pub fn kill_with_parent(_child: &std::process::Child) {}
