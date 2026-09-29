//! 构建脚本：给 exe 嵌一张真正的 PE 图标。
//!
//! ## 为什么要这个文件
//!
//! v1.11.1 排查"快捷方式没有图标"时挖出的根因比表面深一层：
//! **`deskbase.exe` 里根本没有图标资源**（`RT_ICON` / `RT_GROUP_ICON` 都是空的）。
//! 窗口与任务栏上看到的图标，是运行时用 `tao::window::Icon::from_rgba()` 设的 ——
//! **它只进内存，不进 PE 资源段**。所以：
//!
//! | 位置 | 之前 | 现在 |
//! |------|------|------|
//! | 窗口 / 任务栏（程序运行时）| ✅ 运行时设的 | ✅ |
//! | **资源管理器里看这个 exe 文件** | ❌ 白纸一张 | ✅ 有图标了 |
//! | **快捷方式的"默认图标"** | ❌ 只能靠 `SetIconLocation` 指到外部文件 | ✅ 可以直接指向 exe |
//! | 安装器 / 托盘 / 任何按文件读图标的地方 | ❌ | ✅ |
//!
//! ## 为什么不引依赖
//!
//! 常见做法是加 `winres` / `embed-resource` crate。但本项目的一贯立场是
//! **不加能不加的依赖**（便携包才 2.9 MB）。而 Windows SDK 自带 `rc.exe`，
//! 编译一个 `.rc` 就完事 —— 零依赖，代价只是几十行本文件。
//!
//! ## 找不到 rc.exe 时会怎样
//!
//! **不报错，只是没图标**。理由：图标是锦上添花，而构建失败是硬伤。
//! CI 机器、别人 clone 下来编译，SDK 位置可能不一样 —— 那种情况下
//! 应该照常出一个能跑的 exe，而不是把人卡住。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/brand/icon.ico");

    // 只管 Windows + MSVC。GNU 工具链走的是 windres，路径与参数都不同，
    // 硬凑容易把别人的构建搞坏 —— 本项目的 GNU 路线是历史记录，不再维护。
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") || !target.contains("msvc") {
        return;
    }

    let manifest = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };
    let ico = manifest.join("ui").join("brand").join("icon.ico");
    if !ico.exists() {
        println!(
            "cargo:warning=找不到 {}，exe 将不带图标（不影响功能）",
            ico.display()
        );
        return;
    }

    let rc_exe = match find_rc() {
        Some(p) => p,
        None => {
            println!(
                "cargo:warning=没找到 rc.exe（Windows SDK），exe 将不带图标。\
                 功能不受影响；若要图标，装一下 Windows SDK 即可。"
            );
            return;
        }
    };

    let out_dir = match std::env::var("OUT_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };

    // .rc 的内容就一行：把 ico 挂到 ID 1（最小的图标资源 ID）
    //
    // ⚠️ 路径里的反斜杠在 .rc 里要写成 `\\` —— 它是 C 风格的字符串字面量。
    // 而且**不能用 canonicalize()**：它返回 `\\?\C:\...` 这种长路径前缀，
    // rc.exe 不认。这里直接用 CARGO_MANIFEST_DIR 拼出来的普通路径。
    let ico_str = ico.to_string_lossy().replace('\\', "\\\\");
    let rc_path = out_dir.join("deskbase-icon.rc");
    if let Err(e) = std::fs::write(&rc_path, format!("1 ICON \"{ico_str}\"\n")) {
        println!("cargo:warning=写 .rc 失败：{e}（exe 将不带图标）");
        return;
    }

    let res_path = out_dir.join("deskbase-icon.res");
    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            // MSVC 链接器直接吃 .res
            println!("cargo:rustc-link-arg={}", res_path.display());
            println!("cargo:warning=已嵌入 exe 图标（{} 字节的 .ico）", ico_len(&ico));
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {:?}，exe 将不带图标", s.code()),
        Err(e) => println!("cargo:warning=调用 rc.exe 失败：{e}，exe 将不带图标"),
    }
}

fn ico_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// 找 Windows SDK 的 rc.exe。
///
/// 顺序：环境变量 → 本机已知的 SDK 路径 → `where` 兜底。
/// **本机的 SDK 装在 E 盘**（不是默认的 C 盘），所以"常见位置"里
/// 两个盘都要试 —— 这也是为什么不用写死路径的原因。
fn find_rc() -> Option<PathBuf> {
    // 1) 环境变量（如果外部已经配好）
    for var in ["RC", "WindowsSdkVerBinPath", "WindowsSdkBinPath"] {
        if let Ok(v) = std::env::var(var) {
            if v.trim().is_empty() {
                continue;
            }
            let p = PathBuf::from(&v);
            let cand = if p.is_dir() { p.join("x64").join("rc.exe") } else { p.clone() };
            if cand.exists() {
                return Some(cand);
            }
            if p.exists() && p.extension().map(|e| e == "exe").unwrap_or(false) {
                return Some(p);
            }
        }
    }

    // 2) 扫 SDK 的 bin 目录，挑版本号最大的那一份
    let mut roots: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin"),
        PathBuf::from(r"E:\Windows Kits\10\bin"),
        PathBuf::from(r"D:\Windows Kits\10\bin"),
    ];
    if let Ok(kits) = std::env::var("WindowsSdkDir") {
        roots.insert(0, PathBuf::from(kits).join("bin"));
    }

    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // 版本目录形如 10.0.26100.0
            let ver: Vec<u32> = name
                .split('.')
                .filter_map(|x| x.parse::<u32>().ok())
                .collect();
            if ver.len() < 2 {
                continue;
            }
            let cand = e.path().join("x64").join("rc.exe");
            if cand.exists() && best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                best = Some((ver, cand));
            }
        }
    }
    if let Some((_, p)) = best {
        return Some(p);
    }

    // 3) 实在找不到就交给 PATH
    let probe = Command::new("where").arg("rc.exe").output().ok()?;
    if probe.status.success() {
        let s = String::from_utf8_lossy(&probe.stdout);
        if let Some(first) = s.lines().next() {
            let p = PathBuf::from(first.trim());
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}
