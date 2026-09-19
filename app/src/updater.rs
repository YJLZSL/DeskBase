//! 更新器：检查、下载、五步校验、自替换与回滚（见 `docs/adr/0018`）。
//!
//! 本模块分两层，是刻意的：
//!
//! · **纯逻辑与本地副作用**（版本比较、资产挑选、校验和比对、包布局检查、
//!   替换计划与回滚）—— 就在这个文件里，全部有测试。
//! · **网络传输**（取 Releases API、下载资产）—— 需要先定 HTTPS 客户端怎么选
//!   （ADR-0018 要求先做体积 PoC：WinHTTP 还是引 HTTP crate），所以**尚未实现**。
//!   没有网络传输时，检查更新会明确报"本版尚未接通网络"，
//!   **不会假装检查过了**。
//!
//! 一条贯穿的原则：**任何一步校验不过，就中止并保持程序原状态。**
//! 宁可让用户继续用旧版本，也不能把一个没验证过的 exe 换上去。

// ⚠️ 临时的死代码豁免 —— **有终止条件，接线时必须删掉这一行**。
//
// 本模块是"半接线"状态：`--apply-update` / `--version` 两个命令行模式是**真的在用**
// （替换、备份、回滚全都能跑），但下面这一半还没有调用方：
//
//   下载与校验：`Asset` / `pick_zip` / `pick_sha256` / `version_from_asset_name` /
//   `parse_sha256_file` / `sha256_file` / `verify_sha256` / `plan_managed_files` /
//   `ManagedPlan` / `plan_apply` / `same_path` / `Version::is_newer_than` / `is_prerelease`
//
// 它们要等「更新向导」接上（取 Releases API → 下载 → 五步校验 → 写 plan.json）。
//
// **终止条件**（写死在代码里）：IPC `app.updateCheck` / `app.updateDownload` 接通后，
// 删除本行 —— 一个都不许留。
//
// 已知代价：模块级豁免会**连带盖住将来新出现的死代码**。所以它必须尽快消失，
// 而不是变成常驻。之所以现在不动它：把这一半拆成子模块要跨 `impl` 边界搬代码，
// 在当前改动量下风险大于收益。这条记在 OPEN_QUESTIONS 的 Q-046 里。
#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 官方的仓库与资产命名。**写死在代码里**，不接受任何配置 ——
/// 下载源一旦可配置，它就是一个注入点（ADR-0018 第 1 条）。
pub const REPO: &str = "YJLZSL/DeskBase";
pub const ASSET_PREFIX: &str = "deskbase-";
pub const ASSET_SUFFIX: &str = "-windows-x64-portable.zip";

/// 替换时允许动的文件**白名单**。
///
/// 解压出来的目录里如果有别的东西（比如有人往里塞了个 `startup.bat`），
/// 一律不复制，而且要报出来 —— 更新器不该成为"往安装目录写任意文件"的通道。
pub const MANAGED_FILES: &[&str] = &[
    "deskbase.exe",
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "licenses/OFL-smiley-sans.txt",
    "便携版说明.txt",
];

/// 当前程序版本（编译期确定，唯一的版本事实来源）。
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION 必须是合法版本号")
}

// ---------- 地址 ----------
//
// 仓库 slug 只此一处定义（`REPO`），下面三个函数是它的三个视图。
// 以前 `main.rs` 里另有一个写死全 URL 的 `PROJECT_REPO`，两处各写一遍同一个仓库 ——
// 改一处忘一处就是"界面指向的仓库和更新器检查的仓库不是同一个"。

/// 仓库主页
pub fn repo_url() -> String {
    format!("https://github.com/{REPO}")
}

/// Releases 列表页（用户手动下载的入口）
pub fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases")
}

/// 仓库内的文档目录
pub fn docs_url() -> String {
    format!("https://github.com/{REPO}/tree/main/docs")
}

// 「最新 Release」的 API 地址（`https://api.github.com/repos/{REPO}/releases/latest`）
// **不在这里预先定义** —— 它要等网络传输落地时才有调用方，现在写出来就是死代码。
// 本项目不许用 `allow(dead_code)` 糊过去，也不留"将来会用到"的函数。

