//! 小型配置／偏好文件的安全写回：同目录唯一临时文件写完并同步后替换，不先截断旧文件。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// 原子替换目标文件；父目录必须存在，内容大小由调用方限制。
///
/// 写入、同步或替换失败均保留原目标，并尽力移除临时文件。临时文件使用 create_new，
/// 不覆盖其他写者的文件；已有文件的 Windows DACL（含继承保护）或 Unix mode 会保留。
/// Windows 临时文件独占打开，复制 DACL 成功前不写入内容，失败则拒绝保存；
/// 不复制 Windows owner/SACL，目标不存在时沿用目录权限。Unix 新文件默认仅当前用户可读写。
/// 仅同步文件内容，不承诺突然掉电后目录项持久化；父目录同步和文件系统差异另行验证。
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let name = path
        .file_name()
        .ok_or("写入目标必须是文件")?
        .to_string_lossy();
    let seq = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // 禁止其他句柄在复制 DACL 前打开临时文件并在之后读取内容。
            options.share_mode(0).access_mode(0x4000_0000 | 0x0004_0000); // GENERIC_WRITE | WRITE_DAC
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        created = true;
        #[cfg(windows)]
        windows_acl::copy_dacl(path, &file)?;
        match std::fs::metadata(path) {
            Ok(metadata) => file.set_permissions(metadata.permissions())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| {
        format!(
            "保存 {} 失败：{e}；原文件保持不变，请检查目录权限或可用空间",
            path.display()
        )
    })
}

/// 标准库 Permissions 不包含 Windows ACL；独占句柄上的 DACL 必须先于内容写入。
#[cfg(windows)]
mod windows_acl {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use std::path::Path;

    const DACL: u32 = 4;
    #[link(name = "advapi32")]
    extern "system" {
        fn GetFileSecurityW(
            path: *const u16,
            info: u32,
            sd: *mut c_void,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorControl(
            sd: *const c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            sd: *const c_void,
            present: *mut i32,
            acl: *mut *mut c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            kind: u32,
            info: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
    }

    pub(super) fn copy_dacl(path: &Path, file: &File) -> io::Result<()> {
        // canonicalize 同时支持标准库接受的长路径，并拒绝不存在以外的读取失败。
        let path = match std::fs::canonicalize(path) {
            Ok(path) => path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut needed = 0;
        // SAFETY: 路径以 NUL 结束；第一次调用只查询长度，不写入空缓冲区。
        unsafe {
            if GetFileSecurityW(wide.as_ptr(), DACL, std::ptr::null_mut(), 0, &mut needed) != 0 {
                return Err(io::Error::other("读取文件 DACL 未返回安全描述符"));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(122) {
                // ERROR_INSUFFICIENT_BUFFER
                return Err(error);
            }
        }
        if needed == 0 || needed > 1024 * 1024 {
            return Err(io::Error::other("文件安全描述符大小异常，拒绝保存"));
        }
        // 使用 DWORD 对齐的有界缓冲区；ACL 并发变大时失败，不降级成目录继承权限。
        let mut descriptor = vec![0u32; (needed as usize).div_ceil(4)];
        let size = (descriptor.len() * 4) as u32;
        let sd = descriptor.as_mut_ptr().cast::<c_void>();
        let mut control = 0u16;
        let mut revision = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        // SAFETY: 缓冲区对齐、大小匹配，描述符由系统填充；file 在调用期间独占且有效。
        unsafe {
            if GetFileSecurityW(wide.as_ptr(), DACL, sd, size, &mut needed) == 0
                || GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0
                || GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) == 0
            {
                return Err(io::Error::last_os_error());
            }
            if present == 0 {
                return Err(io::Error::other("原文件缺少 DACL，拒绝隐式扩大权限"));
            }
            let protection = if control & 0x1000 != 0 {
                0x8000_0000
            } else {
                0x2000_0000
            };
            let error = SetSecurityInfo(
                file.as_raw_handle(),
                1,
                DACL | protection,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            );
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn acl_script(path: &Path, script: &str) -> String {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("XD_ATOMIC_TEST_FILE", path)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    #[cfg(windows)]
    fn atomic_write_preserves_protected_and_inherited_dacl() {
        let root = std::env::temp_dir().join(format!("xdview-acl-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for protected in [false, true] {
            let path = root.join(format!("{protected}.toml"));
            std::fs::write(&path, b"old").unwrap();
            if protected {
                acl_script(
                    &path,
                    r#"
                    $ErrorActionPreference = 'Stop'
                    $acl = [System.Security.AccessControl.FileSecurity]::new()
                    $acl.SetAccessRuleProtection($true, $false)
                    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
                    $rule = [System.Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow')
                    $acl.AddAccessRule($rule)
                    Set-Acl -LiteralPath $env:XD_ATOMIC_TEST_FILE -AclObject $acl
                "#,
                );
            }
            let script = "(Get-Acl -LiteralPath $env:XD_ATOMIC_TEST_FILE).GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::Access)";
            let before = acl_script(&path, script);
            write_atomic(&path, b"synthetic-password").unwrap();
            assert_eq!(acl_script(&path, script), before, "DACL 和继承标志必须保留");
            assert_eq!(std::fs::read(&path).unwrap(), b"synthetic-password");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn unreadable_dacl_refuses_save_without_changing_original_or_leaving_temporary() {
        use std::io::Read;
        let root = std::env::temp_dir().join(format!("xdview-acl-denied-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        std::fs::write(&path, b"original").unwrap();
        let mut original = std::fs::File::open(&path).unwrap();
        acl_script(
            &path,
            r#"
            $ErrorActionPreference = 'Stop'
            $acl = [System.Security.AccessControl.FileSecurity]::new()
            $acl.SetAccessRuleProtection($true, $false)
            $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
            $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow'))
            $ownerRights = [System.Security.Principal.SecurityIdentifier]::new('S-1-3-4')
            $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($ownerRights, 'ReadPermissions', 'Deny'))
            Set-Acl -LiteralPath $env:XD_ATOMIC_TEST_FILE -AclObject $acl
        "#,
        );
        let result = write_atomic(&path, b"replacement");
        assert!(result.is_err(), "不能读取原 DACL 时必须拒绝保存");
        let mut contents = Vec::new();
        original.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"original");
        drop(original);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaces_existing_file_without_leaving_temporary_files() {
        let root = std::env::temp_dir().join(format!("xdview-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        std::fs::write(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let blocked = root.join("directory");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_atomic(&blocked, b"data").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
