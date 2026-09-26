//! 安装版 · 把便携版装到本机
//!
//! **为什么用 Win32 API 写注册表，而不是调 `reg.exe`**：
//! 本机的安全策略把 `reg.exe` 列进了程序黑名单，进程一起来就被杀掉（退出码 null、
//! 零输出）。而注册表本身可以用 Win32 API 直接读写 —— 命令行工具只是一个壳，
//! 绕开壳不影响能力。`windows` crate 本来就在依赖树里（webview2-com 在用），
//! 加 `Win32_System_Registry` 特性不新增任何包。
//!
//! **为什么快捷方式走 COM（`IShellLink` + `IPersistFile`），而不是自己拼 `.lnk` 二进制**：
//! `.lnk` 是带 CLSID、链路跟踪信息（LinkInfo）与 LinkTargetIDList 的复合文档格式，
//! 微软没有公开完整格式文档，手写等于赌一个会随系统版本漂移的格式；而系统自己
//! 那套实现只要几十行胶水，写出来的 `.lnk` 还天然带齐外壳要用的块（跳转列表、
//! "以管理员身份运行"等）。体积代价实测约 +1.5 KB（见 `app/Cargo.toml` 的注释）。
//!
//! **红线对齐**（CONTRIBUTING 第十节）：
//! - **便携版零注册表**：只有用户主动点「安装到本机」才会写，便携运行一个字都不写
//! - **安装版用户级安装**：全部落在 `HKCU` 与 `%LOCALAPPDATA%`，**不碰 HKLM、不要管理员**
//! - **卸载残留 = 0**：删注册表项 + 删安装目录 + 删我们那两个快捷方式；
//!   **用户的数据目录不动**（那是他的东西）
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
/// 快捷方式文件名。名字固定，卸载时才能只删"我们自己那一个"。
const LINK_FILE_NAME: &str = "DeskBase.lnk";
/// 快捷方式悬停时显示的说明
const SHORTCUT_DESC: &str = "DeskBase 桌库";
/// 写值时不指定 reserved —— 这个参数是给系统保留的，传 None 是正确用法
const NO_RESERVED: Option<u32> = None;

/// 「用户外壳文件夹」的真实位置就存在这个键里。
///
/// 为什么要读它而不是写死 `%USERPROFILE%\Desktop`：**OneDrive 会把桌面重定向**到
/// `%USERPROFILE%\OneDrive\桌面`（开始菜单也可能被重定向）。写死的话快捷方式会落到
/// 一个用户根本看不到的旧目录里，而界面上又会说"已创建桌面图标" —— 那是最糟的一种错。
const SHELL_FOLDERS_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders";

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

