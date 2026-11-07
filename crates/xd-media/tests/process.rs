//! 专用测试进程验证 Windows Job；不操作用户的应用或摄像机。
#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "专用子进程 fixture，由 job_lifecycle 单独启动"]
fn process_fixture() {
    match std::env::var("XD_JOB_FIXTURE_ROLE").as_deref() {
        Ok("nested") => unsafe {
            let outer = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            assert!(!outer.is_null());
            assert_ne!(
                AssignProcessToJobObject(outer, GetCurrentProcess()),
                0,
                "外层 Job 初始化失败：{}",
                std::io::Error::last_os_error()
            );
            let mut command = Command::new("cmd.exe");
            command
                .args(["/C", "exit", "0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child =
                xd_media::sys::spawn_managed(&mut command).expect("嵌套 Job 中仍应能受保护启动");
            assert!(child.wait().unwrap().success());
            CloseHandle(outer);
        },
        Ok("leaf") => std::thread::sleep(Duration::from_secs(30)),
        Ok("parent") => {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", "process_fixture", "--ignored"])
                .env("XD_JOB_FIXTURE_ROLE", "leaf")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = xd_media::sys::spawn_managed(&mut cmd).unwrap();
            std::fs::write(
                std::env::var_os("XD_JOB_FIXTURE_FILE").unwrap(),
                child.id().to_string(),
            )
            .unwrap();
            let _guard = ChildGuard(child);
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => {}
    }
}

#[test]
#[ignore = "需要 Windows 专用进程生命周期验证"]
fn job_lifecycle() {
    fn handles() -> u32 {
        let mut count = 0;
        unsafe {
            assert_ne!(GetProcessHandleCount(GetCurrentProcess(), &mut count), 0);
        }
        count
    }
    fn short_child() {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = xd_media::sys::spawn_managed(&mut command).unwrap();
        assert!(child.wait().unwrap().success());
    }
    short_child(); // 预热进程级 Job 和标准库初始化。
    let before = handles();
    for _ in 0..24 {
        short_child();
    }
    assert!(
        handles() <= before + 2,
        "反复启动/回收不能逐次泄漏 Job 句柄"
    );

    // 独立进程进入外层 Job，避免改变测试主进程的 Job 层级。
    let mut nested = Command::new(std::env::current_exe().unwrap());
    nested
        .args(["--exact", "process_fixture", "--ignored"])
        .env("XD_JOB_FIXTURE_ROLE", "nested")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut nested = ChildGuard(nested.spawn().unwrap());
    assert!(
        xd_media::sys::wait_timeout(&mut nested.0, Duration::from_secs(5))
            .unwrap()
            .success(),
        "支持正常嵌套 Job 的环境应能启动媒体进程"
    );

    let file = std::env::temp_dir().join(format!("xdview-job-fixture-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "process_fixture", "--ignored"])
        .env("XD_JOB_FIXTURE_ROLE", "parent")
        .env("XD_JOB_FIXTURE_FILE", &file)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // 此处故意绕过生产 Job：测试父进程必须由测试独立强杀。
    let mut parent = ChildGuard(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "专用父进程没有报告子进程");
        std::thread::sleep(Duration::from_millis(20));
    };
    let process = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    assert!(!process.is_null());
    parent.0.kill().unwrap();
    parent.0.wait().unwrap();
    let result = unsafe { WaitForSingleObject(process, 5000) };
    unsafe {
        CloseHandle(process);
    }
    assert_eq!(result, 0, "强杀专用父进程后，其 Job 内子进程必须结束");
    std::fs::remove_file(file).unwrap();
}