// ============================================================
// 版本号
// ============================================================

/// 只支持 `x.y.z` 与 `x.y.z-预发布` 两种形态。
///
/// 遇到别的形态（`v0.2.1` 带 v 前缀、`1.2`、`latest`）**直接拒绝**，不去猜 ——
/// 版本比较错了的后果是"该更新的不更新"或"不该更新的更新"，两个都不能接受。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `-` 之后的部分。`None` 表示正式版。
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        if s.is_empty() || s.starts_with('v') || s.starts_with('V') {
            return None;
        }
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => {
                if p.is_empty() {
                    return None;
                }
                (c, Some(p.to_string()))
            }
            None => (s, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let num = |t: &str| -> Option<u64> {
            if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            t.parse().ok()
        };
        Some(Version {
            major: num(parts[0])?,
            minor: num(parts[1])?,
            patch: num(parts[2])?,
            pre,
        })
    }

    /// 自己是否比 `other` 新。
    ///
    /// 预发布的比较规则按语义化版本：`0.3.0-beta.1 < 0.3.0`。
    /// 预发布之间只做**字符串比较** —— 我们不打算靠它排序，够判断"是不是新的"就行。
    pub fn is_newer_than(&self, other: &Version) -> bool {
        let key = |v: &Version| (v.major, v.minor, v.patch);
        if key(self) != key(other) {
            return key(self) > key(other);
        }
        match (&self.pre, &other.pre) {
            (None, None) => false,
            (None, Some(_)) => true,  // 正式版 > 预发布
            (Some(_), None) => false, // 预发布 < 正式版
            (Some(a), Some(b)) => a > b,
        }
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

// ============================================================
// 资产挑选
// ============================================================

#[derive(Debug, Clone, serde::Serialize)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

/// 我们发布的便携包名：`deskbase-<版本>-windows-x64-portable.zip`
pub fn asset_name(version: &str) -> String {
    format!("{ASSET_PREFIX}{version}{ASSET_SUFFIX}")
}

/// 从一个资产名里读出它自报的版本。名字不严格匹配就返回 `None`。
pub fn version_from_asset_name(name: &str) -> Option<Version> {
    let mid = name.strip_prefix(ASSET_PREFIX)?.strip_suffix(ASSET_SUFFIX)?;
    Version::parse(mid)
}

/// 在资产清单里挑出目标版本的便携包。
///
/// **严格匹配名字，不做"差不多就行"的匹配** —— MAA 官方自己承认过
/// "包内容主要按文件名匹配，把 arm64 改名成 x64 也可能装错"；
/// 我们至少保证"名字必须完全等于我们生成的那个"。
pub fn pick_zip<'a>(assets: &'a [Asset], target: &Version) -> Option<&'a Asset> {
    let want = asset_name(&target.to_string());
    assets.iter().find(|a| a.name == want)
}

/// 挑出配套的校验和资产：`<包名>.sha256`。
pub fn pick_sha256<'a>(assets: &'a [Asset], zip: &Asset) -> Option<&'a Asset> {
    let want = format!("{}.sha256", zip.name);
    assets.iter().find(|a| a.name == want)
}

/// 解析 `.sha256` 文件内容。
///
/// 兼容 `sha256sum` 的两种常见写法：`<64 hex>  <文件名>` 与只有哈希的裸形式。
/// 返回 `(十六进制小写, 可选文件名)`。
pub fn parse_sha256_file(text: &str) -> Option<(String, Option<String>)> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // sha256sum 在文本模式下会写成 "*文件名"
    let name = parts.next().map(|n| n.trim_start_matches('*').to_string());
    Some((hash.to_ascii_lowercase(), name))
}

// ============================================================
// 五步校验链（ADR-0018 第 1 条）
// ============================================================

