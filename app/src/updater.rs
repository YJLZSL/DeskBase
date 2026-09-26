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

// 本模块曾经有一处临时的 `#![allow(dead_code)]`（下载/校验那一半还没接线时）。
// **2026-09-19 已删除**：IPC `app.updateDownload` / `app.updateApply` 接通后，
// 全部符号都有了真实调用方，release 构建零警告 —— 终止条件达成，一个都不留。
// 当时刻意没用 allow 盖住问题，而是把它当成**待还的债**记进 OPEN_QUESTIONS Q-046，
// 这条债现在结清了。顺带删掉两个确实多余的辅助函数（`version_from_asset_name` / `is_prerelease`）：
// 严格同名匹配已经覆盖了它们的用途，留着就只是死代码。

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

// 曾经有一个 `version_from_asset_name()`（从资产名反解版本）。**已删** ——
// 它是多余的：`pick_zip()` 是拿"发布标签的版本"去拼出期望的名字再**要求完全相等**，
// 所以"资产名里的版本 == 标签版本"这件事已经被它保证了。
// 留着一个只有测试在用的函数就是死代码 —— 本项目不许用 `allow(dead_code)` 盖住它。

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
// 发布清单（Releases API 的响应）
// ============================================================
//
// ⚠️ 为什么不是 `/releases/latest`：那个接口的语义是"最近一个**非预发布**的 Release"。
// 本仓库迄今四个发布全部是 `prerelease: true`，实测它**一律返回 404** ——
// 照原设计实现的话，每个用户点"检查更新"都会看到 404，而且看起来像网络问题。
// 详见 ADR-0018 的「补充（2026-09-19 实测修正）」。

/// 通道。**默认稳定通道** —— 不能默认把人带上 beta。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Channel {
    Stable,
    Prerelease,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ApiAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    /// GitHub 会给 `uploaded`；不是这个值说明资产还没传完
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Release {
    #[serde(rename = "tag_name")]
    pub tag: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ApiAsset>,
}

impl Release {
    /// 从 tag 解析版本。tag 形如 `v0.2.0-beta.3`，**允许带前缀 v**（打标签的习惯如此），
    /// 但 `Version::parse` 本身拒绝 v —— 所以这里单独剥一次，不放松 parse 的规则。
    pub fn version(&self) -> Option<Version> {
        Version::parse(self.tag.strip_prefix('v').unwrap_or(&self.tag))
    }

    /// 资产里可用的那些（`state` 是 uploaded 或字段缺失时按可用处理）
    pub fn usable_assets(&self) -> Vec<Asset> {
        self.assets
            .iter()
            .filter(|a| a.state.is_empty() || a.state == "uploaded")
            .map(|a| Asset {
                id: a.id,
                name: a.name.clone(),
                size: a.size,
            })
            .collect()
    }
}

/// 检查的结果。**刻意做成三个分支而不是一个 Option** ——
/// "没有更新"和"只有测试版"是两件必须分开告诉用户的事，
/// 用一个 `None` 糊过去，界面就只能说"已是最新"，而那是**假话**。
#[derive(Debug, Clone, serde::Serialize)]
pub enum CheckOutcome {
    /// 已经是最新（或没有可比当前更新的）
    UpToDate,
    /// 有更新
    Newer {
        tag: String,
        version: String,
        /// 这个版本是不是预发布
        prerelease: bool,
    },
    /// 稳定通道下：比当前新的只有预发布
    OnlyPrerelease { tag: String, version: String },
}

/// 解析 Releases API 的响应。响应的顶层是一个数组。
pub fn parse_releases(json: &str) -> Result<Vec<Release>, String> {
    serde_json::from_str::<Vec<Release>>(json)
        .map_err(|e| format!("发布清单解析失败（GitHub 的响应格式变了？）：{e}"))
}

/// 在清单里挑出"该装的那个"。
///
/// 规则（ADR-0018 补充）：
/// 1. 丢掉 `draft`（草稿不该被任何人拿到）
/// 2. 按通道过滤：稳定通道只看 `prerelease = false`
/// 3. 只留比当前版本新的
/// 4. 取版本号最高的那个
pub fn check(releases: &[Release], current: &Version, channel: Channel) -> CheckOutcome {
    let parsed: Vec<(&Release, Version)> = releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| r.version().map(|v| (r, v)))
        .collect();

    // 稳定通道下，比当前新的预发布也要单独记下来 —— 用于"只有测试版"这句提示
    let stable_newer = parsed
        .iter()
        .filter(|(r, v)| !r.prerelease && v.is_newer_than(current))
        .map(|(r, _)| *r)
        .collect::<Vec<_>>();

    if channel == Channel::Stable {
        if let Some(best) = max_by_version(&stable_newer) {
            return CheckOutcome::Newer {
                tag: best.tag.clone(),
                version: best.version().map(|v| v.to_string()).unwrap_or_default(),
                prerelease: false,
            };
        }
        // 稳定通道没得升 —— 看看是不是只有预发布
        let pre_newer = parsed
            .iter()
            .filter(|(r, v)| r.prerelease && v.is_newer_than(current))
            .map(|(r, _)| *r)
            .collect::<Vec<_>>();
        if let Some(best) = max_by_version(&pre_newer) {
            return CheckOutcome::OnlyPrerelease {
                tag: best.tag.clone(),
                version: best.version().map(|v| v.to_string()).unwrap_or_default(),
            };
        }
        return CheckOutcome::UpToDate;
    }

    // 测试通道：预发布也看
    let any_newer = parsed
        .iter()
        .filter(|(_, v)| v.is_newer_than(current))
        .map(|(r, _)| *r)
        .collect::<Vec<_>>();
    match max_by_version(&any_newer) {
        Some(best) => CheckOutcome::Newer {
            tag: best.tag.clone(),
            version: best.version().map(|v| v.to_string()).unwrap_or_default(),
            prerelease: best.prerelease,
        },
        None => CheckOutcome::UpToDate,
    }
}

fn max_by_version<'a>(items: &[&'a Release]) -> Option<&'a Release> {
    let mut best: Option<&'a Release> = None;
    for r in items {
        let rv = match r.version() {
            Some(v) => v,
            None => continue,
        };
        match best {
            None => best = Some(r),
            Some(b) => {
                if let Some(bv) = b.version() {
                    if rv.is_newer_than(&bv) {
                        best = Some(r);
                    }
                }
            }
        }
    }
    best
}

// ============================================================
// 网络传输（HTTPS，走系统 WinHTTP）
// ============================================================
//
// **为什么是 WinHTTP 而不是引一个 HTTP crate**：HTTPS 必然要 TLS，
// 引纯 Rust 的 rustls 会拉进上 MB；WinHTTP 用系统 schannel，
// 体积代价只落在"我们自己的胶水代码"上（winhttp.dll 是系统 DLL，只有导入表）。
//
// 实测（2026-09-19，`windows` 0.62 开 `Win32_Networking_WinHttp` 特性）：
// **不新增任何包**（Cargo.lock 无变化 —— `windows` 本来就在依赖树里，webview2-com 在用），
// **体积不变**（5.985 MB → 5.985 MB，差 512 字节是布局噪声）。
// 这一步只证明"声明是免费的"；真正的代价在调用处，见实现后的复测。
//
// 代理：用 `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` —— 它读系统代理设置，
// 所以本机开着 Clash（7890）时它能自己走对路，不需要我们再实现一遍代理逻辑。

/// 「最新发布」的取数地址。
///
/// ⚠️ **不是 `/releases/latest`** —— 那个接口的语义是"最近一个**非预发布**的 Release"，
/// 而本仓库迄今所有发布都是 alpha/beta，实测它**一律 404**。详见 ADR-0018 的补充。
pub fn releases_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases?per_page=20")
}

/// 单次请求的超时（毫秒）。检查更新是"顺手做一下"的事，**不该让人等**。
pub const HTTP_TIMEOUT_MS: i32 = 15_000;

