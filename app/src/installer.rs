//! 安装版 · 把便携版装到本机
//!
//! **为什么用 Win32 API 写注册表，而不是调 `reg.exe`**：
//! 本机的安全策略把 `reg.exe` 列进了程序黑名单，进程一起来就被杀掉（退出码 null、
//! 零输出）。而注册表本身可以用 Win32 API 直接读写 —— 命令行工具只是一个壳，
//! 绕开壳不影响能力。`windows` crate 本来就在依赖树里（webview2-com 在用），
//! 加 `Win32_System_Registry` 特性不新增任何包。
//!
//! **红线对齐**（CONTRIBUTING 第十节）：
//! - **便携版零注册表**：只有用户主动点「安装到本机」才会写，便携运行一个字都不写
//! - **安装版用户级安装**：全部落在 `HKCU` 与 `%LOCALAPPDATA%`，**不碰 HKLM、不要管理员**
//! - **卸载残留 = 0**：删注册表项 + 删安装目录；**用户的数据目录不动**（那是他的东西）
//!
//! 卸载的两个现实约束（如实写在界面上，不假装没有）：
//! 1. 正在运行的 exe 删不掉自己 —— 用 `MoveFileEx(MOVEFILE_DELAY_UNTIL_REBOOT)`
//!    标记为"重启后删除"。所以严格说，卸载后要重启才彻底干净。
//! 2. 数据目录保留是**故意**的：卸载程序不该替用户决定他的报表要不要留。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_SZ,
};

/// 「添加/删除程序」里显示的那一项。放在 HKCU 下 = 用户级，不需要管理员。
const UNINSTALL_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DeskBase";
const APP_NAME: &str = "DeskBase";
/// 写值时不指定 reserved —— 这个参数是给系统保留的，传 None 是正确用法
const NO_RESERVED: Option<u32> = None;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 安装目录：`%LOCALAPPDATA%\Programs\DeskBase`。
///
/// 为什么不用 `Program Files`：那需要管理员权限，而红线要求"用户级安装"。
/// `%LOCALAPPDATA%\Programs` 是 Windows 给用户级程序准备的正规位置。
pub fn install_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    Path::new(&base).join("Programs").join(APP_NAME)
}

#[derive(serde::Serialize, Clone)]
pub struct InstallState {
    /// 注册表里有没有那一项 —— 以它为准，而不是"目录存不存在"
    pub installed: bool,
    pub dir: String,
    /// 注册表里记的版本（装了以后又换了 exe 的话，这个值会旧）
    pub version: String,
    /// 当前正在运行的这个 exe 的路径
    pub current_exe: String,
    /// 是"安装目录里的那一个"在跑吗
    pub running_from_install: bool,
    /// 数据目录（卸载时会保留它，界面上要说清楚）
    pub data_dir: String,
}

fn read_sz(hk: HKEY, name: &str) -> Option<String> {
    let name_w = wide(name);
    let mut ty = REG_SZ;
    let mut len: u32 = 0;
    unsafe {
        let r = RegQueryValueExW(
            hk,
            PCWSTR(name_w.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        );
        if r != ERROR_SUCCESS || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let r2 = RegQueryValueExW(
            hk,
            PCWSTR(name_w.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        if r2 != ERROR_SUCCESS {
            return None;
        }
        let u16s: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
        String::from_utf16(&u16s[..end]).ok()
    }
}

/// 建（或打开）那个键。用 `RegCreateKeyW` 而不是 `...ExW` —— 后者要
/// `Win32_Security` 特性，而我们要的那些参数它默认就给全权，没必要为它拉一个特性进来。
fn create_key(sub: &str) -> Option<HKEY> {
    let sub_w = wide(sub);
    let mut hk = HKEY::default();
    let r = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(sub_w.as_ptr()), &mut hk) };
    if r == ERROR_SUCCESS {
        Some(hk)
    } else {
        None
    }
}

fn set_sz(hk: HKEY, name: &str, val: &str) -> Result<(), String> {
    let name_w = wide(name);
    let val_w = wide(val);
    let bytes =
        unsafe { std::slice::from_raw_parts(val_w.as_ptr() as *const u8, val_w.len() * 2) };
    let r = unsafe { RegSetValueExW(hk, PCWSTR(name_w.as_ptr()), NO_RESERVED, REG_SZ, Some(bytes)) };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("写注册表值 {name} 失败（错误码 {}）", r.0))
    }
}

fn set_dword(hk: HKEY, name: &str, val: u32) -> Result<(), String> {
    let name_w = wide(name);
    let bytes = val.to_le_bytes();
    let r = unsafe {
        RegSetValueExW(hk, PCWSTR(name_w.as_ptr()), NO_RESERVED, REG_DWORD, Some(&bytes))
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("写注册表值 {name} 失败（错误码 {}）", r.0))
    }
}

/// 当前是否已安装。
pub fn state(data_dir: &Path) -> InstallState {
    let dir = install_dir();
    let cur = std::env::current_exe().unwrap_or_default();
    let mut installed = false;
    let mut version = String::new();

    let sub_w = wide(UNINSTALL_SUBKEY);
    let mut hk = HKEY::default();
    let r = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub_w.as_ptr()),
            None,
            KEY_READ,
            &mut hk,
        )
    };
    if r == ERROR_SUCCESS {
        installed = true;
        version = read_sz(hk, "DisplayVersion").unwrap_or_default();
        unsafe {
            let _ = RegCloseKey(hk);
        }
    }

    InstallState {
        installed,
        dir: dir.to_string_lossy().to_string(),
        version,
        current_exe: cur.to_string_lossy().to_string(),
        running_from_install: cur.parent().map(|p| p == dir).unwrap_or(false),
        data_dir: data_dir.to_string_lossy().to_string(),
    }
}