/// 算一个文件的 SHA-256（小写十六进制）。
///
/// 流式读，不把整个包塞进内存 —— 包会变大，而内存目标是写死的（≤400 MB）。
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打不开文件：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("读文件失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 第 3 步：比对校验和。不一致时把**两个值都报出来**，
/// 让用户/我们能看出是"下坏了"还是"根本不是这个包"。
pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<(), String> {
    let actual = sha256_file(path)?;
    let expected = expected_hex.trim().to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "校验和不符 —— 下载到的包可能损坏或被改动。\n实际：{actual}\n应为：{expected}"
        ))
    }
}

/// 第 4 步：打开 zip，确认里面有 `deskbase.exe`。
///
/// 为什么要真打开而不是看文件名：文件名是最容易造假的东西，
/// 而这一步能挡住"内容与名字完全无关"的包。
pub fn verify_zip_layout(path: &Path) -> Result<Vec<String>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("打不开安装包：{e}"))?;
    let mut zip =
        zip::ZipArchive::new(f).map_err(|e| format!("这不是一个有效的压缩包：{e}"))?;

    let mut found: Vec<String> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| format!("读包内目录失败：{e}"))?;
        let name = entry.name().to_string();
        // 拒绝绝对路径与 .. —— 防止"解压到哪里由包说了算"
        if name.starts_with('/') || name.starts_with('\\') || name.contains("..") {
            return Err(format!("包里有不安全的路径：{name}"));
        }
        if !entry.is_dir() {
            found.push(name);
        }
    }

    if !found.iter().any(|n| n == "deskbase.exe") {
        return Err(
            "这个包里没有 deskbase.exe —— 它可能不是 DeskBase 的安装包，或者包不完整".into(),
        );
    }
    Ok(found)
}

/// 包内清单与白名单的比对结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ManagedPlan {
    /// 会被替换的文件（白名单内，且包里有）
    pub replace: Vec<String>,
    /// 包里出现、但不在白名单内 —— **不复制**，只报出来
    pub ignored: Vec<String>,
}

/// 第 4 步的另一半：算出"到底要动哪些文件"。
pub fn plan_managed_files(zip_entries: &[String]) -> ManagedPlan {
    let mut replace = Vec::new();
    let mut ignored = Vec::new();
    for name in zip_entries {
        if MANAGED_FILES.contains(&name.as_str()) {
            replace.push(name.clone());
        } else {
            ignored.push(name.clone());
        }
    }
    replace.sort();
    ignored.sort();
    ManagedPlan { replace, ignored }
}

// ============================================================
// 替换（ADR-0018 第 4、5 条）
// ============================================================

/// 备份目录名。放在安装目录里，名字带旧版本号，方便用户手动回滚。
pub fn backup_dir_name(current: &Version) -> String {
    format!("update-backup-{current}")
}

/// 替换计划 —— 先算出来给用户看，再执行。
///
/// 用户点"替换"之前必须看到：从哪个版本到哪个版本、动哪些文件、备份在哪。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyPlan {
    pub from: String,
    pub to: String,
    pub install_dir: String,
    pub staged_dir: String,
    pub replace: Vec<String>,
    pub backup_dir: String,
    /// 数据目录 —— **不在这份计划里，一个文件都不碰**（ADR-0018 第 5 条）
    pub data_dir_note: String,
}

/// 数据目录是非 ASCII 路径的常见来源，也是我们**绝不能写**的地方。
/// 检查用的大小写不敏感比较，Windows 上 `C:\A` 与 `c:\a` 是同一个地方。
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// 算替换计划，并把"不许写哪"这类底线检查放在这里。
pub fn plan_apply(
    staged_dir: &Path,
    install_dir: &Path,
    data_dir: &Path,
    zip_entries: &[String],
    current: &Version,
    target: &Version,
) -> Result<ApplyPlan, String> {
    if same_path(staged_dir, install_dir) {
        return Err("暂存目录与安装目录是同一个 —— 拒绝执行".into());
    }
    if same_path(install_dir, data_dir) || install_dir.starts_with(data_dir) {
        return Err(
            "安装目录在数据目录里 —— 拒绝执行。替换只允许动程序文件，不能碰用户数据。".into(),
        );
    }

    let managed = plan_managed_files(zip_entries);
    if !managed.replace.iter().any(|n| n == "deskbase.exe") {
        return Err("包里没有 deskbase.exe，不能替换".into());
    }

    Ok(ApplyPlan {
        from: current.to_string(),
        to: target.to_string(),
        install_dir: install_dir.to_string_lossy().to_string(),
        staged_dir: staged_dir.to_string_lossy().to_string(),
        replace: managed.replace,
        backup_dir: install_dir
            .join(backup_dir_name(current))
            .to_string_lossy()
            .to_string(),
        data_dir_note: format!(
            "数据目录 {} 不在替换范围内，一个文件都不会动",
            data_dir.to_string_lossy()
        ),
    })
}