/// 响应体上限。清单本身只有几十 KB；给 8 MB 是防御性的天花板 ——
/// 没有上限的话，一个被投毒的响应就能把内存吃光。
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[cfg(target_os = "windows")]
pub mod http {
    use std::ffi::c_void;

    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
        WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
        WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
        WINHTTP_OPTION_REDIRECT_POLICY,
        WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS, WINHTTP_QUERY_FLAG_NUMBER,
        WINHTTP_QUERY_STATUS_CODE,
    };

    /// 转成给 `PCWSTR` 参数用的宽字符串 —— **必须带结尾 NUL**。
    ///
    /// ⚠️ 这里踩过一次坑：原先不带 NUL，`WinHttpOpenRequest` 直接返回
    /// `ERROR_INVALID_PARAMETER(87)`。原因是 `PCWSTR` 按 **NUL 结尾**读，
    /// 不带 NUL 的缓冲区会让 WinHTTP 读到缓冲区之外的内容。
    /// 报错是 87 而不是崩溃，属于运气好 —— 换成别的 API 可能就是读越界。
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 给 `WinHttpSendRequest` 用的头 —— **不能带 NUL**。
    ///
    /// 与上面相反：windows-rs 把 `slice.len()` 当作字符数传给 `dwHeadersLength`，
    /// 多一个 NUL 会被当成头的最后一个字符。
    fn wide_len(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// 最近一次 Win32 错误码。
    ///
    /// **必须带上它**：WinHTTP 的函数失败时只返回 NULL，不给原因。
    /// 上一次就是因为在"构造请求失败"里没带错误码，只能靠猜 —— 那是最浪费时间的一种调试。
    fn last_error() -> u32 {
        unsafe { windows::Win32::Foundation::GetLastError().0 }
    }

    /// 把常见的 WinHTTP 错误码翻成人话。翻不出来的就原样给出码值，别假装知道。
    ///
    /// ⚠️ 2026-09-19 逐条对着 `windows` crate 的常量表核了一遍 —— 之前**三条标错了**
    /// （12006/12007/12029），其中 12029 被错标成"TLS 握手失败"，而它其实是
    /// "连不上服务器"。实测里的 `0x80072EFD`（低 16 位 = 12029）因此看起来像证书
    /// 问题，把排查方向整个带偏。**错误码翻译错了比不翻译更坏**。
    pub(super) fn explain(code: u32) -> String {
        match code {
            12001 => "没有足够的句柄（资源耗尽）".to_string(),
            12002 => "请求超时".to_string(),
            12005 => "地址无效".to_string(),
            12006 => "协议不认识（更新器只支持 https）".to_string(),
            12007 => "无法解析主机名（DNS 查不到）".to_string(),
            12009 => "选项无效".to_string(),
            12017 => "操作被取消".to_string(),
            12029 => "连不上服务器（连接被拒或超时）".to_string(),
            12030 => "连接被重置或中断".to_string(),
            12037 => "证书过期或无效".to_string(),
            // 12152 = ERROR_WINHTTP_INVALID_SERVER_RESPONSE。
            //
            // 为什么值得单列一条：AI 对话走本机模型时，**最常见的失败就是它** ——
            // 用户没启动 Ollama / LM Studio，或者模型还在加载。而端口上什么都没有时，
            // WinHTTP 报的不是"连不上"（12029）而是这一条。落回默认文案的话，
            // 用户看到的是"Win32 错误码 12152"，他没法从这串数字想到"我模型没开"。
            12152 => "服务器返回了无法识别的应答（本机模型常见：模型服务没启动，或还在加载中）"
                .to_string(),
            12175 => "安全通道错误：证书校验没通过（可能是网络中间人，或系统时间不对）"
                .to_string(),
            other => format!("Win32 错误码 {other}"),
        }
    }

    /// RAII：句柄离开作用域就关掉。WinHTTP 的每个句柄都要显式关闭，
    /// 中途 return 的每条路径都不能漏 —— 交给 Drop 比手写 close 可靠。
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // 关闭失败也没别的办法，忽略返回值；但**不能**因此跳过关闭
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    /// 两条接入方式与它们的出场顺序。
    ///
    /// 顺序的道理：**先尊重用户的系统代理**（企业网络里那可能是唯一通路），
    /// 代理走不通再用直连兜底 —— 反过来就等于默认绕过用户有意的代理设置。
    ///
    /// 为什么必须有第二条（2026-09-19 实测）：系统代理（本机 FlClash:7890）进入
    /// "端口还在监听、但不再应答"的状态后，检查更新 **5/5 全失败**（0x80072EFD
    /// = 12029 连不上）；而**同一时刻直连 api.github.com 完全正常**。
    /// 用户什么都没改，更新却一半概率不可用 —— 这不是能留给用户
    /// 自己去发现并绕开的问题。
    ///
    /// 哪条路失败过就在本次会话里靠后放：挂掉的代理几乎不会自愈，而它的代价是
    /// 让每个请求先白等一个超时（下载的超时是 120 秒 —— 让用户干等两分钟
    /// 才看到下载开始，是不能接受的）。这个开关只活在内存里，重启即清。
    static DIRECT_FIRST: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    fn routes() -> [(u32, &'static str, bool); 2] {
        use std::sync::atomic::Ordering;
        order_routes(DIRECT_FIRST.load(Ordering::Relaxed))
    }

    /// 顺序的纯函数版本 —— 不碰全局状态，测试能直接钉住它。
    pub(super) fn order_routes(direct_first: bool) -> [(u32, &'static str, bool); 2] {
        let auto = (WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY.0, "系统代理", false);
        let direct = (WINHTTP_ACCESS_TYPE_NO_PROXY.0, "直连", true);
        if direct_first {
            [direct, auto]
        } else {
            [auto, direct]
        }
    }

    /// 兜底成功之后：记住哪条路通了，并把过程写进日志。
    fn note_fallback(attempt: usize, problems: &[String], label: &str, is_direct: bool) {
        if attempt == 0 {
            return;
        }
        DIRECT_FIRST.store(is_direct, std::sync::atomic::Ordering::Relaxed);
        eprintln!("更新器：{}；已换另一条路（{label}）成功", problems.join("；"));
    }

    /// 两条路都走完后给用户的交代。**必须逐条列出失败原因** ——
    /// 一句"更新失败"对着急的用户等于没说。
    fn routes_failed(problems: &[String]) -> String {
        format!(
            "系统代理与直连都试过了，没有一条能走通 —— {}。\
             这类失败通常是网络环境的问题（代理软件没起作用 / 出口被限制），不是程序坏了；\
             如果开着代理软件，可以试试重启它。",
            problems.join("；")
        )
    }

    /// HRESULT（形如 0x80072EFD）→ 人话。**低 16 位才是 WinHTTP 的错误码**
    /// （0x80072EFD & 0xFFFF = 0x2EFD = 12029 连不上）。
    fn hr_explain(e: &windows::core::Error) -> String {
        let hr = e.code().0 as u32;
        format!("{}（0x{hr:08X}）", explain(hr & 0xFFFF))
    }

    /// 一次尝试的失败 —— **分类决定要不要换一条接入方式再试**。
    enum Fail {
        /// 没拿到服务器应答（连不上 / 超时 / 断流 / TLS 失败）。换一条路值得一试。
        Transport(String),
        /// 服务器明确拒绝，但**换一个出口 IP 可能就不一样**（403 / 429）：
        /// GitHub 的匿名限额按 IP 算，而代理出口是共享 IP —— 很容易撞上。
        RetryHttp(String),
        /// 其他失败：服务器已给出明确应答（404 等），或问题在本地
        /// （地址非法 / 写文件失败 / 超体积上限）。换路不会改变结果。
        Settled(String),
    }

    /// host 是不是"就在本机"。
    ///
    /// **只认这几种写法**，多认一种就等于给公网开一道门。大小写不敏感
    /// （`LocalHost` 与 `localhost` 在 DNS 上是同一个名字）。
    ///
    /// ⚠️ 已知的、可接受的边界：`localhost` 的解析交给系统（hosts 文件理论上
    /// 可以被指向别处）。能改本机 hosts 的已经拿到了本机管理权 —— 那不是
    /// 这个闸门要拦的东西；闸门要拦的是"用户在设置里填了一个公网地址、
    /// 明文把自己的数据发出去"。
    pub(super) fn is_loopback_host(host: &str) -> bool {
        let h = host
            .trim()
            .trim_matches(|c| c == '[' || c == ']')
            .to_ascii_lowercase();
        // `::1` 的完整写法是 `0:0:0:0:0:0:0:1`，是同一个地址，一并认下
        h == "localhost" || h == "127.0.0.1" || h == "::1" || h == "0:0:0:0:0:0:0:1"
    }

    /// 判定这个地址允不允许访问，并解析出 `(要不要 TLS, host, port, path)`。
    ///
    /// **为什么明文只给回环开**（这是隐私取舍，不是便利取舍）：
    /// DeskBase 的 AI 默认走本机推理（ADR-0007 / ADR-0017「默认本地」），
    /// 本机服务商的默认地址就是 `http://127.0.0.1:11434/v1`（Ollama）与
    /// `http://127.0.0.1:1234/v1`（LM Studio）—— 明文 HTTP + 非 443 端口。
    /// 只认 https 会让"本机跑模型"这条默认路径直接报一句"只允许 https 地址"，
    /// 而用户会以为是模型没启动 —— 一个极其费时间的误判。
    ///
    /// 但**不是顺手放宽**：到回环地址的明文流量不会离开这台机器，与"数据不出本机"
    /// 的承诺不冲突；放开任意主机（含内网 `192.168.x.x`，明文一样离开这台机器）的
    /// http，等于允许用户把自己的数据明文发到网上。所以这里只有一条口子：
    /// **回环 + http**。外部服务继续一律 https。
    ///
    /// 抽成纯函数是为了可测：它是唯一闸门，而闸门错了不会有任何报错 ——
    /// 只会安静地放行（或安静地拒绝，让人查半天）。
    pub(super) fn url_allowed(url: &str) -> Result<(bool, String, u16, String), String> {
        // 空白与 CR/LF 一律拒：它们要么把请求行拆坏，要么能拼出"请求行注入"。
        // 判定用的字符串与真正交给 WinHTTP 的字符串必须是同一个 ——
        // 两处不一致正是这类闸门最典型的失效方式。
        if url.chars().any(|c| c.is_control() || c == ' ') {
            return Err(format!("地址里不能有空白或控制字符：{url}"));
        }

        let (tls, rest) = match url.split_once("://") {
            Some(("https", r)) => (true, r),
            Some(("http", r)) => (false, r),
            Some((scheme, _)) => {
                return Err(format!(
                    "不支持的地址协议 {scheme}://（只支持 https；本机地址可以用 http）：{url}"
                ));
            }
            None => {
                return Err(format!("地址要以 https:// 开头（本机地址可以用 http://）：{url}"));
            }
        };

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            // 没有路径就是根目录 —— 与改动前一致
            None => (rest, "/"),
        };

        // 用户名/密码形式的地址（`https://user:pass@host/`）不支持：
        // 不显式拒绝的话 `user:pass@host` 会被整个当主机名，最后报一句
        // 与事实无关的"连不上"。
        if authority.contains('@') {
            return Err(format!("地址里不支持用户名/密码：{url}"));
        }

        // 端口解析。`[::1]:11434` 的冒号属于 IPv6 地址，不能当端口分隔符 ——
        // 所以带方括号的单独走一条路；不带方括号又出现多个冒号的，整体按
        // （不带端口的）IPv6 处理，这也是 RFC 3986 要求方括号的原因。
        let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
            let (h, tail) = after_bracket
                .split_once(']')
                .ok_or_else(|| format!("IPv6 地址缺少右方括号：{url}"))?;
            let p = match tail.strip_prefix(':') {
                Some(v) => parse_port(v, url)?,
                None if tail.is_empty() => default_port(tls),
                None => return Err(format!("地址的端口写法不对：{url}")),
            };
            (h.to_string(), p)
        } else if authority.matches(':').count() > 1 {
            // `::1` 这种不带方括号的写法：整个 authority 就是主机名，没有端口
            // （带端口的 IPv6 必须写成 `[::1]:11434`）
            (authority.to_string(), default_port(tls))
        } else if let Some((h, v)) = authority.rsplit_once(':') {
            (h.to_string(), parse_port(v, url)?)
        } else {
            (authority.to_string(), default_port(tls))
        };

        // 规范化成"真正会交给 WinHttpConnect 的那个字符串"：
        // 判定与使用必须是同一个值（否则 `" localhost"` 这类写法会被判成回环、
        // 却把带空格的字符串交给 WinHTTP）。
        let host = host.trim().to_string();

        if host.is_empty() {
            return Err(format!("地址里没有主机名：{url}"));
        }

        if !tls && !is_loopback_host(&host) {
            // 不带方括号又带端口的 IPv6（`http://::1:11434/v1`）会被整体当成
            // 主机名，于是落进这一条 —— 它真正的错是写法。不说清的话，用户看到的
            // 是一句与事实无关的隐私提示，只能自己猜。
            let hint = if host.matches(':').count() > 1 {
                "（IPv6 地址要写成 [::1] 这样带方括号的形式）"
            } else {
                ""
            };
            return Err(format!(
                "出于隐私考虑，只允许 https 地址（本机地址除外）{hint}：{url}"
            ));
        }

        Ok((tls, host, port, path.to_string()))
    }

    /// https 默认 443、http 默认 80 —— 与浏览器一致，用户不必手写端口。
    fn default_port(tls: bool) -> u16 {
        if tls {
            443
        } else {
            80
        }
    }

    /// 端口必须是 1..=65535 的十进制数。0 会被 `WinHttpConnect` 拒掉，
    /// 但那时报的是一句 Win32 错误码，不如在这里说清楚是地址写错了。
    fn parse_port(v: &str, url: &str) -> Result<u16, String> {
        match v.parse::<u16>() {
            Ok(p) if p > 0 => Ok(p),
            _ => Err(format!("端口号不合法（应当是 1-65535）：{url}")),
        }
    }

    /// 非 200 时给用户的一句话解释。
    ///
    /// 为什么把措辞也抽成纯函数：措辞错了的代价是**把排查方向整个带偏**
    /// （参见 `explain()` 里 12029 被错标成 TLS 的那条教训）。
    /// `is_post == false` 的三句**一个字都不能变** —— 它们是对着 GitHub 的 API
    /// 写的，用户与既有测试都在读；POST 走的是 AI 端点，另给一套更贴切的
    /// （401/403 在那边几乎总是"API Key 不对"，而 401 在 GET 路径上
    /// 保持原来的"非预期状态码"，不改变既有行为）。
    pub(super) fn status_head(status: u32, is_post: bool) -> &'static str {
        match status {
            401 if is_post => "401 —— 没通过身份验证（API Key 缺失或不对）",
            403 if is_post => "403 —— 端点拒绝了这次请求（多半是 API Key 不对或没有权限）",
            404 if is_post => "404 —— 端点不存在（检查 base_url 有没有写错）",
            429 if is_post => "429 —— 请求太频繁（也可能是额度用完了）",
            403 => "403 —— GitHub 的 API 拒绝了这个请求",
            404 => "404 —— 仓库 / 发布 / 资产不存在（也可能是还没有任何发布）",
            429 => "429 —— 请求太频繁",
            _ => "非预期状态码",
        }
    }

    /// 请求方法：**只有真的要带请求体时才用 POST**。
    ///
    /// 为什么零长度的 `Some(&[])` 也归 GET：长度 0 的 POST 在 HTTP 层与 GET
    /// 没有实际区别，却要多声明两个头。需要发"空 JSON"的调用方显式传 `b"{}"`。
    pub(super) fn request_method(body: Option<&[u8]>) -> &'static str {
        match body {
            Some(b) if !b.is_empty() => "POST",
            _ => "GET",
        }
    }

    /// 拼发给 `WinHttpSendRequest` 的请求头（**不带结尾 NUL**，见 `wide_len`）。
    ///
    /// `extra` 是调用方给的头（`Accept` / `Authorization` 等，每行以 CRLF 收尾）；
    /// `User-Agent` 统一由这里加 —— GitHub 的 API 不带 UA 直接 403，
    /// 让每个调用方各记一遍这种事迟早会漏一个。
    ///
    /// **为什么抽成纯函数**：头拼错了不会报任何错，只会换来一个看不懂的 4xx/5xx，
    /// 所以它必须能被离线钉住（见 `tests_net` 里的 POST 那组测试）。
    ///
    /// 调用方**不要自己带 Content-Type** —— 这里在带请求体时会统一加一行，
    /// 重复的 Content-Type 是非法头，有的服务器会直接 400。
    pub(super) fn request_headers(extra: &str, body: Option<&[u8]>) -> String {
        let mut h = String::with_capacity(extra.len() + 96);
        h.push_str("User-Agent: DeskBase\r\n");
        h.push_str(extra);
        if let Some(b) = body.filter(|b| !b.is_empty()) {
            // 调用方漏了 CRLF 时补一个：否则 Content-Type 会粘到上一行的值里，
            // 变成非法头。只在"有请求体"这条新路径上补，GET 的字节一个都不动。
            if !h.ends_with("\r\n") {
                h.push_str("\r\n");
            }
            // Content-Length 必须是**字节数**（中文按 UTF-8 多字节算），
            // 并且与下面 `WinHttpSendRequest` 报的 dwTotalLength 是同一个数字。
            h.push_str("Content-Type: application/json\r\n");
            h.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        h
    }

    /// 发一个 HTTPS 请求（**走一条指定的路**），边收边交给 `on_chunk`，不在内存里攒整份。
    ///
    /// 为什么是流式：清单只有几十 KB，但**安装包会变大**（现在 3.2 MB，将来可能几十 MB）。
    /// 攒在内存里跑得通不代表应该这么写 —— 内存目标是写死的（≤400 MB）。
    ///
    /// `extra_headers` 是调用方给的头（`Accept` / `Authorization`，每行 CRLF 收尾），
    /// `User-Agent` 由 `request_headers` 统一补；`body` 为 `None` 或空 = GET，
    /// 非空 = POST。**GET 的请求方法、头、参数必须与加 POST 之前逐字节一致** ——
    /// 检查更新、下载安装包、`ai::list_models` 全走这条路。
    ///
    /// `on_chunk` 返回 Err 就直接中止（用于"写文件失败"这类不该继续的情况）。
    fn stream_once(
        url: &str,
        extra_headers: &str,
        body: Option<&[u8]>,
        timeout_ms: i32,
        max_bytes: usize,
        access_type: u32,
        on_chunk: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), Fail> {
        // 协议 / 回环 / 端口 / 路径的判定全部交给纯函数 —— 它是"能不能发出去"
        // 的唯一闸门，必须能被离线测试钉死（见 tests_net 的 url_allowed 那组）。
        // 判定失败属于本地问题（地址非法 / 明文要发去公网），换路不会改变结果。
        let (tls, host, port, path) = url_allowed(url).map_err(Fail::Settled)?;

        unsafe {
            let agent = wide("DeskBase");
            let session = Handle(WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE(access_type),
                PCWSTR(std::ptr::null()),
                PCWSTR(std::ptr::null()),
                0,
            ));
            if session.0.is_null() {
                return Err(Fail::Transport(format!(
                    "WinHttpOpen 失败：{}",
                    explain(last_error())
                )));
            }
            // 四个超时都要设：只设一个等于没设 —— 卡在 DNS 或 TLS 握手同样会挂住
            let _ = WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms);

            let host_w = wide(&host);
            // 端口用解析出来的真实值：本机模型普遍不是 443（Ollama 11434、
            // LM Studio 1234），写死 443 会让"本机跑模型"直接连不上。
            let connect = Handle(WinHttpConnect(
                session.0,
                PCWSTR(host_w.as_ptr()),
                port,
                0,
            ));
            if connect.0.is_null() {
                return Err(Fail::Transport(format!(
                    "连不上 {host}：{}",
                    explain(last_error())
                )));
            }

            // 方法：没有请求体就是 GET（既有三个调用方），有非空体才是 POST。
            let verb = wide(request_method(body));
            let obj = wide(&path);
            // 明文（只有回环地址能走到这里）**不能带 SECURE 标志**：
            // 带了 WinHTTP 会拿 TLS 去连一个明文端口，报一句"安全通道错误"，
            // 把"地址写错了"伪装成"证书有问题" —— 又是一个会带偏排查方向的假象。
            let flags = if tls {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            };
            let req = Handle(WinHttpOpenRequest(
                connect.0,
                PCWSTR(verb.as_ptr()),
                PCWSTR(obj.as_ptr()),
                PCWSTR(std::ptr::null()),
                PCWSTR(std::ptr::null()),
                std::ptr::null(),
                flags,
            ));
            if req.0.is_null() {
                return Err(Fail::Settled(format!(
                    "构造请求失败：{}",
                    explain(last_error())
                )));
            }

            // 跟着重定向走：资产下载会 302 到 CDN，不跟就拿不到东西。
            // 注意这是**请求级**选项，不是会话级。
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS.to_le_bytes();
            let _ = WinHttpSetOption(
                Some(req.0 as *const c_void),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&policy),
            );

            // GitHub 的 API 要求带 User-Agent，不带会被 403。
            // 用 wide_len（不带 NUL）—— 见它自己的注释，这里与 PCWSTR 的规则相反。
            let header_text = request_headers(extra_headers, body);
            let headers = wide_len(&header_text);
            // 有请求体时把缓冲区的指针与长度一起交给 WinHTTP —— 一次发完，
            // 不拆成 SendRequest + WriteData。
            // dwOptionalLength 与 dwTotalLength 都必须是 body 的**字节数**，
            // 否则服务器会按错误的长度等待或截断（表现为"请求发出去就没回音"）。
            // 无请求体时保持原来的 `None, 0, 0, 0` —— GET 的参数一个都不动。
            let sent = match body.filter(|b| !b.is_empty()) {
                Some(b) => WinHttpSendRequest(
                    req.0,
                    Some(&headers),
                    Some(b.as_ptr() as *const c_void),
                    b.len() as u32,
                    b.len() as u32,
                    0,
                ),
                None => WinHttpSendRequest(req.0, Some(&headers), None, 0, 0, 0),
            };
            sent.map_err(|e| Fail::Transport(format!("发送请求失败：{}", hr_explain(&e))))?;
            WinHttpReceiveResponse(req.0, std::ptr::null_mut())
                .map_err(|e| Fail::Transport(format!("接收响应失败：{}", hr_explain(&e))))?;

            let mut status: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR(std::ptr::null()),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
            .map_err(|e| Fail::Settled(format!("读状态码失败：{e}")))?;

            // 先收一段 body 再判状态码：**非 200 时响应体里往往有原因**
            // （GitHub 会写明是限流还是缺 User-Agent），少了它就只剩一个干巴巴的 403。
            // 但 200 时不能攒 —— 那时候内容是安装包本身，只往前端（文件）递。
            let mut total: usize = 0;
            let mut err_snippet: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut read: u32 = 0;
                WinHttpReadData(
                    req.0,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    &mut read,
                )
                .map_err(|e| Fail::Transport(format!("读响应体失败：{}", hr_explain(&e))))?;
                if read == 0 {
                    break;
                }
                let chunk = &buf[..read as usize];
                total += chunk.len();
                if total > max_bytes {
                    return Err(Fail::Settled(format!(
                        "响应超过 {} MB 上限，已中止（不把内存交给对方说了算）",
                        max_bytes / 1024 / 1024
                    )));
                }
                if status == 200 {
                    on_chunk(chunk).map_err(Fail::Settled)?;
                } else if err_snippet.len() < 200 {
                    err_snippet.extend_from_slice(chunk);
                }
            }

            if status != 200 {
                let snippet: String =
                    String::from_utf8_lossy(&err_snippet).chars().take(200).collect();
                let head = status_head(status, request_method(body) == "POST");
                let msg = format!("服务器返回 HTTP {status}（{head}）：{snippet}");
                // 403 / 429 换一条出口 IP 可能就变了（匿名限额按 IP 算）—— 值得换路再试；
                // 其余状态码是服务器的明确表态，换路没有意义。
                return Err(if status == 403 || status == 429 {
                    Fail::RetryHttp(msg)
                } else {
                    Fail::Settled(msg)
                });
            }
            Ok(())
        }
    }

    /// 收进内存。只用于**小体积**响应（JSON 清单）。
    ///
    /// 两条接入方式依次尝试；每次尝试各自一个缓冲区 ——
    /// 上一次的半截数据绝不能带进下一次（那会拼出"JSON 解析失败"的假象）。
    pub fn get(url: &str, accept: &str, timeout_ms: i32, max_bytes: usize) -> Result<Vec<u8>, String> {
        // 这里拼出来的头必须与加 POST 之前逐字节一致（`Accept: {accept}\r\n`），
        // `User-Agent` 由 stream_once 里的 request_headers 统一补。
        let extra = format!("Accept: {accept}\r\n");
        let mut problems: Vec<String> = Vec::new();
        for (i, (ty, label, is_direct)) in routes().iter().enumerate() {
            let mut out: Vec<u8> = Vec::new();
            let r = stream_once(url, &extra, None, timeout_ms, max_bytes, *ty, &mut |c| {
                out.extend_from_slice(c);
                Ok(())
            });
            match r {
                Ok(()) => {
                    note_fallback(i, &problems, label, *is_direct);
                    return Ok(out);
                }
                Err(Fail::Settled(e)) => return Err(e),
                Err(Fail::Transport(e)) | Err(Fail::RetryHttp(e)) => {
                    problems.push(format!("{label}：{e}"));
                }
            }
        }
        Err(routes_failed(&problems))
    }

    /// 一次尝试失败之后该做什么。
    ///
    /// **抽成纯函数是为了可测**：真发网络请求才能走到这个决定，而
    /// "非幂等不重发"这条策略恰恰最不该靠联网测试去证明 ——
    /// 它错了的代价是用户被重复扣费，而那种损失他自己看不见。
    pub(super) enum Step {
        /// 换下一条路再试（幂等请求：取清单 / 下安装包 / 列模型）
        Next(String),
        /// 不换了，把这条路的原因直接报给用户（非幂等 + 计费的请求）
        Stop(String),
    }

    /// 不换路时的交代。
    ///
    /// **"可能已经发出去了"这句必须在**：非幂等 + 按次计费的接口（聊天补全）
    /// 一旦重发，用户会被重复扣费 —— 那是他自己发现不了的损失；
    /// 但也不能让他以为"什么都没发生"就直接再问一遍，所以话要说全：
    /// 可能已经发出去了、只是没拿到回应，请他先确认。
    ///
    /// `answered` 区分两种失败：服务器已经应答（403/429 —— 请求确实到达了，
    /// 一般不会被计费）与完全没拿到应答（连不上 / 超时 / 断流）。后者才是
    /// "可能已经发出去了"最要紧的场景，不能和前者混成一句话。
    fn no_retry_failed(label: &str, reason: &str, answered: bool) -> String {
        let tail = if answered {
            "这次请求**可能已经发出去了**（服务器已经应答，一般不会被计费）；\
             为免重复发送，这里不会换另一条路重发。"
        } else {
            "这次请求**可能已经发出去了**，只是没拿到回应；\
             为免重复计费，这里不会换另一条路重发 —— \
             请先确认上一次有没有真的发出去，再决定要不要重问。"
        };
        format!("{label}这条路没走通 —— {reason}。{tail}")
    }

    /// 失败分类 → 下一步。`retry_other_route=false` 时**只走第一条路** ——
    /// 调用方靠 `Step::Stop` 立刻返回，第二条路根本不会被尝试。
    pub(super) fn after_failure(
        retry_other_route: bool,
        answered: bool,
        label: &str,
        reason: &str,
    ) -> Step {
        if retry_other_route {
            Step::Next(format!("{label}：{reason}"))
        } else {
            Step::Stop(no_retry_failed(label, reason, answered))
        }
    }

    /// 发一个 HTTPS **POST（JSON 请求体）**，收进内存。
    ///
    /// 给 AI 对话用（`ai.rs` 只管拼头与解析，传输与代理回退复用这里，不新增依赖）。
    /// `extra_headers` 由调用方拼好（`Accept`、`Authorization: Bearer ...`，
    /// 每行以 CRLF 收尾）；**不要自己带 Content-Type / Content-Length** ——
    /// 这两个由 `request_headers` 按 body 的字节数统一加。
    ///
    /// 两条接入方式依次尝试、每次尝试各自一个缓冲区 —— 与 `get` 同样的道理：
    /// 上一次的半截响应绝不能带进下一次（那会拼出"JSON 解析失败"的假象）。
    ///
    /// # `retry_other_route` 怎么选（这是钱的问题，不是风格问题）
    ///
    /// * `true` —— **幂等**请求可以换路重试：列模型、将来任何"再发一次结果一样"的接口。
    /// * `false` —— **非幂等 / 按次计费**的请求（发一条对话消息）：只走第一条路，
    ///   失败就如实报错，不换路重发。
    ///
    /// 为什么宁可失败也不重发：`Fail::Transport` 里包含"请求已经到达服务器、
    /// 只是响应断了"这一种 —— 这时换条路重发，用户会被**计两次费**，
    /// 而他只会看到一次回答。**失败是看得见的，重复扣费看不见。**
    /// 所以 AI 对话那条链路必须传 `false`（调用点在 `main.rs`）。
    pub fn post(
        url: &str,
        extra_headers: &str,
        body: &[u8],
        timeout_ms: i32,
        max_bytes: usize,
        retry_other_route: bool,
    ) -> Result<Vec<u8>, String> {
        let mut problems: Vec<String> = Vec::new();
        for (i, (ty, label, is_direct)) in routes().iter().enumerate() {
            let mut out: Vec<u8> = Vec::new();
            let r = stream_once(url, extra_headers, Some(body), timeout_ms, max_bytes, *ty, &mut |c| {
                out.extend_from_slice(c);
                Ok(())
            });
            match r {
                Ok(()) => {
                    note_fallback(i, &problems, label, *is_direct);
                    return Ok(out);
                }
                // 服务器已给出明确应答（400/404 等）：换路不会改变结果。
                // 这是**加这个开关之前就有的**行为，不在本次改动范围内。
                Err(Fail::Settled(e)) => return Err(e),
                // answered=false：连不上 / 超时 / 断流（响应没拿到）
                Err(Fail::Transport(e)) => match after_failure(retry_other_route, false, label, &e) {
                    Step::Stop(msg) => return Err(msg),
                    Step::Next(p) => problems.push(p),
                },
                // answered=true：403 / 429（服务器明确应答过）
                Err(Fail::RetryHttp(e)) => match after_failure(retry_other_route, true, label, &e) {
                    Step::Stop(msg) => return Err(msg),
                    Step::Next(p) => problems.push(p),
                },
            }
        }
        Err(routes_failed(&problems))
    }

    /// 收进文件。用于**大体积**响应（安装包），全程不驻留内存。
    ///
    /// 两条接入方式依次尝试；**每条路都从零开始写文件** ——
    /// 上一次尝试可能落了半截数据，往半截上续写会拼出一个
    /// "校验必不过、又看不出为什么"的坏包。
    pub fn download_to(
        url: &str,
        accept: &str,
        timeout_ms: i32,
        max_bytes: usize,
        dest: &std::path::Path,
    ) -> Result<u64, String> {
        use std::io::Write;
        // 与 get 同样：这里的头必须与加 POST 之前逐字节一致。
        let extra = format!("Accept: {accept}\r\n");
        let mut problems: Vec<String> = Vec::new();
        for (i, (ty, label, is_direct)) in routes().iter().enumerate() {
            let mut f = match std::fs::File::create(dest) {
                Ok(f) => f,
                Err(e) => return Err(format!("建不了临时文件 {}：{e}", dest.display())),
            };
            let mut written: u64 = 0;
            let r = stream_once(url, &extra, None, timeout_ms, max_bytes, *ty, &mut |c| {
                f.write_all(c).map_err(|e| format!("写文件失败：{e}"))?;
                written += c.len() as u64;
                Ok(())
            });
            match r {
                Ok(()) => {
                    if let Err(e) = f.flush() {
                        drop(f);
                        let _ = std::fs::remove_file(dest);
                        return Err(format!("落盘失败：{e}"));
                    }
                    note_fallback(i, &problems, label, *is_direct);
                    return Ok(written);
                }
                Err(e) => {
                    // 半截文件必须删掉：留着它比没有更危险 —— 下次可能被当成完整的包。
                    // 先放开句柄再删：Windows 上文件还开着就删不掉。
                    drop(f);
                    let _ = std::fs::remove_file(dest);
                    match e {
                        Fail::Settled(msg) => return Err(msg),
                        Fail::Transport(msg) | Fail::RetryHttp(msg) => {
                            problems.push(format!("{label}：{msg}"));
                        }
                    }
                }
            }
        }
        Err(routes_failed(&problems))
    }
}