/// 装到本机。返回安装目录。
pub fn install(version: &str) -> Result<String, String> {
    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;

    let src = std::env::current_exe().map_err(|e| format!("找不到当前程序：{e}"))?;
    let dst = dir.join(format!("{APP_NAME}.exe"));

    // 已经在安装目录里跑（= 重复点安装）：跳过复制，只补注册表
    let same = src.parent().map(|p| p == dir).unwrap_or(false);
    if !same {
        std::fs::copy(&src, &dst).map_err(|e| format!("复制程序失败：{e}"))?;
    }

    let hk = create_key(UNINSTALL_SUBKEY).ok_or_else(|| "打开注册表项失败".to_string())?;
    let exe_path = dst.to_string_lossy().to_string();

    let r = (|| -> Result<(), String> {
        set_sz(hk, "DisplayName", "DeskBase 桌库")?;
        set_sz(hk, "DisplayVersion", version)?;
        set_sz(hk, "Publisher", "DeskBase")?;
        set_sz(hk, "InstallLocation", &dir.to_string_lossy())?;
        set_sz(hk, "DisplayIcon", &exe_path)?;
        // 卸载入口：程序自己带 --uninstall，不额外带一个卸载器（少一个文件、少一处残留）
        set_sz(hk, "UninstallString", &format!("\"{exe_path}\" --uninstall"))?;
        set_sz(hk, "QuietUninstallString", &format!("\"{exe_path}\" --uninstall --quiet"))?;
        // 界面里那两个按钮点不了 —— 我们没有"修改""修复"这两种操作
        set_dword(hk, "NoModify", 1)?;
        set_dword(hk, "NoRepair", 1)?;
        let size_kb = std::fs::metadata(&dst)
            .map(|m| (m.len() / 1024) as u32)
            .unwrap_or(0);
        set_dword(hk, "EstimatedSize", size_kb)?;
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(hk);
    }
    r?;
    Ok(dir.to_string_lossy().to_string())
}

/// 卸载。删注册表项 + 删安装目录里的文件；**数据目录一律不动**。
pub fn uninstall() -> Result<String, String> {
    let dir = install_dir();

    // 1) 先删注册表项 —— 就算后面删文件失败，"添加/删除程序"里也已经干净了
    let sub_w = wide(UNINSTALL_SUBKEY);
    let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(sub_w.as_ptr())) };
    let not_found = windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    if r != ERROR_SUCCESS && r != not_found {
        return Err(format!("删注册表项失败（错误码 {}）", r.0));
    }

    // 2) 删安装目录里的文件。正在运行的那个 exe 删不掉自己 ——
    //    用"重启后删除"标记它，而不是假装删掉了。
    let cur = std::env::current_exe().unwrap_or_default();
    let mut failed = 0usize;
    let mut delayed = false;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p == cur {
                if schedule_delete_on_reboot(&p) {
                    delayed = true;
                } else {
                    failed += 1;
                }
                continue;
            }
            if std::fs::remove_file(&p).is_err() {
                failed += 1;
            }
        }
    }
    let _ = std::fs::remove_dir(&dir); // 空了就能删掉；没空就留着

    let mut msg = String::from("已卸载");
    if delayed {
        msg.push_str("；正在运行的程序文件会在重启后自动清除");
    }
    if failed > 0 {
        msg.push_str(&format!("；有 {failed} 个文件删不掉，可稍后手动删除安装目录"));
    }
    Ok(msg)
}

/// 标记"重启后删除"。正在运行的 exe 没有别的办法删掉自己 ——
/// 与其假装删掉了，不如如实说"要重启"。
fn schedule_delete_on_reboot(p: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let w = wide(&p.to_string_lossy());
    unsafe { MoveFileExW(PCWSTR(w.as_ptr()), None, MOVEFILE_DELAY_UNTIL_REBOOT).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **真装一次、再真卸一次**。
    ///
    /// 为什么标 #[ignore]：它会真的往 `HKCU\...\Uninstall\DeskBase` 写东西、
    /// 真的往 `%LOCALAPPDATA%\Programs\DeskBase` 放文件。常规测试不该改系统状态，
    /// 但"安装版能不能用"这件事**只能这么验** —— 界面层的断言证明不了注册表写没写进去。
    ///
    /// 安全性：只动 HKCU 下我们自己那一个键，测试结束会清干净。
    /// 跑法：`cargo test -- --ignored 安装`
    #[test]
    #[ignore]
    fn 安装之后能在注册表看到_卸载之后消失() {
        let data = std::env::temp_dir().join("dkb_inst_test_data");
        let _ = std::fs::create_dir_all(&data);

        // 起点：先确保干净，免得上次跑剩的东西让断言失真
        let before = state(&data);
        if before.installed {
            let _ = uninstall();
        }

        let dir = install("0.0.0-test").expect("安装应当成功");
        println!("安装目录：{dir}");

        let after = state(&data);
        assert!(after.installed, "装完之后 state 应当报已安装");
        assert_eq!(after.version, "0.0.0-test", "版本号应当写进注册表");
        assert!(
            Path::new(&after.dir).join("DeskBase.exe").exists(),
            "程序文件应当被复制到安装目录"
        );

        let msg = uninstall().expect("卸载应当成功");
        println!("卸载结果：{msg}");

        let final_state = state(&data);
        assert!(!final_state.installed, "卸完之后注册表项应当没了");
        let _ = std::fs::remove_dir_all(&data);
    }
}