/// 等旧进程把 exe 放开。
///
/// 用的办法很朴素：**试着以写方式打开那个 exe**。Windows 上运行中的程序映像
/// 不允许被写打开，所以"能写打开"就等于"旧进程真的退了"。
/// 这比查 PID 更贴近我们真正关心的条件 —— 我们要的不是"进程没了"，
/// 而是"这个文件现在能覆盖了"。
pub fn wait_until_writable(exe: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::fs::OpenOptions::new().write(true).open(exe).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 执行替换。**先备份，失败就回滚。**
///
/// 返回 `(备份目录, 实际替换的文件)` —— 备份目录成功时保留，供用户手动回滚。
pub fn apply_update(
    plan: &ApplyPlan,
    staged_dir: &Path,
    install_dir: &Path,
) -> Result<(PathBuf, Vec<String>), String> {
    let from = Version::parse(&plan.from).ok_or_else(|| {
        format!("计划里的当前版本号不合法：{}", plan.from)
    })?;
    let backup = install_dir.join(backup_dir_name(&from));
    std::fs::create_dir_all(&backup).map_err(|e| format!("建备份目录失败：{e}"))?;

    // ---------- ① 备份要动的文件 ----------
    let mut backed_up: Vec<String> = Vec::new();
    for rel in &plan.replace {
        let dst = install_dir.join(rel);
        if dst.exists() {
            let bak = backup.join(rel);
            if let Some(parent) = bak.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("建备份子目录失败：{e}"))?;
            }
            std::fs::copy(&dst, &bak).map_err(|e| format!("备份 {rel} 失败：{e}"))?;
            backed_up.push(rel.clone());
        }
    }

    // ---------- ② 覆盖 ----------
    let mut copied: Vec<String> = Vec::new();
    for rel in &plan.replace {
        let src = staged_dir.join(rel);
        let dst = install_dir.join(rel);
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                rollback(&backup, install_dir, &backed_up);
                return Err(format!("建目录失败（已回滚）：{e}"));
            }
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            rollback(&backup, install_dir, &backed_up);
            return Err(format!("写入 {rel} 失败（已回滚）：{e}"));
        }
        copied.push(rel.clone());
    }

    Ok((backup, copied))
}

/// 回滚：把备份里的文件盖回去。
///
/// 刻意**不返回 Result** —— 回滚失败时我们做不了更多事，
/// 但必须把失败事实留在日志里（调用方会记）。静默失败比失败更糟。
pub fn rollback(backup: &Path, install_dir: &Path, files: &[String]) {
    for rel in files {
        let bak = backup.join(rel);
        let dst = install_dir.join(rel);
        if bak.exists() {
            if let Err(e) = std::fs::copy(&bak, &dst) {
                eprintln!("回滚 {rel} 失败：{e}（备份仍在 {}）", backup.display());
            }
        }
    }
}