#[cfg(not(target_os = "windows"))]
mod http {
    pub fn stream(
        _url: &str,
        _accept: &str,
        _timeout_ms: i32,
        _max_bytes: usize,
        _on_chunk: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        Err("更新器的网络传输目前只实现了 Windows（项目也只做 Windows）".into())
    }

    pub fn get(
        _url: &str,
        _accept: &str,
        _timeout_ms: i32,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        Err("更新器的网络传输目前只实现了 Windows（项目也只做 Windows）".into())
    }

    /// 与 Windows 版同名的 POST stub —— **必须存在**，否则非 Windows 编译不过
    /// （`ai.rs` 的对话路径是按平台无关写的，只是这条腿在别的系统上直接给 Err）。
    pub fn post(
        _url: &str,
        _extra_headers: &str,
        _body: &[u8],
        _timeout_ms: i32,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        Err("更新器的网络传输目前只实现了 Windows（项目也只做 Windows）".into())
    }

    pub fn download_to(
        _url: &str,
        _accept: &str,
        _timeout_ms: i32,
        _max_bytes: usize,
        _dest: &std::path::Path,
    ) -> Result<u64, String> {
        Err("更新器的网络传输目前只实现了 Windows（项目也只做 Windows）".into())
    }
}

/// 取 JSON（发布清单用）
pub const ACCEPT_JSON: &str = "application/vnd.github+json";