/// 安装完成后的结果。字段全 `snake_case`（IPC 约定，前端照读）。
///
/// 为什么 `desktop_link` / `desktop_error` 是一对 `Option`：桌面快捷方式**是可选的**，
/// 而且**建失败不算安装失败** —— 开始菜单那份已经能用了，用户找得到程序；
/// 把失败原因如实带回去让界面说清楚，而不是把整件事判成失败。
#[derive(serde::Serialize, Clone)]
pub struct InstallReport {
    pub dir: String,
    /// 开始菜单里那份（总是会建；建不出来就算安装失败）
    pub start_menu_link: String,
    /// 桌面那份；用户没勾、或者建失败时是 None
    pub desktop_link: Option<String>,
    /// 只勾了桌面却建失败时的原因（`desktop_link` 为 None 时才有值）
    pub desktop_error: Option<String>,
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

// ---------------------------------------------------------------- 外壳文件夹定位

/// 展开 `%VAR%`。抽成"注入查环境变量的函数"是为了能单测 ——
/// 直接调 `std::env::var` 的版本没法在测试里断言（并行测试改环境变量会串台）。
///
/// 注册表里存的是 `REG_EXPAND_SZ` 的**原文**（`RegQueryValueExW` 不做展开），
/// 所以要自己展开。未知变量与落单的 `%` **原样保留**：宁可路径难看，
/// 也不能把用户给的路径悄悄吃掉一段 —— 那会写出一个谁都不知道在哪的快捷方式。
fn expand_vars_with<F: Fn(&str) -> Option<String>>(raw: &str, lookup: F) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // 落单的 `%`：原样留下，剩下的字符串按普通文本拼回去
                out.push('%');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn resolve_dir(reg_value: Option<&str>, fallback: Option<PathBuf>) -> Option<PathBuf> {
    // 真实进程环境那一版；纯函数版（注入环境变量表）留给单测用
    resolve_dir_with(reg_value, fallback, |n| std::env::var(n).ok())
}

/// 「注册表值优先、环境变量兜底」这条选择本身。抽成纯函数以便单测。
fn resolve_dir_with<F: Fn(&str) -> Option<String>>(
    reg_value: Option<&str>,
    fallback: Option<PathBuf>,
    lookup: F,
) -> Option<PathBuf> {
    if let Some(v) = reg_value {
        let v = v.trim();
        if !v.is_empty() {
            let expanded = expand_vars_with(v, &lookup);
            let expanded = expanded.trim();
            if !expanded.is_empty() {
                return Some(PathBuf::from(expanded));
            }
        }
    }
    fallback.filter(|p| !p.as_os_str().is_empty())
}

/// 读「用户外壳文件夹」里某一项（`Desktop` / `Programs`）。复用已有的 `read_sz`。
fn shell_folder_value(name: &str) -> Option<String> {
    let sub_w = wide(SHELL_FOLDERS_SUBKEY);
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
    if r != ERROR_SUCCESS {
        return None;
    }
    let v = read_sz(hk, name);
    unsafe {
        let _ = RegCloseKey(hk);
    }
    v
}

fn start_menu_dir() -> Option<PathBuf> {
    let fallback = std::env::var("APPDATA")
        .ok()
        .map(|a| Path::new(&a).join("Microsoft").join("Windows").join("Start Menu").join("Programs"));
    resolve_dir(shell_folder_value("Programs").as_deref(), fallback)
}

fn desktop_dir() -> Option<PathBuf> {
    let fallback = std::env::var("USERPROFILE")
        .ok()
        .map(|u| Path::new(&u).join("Desktop"));
    resolve_dir(shell_folder_value("Desktop").as_deref(), fallback)
}

/// 开始菜单里我们的那个 `.lnk` 的完整路径
pub fn start_menu_link_path() -> Result<PathBuf, String> {
    let dir = start_menu_dir()
        .ok_or_else(|| "找不到开始菜单目录（APPDATA 未设置且注册表读不到）".to_string())?;
    Ok(dir.join(LINK_FILE_NAME))
}

/// 桌面里我们的那个 `.lnk` 的完整路径
pub fn desktop_link_path() -> Result<PathBuf, String> {
    let dir = desktop_dir()
        .ok_or_else(|| "找不到桌面目录（注册表与 USERPROFILE 都读不到）".to_string())?;
    Ok(dir.join(LINK_FILE_NAME))
}

// ---------------------------------------------------------------- 快捷方式（COM）

/// 初始化 COM，幂等。
///
/// 三种"其实没事"的情况都要当成成功，否则用户会看到一次莫名其妙的安装失败：
/// - `S_OK`：刚初始化完成
/// - `S_FALSE`：本线程早就初始化过了（重复调用/重装都会走到这里）
/// - `RPC_E_CHANGED_MODE`：本线程已经在别的套间模型里（比如 MTA）。这不是错误，
///   只是我们没拿到那次初始化计数 —— 所以**不去**配一次 `CoUninitialize`，
///   直接往下走：ShellLink 是进程内组件，照样能创建。
///
/// 反过来，我们自己初始化成功之后**也不调用 `CoUninitialize`**：
/// 这个初始化是进程级长期需要的，而且 App 的主线程可能还有别的东西（WebView2）
/// 依赖它 —— 拆掉别人的套间是比"少一次配对"严重得多的错。
fn ensure_com() -> Result<(), String> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    const S_FALSE: i32 = 1;
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.0 == 0 || hr.0 == S_FALSE || hr.0 == RPC_E_CHANGED_MODE {
        return Ok(());
    }
    Err(format!("初始化 COM 失败（HRESULT 0x{:08X}）", hr.0 as u32))
}