/// `--apply-update` 模式的入口：等旧进程退出 → 替换 → 报告。
///
/// 为什么用"同一份 exe 的另一个模式"而不是第二个 updater.exe 或 .bat：
/// 本机踩过"脚本文件 + 非 ASCII 路径 = 编码损坏"，而安装目录带中文是常态；
/// 同一份 exe 还意味着"版本对不对"在替换前后是同一个判断逻辑（ADR-0018 第 4 条）。
pub fn run_apply_update(staged_dir: &Path, install_dir: &Path) -> Result<String, String> {
    let exe = install_dir.join("deskbase.exe");
    if !wait_until_writable(&exe, std::time::Duration::from_secs(30)) {
        return Err(
            "等了 30 秒，旧版本还没退出（exe 仍被占用）。已放弃替换，程序保持原状。".into(),
        );
    }

    let zip = staged_dir.join("update.zip");
    let entries = if zip.exists() {
        verify_zip_layout(&zip)?
    } else {
        return Err(format!(
            "暂存目录里没有 update.zip：{}",
            staged_dir.display()
        ));
    };

    // 目标版本从"包内清单所在的那个名字"推不出来，改由调用方写在 plan.json 里；
    // 没有 plan.json 就明确拒绝，不做"猜一个版本"这种事
    let plan_file = staged_dir.join("plan.json");
    let plan_text = std::fs::read_to_string(&plan_file)
        .map_err(|e| format!("读不到替换计划 {}：{e}", plan_file.display()))?;
    let plan: ApplyPlan =
        serde_json::from_str(&plan_text).map_err(|e| format!("替换计划不是合法 JSON：{e}"))?;

    let (backup, copied) = apply_update(&plan, staged_dir, install_dir)?;
    Ok(format!(
        "已从 {} 更新到 {}；替换了 {} 个文件（包内共 {} 个）；备份在 {}",
        plan.from,
        plan.to,
        copied.len(),
        entries.len(),
        backup.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskbase_update_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn v(items: &[(&str, u64)]) -> Vec<Asset> {
        items
            .iter()
            .map(|(n, s)| Asset {
                id: 1,
                name: n.to_string(),
                size: *s,
            })
            .collect()
    }

    // ---------- 版本号 ----------

    #[test]
    fn 版本号解析认正式版与预发布() {
        let a = Version::parse("0.2.1").unwrap();
        assert_eq!((a.major, a.minor, a.patch), (0, 2, 1));
        assert!(a.pre.is_none() && !a.is_prerelease());
        let b = Version::parse("0.2.0-beta.3").unwrap();
        assert_eq!(b.pre.as_deref(), Some("beta.3"));
        assert!(b.is_prerelease());
    }

    #[test]
    fn 版本号形态不对就拒绝_绝不猜() {
        for bad in ["", "v0.2.1", "1.2", "1.2.3.4", "abc", "0.2.1-", "0.-1.0"] {
            assert!(Version::parse(bad).is_none(), "不该接受：{bad}");
        }
    }

    #[test]
    fn 版本比较() {
        let p = |s: &str| Version::parse(s).unwrap();
        assert!(p("0.2.1").is_newer_than(&p("0.2.0")));
        assert!(p("0.3.0").is_newer_than(&p("0.2.99")));
        assert!(p("1.0.0").is_newer_than(&p("0.9.9")));
        // 语义化版本：预发布小于同号正式版
        assert!(p("0.3.0").is_newer_than(&p("0.3.0-beta.1")));
        assert!(!p("0.3.0-beta.1").is_newer_than(&p("0.3.0")));
        // 预发布之间
        assert!(p("0.3.0-beta.2").is_newer_than(&p("0.3.0-beta.1")));
        // 相等不算更新
        assert!(!p("0.2.1").is_newer_than(&p("0.2.1")));
    }

    #[test]
    fn 版本显示能与解析互逆() {
        for s in ["0.2.1", "0.2.0-beta.3", "1.0.0"] {
            assert_eq!(Version::parse(s).unwrap().to_string(), s);
        }
    }

    // ---------- 资产挑选 ----------

    #[test]
    fn 资产名生成与反解() {
        assert_eq!(asset_name("0.2.1"), "deskbase-0.2.1-windows-x64-portable.zip");
        let got = version_from_asset_name("deskbase-0.2.0-beta.3-windows-x64-portable.zip").unwrap();
        assert_eq!(got.to_string(), "0.2.0-beta.3");
        // 别的平台 / 别的后缀一律不认
        assert!(version_from_asset_name("deskbase-0.2.1-linux-x64.zip").is_none());
        assert!(version_from_asset_name("MAA-v5.20.0-win-x64.zip").is_none());
    }

    #[test]
    fn 挑包必须严格同名() {
        let assets = v(&[
            ("deskbase-0.2.0-windows-x64-portable.zip", 100),
            ("deskbase-0.2.1-windows-x64-portable.zip", 200),
            ("deskbase-0.2.1-windows-x64-portable.zip.sha256", 64),
        ]);
        let target = Version::parse("0.2.1").unwrap();
        let zip = pick_zip(&assets, &target).expect("应当选中 0.2.1");
        assert_eq!(zip.size, 200);
        let sha = pick_sha256(&assets, zip).expect("应当找到配套校验和");
        assert_eq!(sha.size, 64);

        // 差一点点都不行 —— 这正是 MAA 那个"改名 arm64→x64 也能装"的反面
        let wrong = v(&[("deskbase-0.2.1-windows-arm64-portable.zip", 200)]);
        assert!(pick_zip(&wrong, &target).is_none());
    }

    // ---------- 校验和 ----------

    #[test]
    fn 校验和文件两种写法都认() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let (a, n) = parse_sha256_file(&format!("{h}  deskbase-0.2.1-windows-x64-portable.zip")).unwrap();
        assert_eq!(a, h);
        assert!(n.unwrap().ends_with(".zip"));
        let (b, n2) = parse_sha256_file(h).unwrap();
        assert_eq!(b, h);
        assert!(n2.is_none());
        // 长度不对 / 有非十六进制字符 → 拒绝
        assert!(parse_sha256_file("abc123").is_none());
        assert!(parse_sha256_file(&"z".repeat(64)).is_none());
        assert!(parse_sha256_file("").is_none());
    }

    #[test]
    fn 算出来的哈希与权威测试向量一致() {
        // NIST 的 "abc" 向量 —— 这条要是错了，整个校验链都是白搭
        let d = tmp("sha256");
        let f = d.join("abc.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 空文件
        let e = d.join("empty.bin");
        std::fs::write(&e, b"").unwrap();
        assert_eq!(
            sha256_file(&e).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 校验和不符时要把两个值都说出来() {
        let d = tmp("verify");
        let f = d.join("pkg.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert!(verify_sha256(&f, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").is_ok());
        let err = verify_sha256(&f, &"0".repeat(64)).unwrap_err();
        assert!(err.contains("实际") && err.contains("应为"), "要能看出是哪一种错：{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    // ---------- 包布局 ----------

    fn make_zip(path: &Path, names: &[&str]) {
        let f = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for n in names {
            w.start_file(*n, opts).unwrap();
            w.write_all(b"x").unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn 包里有可执行文件才算数() {
        let d = tmp("zip");
        let good = d.join("good.zip");
        make_zip(&good, &["deskbase.exe", "README.md", "LICENSE"]);
        let entries = verify_zip_layout(&good).unwrap();
        assert_eq!(entries.len(), 3);

        let bad = d.join("bad.zip");
        make_zip(&bad, &["README.md", "LICENSE"]);
        let err = verify_zip_layout(&bad).unwrap_err();
        assert!(err.contains("deskbase.exe"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 包里的路径穿越要被拒() {
        let d = tmp("traversal");
        let evil = d.join("evil.zip");
        make_zip(&evil, &["..\\..\\Windows\\System32\\evil.exe", "deskbase.exe"]);
        let err = verify_zip_layout(&evil).unwrap_err();
        assert!(err.contains("不安全"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 白名单外的文件不复制但要说出来() {
        let entries: Vec<String> = ["deskbase.exe", "README.md", "startup.bat", "payload.dll"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plan = plan_managed_files(&entries);
        assert!(plan.replace.contains(&"deskbase.exe".to_string()));
        assert!(plan.replace.contains(&"README.md".to_string()));
        assert!(!plan.replace.contains(&"startup.bat".to_string()));
        assert_eq!(plan.ignored, vec!["payload.dll", "startup.bat"]);
    }

    // ---------- 替换计划与底线 ----------

    #[test]
    fn 拒绝把安装目录写成暂存目录() {
        let d = tmp("plan_same");
        let err = plan_apply(
            &d,
            &d,
            &d.join("data"),
            &["deskbase.exe".to_string()],
            &Version::parse("0.2.1").unwrap(),
            &Version::parse("0.2.2").unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("同一个"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 拒绝安装目录落在数据目录里() {
        let d = tmp("plan_data");
        let data = d.join("data");
        let install = data.join("program");
        let err = plan_apply(
            &d.join("staged"),
            &install,
            &data,
            &["deskbase.exe".to_string()],
            &Version::parse("0.2.1").unwrap(),
            &Version::parse("0.2.2").unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("不能碰用户数据"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 包内没有可执行文件就不出计划() {
        let d = tmp("plan_noexe");
        let err = plan_apply(
            &d.join("staged"),
            &d.join("install"),
            &d.join("data"),
            &["README.md".to_string()],
            &Version::parse("0.2.1").unwrap(),
            &Version::parse("0.2.2").unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("deskbase.exe"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 正常情形出的计划里写明数据目录不受影响() {
        let d = tmp("plan_ok");
        let plan = plan_apply(
            &d.join("staged"),
            &d.join("install"),
            &d.join("data"),
            &["deskbase.exe".to_string(), "README.md".to_string()],
            &Version::parse("0.2.1").unwrap(),
            &Version::parse("0.2.2").unwrap(),
        )
        .unwrap();
        assert_eq!(plan.from, "0.2.1");
        assert_eq!(plan.to, "0.2.2");
        assert!(plan.backup_dir.contains("update-backup-0.2.1"));
        assert!(plan.data_dir_note.contains("一个文件都不会动"), "{}", plan.data_dir_note);
        let _ = std::fs::remove_dir_all(d);
    }

    // ---------- 真的替换一次 ----------

    #[test]
    fn 替换会把新文件覆盖上去并留下备份() {
        let d = tmp("apply");
        let install = d.join("install");
        let staged = d.join("staged");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&staged).unwrap();

        std::fs::write(install.join("deskbase.exe"), b"OLD").unwrap();
        std::fs::write(install.join("README.md"), b"OLD-README").unwrap();
        std::fs::write(staged.join("deskbase.exe"), b"NEW").unwrap();
        std::fs::write(staged.join("README.md"), b"NEW-README").unwrap();

        let plan = plan_apply(
            &staged,
            &install,
            &d.join("data"),
            &["deskbase.exe".to_string(), "README.md".to_string()],
            &Version::parse("0.2.1").unwrap(),
            &Version::parse("0.2.2").unwrap(),
        )
        .unwrap();

        let (backup, copied) = apply_update(&plan, &staged, &install).unwrap();
        assert_eq!(copied.len(), 2);
        assert_eq!(std::fs::read(install.join("deskbase.exe")).unwrap(), b"NEW");
        assert_eq!(std::fs::read(install.join("README.md")).unwrap(), b"NEW-README");
        // 备份留着，用户能手动回滚
        assert_eq!(std::fs::read(backup.join("deskbase.exe")).unwrap(), b"OLD");
        assert_eq!(std::fs::read(backup.join("README.md")).unwrap(), b"OLD-README");

        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 替换失败要能回滚回原样() {
        let d = tmp("rollback");
        let install = d.join("install");
        let staged = d.join("staged");
        let backup = d.join("backup");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&backup).unwrap();

        std::fs::write(install.join("deskbase.exe"), b"OLD").unwrap();
        std::fs::write(backup.join("deskbase.exe"), b"OLD").unwrap();
        std::fs::write(install.join("deskbase.exe"), b"BROKEN").unwrap();

        rollback(&backup, &install, &["deskbase.exe".to_string()]);
        assert_eq!(std::fs::read(install.join("deskbase.exe")).unwrap(), b"OLD");

        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 备份目录名带旧版本号方便手动回滚() {
        let v = Version::parse("0.2.1").unwrap();
        assert_eq!(backup_dir_name(&v), "update-backup-0.2.1");
        let v2 = Version::parse("0.2.0-beta.3").unwrap();
        assert_eq!(backup_dir_name(&v2), "update-backup-0.2.0-beta.3");
    }
}