/// 取原始字节（下载发布资产用）。
/// **必须显式声明**，否则 GitHub 会返回资产的 JSON 描述而不是文件本身 —— 那是个很难一眼看出的坑：
/// 下载下来的"zip"其实是几百字节的 JSON。
pub const ACCEPT_OCTET: &str = "application/octet-stream";

/// 取发布清单并解析。
///
/// 这是"检查更新"的全部网络行为：**一次 GET，不发任何用户数据**
/// —— 按 ADR-0019 它属于 R2（默认关闭 + 记审计），不属于 R1 数据红线。
pub fn fetch_releases() -> Result<Vec<Release>, String> {
    let body = http::get(
        &releases_api_url(),
        ACCEPT_JSON,
        HTTP_TIMEOUT_MS,
        MAX_RESPONSE_BYTES,
    )?;
    let text = String::from_utf8(body).map_err(|_| "响应不是合法的 UTF-8".to_string())?;
    parse_releases(&text)
}

// ============================================================
// 设置（存 sys_meta，与工作区状态同一个表、不同的键）
// ============================================================
//
// 为什么存库而不是像主题那样放 localStorage：**这是网络开关**。
// 它必须跟"这个数据目录"绑定 —— 换个数据目录就不该继承上一个目录的联网许可。
// 而且 ADR-0005 要求它可审计，存库才好查。

/// 更新检查的档位。**默认 `Never`** —— ADR-0005「网络默认关闭」，这是红线级的默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UpdateMode {
    /// 从不检查（默认）
    Never,
    /// 只检查并提醒，**不下载**
    Notify,
    /// 检查并下载，替换前询问用户
    DownloadAsk,
}

impl UpdateMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdateMode::Never => "never",
            UpdateMode::Notify => "notify",
            UpdateMode::DownloadAsk => "download_ask",
        }
    }
    pub fn parse(s: &str) -> Option<UpdateMode> {
        match s {
            "never" => Some(UpdateMode::Never),
            "notify" => Some(UpdateMode::Notify),
            "download_ask" => Some(UpdateMode::DownloadAsk),
            _ => None,
        }
    }
}

impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Prerelease => "prerelease",
        }
    }
    pub fn parse(s: &str) -> Option<Channel> {
        match s {
            "stable" => Some(Channel::Stable),
            "prerelease" => Some(Channel::Prerelease),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct UpdateSettings {
    pub mode: UpdateMode,
    pub channel: Channel,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        // 保守到底：默认不检查、默认稳定通道
        UpdateSettings {
            mode: UpdateMode::Never,
            channel: Channel::Stable,
        }
    }
}

const KEY_MODE: &str = "net.update.mode";
const KEY_CHANNEL: &str = "net.update.channel";

/// 读设置。**任何异常都回退到默认（从不检查）** —— 读不出来时倾向于不联网，
/// 而不是倾向于联网。方向不能反。
pub fn load_settings(db: &crate::model::Db) -> UpdateSettings {
    let get = |key: &str| -> Option<String> { db.meta_get(key) };
    let mut s = UpdateSettings::default();
    if let Some(v) = get(KEY_MODE) {
        if let Some(m) = UpdateMode::parse(&v) {
            s.mode = m;
        }
    }
    if let Some(v) = get(KEY_CHANNEL) {
        if let Some(c) = Channel::parse(&v) {
            s.channel = c;
        }
    }
    s
}

pub fn save_settings(
    db: &mut crate::model::Db,
    s: &UpdateSettings,
) -> Result<(), String> {
    for (k, v) in [
        (KEY_MODE, s.mode.as_str().to_string()),
        (KEY_CHANNEL, s.channel.as_str().to_string()),
    ] {
        db.meta_set(k, &v)
            .map_err(|e| format!("保存更新设置失败：{e}"))?;
    }
    Ok(())
}

/// 一次检查的结果，连"为什么没检查"一起给出来。
///
/// `checked = false` 时**没有发生任何网络行为** —— 界面要能明确区分
/// "检查过，是最新"与"压根没检查（网络默认关闭）"，这两件事对用户的意义完全不同。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckReport {
    pub checked: bool,
    pub mode: String,
    pub channel: String,
    pub current: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CheckOutcome>,
}

/// 按设置执行一次检查。**`Never` 档位下不会发起任何网络请求。**
///
/// 网络行为本身由调用方（IPC 层）记审计 —— 这里只做"该不该发"的判断。
pub fn check_by_settings(
    db: &crate::model::Db,
    current: &Version,
) -> Result<CheckReport, String> {
    let s = load_settings(db);
    let mut rep = CheckReport {
        checked: false,
        mode: s.mode.as_str().to_string(),
        channel: s.channel.as_str().to_string(),
        current: current.to_string(),
        reason: None,
        outcome: None,
    };

    if s.mode == UpdateMode::Never {
        rep.reason = Some(
            "网络默认关闭 —— 更新检查在设置里开启之后才会联网（ADR-0005）。\
             现在也可以直接到发布页手动下载。"
                .into(),
        );
        return Ok(rep);
    }

    let releases = fetch_releases()?;
    rep.checked = true;
    rep.outcome = Some(check(&releases, current, s.channel));
    Ok(rep)
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
// 下载 + 暂存（ADR-0018 第 2–4 步的落地）
// ============================================================

/// 下安装包用的超时：比清单长得多（3 MB 在慢网络上也要一会儿）。
pub const DOWNLOAD_TIMEOUT_MS: i32 = 120_000;

/// 暂存好的更新。`dir` 里会有：`update.zip`、`update.zip.sha256`、
/// 解压出来的受管文件、以及给 `--apply-update` 读的 `plan.json`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Staged {
    pub dir: String,
    pub plan: ApplyPlan,
    pub zip_entries: Vec<String>,
    pub zip_bytes: u64,
}

/// 资产的下载地址 —— **走 API 的 asset 端点**，不用 `browser_download_url`。
/// 理由：这样连下载请求的第一跳也落在 `api.github.com`（ADR-0018 第 7 节）。
pub fn asset_api_url(asset: &Asset) -> String {
    format!(
        "https://api.github.com/repos/{REPO}/releases/assets/{}",
        asset.id
    )
}

/// 暂存目录：放在**数据目录**下。
///
/// 为什么不放程序目录：安装版的程序目录是只读的（Program Files），
/// 而数据目录一定有写权限。且数据目录本来就有 `backups/`，
/// 更新暂存放进去与它同级，用户找得到也删得掉。
pub fn staging_dir(data_dir: &Path, version: &Version) -> PathBuf {
    data_dir.join("updates").join(version.to_string())
}

/// 把 zip 里**白名单内**的文件解出来。白名单外的连碰都不碰。
fn extract_managed(zip_path: &Path, dest: &Path, wanted: &[String]) -> Result<(), String> {
    use std::io::copy;
    let f = std::fs::File::open(zip_path).map_err(|e| format!("打不开安装包：{e}"))?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("这不是一个有效的压缩包：{e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("读包内条目失败：{e}"))?;
        let name = entry.name().to_string();
        if !wanted.iter().any(|w| w == &name) {
            continue; // 白名单外一律不落地（verify_zip_layout 已经拦过路径穿越）
        }
        let out = dest.join(&name);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| format!("建文件失败：{e}"))?;
        copy(&mut entry, &mut w).map_err(|e| format!("解压 {name} 失败：{e}"))?;
    }
    Ok(())
}

/// **第 2–4 步 + 解压到暂存目录。全是本地操作，可以离线测。**
///
/// 拆出来的目的就是这一点：五步校验里真正容易错的是"边界判断"（大小不符、
/// 哈希不符、包里没可执行文件、路径穿越），而这些不需要联网就能构造出来。
pub fn verify_and_stage_zip(
    zip_path: &Path,
    sha256_text: &str,
    expected_size: u64,
    staging: &Path,
    install_dir: &Path,
    data_dir: &Path,
    current: &Version,
    target: &Version,
) -> Result<Staged, String> {
    // ① 第 2 步后半：API 报的 size 与实下载字节数一致
    let actual = std::fs::metadata(zip_path)
        .map_err(|e| format!("读不到下载的包：{e}"))?
        .len();
    if actual != expected_size {
        return Err(format!(
            "包大小与清单不符 —— 可能只下了一半，或者根本不是同一个包。\n实际：{actual} 字节\n清单：{expected_size} 字节"
        ));
    }

    // ② 第 3 步：配套的 SHA-256 逐字节比对
    let (want, _) = parse_sha256_file(sha256_text)
        .ok_or("校验和文件的内容不是 64 位十六进制 —— 拒绝继续（没有它能比的东西）")?;
    verify_sha256(zip_path, &want)?;

    // ③ 第 4 步：真打开包，确认有 deskbase.exe、且没有路径穿越
    let entries = verify_zip_layout(zip_path)?;

    // ④ 算替换计划（内含两条底线：拒绝安装目录==暂存目录、拒绝写进数据目录）
    let plan = plan_apply(staging, install_dir, data_dir, &entries, current, target)?;

    // ⑤ 解压白名单内的文件
    std::fs::create_dir_all(staging).map_err(|e| format!("建暂存目录失败：{e}"))?;
    extract_managed(zip_path, staging, &plan.replace)?;

    // ⑥ 写 plan.json —— `--apply-update` 靠它知道"从哪个版本到哪个版本、动哪些文件"
    let text = serde_json::to_string_pretty(&plan)
        .map_err(|e| format!("序列化替换计划失败：{e}"))?;
    std::fs::write(staging.join("plan.json"), text)
        .map_err(|e| format!("写替换计划失败：{e}"))?;

    Ok(Staged {
        dir: staging.to_string_lossy().to_string(),
        plan,
        zip_entries: entries,
        zip_bytes: actual,
    })
}