/// 在 `link` 位置建一个指向 `target` 的 `.lnk`（工作目录设为 `work_dir`）。
fn create_shortcut(link: &Path, target: &Path, work_dir: &Path) -> Result<(), String> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    // 父目录理论上一定存在（桌面/开始菜单），但精简系统上真可能缺 ——
    // Save 不会自己建目录，先补上比报一个看不懂的 HRESULT 强
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建快捷方式目录失败：{e}"))?;
    }

    ensure_com()?;

    let target_w = wide(&target.to_string_lossy());
    let work_w = wide(&work_dir.to_string_lossy());
    let desc_w = wide(SHORTCUT_DESC);
    let link_w = wide(&link.to_string_lossy());

    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 ShellLink 对象失败：{e}"))?;
        sl.SetPath(PCWSTR(target_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式目标失败：{e}"))?;
        sl.SetWorkingDirectory(PCWSTR(work_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式工作目录失败：{e}"))?;
        sl.SetDescription(PCWSTR(desc_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式说明失败：{e}"))?;
        // .lnk 的落盘要靠 IPersistFile::Save —— IShellLink 自己不会写文件
        let pf: IPersistFile = sl.cast().map_err(|e| format!("取 IPersistFile 失败：{e}"))?;
        pf.Save(PCWSTR(link_w.as_ptr()), true)
            .map_err(|e| format!("写快捷方式文件失败：{e}"))?;
    }
    Ok(())
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

/// 装到本机。`create_desktop_shortcut` 决定要不要额外放一份到桌面（开始菜单总是建）。
pub fn install(version: &str, create_desktop_shortcut: bool) -> Result<InstallReport, String> {
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

    // ---- 快捷方式 ----
    // 先建开始菜单那份：它是"装完之后找得到"的底线。建不出来**算安装失败**，
    // 但错误里必须说清"程序文件与卸载入口已经就位" —— 否则用户以为白装了、
    // 又不敢乱删（其实这时候从"添加/删除程序"里能正常卸掉）。
    let sm_link = match start_menu_link_path() {
        Ok(p) => p,
        Err(e) => {
            return Err(format!(
                "程序文件与卸载入口已写入 {}，但{e}",
                dir.to_string_lossy()
            ))
        }
    };
    if let Err(e) = create_shortcut(&sm_link, &dst, &dir) {
        return Err(format!(
            "程序文件与卸载入口已写入 {}，但开始菜单快捷方式创建失败：{e}",
            dir.to_string_lossy()
        ));
    }

    // 桌面那份是**可选**的，而且**建失败不算安装失败**：开始菜单那份已经能用，
    // 用户找得到程序；把原因如实带回去让界面说清楚，而不是整件事报错。
    let (desktop_link, desktop_error) = if create_desktop_shortcut {
        match desktop_link_path() {
            Err(e) => (None, Some(e)),
            Ok(p) => match create_shortcut(&p, &dst, &dir) {
                Ok(()) => (Some(p.to_string_lossy().to_string()), None),
                Err(e) => (None, Some(e)),
            },
        }
    } else {
        (None, None)
    };

    Ok(InstallReport {
        dir: dir.to_string_lossy().to_string(),
        start_menu_link: sm_link.to_string_lossy().to_string(),
        desktop_link,
        desktop_error,
    })
}

/// 卸载。删注册表项 + 删安装目录里的文件 + 删我们那两个快捷方式；
/// **数据目录一律不动**。
pub fn uninstall() -> Result<String, String> {
    let dir = install_dir();

    // 1) 先删注册表项 —— 就算后面删文件失败，"添加/删除程序"里也已经干净了
    let sub_w = wide(UNINSTALL_SUBKEY);
    let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(sub_w.as_ptr())) };
    let not_found = windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    if r != ERROR_SUCCESS && r != not_found {
        return Err(format!("删注册表项失败（错误码 {}）", r.0));
    }

    // 2) 删快捷方式。**只删我们自己那两个确切路径**，不去遍历开始菜单或桌面 ——
    //    那是用户的目录，多删一个都是事故。指向不存在文件的图标点了会报错，
    //    留着它等于给用户留一个坏掉的入口。
    let mut link_failed = 0usize;
    for p in [start_menu_link_path().ok(), desktop_link_path().ok()] {
        let Some(p) = p else { continue };
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => link_failed += 1,
        }
    }

    // 3) 删安装目录里的文件。正在运行的那个 exe 删不掉自己 ——
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
    if link_failed > 0 {
        msg.push_str(&format!("；有 {link_failed} 个快捷方式删不掉，可手动删除"));
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
    use std::collections::HashMap;

    /// 固定的"环境变量表"，替代真实进程环境 —— 测试不许依赖跑测试那台机器的
    /// 环境变量（也不许改它：cargo 的测试是并行线程，改环境变量会串台）。
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| m.get(name).cloned()
    }

    #[test]
    fn expand_vars_expands_percent_syntax_and_keeps_unknown_ones() {
        let env = fake_env(&[
            ("USERPROFILE", r"C:\Users\me"),
            ("APPDATA", r"C:\Users\me\AppData\Roaming"),
        ]);
        assert_eq!(
            expand_vars_with(r"%USERPROFILE%\Desktop", &env),
            r"C:\Users\me\Desktop"
        );
        assert_eq!(
            expand_vars_with(r"%USERPROFILE%\OneDrive\桌面", &env),
            r"C:\Users\me\OneDrive\桌面"
        );
        // 没有 %VAR% 的普通路径原样返回
        assert_eq!(expand_vars_with(r"D:\Desk", &env), r"D:\Desk");
        // 未知变量原样保留：宁可路径难看，也不能悄悄吃掉一段
        assert_eq!(expand_vars_with(r"%NOPE%\x", &env), r"%NOPE%\x");
        // 落单的 % 不是变量，不能被当成变量吃下去
        assert_eq!(expand_vars_with("100%", &env), "100%");
        assert_eq!(expand_vars_with("", &env), "");
    }

    #[test]
    fn resolve_dir_prefers_registry_value_and_falls_back() {
        let env = fake_env(&[("USERPROFILE", r"C:\Users\me")]);
        let fallback = Some(PathBuf::from(r"C:\Users\me\Desktop"));

        // 注册表里是 OneDrive 重定向后的真实位置 —— 它优先，且要展开 %USERPROFILE%
        assert_eq!(
            resolve_dir_with(
                Some(r"%USERPROFILE%\OneDrive\桌面"),
                fallback.clone(),
                &env
            ),
            Some(PathBuf::from(r"C:\Users\me\OneDrive\桌面"))
        );
        // 绝对路径的注册表值直接采用
        assert_eq!(
            resolve_dir_with(Some(r"D:\OneDriveBackup\Desktop"), fallback.clone(), &env),
            Some(PathBuf::from(r"D:\OneDriveBackup\Desktop"))
        );
        // 读不到 / 空值 / 全空白 → 用兜底
        assert_eq!(resolve_dir_with(None, fallback.clone(), &env), fallback);
        assert_eq!(resolve_dir_with(Some("   "), fallback.clone(), &env), fallback);
        // 两个都没有 → None（调用方会把它变成"快捷方式建不了"的如实说明）
        assert_eq!(resolve_dir_with(None, None, &env), None);
    }

    /// 真实环境里的两个路径解析：只断言"形状"（绝对路径 + 我们那个文件名），
    /// 不断言具体目录 —— 不同机器上桌面/开始菜单可能被 OneDrive 等重定向到别处。
    #[test]
    fn link_paths_are_absolute_and_named_after_our_app() {
        let sm = start_menu_link_path().expect("开始菜单目录应当能解析出来（注册表或 APPDATA）");
        assert!(sm.is_absolute(), "开始菜单快捷方式必须是绝对路径：{sm:?}");
        assert_eq!(sm.file_name().and_then(|s| s.to_str()), Some(LINK_FILE_NAME));

        let dt = desktop_link_path().expect("桌面目录应当能解析出来（注册表或 USERPROFILE）");
        assert!(dt.is_absolute(), "桌面快捷方式必须是绝对路径：{dt:?}");
        assert_eq!(dt.file_name().and_then(|s| s.to_str()), Some(LINK_FILE_NAME));

        // 两个位置不能是同一个文件 —— 否则卸载时删一次就少删一个入口的判断依据
        assert_ne!(sm, dt, "开始菜单与桌面的快捷方式不能落在同一个路径");
    }

    /// 测试用的"清场"守卫：无论断言在哪一步炸掉，都把测试自己建的快捷方式收掉。
    /// （这条测试的纪律是**跑完不留东西**：不留注册表项、不留开始菜单/桌面图标。）
    struct LinkCleanup(Vec<PathBuf>);

    impl LinkCleanup {
        fn new() -> Self {
            LinkCleanup(
                [start_menu_link_path().ok(), desktop_link_path().ok()]
                    .into_iter()
                    .flatten()
                    .collect(),
            )
        }
    }

    impl Drop for LinkCleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// 把 `.lnk` 读回来，看它到底指向谁。
    ///
    /// 为什么不能只断言"文件存在"：一个存在、但指向别处（或指向空）的快捷方式
    /// 在桌面上看起来一模一样，双击才报错 —— 那正是这次要防的那种坑。
    fn shortcut_target(link: &Path) -> Result<PathBuf, String> {
        use windows::core::Interface;
        use windows::Win32::System::Com::{
            CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        ensure_com()?;
        let link_w = wide(&link.to_string_lossy());
        unsafe {
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("创建 ShellLink 对象失败：{e}"))?;
            let pf: IPersistFile = sl.cast().map_err(|e| format!("取 IPersistFile 失败：{e}"))?;
            // STGM(0) = STGM_READ：只读打开，改不到这个 .lnk
            pf.Load(PCWSTR(link_w.as_ptr()), STGM(0))
                .map_err(|e| format!("读快捷方式失败：{e}"))?;
            let mut buf = vec![0u16; 1024];
            let mut fd: windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW =
                std::mem::zeroed();
            sl.GetPath(&mut buf, &mut fd, 0)
                .map_err(|e| format!("取快捷方式目标失败：{e}"))?;
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
        }
    }

    /// **真装一次、再真卸一次（含快捷方式）**。
    ///
    /// 为什么标 #[ignore]：它会真的往 `HKCU\...\Uninstall\DeskBase` 写东西、
    /// 真的往 `%LOCALAPPDATA%\Programs\DeskBase` 放文件、**真的在开始菜单与桌面上
    /// 建 .lnk**。常规测试不该改系统状态，但"安装版能不能用"这件事**只能这么验** ——
    /// 界面层的断言证明不了注册表写没写进去、快捷方式建没建出来。
    ///
    /// 安全性：只动 HKCU 下我们自己那一个键、只动我们自己那两个 .lnk 路径，
    /// 测试结束会清干净（连断言失败的情况都有 `LinkCleanup` 兜着）。
    /// 跑法：`cargo test --release -- --ignored visible_in_registry`
    #[test]
    #[ignore]
    fn visible_in_registry_after_install_gone_after_uninstall() {
        let data = std::env::temp_dir().join("dkb_inst_test_data");
        let _ = std::fs::create_dir_all(&data);

        // 起点：先确保干净，免得上次跑剩的东西让断言失真
        let _cleanup = LinkCleanup::new();
        if state(&data).installed {
            let _ = uninstall();
        }
        for p in [start_menu_link_path().ok(), desktop_link_path().ok()]
            .into_iter()
            .flatten()
        {
            let _ = std::fs::remove_file(&p);
        }

        // ---------- 第 1 段：不勾桌面 ----------
        let r1 = install("0.0.0-test", false).expect("安装应当成功");
        println!("安装目录：{}", r1.dir);
        println!("开始菜单快捷方式：{}", r1.start_menu_link);

        let after = state(&data);
        assert!(after.installed, "装完之后 state 应当报已安装");
        assert_eq!(after.version, "0.0.0-test", "版本号应当写进注册表");
        assert!(
            Path::new(&after.dir).join("DeskBase.exe").exists(),
            "程序文件应当被复制到安装目录"
        );
        assert!(
            Path::new(&r1.start_menu_link).exists(),
            "开始菜单快捷方式应当被真的建出来：{}",
            r1.start_menu_link
        );
        // 存在还不够：它得**指向安装目录里的那个 exe**，否则就是个点了报错的图标
        let installed_exe = Path::new(&after.dir).join("DeskBase.exe");
        let got = shortcut_target(Path::new(&r1.start_menu_link)).expect("应当能读回快捷方式目标");
        assert_eq!(
            got.to_string_lossy().to_lowercase(),
            installed_exe.to_string_lossy().to_lowercase(),
            "开始菜单快捷方式必须指向 {installed_exe:?}，实际指向 {got:?}"
        );
        assert!(
            r1.desktop_link.is_none() && r1.desktop_error.is_none(),
            "没勾桌面就不该动桌面，也不该报错（link={:?} err={:?}）",
            r1.desktop_link,
            r1.desktop_error
        );

        let msg1 = uninstall().expect("卸载应当成功");
        println!("卸载结果：{msg1}");
        assert!(
            !Path::new(&r1.start_menu_link).exists(),
            "卸载后开始菜单快捷方式应当被删掉：{}",
            r1.start_menu_link
        );

        // ---------- 第 2 段：勾上桌面 ----------
        let r2 = install("0.0.0-test", true).expect("安装应当成功");
        assert!(
            Path::new(&r2.start_menu_link).exists(),
            "开始菜单快捷方式应当被真的建出来：{}",
            r2.start_menu_link
        );
        match (&r2.desktop_link, &r2.desktop_error) {
            (Some(p), None) => {
                println!("桌面快捷方式：{p}");
                assert!(Path::new(p).exists(), "报告说建好了，文件就该真在：{p}");
                let got = shortcut_target(Path::new(p)).expect("应当能读回桌面快捷方式的目标");
                assert!(
                    got.to_string_lossy().to_lowercase().ends_with("deskbase.exe"),
                    "桌面快捷方式必须指向 DeskBase.exe，实际指向 {got:?}"
                );
            }
            // 按设计：桌面建失败不算安装失败，只要有原因说清楚就算如实
            (None, Some(e)) => println!("桌面快捷方式没建成（按设计不算安装失败）：{e}"),
            other => panic!("desktop_link 与 desktop_error 只该有一个是 Some：{other:?}"),
        }
        let desktop_link_r2 = r2.desktop_link.clone();

        let msg2 = uninstall().expect("卸载应当成功");
        println!("卸载结果：{msg2}");
        assert!(
            !Path::new(&r2.start_menu_link).exists(),
            "卸载后开始菜单快捷方式应当被删掉：{}",
            r2.start_menu_link
        );
        if let Some(p) = desktop_link_r2 {
            assert!(!Path::new(&p).exists(), "卸载后桌面快捷方式应当被删掉：{p}");
        }

        let final_state = state(&data);
        assert!(!final_state.installed, "卸完之后注册表项应当没了");
        let _ = std::fs::remove_dir_all(&data);
    }
}