/// 从选定的 Release **下载并暂存**。这是"下载"这一步的全部网络行为。
///
/// 每一次失败都要**保持程序原状态**并说清原因 —— 更新器最不该做的事，
/// 是留下一个"看起来升级了、其实是半截"的状态。
pub fn download_and_stage(
    release: &Release,
    install_dir: &Path,
    data_dir: &Path,
    current: &Version,
) -> Result<Staged, String> {
    let target = release
        .version()
        .ok_or_else(|| format!("发布标签解析不出合法版本号：{}", release.tag))?;
    let assets = release.usable_assets();

    let zip = pick_zip(&assets, &target).ok_or_else(|| {
        format!(
            "这个发布里没有 {} —— 产物可能还没传完",
            asset_name(&target.to_string())
        )
    })?;
    // 没有校验和就不继续 —— SHA-256 是这条链上**唯一的**内容校验，缺它等于裸装
    let sha = pick_sha256(&assets, zip)
        .ok_or("这个发布里没有配套的 .sha256 —— 缺它就没法校验内容，拒绝继续")?;

    let staging = staging_dir(data_dir, &target);
    // 先把旧的暂存清掉：残留文件会让"这次到底装了什么"变得说不清
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("清理旧暂存失败：{e}"))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("建暂存目录失败：{e}"))?;

    // 上限给"清单里说的大小 + 1 MB"：不允许对方比它自己声明的还多塞内容
    let cap = (zip.size as usize).saturating_add(1024 * 1024).max(1024 * 1024);
    let zip_path = staging.join("update.zip");
    http::download_to(
        &asset_api_url(zip),
        ACCEPT_OCTET,
        DOWNLOAD_TIMEOUT_MS,
        cap,
        &zip_path,
    )?;

    // 校验和资产很小，收进内存就够；同时**落到暂存目录留证**
    let sha_body = http::get(
        &asset_api_url(sha),
        ACCEPT_OCTET,
        HTTP_TIMEOUT_MS,
        64 * 1024,
    )?;
    let sha_text =
        String::from_utf8(sha_body).map_err(|_| "校验和文件不是合法的 UTF-8".to_string())?;
    std::fs::write(staging.join("update.zip.sha256"), &sha_text)
        .map_err(|e| format!("保存校验和文件失败：{e}"))?;

    verify_and_stage_zip(
        &zip_path,
        &sha_text,
        zip.size,
        &staging,
        install_dir,
        data_dir,
        current,
        &target,
    )
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

    // 替换完把程序重新拉起来 —— 少了这一步，用户看到的是"更新完成，程序自己没了"。
    // 拉不起来**不算更新失败**（文件已经换好了），但必须如实说清让他手动打开。
    let relaunch = relaunch(install_dir);

    Ok(format!(
        "已从 {} 更新到 {}；替换了 {} 个文件（包内共 {} 个）；备份在 {}{}",
        plan.from,
        plan.to,
        copied.len(),
        entries.len(),
        backup.display(),
        relaunch
    ))
}

/// 替换完成后重新启动程序。返回一句给人看的补充说明。
fn relaunch(install_dir: &Path) -> String {
    let exe = install_dir.join("deskbase.exe");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        return match std::process::Command::new(&exe)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            Ok(_) => "；已重新启动".to_string(),
            Err(e) => format!("；但自动重启失败（{e}），请手动打开 {}", exe.display()),
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = exe;
        "; 请手动重新打开程序".to_string()
    }
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
    fn version_parse_accepts_release_and_prerelease() {
        let a = Version::parse("0.2.1").unwrap();
        assert_eq!((a.major, a.minor, a.patch), (0, 2, 1));
        assert!(a.pre.is_none(), "0.2.1 是正式版，没有预发布后缀");
        let b = Version::parse("0.2.0-beta.3").unwrap();
        assert_eq!(b.pre.as_deref(), Some("beta.3"));
    }

    #[test]
    fn malformed_version_rejected_never_guess() {
        for bad in ["", "v0.2.1", "1.2", "1.2.3.4", "abc", "0.2.1-", "0.-1.0"] {
            assert!(Version::parse(bad).is_none(), "不该接受：{bad}");
        }
    }

    #[test]
    fn version_compare() {
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
    fn version_display_and_parse_inverse() {
        for s in ["0.2.1", "0.2.0-beta.3", "1.0.0"] {
            assert_eq!(Version::parse(s).unwrap().to_string(), s);
        }
    }

    // ---------- 资产挑选 ----------

    #[test]
    fn asset_name_built_by_convention() {
        assert_eq!(asset_name("0.2.1"), "deskbase-0.2.1-windows-x64-portable.zip");
        assert_eq!(
            asset_name("0.2.0-beta.3"),
            "deskbase-0.2.0-beta.3-windows-x64-portable.zip"
        );
    }

    #[test]
    fn package_pick_requires_exact_name() {
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
    fn checksum_file_both_formats_accepted() {
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
    fn computed_hash_matches_authoritative_vector() {
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
    fn checksum_mismatch_reports_both_values() {
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
    fn package_needs_executable_to_count() {
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
    fn path_traversal_in_package_rejected() {
        let d = tmp("traversal");
        let evil = d.join("evil.zip");
        make_zip(&evil, &["..\\..\\Windows\\System32\\evil.exe", "deskbase.exe"]);
        let err = verify_zip_layout(&evil).unwrap_err();
        assert!(err.contains("不安全"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn non_whitelisted_files_skipped_but_reported() {
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
    fn install_dir_equal_staging_rejected() {
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
    fn install_dir_inside_data_dir_rejected() {
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
    fn no_plan_when_no_executable() {
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
    fn normal_plan_states_data_dir_untouched() {
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
    fn apply_overwrites_and_leaves_backup() {
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
    fn failed_apply_rolls_back() {
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
    fn backup_dir_carries_old_version_for_rollback() {
        let v = Version::parse("0.2.1").unwrap();
        assert_eq!(backup_dir_name(&v), "update-backup-0.2.1");
        let v2 = Version::parse("0.2.0-beta.3").unwrap();
        assert_eq!(backup_dir_name(&v2), "update-backup-0.2.0-beta.3");
    }
}

#[cfg(test)]
mod tests_release {
    use super::*;

    /// 真实的 Releases 响应夹具（去掉 body 等体积字段，字段名与类型与 API 一致）。
    /// **对着真实结构测，不对着想象测** —— 上次就是靠它发现 `/releases/latest` 会 404。
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/github-releases-sample.json");

    fn fixture() -> Vec<Release> {
        let text = std::fs::read_to_string(FIXTURE).expect("夹具文件应当存在");
        parse_releases(&text).expect("夹具应当能解析")
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn real_fixture_parses_four_releases() {
        let rs = fixture();
        assert_eq!(rs.len(), 4, "夹具里有 4 个发布");
        for r in &rs {
            assert!(r.version().is_some(), "tag 应当能解析：{}", r.tag);
            assert!(!r.draft, "夹具里没有草稿");
            assert!(r.prerelease, "本仓库迄今四个发布全是预发布（这正是 latest 会 404 的原因）");
            assert_eq!(r.usable_assets().len(), 3, "每个发布三个资产：{}", r.tag);
        }
        // 最新的排在最前
        assert_eq!(rs[0].tag, "v0.2.0-beta.3");
    }

    /// **真联网**取一次 Releases。
    ///
    /// 为什么标 #[ignore]：它依赖网络，而常规测试必须可重复、不联网。
    /// 现有那些测试全是对着夹具测解析 —— 解析对了不等于**发得出请求**。
    /// WinHTTP 那段胶水代码此前没有任何一次真实调用，这是一条没人走过的路。
    /// 跑法：`cargo test -- --ignored`
    #[test]
    #[ignore]
    fn live_network_fetches_release_list() {
        let rs = match fetch_releases() {
            Ok(x) => x,
            Err(e) => panic!("取不到发布列表：{e}"),
        };
        assert!(!rs.is_empty(), "本仓库至少有一个发布");
        let top = &rs[0];
        let v = top.version().expect("最新发布的 tag 应当能解析出版本号");
        println!("最新发布：{} → v{}.{}.{}", top.tag, v.major, v.minor, v.patch);
        println!("资产数：{}", top.usable_assets().len());
    }

    #[test]
    fn tag_with_v_prefix_parses() {
        let r = Release {
            tag: "v0.2.1".into(),
            draft: false,
            prerelease: false,
            assets: vec![],
        };
        assert_eq!(r.version().unwrap(), v("0.2.1"));
        // 不带前缀照样可以
        let r2 = Release {
            tag: "0.2.1".into(),
            draft: false,
            prerelease: false,
            assets: vec![],
        };
        assert_eq!(r2.version().unwrap(), v("0.2.1"));
    }

    #[test]
    fn prerelease_below_release_per_semver() {
        // 夹具里是 0.2.0-beta.1/2/3 与 0.1.0-alpha.1。
        // 从 0.2.0 出发它们**都不算更新** —— beta 小于同号正式版。
        // 这条先钉住，免得后面有人"顺手"把 beta 当成比正式版新。
        let rs = fixture();
        assert!(matches!(
            check(&rs, &v("0.2.0"), Channel::Prerelease),
            CheckOutcome::UpToDate
        ));
    }

    #[test]
    fn stable_channel_reports_when_only_prerelease() {
        // 这是最要紧的一条：不能报"已是最新"（假话），不能报 404，更不能把 beta 当正式版装。
        // 用一个比 beta 低的当前版本，这样 beta 才成立为"更新的预发布"。
        let rs = fixture();
        match check(&rs, &v("0.1.5"), Channel::Stable) {
            CheckOutcome::OnlyPrerelease { tag, version } => {
                assert_eq!(tag, "v0.2.0-beta.3");
                assert_eq!(version, "0.2.0-beta.3");
            }
            other => panic!("应当报「只有测试版」，实际：{other:?}"),
        }
    }

    #[test]
    fn beta_channel_sees_prerelease() {
        let rs = fixture();
        match check(&rs, &v("0.1.5"), Channel::Prerelease) {
            CheckOutcome::Newer { tag, prerelease, .. } => {
                assert_eq!(tag, "v0.2.0-beta.3");
                assert!(prerelease, "它确实是预发布");
            }
            other => panic!("测试通道应当看到 beta.3，实际：{other:?}"),
        }
    }

    #[test]
    fn up_to_date_reports_nothing() {
        let rs = fixture();
        assert!(matches!(
            check(&rs, &v("0.2.0-beta.3"), Channel::Prerelease),
            CheckOutcome::UpToDate
        ));
        // 比最新还新（本地开发版）也不该报"有更新"
        assert!(matches!(
            check(&rs, &v("0.3.0"), Channel::Prerelease),
            CheckOutcome::UpToDate
        ));
    }

    fn rel(tag: &str, prerelease: bool, draft: bool) -> Release {
        Release {
            tag: tag.into(),
            draft,
            prerelease,
            assets: vec![ApiAsset {
                id: 1,
                name: asset_name(tag.trim_start_matches('v')),
                size: 100,
                state: "uploaded".into(),
            }],
        }
    }

    #[test]
    fn stable_channel_prefers_release() {
        let rs = vec![
            rel("v0.3.0-beta.1", true, false),
            rel("v0.2.5", false, false),
            rel("v0.2.1", false, false),
        ];
        match check(&rs, &v("0.2.0"), Channel::Stable) {
            CheckOutcome::Newer { tag, prerelease, .. } => {
                assert_eq!(tag, "v0.2.5", "应当挑正式版里最高的那个");
                assert!(!prerelease);
            }
            other => panic!("实际：{other:?}"),
        }
    }

    #[test]
    fn drafts_never_considered() {
        let rs = vec![rel("v0.9.0", false, true), rel("v0.2.5", false, false)];
        match check(&rs, &v("0.2.0"), Channel::Stable) {
            CheckOutcome::Newer { tag, .. } => assert_eq!(tag, "v0.2.5", "草稿 v0.9.0 不该被选中"),
            other => panic!("实际：{other:?}"),
        }
    }

    #[test]
    fn older_only_means_up_to_date() {
        let rs = vec![rel("v0.1.0", false, false)];
        assert!(matches!(
            check(&rs, &v("0.2.0"), Channel::Stable),
            CheckOutcome::UpToDate
        ));
    }

    #[test]
    fn incomplete_asset_not_usable() {
        let r = Release {
            tag: "v0.3.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![
                ApiAsset { id: 1, name: "a.zip".into(), size: 1, state: "uploaded".into() },
                ApiAsset { id: 2, name: "b.zip".into(), size: 1, state: "starter".into() },
                ApiAsset { id: 3, name: "c.zip".into(), size: 1, state: String::new() },
            ],
        };
        let names: Vec<String> = r.usable_assets().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["a.zip", "c.zip"], "上传中的 b.zip 要排除；state 缺失按可用处理");
    }

    #[test]
    fn bad_manifest_errors_not_empty() {
        // 给空结果会让界面显示"已是最新" —— 那是假话。必须报错。
        assert!(parse_releases("not json").is_err());
        assert!(parse_releases("{}").is_err(), "顶层应当是数组");
        assert!(parse_releases("[]").unwrap().is_empty(), "空数组是合法的");
    }
}

#[cfg(test)]
mod tests_net {
    use super::*;

    /// 真实的联网检查。**默认忽略**（`#[ignore]`）。
    ///
    /// 为什么不放进常规测试集：CI 与用户机器的网络都不可靠，
    /// 一个"有时红有时绿"的测试最后一定会被人无视掉 —— **那比没有测试更糟**。
    /// 这里要的是一次**真跑**，所以手动执行：
    ///
    /// ```bash
    /// cd app && cargo test -- --ignored 联网检查更新能拿到清单 --nocapture
    /// ```
    #[test]
    #[ignore = "需要网络；手动用 cargo test -- --ignored 跑"]
    fn online_check_fetches_manifest() {
        match fetch_releases() {
            Ok(rs) => {
                assert!(!rs.is_empty(), "至少应当有一个发布");
                for r in &rs {
                    assert!(r.version().is_some(), "tag 应当能解析：{}", r.tag);
                }
                println!("✔ 取到 {} 个发布，最新的 tag = {}", rs.len(), rs[0].tag);
            }
            // 未认证的 GitHub API 是 **60 次/小时/出口 IP**。走系统代理时出口 IP 是共享的，
            // 所以这条很容易撞上 —— 这不是我们代码的问题，但**必须能被认出来**，
            // 而且测试不能因此变红（否则又会变成"随机红的测试最后被无视"）。
            Err(e) if e.contains("rate limit") => {
                println!("⚠ 撞上 GitHub 限流 —— 这恰好说明请求确实到达了 API：\n{e}");
            }
            Err(e) => panic!("本该能取到清单（限流之外的错误都算失败）：{e}"),
        }
    }

    /// 取数地址必须写死在官方仓库上 —— 它是"下载源不可配置"这条要求的锚点。
    #[test]
    fn endpoint_hardcoded_official_and_not_latest() {
        let u = releases_api_url();
        assert!(u.starts_with("https://api.github.com/repos/YJLZSL/DeskBase/releases"));
        assert!(
            !u.ends_with("/latest"),
            "不能用 /releases/latest —— 本仓库所有发布都是预发布，实测它一律 404"
        );
    }

    /// 非 https 的地址必须被拒（不进网络栈就拒）。
    #[test]
    fn only_https_allowed() {
        let err = http::get("http://api.github.com/x", ACCEPT_JSON, 1000, 1024).unwrap_err();
        assert!(err.contains("https"), "{err}");
    }

    /// 错误码翻译必须对上号 —— 尤其是**实测踩过的那两个**。
    ///
    /// 这条测试的存在性本身有故事：0x80072EFD 低 16 位 = 12029 = CANNOT_CONNECT，
    /// 而表里它曾经被标成"TLS 握手失败" —— 于是"代理挂了"看起来像"证书出问题"，
    /// 排查方向整个被带偏。错误码翻译错了比不翻译更坏。
    #[test]
    #[cfg(target_os = "windows")]
    fn error_code_translation_matches() {
        assert!(
            http::explain(12029).contains("连不上"),
            "12029 是 CANNOT_CONNECT：{}",
            http::explain(12029)
        );
        assert!(
            http::explain(12007).contains("DNS"),
            "12007 是 NAME_NOT_RESOLVED：{}",
            http::explain(12007)
        );
        // 12152 是 AI 对话走本机模型时最常见的失败（模型服务没起）。
        // 翻译里必须给出**可操作的方向**，而不是只把数字换个说法 ——
        // 用户看不懂"服务器返回无法识别的应答"，但他看得懂"模型可能没启动"。
        let e = http::explain(12152);
        assert!(
            e.contains("模型"),
            "12152 应当指向本机模型的常见原因，而不是一句泛泛的应答异常：{e}"
        );
        // 未知码仍要回落到"Win32 错误码 N"，不能装作认识（错译比不译更坏）
        assert_eq!(http::explain(99999), "Win32 错误码 99999");
    }

    /// 兜底顺序：默认"系统代理 → 直连"；兜底过一次后翻成"直连 → 系统代理"。
    #[test]
    #[cfg(target_os = "windows")]
    fn dual_route_order_flips_on_fallback() {
        let def = http::order_routes(false);
        assert_eq!(def[0].1, "系统代理", "默认必须先尊重用户的系统代理");
        assert_eq!(def[1].1, "直连");
        let flipped = http::order_routes(true);
        assert_eq!(flipped[0].1, "直连", "兜底过一次后直连优先（少等一个超时）");
        assert_eq!(flipped[1].1, "系统代理");
        // 两条路必须是**不同的接入方式**，否则"兜底"等于同一件事做两遍
        assert_ne!(def[0].0, def[1].0);
    }

    // ---------- POST 传输层（纯函数，全程不联网） ----------

    /// 从拼好的头里取出 Content-Length —— **独立于拼装实现**地核对它。
    /// 复用被测代码自己算出来的数字去断言，等于没测。
    #[cfg(target_os = "windows")]
    fn content_length(headers: &str) -> Option<usize> {
        headers
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
    }

    /// 请求方法由"有没有请求体"决定：没有就 GET，有非空体才 POST。
    ///
    /// 为什么零长度的 `Some(&[])` 也当 GET：长度 0 的 POST 在 HTTP 层
    /// 与 GET 没有实际区别，却要多声明两个头；调用方（AI 对话）发的
    /// 永远是非空 JSON，真要发"空 JSON"得显式传 `b"{}"`。
    #[test]
    #[cfg(target_os = "windows")]
    fn request_method_picks_post_only_for_nonempty_body() {
        assert_eq!(http::request_method(None), "GET", "没有请求体就是 GET");
        assert_eq!(http::request_method(Some(b"")), "GET", "空 body 不该变成 POST");
        assert_eq!(http::request_method(Some(b"{}")), "POST");
        assert_eq!(http::request_method(Some(br#"{"a":1}"#)), "POST");
    }

    /// **既有 GET 的字节不许变** —— 三个调用方（清单 / 下载 / list_models）
    /// 都走这一行头；给 POST 让路时最容易在这里手滑。
    #[test]
    #[cfg(target_os = "windows")]
    fn request_headers_keep_get_bytes_identical() {
        assert_eq!(
            http::request_headers("Accept: application/vnd.github+json\r\n", None),
            "User-Agent: DeskBase\r\nAccept: application/vnd.github+json\r\n"
        );
    }

    /// POST 必须带 Content-Type，且 Content-Length 与 body 的**字节数**一致
    /// （中文按 UTF-8 多字节算 —— 按字符数算是经典的 off-by-N）。
    #[test]
    #[cfg(target_os = "windows")]
    fn request_headers_post_length_matches_body_bytes() {
        let extra = "Accept: application/json\r\nAuthorization: Bearer sk-test\r\n";
        for body in [
            &b"{}"[..],
            &b"{\"model\":\"gpt-4o-mini\"}"[..],
            "{\"q\":\"你好，世界\"}".as_bytes(),
        ] {
            let h = http::request_headers(extra, Some(body));
            assert!(
                h.contains("Content-Type: application/json\r\n"),
                "缺 Content-Type：{h}"
            );
            assert!(
                h.contains("Authorization: Bearer sk-test\r\n"),
                "调用方给的头不许被吃掉：{h}"
            );
            assert_eq!(
                content_length(&h),
                Some(body.len()),
                "Content-Length 必须等于 body 的字节数：{h}"
            );
        }
    }

    /// 调用方给的头没以 CRLF 收尾时，**不能把 Content-Type 粘到上一行的值里** ——
    /// 粘上去就是一个非法头，服务器只会回一个看不出原因的 400。
    #[test]
    #[cfg(target_os = "windows")]
    fn request_headers_repair_missing_final_crlf() {
        let h = http::request_headers("Accept: application/json", Some(b"{}"));
        assert!(h.contains("\r\nContent-Type: application/json\r\n"), "{h}");
        assert!(!h.contains("application/jsonContent-Type"), "{h}");
        assert_eq!(content_length(&h), Some(2), "{h}");
    }

    /// 非 200 的措辞：**GET 那三句一个字都不许变**（用户与既有测试都在读它），
    /// POST 走 AI 端点时不能把"GitHub 的 API 拒绝了"当成线索甩给用户。
    #[test]
    #[cfg(target_os = "windows")]
    fn status_head_keeps_get_wording_and_retargets_post() {
        assert_eq!(http::status_head(403, false), "403 —— GitHub 的 API 拒绝了这个请求");
        assert_eq!(
            http::status_head(404, false),
            "404 —— 仓库 / 发布 / 资产不存在（也可能是还没有任何发布）"
        );
        assert_eq!(http::status_head(429, false), "429 —— 请求太频繁");
        // 401 在 GET 路径上没有专属文案（保持加 POST 之前的样子），POST 上必须有
        assert_eq!(http::status_head(401, false), "非预期状态码");
        assert!(http::status_head(401, true).contains("身份验证"), "{}", http::status_head(401, true));
        assert!(http::status_head(403, true).contains("API Key"), "{}", http::status_head(403, true));
        // 认不出来的状态码两边都是同一句
        assert_eq!(http::status_head(500, true), "非预期状态码");
        assert_eq!(http::status_head(500, false), "非预期状态码");
    }

    // ---------- 地址闸门（纯函数，全程不联网） ----------

    /// https 的既有行为一个字节都不许变：默认 443、要 TLS、路径原样。
    /// 三个既有调用方（清单 / 下载 / list_models）全走这条。
    #[test]
    #[cfg(target_os = "windows")]
    fn url_allowed_https_defaults_to_443_and_tls() {
        assert_eq!(
            http::url_allowed("https://api.github.com/repos/YJLZSL/DeskBase/releases?per_page=20")
                .unwrap(),
            (
                true,
                "api.github.com".to_string(),
                443,
                "/repos/YJLZSL/DeskBase/releases?per_page=20".to_string()
            )
        );
        // 地址里没有路径 = 根目录（与改动前一致）
        assert_eq!(
            http::url_allowed("https://example.com").unwrap(),
            (true, "example.com".to_string(), 443, "/".to_string())
        );
    }

    /// 本机模型这条路：http + 回环 + 自带端口，必须放行且端口要对。
    /// Ollama 是 11434、LM Studio 是 1234 —— 写死 443 就等于本机模型不可用。
    #[test]
    #[cfg(target_os = "windows")]
    fn url_allowed_loopback_http_keeps_port() {
        assert_eq!(
            http::url_allowed("http://127.0.0.1:11434/v1/chat/completions").unwrap(),
            (false, "127.0.0.1".to_string(), 11434, "/v1/chat/completions".to_string())
        );
        assert_eq!(
            http::url_allowed("http://localhost:1234/v1").unwrap(),
            (false, "localhost".to_string(), 1234, "/v1".to_string())
        );
        // http 不写端口 = 80，https 不写端口 = 443
        assert_eq!(
            http::url_allowed("http://127.0.0.1/v1/models").unwrap(),
            (false, "127.0.0.1".to_string(), 80, "/v1/models".to_string())
        );
        // 主机名大小写不敏感：`LocalHost` 与 `localhost` 是同一个名字
        assert_eq!(
            http::url_allowed("http://LocalHost:11434/v1").unwrap(),
            (false, "LocalHost".to_string(), 11434, "/v1".to_string())
        );
    }

    /// `[::1]:11434` 的冒号属于 IPv6 地址，不能被当成端口分隔符。
    #[test]
    #[cfg(target_os = "windows")]
    fn url_allowed_ipv6_loopback_parses_host_and_port() {
        assert_eq!(
            http::url_allowed("http://[::1]:11434/v1/models").unwrap(),
            (false, "::1".to_string(), 11434, "/v1/models".to_string())
        );
        assert_eq!(
            http::url_allowed("https://[::1]/x").unwrap(),
            (true, "::1".to_string(), 443, "/x".to_string())
        );
        // 不带方括号的 `::1` 也认识，但只能配默认端口（这是 RFC 3986 的写法要求）
        assert_eq!(
            http::url_allowed("http://::1/v1").unwrap(),
            (false, "::1".to_string(), 80, "/v1".to_string())
        );
    }

    /// **明文不许发去公网**（含内网地址）—— 这条闸门是 R1/R2 的地基，
    /// 放宽它等于允许用户把自己的数据明文发出去。
    #[test]
    #[cfg(target_os = "windows")]
    fn url_allowed_rejects_plaintext_beyond_loopback() {
        for bad in [
            "http://api.github.com/x",
            "http://example.com:8080/v1",
            // 内网也别想：明文一样离开这台机器（只是没离开这个网段）
            "http://192.168.1.10:11434/v1",
            "ftp://127.0.0.1/x",
            "file:///c:/x",
            // 没有协议头的写法
            "api.github.com/x",
            // 空主机名
            "https://",
            "http://",
            // 端口不是数字 / 是 0 / 缺右方括号
            "http://127.0.0.1:abc/v1",
            "http://127.0.0.1:0/v1",
            "http://[::1:11434/v1",
            // 用户名密码形式不支持（会被整个当主机名，报一句与事实无关的"连不上"）
            "https://user:pass@api.github.com/x",
            // 空白 / 控制字符会拆坏请求行（判定用的串必须与交给 WinHTTP 的串一致）
            "https://api.github.com/x y",
            "https://api.github.com/x\r\nX-Evil: 1",
        ] {
            let e = http::url_allowed(bad).unwrap_err();
            assert!(!e.is_empty(), "{bad} 必须被拒");
        }
        // 拒绝的理由要能看懂：公网明文要说隐私，不是"网络故障"
        let e = http::url_allowed("http://example.com/v1").unwrap_err();
        assert!(e.contains("隐私") && e.contains("https"), "{e}");
        // 不带方括号又带端口的 IPv6：真正的错是写法，得说清楚，别只给一句隐私提示
        let e = http::url_allowed("http://::1:11434/v1").unwrap_err();
        assert!(e.contains("方括号"), "{e}");
    }

    /// 只有 https 能进网络栈 —— POST 与 GET 守同一条底线。
    ///
    /// 这条测试**不联网**：非 https（且非回环）在进网络栈之前就被拒了。
    /// ⚠️ 它是"给回环开明文"那次改动的保险：`api.example.com` 不是回环，
    /// 必须仍然被拒。
    #[test]
    #[cfg(target_os = "windows")]
    fn post_only_https_allowed() {
        let err = http::post(
            "http://api.example.com/v1/chat/completions",
            "Accept: application/json\r\n",
            b"{}",
            1000,
            1024,
            false,
        )
        .unwrap_err();
        assert!(err.contains("https"), "{err}");
        // 本机地址则相反：必须能过闸门（否则本机模型这条路根本走不到网络栈）
        assert!(http::url_allowed("http://127.0.0.1:11434/v1").is_ok());
    }

    // ---------- 真的发一次（只走回环，仍然不碰外网） ----------

    /// 从服务器看到的原始请求里取 Content-Length。
    #[cfg(target_os = "windows")]
    fn seen_content_length(raw: &str) -> Option<usize> {
        raw.split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
    }

    /// 等服务器把你发出去的请求记下来（最多 20 秒）。
    #[cfg(target_os = "windows")]
    fn wait_for_capture(
        seen: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if let Some(s) = seen.lock().unwrap().clone() {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("20 秒内没有收到任何请求 —— 请求根本没发出去（或被代理吞了）");
    }

    /// 极简回环服务器：收一条完整 HTTP 请求（按 header + Content-Length 判定收齐，
    /// 500ms 静默兜底），把**原始请求**交回给测试，然后回一个 200 JSON。
    ///
    /// 为什么要真的开一个 socket：WinHTTP 的参数（verb / Content-Length / 端口 /
    /// 明文标志）传错了**不会有任何编译错误**，只有让对面真的收到才算验过。
    /// 只绑 127.0.0.1 的系统分配端口：不碰外网、不依赖 DNS、不占固定端口 ——
    /// 所以它不会变成"有时红有时绿"的测试。
    #[cfg(target_os = "windows")]
    fn loopback_server(
        response_body: &'static str,
    ) -> (
        u16,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑不上回环端口");
        let port = listener.local_addr().map(|a| a.port()).expect("取不到端口");
        let seen: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let seen_in_thread = seen.clone();
        let handle = std::thread::spawn(move || {
            // 只服务一条连接：能到这个 listener 的只有"直连"那条路
            // （系统代理会连到代理自己的地址，不会连到这里）。
            let Ok((mut s, _)) = listener.accept() else { return };
            s.set_read_timeout(Some(std::time::Duration::from_millis(500))).ok();
            let mut raw: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,  // 对端关了
                    Err(_) => break, // 静默 500ms ⇒ 请求发完了（本地回环够用）
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        // 收齐了才停：headers 结束 + Content-Length 指定的字节数
                        let text = String::from_utf8_lossy(&raw).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let need = seen_content_length(&text[..i]).unwrap_or(0);
                            if raw.len() >= i + 4 + need {
                                break;
                            }
                        }
                    }
                }
            }
            *seen_in_thread.lock().unwrap() =
                Some(String::from_utf8_lossy(&raw).to_string());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        });
        (port, seen, handle)
    }

    /// POST 真的发出一趟：verb、路径、Content-Length、body 字节、明文回环与端口。
    /// 这是"本机模型能用"的最小可信证据（`http://127.0.0.1:11434` 那类地址）。
    #[test]
    #[cfg(target_os = "windows")]
    fn post_over_loopback_sends_verb_length_and_body() {
        let payload = "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"你好\"}]}";
        let (port, seen, srv) = loopback_server("{\"ok\":true}");
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let got = http::post(
            &url,
            "Accept: application/json\r\n",
            payload.as_bytes(),
            3000,
            64 * 1024,
            // 幂等语义的传输自检：这里要的是"把请求发出去"，换路无所谓
            true,
        )
        .expect("回环 POST 应当成功");
        assert_eq!(got, b"{\"ok\":true}".to_vec(), "响应体应当原样收到");
        let raw = wait_for_capture(&seen);
        let head = raw.split("\r\n\r\n").next().unwrap_or("").to_string();
        assert!(
            head.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "请求行不对：{head}"
        );
        assert_eq!(
            seen_content_length(&head),
            Some(payload.len()),
            "Content-Length 必须等于 body 的字节数：{head}"
        );
        assert!(head.contains("Content-Type: application/json"), "{head}");
        assert!(head.contains("Accept: application/json"), "{head}");
        assert!(raw.ends_with(payload), "body 必须原样发出去：{raw}");
        // 把线上真实收到的东西打出来 —— 出问题时这是第一手证据
        println!("[实测] 回环服务器收到的 POST：\n{raw}");
        srv.join().ok();
    }

    /// GET 在线上仍然是 GET、头仍然是那一份、且**没有请求体** ——
    /// 检查更新 / 下载 / list_models 三个既有调用方靠这几条。
    #[test]
    #[cfg(target_os = "windows")]
    fn get_over_loopback_stays_plain_get() {
        let (port, seen, srv) = loopback_server("{\"ok\":1}");
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let got = http::get(&url, ACCEPT_JSON, 3000, 64 * 1024).expect("回环 GET 应当成功");
        assert_eq!(got, b"{\"ok\":1}".to_vec());
        let raw = wait_for_capture(&seen);
        let head = raw.split("\r\n\r\n").next().unwrap_or("").to_string();
        assert!(head.starts_with("GET /v1/models HTTP/1.1"), "请求行不对：{head}");
        assert!(head.contains("User-Agent: DeskBase\r\n"), "{head}");
        assert!(head.contains(&format!("Accept: {ACCEPT_JSON}\r\n")), "{head}");
        // 没有请求体 ⇒ headers 之后不该再有任何字节
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(body.is_empty(), "GET 不该带请求体：{body}");
        println!("[实测] 回环服务器收到的 GET：\n{raw}");
        srv.join().ok();
    }

    /// 策略本身（纯函数）：非幂等请求**不许换路**，且必须把"可能已经发出去了"
    /// 说给用户听；幂等请求照旧换下一条路。
    ///
    /// 这条测试不依赖任何网络环境，所以在任何机器上都是同一个结论 ——
    /// 这正是把"要不要重发"这个决定抽成纯函数的意义。
    #[test]
    #[cfg(target_os = "windows")]
    fn non_idempotent_failure_stops_and_warns_about_duplicate_send() {
        let (label, reason) = ("系统代理", "连不上 api.example.com：连不上服务器");
        // 没拿到应答：最要紧的那种 —— 请求可能已经到达服务器并被计费
        match http::after_failure(false, false, label, reason) {
            http::Step::Stop(m) => {
                assert!(m.contains("可能已经发出去了"), "{m}");
                assert!(m.contains("不会换另一条路重发"), "{m}");
                assert!(m.contains(label) && m.contains(reason), "{m}");
            }
            http::Step::Next(_) => panic!("非幂等请求不许换路重发 —— 那会重复计费"),
        }
        // 服务器已应答（403 / 429）：请求确实到了服务器，同样不重发
        match http::after_failure(false, true, label, reason) {
            http::Step::Stop(m) => assert!(m.contains("可能已经发出去了"), "{m}"),
            http::Step::Next(_) => panic!("非幂等请求不许换路重发"),
        }
        // 幂等请求：照旧把这条路的原因收集起来，换下一条路
        for answered in [false, true] {
            match http::after_failure(true, answered, label, reason) {
                http::Step::Next(p) => assert_eq!(p, format!("{label}：{reason}")),
                http::Step::Stop(_) => panic!("幂等请求应当换路再试（answered={answered}）"),
            }
        }
    }

    /// 不换路的 POST：**真的只试一条路**，报出来的是"可能已经发出去了"。
    ///
    /// 让第一路必然失败的方式：回环端口**接了立刻关** —— 客户端拿到"连接被重置"，
    /// 不用等一个超时（测试跑得快，才不会被人嫌弃着跳过）。
    /// 只 accept 一次：万一换路了，第二次连接会被拒（listener 随线程结束关闭），
    /// 断言照样成立。本机 `ProxyEnable=0`、CI 也没有系统代理，
    /// 所以第一条路不会被代理接走。
    #[test]
    #[cfg(target_os = "windows")]
    fn post_without_retry_tries_one_route_only() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑不上回环端口");
        let port = listener.local_addr().map(|a| a.port()).expect("取不到端口");
        let srv = std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                drop(s); // 立刻关：制造"请求可能已经发出去了、响应没拿到"
            }
        });
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let err = http::post(&url, "Accept: application/json\r\n", b"{}", 2000, 64 * 1024, false)
            .expect_err("被重置的连接不可能成功");
        // 这句话是给用户看的 —— 把它原样打出来，别只在断言里见过它
        println!("[实测] 不换路的报错原文：\n{err}");
        // 无论如何都不许出现"两条路都试过"的文案 —— 出现就是换路了
        assert!(!err.contains("都试过了"), "retry=false 不许走第二条路：{err}");
        if err.contains("服务器返回 HTTP") {
            // 第一条路被一个会应答的代理接走 ⇒ 服务器明确应答（Settled），
            // 按设计本来就不换路。（本机 ProxyEnable=0，正常走不到这个分支。）
        } else {
            assert!(err.contains("可能已经发出去了"), "{err}");
            assert!(err.contains("不会换另一条路重发"), "{err}");
            // 只报了第一条路：两个标签不会同时出现
            assert!(err.contains("系统代理") ^ err.contains("直连"), "只应报第一条路：{err}");
        }
        srv.join().ok();
    }

    /// 不换路的 POST 在第一条路能走通时，行为与原来完全一样。
    #[test]
    #[cfg(target_os = "windows")]
    fn post_without_retry_succeeds_on_first_route() {
        let (port, seen, srv) = loopback_server("{\"ok\":2}");
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let got = http::post(
            &url,
            "Accept: application/json\r\n",
            b"{\"a\":1}",
            3000,
            64 * 1024,
            false,
        )
        .expect("第一条路能走通时就该成功");
        assert_eq!(got, b"{\"ok\":2}".to_vec());
        let raw = wait_for_capture(&seen);
        assert!(raw.starts_with("POST /v1/chat/completions"), "{raw}");
        srv.join().ok();
    }
}

#[cfg(test)]
mod tests_stage {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskbase_stage_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个**内容真实**的包（不是塞几个 "x"）：这样 SHA-256 才有意义。
    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let f = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// 一次成功的暂存：三件事都要成立 —— 计划对了、文件解出来了、plan.json 写了。
    #[test]
    fn normal_case_stages_and_writes_plan() {
        let d = tmp("ok");
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let zip = d.join("pkg.zip");
        make_zip(
            &zip,
            &[
                ("deskbase.exe", b"NEW-EXE"),
                ("README.md", b"NEW-README"),
                ("startup.bat", b"should-not-be-taken"),
            ],
        );
        let size = std::fs::metadata(&zip).unwrap().len();
        let hash = sha256_file(&zip).unwrap();
        let sha_text = format!("{hash}  deskbase-0.2.2-windows-x64-portable.zip\n");

        let staging = staging_dir(&data, &v("0.2.2"));
        let staged = verify_and_stage_zip(
            &zip,
            &sha_text,
            size,
            &staging,
            &install,
            &data,
            &v("0.2.1"),
            &v("0.2.2"),
        )
        .expect("应当暂存成功");

        assert_eq!(staged.plan.from, "0.2.1");
        assert_eq!(staged.plan.to, "0.2.2");
        assert_eq!(staged.zip_bytes, size);

        // 白名单内的文件解出来了
        assert_eq!(std::fs::read(staging.join("deskbase.exe")).unwrap(), b"NEW-EXE");
        assert_eq!(std::fs::read(staging.join("README.md")).unwrap(), b"NEW-README");
        // 白名单外的**没有**落地
        assert!(!staging.join("startup.bat").exists(), "startup.bat 不该被解出来");

        // plan.json 写了，而且能被读回来（--apply-update 就靠它）
        let text = std::fs::read_to_string(staging.join("plan.json")).unwrap();
        let plan: ApplyPlan = serde_json::from_str(&text).unwrap();
        assert_eq!(plan.to, "0.2.2");
        assert!(plan.replace.contains(&"deskbase.exe".to_string()));

        // 最后一环：**真的替换一次**。这一整条（造包 → 校验 → 暂存 → 替换）全是本地操作，
        // 所以它能一直跑 —— 不依赖网络，也就不会"有时红有时绿"。
        // 安装目录里先放一个"旧版本"。
        std::fs::write(install.join("deskbase.exe"), b"OLD-EXE").unwrap();
        std::fs::write(install.join("README.md"), b"OLD-README").unwrap();
        let (backup, copied) = apply_update(&plan, &staging, &install).expect("替换应当成功");
        assert_eq!(copied.len(), plan.replace.len());
        // 装上去的正是校验过的那一份
        assert_eq!(std::fs::read(install.join("deskbase.exe")).unwrap(), b"NEW-EXE");
        assert_eq!(std::fs::read(install.join("README.md")).unwrap(), b"NEW-README");
        // 旧的那份留了备份，可回滚
        assert_eq!(std::fs::read(backup.join("deskbase.exe")).unwrap(), b"OLD-EXE");
        assert_eq!(std::fs::read(backup.join("README.md")).unwrap(), b"OLD-README");

        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn size_mismatch_must_reject() {
        let d = tmp("size");
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        let zip = d.join("pkg.zip");
        make_zip(&zip, &[("deskbase.exe", b"x")]);
        let size = std::fs::metadata(&zip).unwrap().len();
        let hash = sha256_file(&zip).unwrap();

        let staging = staging_dir(&data, &v("0.2.2"));
        let err = verify_and_stage_zip(
            &zip,
            &format!("{hash}\n"),
            size + 1, // 清单说的大一点
            &staging,
            &install,
            &data,
            &v("0.2.1"),
            &v("0.2.2"),
        )
        .unwrap_err();
        assert!(err.contains("大小与清单不符"), "{err}");
        assert!(err.contains("实际") && err.contains("清单"), "要把两个数都说出来：{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn checksum_mismatch_must_reject_and_report() {
        let d = tmp("sha");
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        let zip = d.join("pkg.zip");
        make_zip(&zip, &[("deskbase.exe", b"real-content")]);
        let size = std::fs::metadata(&zip).unwrap().len();

        let staging = staging_dir(&data, &v("0.2.2"));
        let err = verify_and_stage_zip(
            &zip,
            &format!("{}\n", "0".repeat(64)),
            size,
            &staging,
            &install,
            &data,
            &v("0.2.1"),
            &v("0.2.2"),
        )
        .unwrap_err();
        assert!(err.contains("校验和不符"), "{err}");
        assert!(err.contains("实际") && err.contains("应为"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn bad_checksum_format_refuses_to_continue() {
        let d = tmp("shafmt");
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        let zip = d.join("pkg.zip");
        make_zip(&zip, &[("deskbase.exe", b"x")]);
        let size = std::fs::metadata(&zip).unwrap().len();

        let staging = staging_dir(&data, &v("0.2.2"));
        let err = verify_and_stage_zip(
            &zip,
            "这不是校验和",
            size,
            &staging,
            &install,
            &data,
            &v("0.2.1"),
            &v("0.2.2"),
        )
        .unwrap_err();
        assert!(err.contains("64 位十六进制"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn no_executable_refuses_staging() {
        let d = tmp("noexe");
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        let zip = d.join("pkg.zip");
        make_zip(&zip, &[("README.md", b"only-docs")]);
        let size = std::fs::metadata(&zip).unwrap().len();
        let hash = sha256_file(&zip).unwrap();

        let staging = staging_dir(&data, &v("0.2.2"));
        let err = verify_and_stage_zip(
            &zip,
            &format!("{hash}\n"),
            size,
            &staging,
            &install,
            &data,
            &v("0.2.1"),
            &v("0.2.2"),
        )
        .unwrap_err();
        assert!(err.contains("deskbase.exe"), "{err}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn staging_under_data_dir_not_program_dir() {
        let data = PathBuf::from("C:/data/DeskBaseData");
        let s = staging_dir(&data, &v("0.3.0"));
        assert!(s.starts_with(&data), "暂存放数据目录：{}", s.display());
        assert!(s.ends_with("updates/0.3.0") || s.ends_with("updates\\0.3.0"), "{}", s.display());
    }

    #[test]
    fn download_url_uses_assets_endpoint() {
        let a = Asset { id: 571438036, name: "x.zip".into(), size: 1 };
        let u = asset_api_url(&a);
        assert!(u.contains("/releases/assets/571438036"), "{u}");
        assert!(!u.contains("browser_download_url"), "不该用 browser_download_url：这样第一跳就不在 api 上了");
    }
}

#[cfg(test)]
mod tests_e2e {
    use super::*;

    /// 端到端：真去 GitHub 取清单 → 挑版本 → 下载包 → 走完五步校验 → 落到暂存目录。
    ///
    /// **默认忽略**，手动跑：
    /// ```bash
    /// cd app && cargo test -- --ignored 端到端下载并暂存 --nocapture
    /// ```
    /// 会真的下载几 MB。安装目录与数据目录都用临时目录，**不会碰任何真实文件**。
    #[test]
    #[ignore = "需要网络且会真下载几 MB；手动用 cargo test -- --ignored 跑"]
    fn end_to_end_download_and_stage() {
        let releases = match fetch_releases() {
            Ok(r) => r,
            Err(e) if e.contains("rate limit") => {
                println!("⚠ 撞上 GitHub 限流，这条端到端验证这次跑不了（不是代码问题）：\n{e}");
                return;
            }
            Err(e) => panic!("取清单失败：{e}"),
        };

        // 用一个比全部都低的当前版本，保证"有更新"
        let current = Version::parse("0.0.1").unwrap();
        let picked = match check(&releases, &current, Channel::Prerelease) {
            CheckOutcome::Newer { tag, version, .. } => {
                println!("✔ 挑中 {tag}（{version}）");
                (tag, version)
            }
            other => panic!("本该有更新，实际：{other:?}"),
        };
        let _ = picked;

        let target_tag = match check(&releases, &current, Channel::Prerelease) {
            CheckOutcome::Newer { tag, .. } => tag,
            _ => unreachable!(),
        };
        let release = releases
            .iter()
            .find(|r| r.tag == target_tag)
            .expect("刚挑出来的发布应当还在清单里");

        let d = std::env::temp_dir().join(format!("deskbase_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let data = d.join("data");
        let install = d.join("install");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let staged = match download_and_stage(release, &install, &data, &current) {
            Ok(s) => s,
            // 两路（系统代理 / 直连）都是**传输层失败** → 这台机器到 GitHub 下载 CDN
            // 的路此刻不通（实测：直连 release-assets.githubusercontent.com 超时、
            // 系统代理挂着 —— 这不是代码问题，是网络环境）。如实打印，但**不把测试
            // 变红**：让它"随机红"的下场就是被无视，那样它连环境问题都报不出来了
            // （与上面 rate limit 同一条道理）。注意判据里排除了 HTTP 应答错误 ——
            // 服务器明确回了 4xx/5xx、或校验失败，都仍然是**必须红**的真问题。
            Err(e) if e.contains("系统代理与直连都试过了") && !e.contains("服务器返回 HTTP") => {
                println!(
                    "⚠ 这台机器到 GitHub 下载 CDN 的两条路都不通，本轮跳过下载验证\n\
                     （是网络环境问题，不是代码问题；检查更新的路径不受影响）：\n{e}"
                );
                return;
            }
            Err(e) => panic!("下载并暂存失败：{e}"),
        };

        println!("✔ 暂存到 {}", staged.dir);
        println!("  包 {} 字节，替换清单 {:?}", staged.zip_bytes, staged.plan.replace);

        let dir = PathBuf::from(&staged.dir);
        // ① 包真的落了盘
        assert!(dir.join("update.zip").exists());
        // ② 五步校验过了才会走到这里：可执行文件解出来了
        assert!(dir.join("deskbase.exe").exists(), "包里应当有 deskbase.exe 并已解出");
        // ③ 计划落了盘，且带版本号
        let text = std::fs::read_to_string(dir.join("plan.json")).unwrap();
        let plan: ApplyPlan = serde_json::from_str(&text).unwrap();
        assert_eq!(plan.from, "0.0.1");
        assert!(plan.replace.contains(&"deskbase.exe".to_string()));
        // ④ 校验和文件留了证
        assert!(dir.join("update.zip.sha256").exists());
        // ⑤ 解出来的 exe 应当是个真 PE（头两字节 "MZ"）
        let head = std::fs::read(dir.join("deskbase.exe")).unwrap();
        assert_eq!(&head[..2], b"MZ", "解出来的应当是真正的 Windows 可执行文件");

        // ⑥ 最后一步：**真的替换一次**。安装目录里先放一个"旧版本"。
        //    这里直接调 `apply_update` 而不是 `run_apply_update` —— 后者会顺带把程序拉起来，
        //    在这个测试里会弹出一个窗口。替换逻辑本身是同一个函数。
        std::fs::write(install.join("deskbase.exe"), b"OLD-VERSION-BYTES").unwrap();
        let (backup, copied) = apply_update(&plan, &dir, &install)
            .unwrap_or_else(|e| panic!("替换失败：{e}"));
        println!("✔ 替换了 {} 个文件，备份在 {}", copied.len(), backup.display());

        // 装上去的 exe 与暂存里的一致（即校验过的那一份）
        let staged_exe = std::fs::read(dir.join("deskbase.exe")).unwrap();
        let installed = std::fs::read(install.join("deskbase.exe")).unwrap();
        assert_eq!(installed, staged_exe, "装上去的应当正是校验过的那一份");
        assert_eq!(&installed[..2], b"MZ");
        // 旧的那份被备份下来了，可回滚
        assert_eq!(
            std::fs::read(backup.join("deskbase.exe")).unwrap(),
            b"OLD-VERSION-BYTES",
            "替换前的旧版本必须留在备份目录里，否则没法回滚"
        );

        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod tests_settings {
    use super::*;
    use crate::model::Db;

    // ⚠️ 目录必须**每个用例一份**：Rust 的测试是并行的，共用同一个目录时
    // 一个用例写进去的设置会被另一个用例读到（实测：更新档位被隔壁用例改成了
    // download_ask，「默认从不联网」这条红线断言就红了）。
    fn db() -> Db {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("dkb_updater_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Db::open(&d).unwrap()
    }

    #[test]
    fn defaults_to_never_and_stable_channel() {
        let c = db();
        let s = load_settings(&c);
        assert_eq!(s.mode, UpdateMode::Never, "「网络默认关闭」是红线级默认值，不许改松");
        assert_eq!(s.channel, Channel::Stable, "不能默认把人带上 beta");
    }

    #[test]
    fn settings_persist_and_repeat_save_safe() {
        let mut c = db();
        let s = UpdateSettings {
            mode: UpdateMode::DownloadAsk,
            channel: Channel::Prerelease,
        };
        save_settings(&mut c, &s).unwrap();
        assert_eq!(load_settings(&c).mode, UpdateMode::DownloadAsk);
        assert_eq!(load_settings(&c).channel, Channel::Prerelease);
        // UPSERT：再存一次不该报错
        save_settings(&mut c, &s).unwrap();
        assert_eq!(load_settings(&c).channel, Channel::Prerelease);
    }

    #[test]
    fn unknown_mode_falls_back_to_never() {
        let mut c = db();
        // 直接塞一个读不懂的档位值（原来靠 INSERT INTO sys_meta，现在写键值即可）
        c.meta_set("net.update.mode", "whatever").unwrap();
        assert_eq!(
            load_settings(&c).mode,
            UpdateMode::Never,
            "读不懂就倾向于不联网 —— 方向不能反"
        );
    }

    /// **这条是红线级的行为**：默认档位下必须一个包都不发。
    /// 能离线跑本身就是证明 —— 它没有进网络栈。
    #[test]
    fn never_mode_makes_no_network_call() {
        let c = db();
        let rep = check_by_settings(&c, &current_version()).unwrap();
        assert!(!rep.checked, "Never 档位下 checked 必须是 false");
        assert!(rep.outcome.is_none(), "不该有结果");
        let why = rep.reason.unwrap_or_default();
        assert!(why.contains("网络默认关闭"), "要说清为什么没检查：{why}");
        assert!(why.contains("手动下载"), "要给出替代路径，而不是只说不行：{why}");
    }

    #[test]
    fn mode_and_channel_strings_roundtrip() {
        for m in [
            UpdateMode::Never,
            UpdateMode::Notify,
            UpdateMode::DownloadAsk,
        ] {
            assert_eq!(UpdateMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(UpdateMode::parse("nope"), None);
        for ch in [Channel::Stable, Channel::Prerelease] {
            assert_eq!(Channel::parse(ch.as_str()), Some(ch));
        }
        assert_eq!(Channel::parse("nope"), None);
    }
}
