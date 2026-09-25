//! 截长图：把滚动中的屏幕区域一帧帧抓下来，按位移拼成一张长图。
//!
//! ## 抓屏走的是哪条路，为什么
//!
//! 需求上最优的是 **Windows Graphics Capture (WGC)**，其次是 **DXGI Desktop Duplication**，
//! 最后才是 GDI `BitBlt`。这里先用 GDI，理由不是"GDI 更好"，而是三条现实约束：
//!
//! 1. **零新增依赖**。本模块只用 `#[link]` 直接绑定 user32 / gdi32 里那几个从
//!    Windows 2000 起签名就没变过的函数，不需要 `windows` crate。WGC 与 DXGI 都是 COM
//!    接口，要引入 `windows` crate 的 `Graphics_Capture` / `Direct3D11` / `Win32_Graphics_Dxgi`
//!    等模块并手写大量 vtable 调用 —— 对一个要过审计的项目，这是真实成本。
//! 2. **能验证**。GDI 这条路本机真的跑得起来，测试也能在没有屏幕的环境里覆盖到除
//!    "真正抓一帧"以外的全部逻辑；WGC/DXGI 在 CI 上一行都验不了。
//! 3. 对**滚动截图**这个用途，GDI 的功能是够的：截图是用户主动触发、帧与帧之间有
//!    几百毫秒静止时间的操作，不像录屏那样要求 60fps 与零撕裂。
//!
//! ### 升级到 DXGI / WGC 要改什么
//!
//! **只有 `grab_impl` 一个函数需要换实现**，位移估计、拼接、PNG 编码与抓屏方式无关。
//!
//! · **DXGI Desktop Duplication**：`IDXGIOutputDuplication::AcquireNextFrame` 拿
//!   `IDXGIResource` → 转 `ID3D11Texture2D` → 拷到 staging texture → `Map` 读像素。
//!   典型失败场景是**远程桌面**（RDP 会话里 Duplication 直接返回 `DXGI_ERROR_UNSUPPORTED`）、
//!   虚拟机、以及笔记本的动态显卡切换 —— 所以它**只能当快路径，不能当唯一路径**，
//!   必须保留本文的 GDI 兜底。另外一个坑：Duplication 给出的坐标是**该输出（显示器）内**的，
//!   还带 0–3 的旋转标志，多显示器场景比桌面 DC 难处理得多。
//! · **WGC**：`IGraphicsCaptureItemInterop::CreateForMonitor` + `Direct3D11CaptureFramePool`。
//!   它取的是 DWM 合成后的画面，能拿到 GDI 拿不到的**硬件叠加层**（视频播放、部分游戏界面）。
//!   代价是需要 `windows` crate 的 `Graphics_Capture`/`Direct3D11` 模块，还要处理
//!   光标默认不抓（`IsCursorCaptureEnabled`）、以及 Win10 1803+ 的版本门槛。
//! · 落地顺序建议：**WGC → Duplication → GDI** 逐级降级，并把"这次走的是哪条路"
//!   写进界面（用户在自己机器上能看见），否则"某台机器截出来是黑的"这种问题无从排查。
//!
//! ### 为什么 GDI 也要先做越界检查
//!
//! `BitBlt` 对屏幕外的区域**不报错**，它给你一张全黑图。用户看到的是"长图截着截着变黑了"，
//! 完全不知道发生了什么。所以 `capture_region` 会先跟虚拟桌面求交，越界直接返回 Err
//! 并把虚拟桌面的真实范围写进消息里。
//!
//! ## 这个模块不做什么
//!
//! · **不合成输入**。滚动由调用方注入（WebView 里是 `scrollTop += n` 的 JS），本模块不调
//!   `SendInput`。这样"能截屏"与"能操作用户键鼠"是两件分开的事，少一个会被滥用的能力。
//! · **不写日志、不落临时文件、不出网**。长图里是用户屏幕上的全部内容（可能就有客户名单
//!   和身份证号），所以本模块不打印任何像素、不往系统临时目录丢中间文件、
//!   `save_png` 只写调用方给的路径。整个文件没有任何网络调用（CONTRIBUTING 第十节）。
//!
//! ## 已知的误差与未覆盖的情况（宁可不做也不假装能做对）
//!
//! · **位移只支持"向上滚"**。内容向下滚（用户往回翻）时估不出负位移，会被判成"没有位移"
//!   并提前结束 —— 结果是少截一截，而不是拼出一张错位的长图。
//! · **整数像素对齐，接缝可能有 ±1 像素的重复行或缺失行**。做完亚像素插值能消掉，
//!   代价是每一帧都要做一次重采样。可见程度：正文里肉眼看不出来；**表格边框、细分割线**
//!   上能看到一条比别处粗/细的线。要做的话在 `estimate_scroll_offset` 里用抛物线拟合
//!   峰值位置得到小数位移，再在 `stitch` 里按权重混合相邻两行。
//! · **有严格垂直周期的内容**（等行高的空表格、纯色横条）会把位移估成周期的小倍数 ——
//!   那些 d 的相关性一样高。真实页面里这种情况少见，但要记着。
//! · **不抓光标、不抓部分硬件叠加层**（与 GDI 的能力边界有关，见上）。
//! · 懒加载列表（滚动时才插入 DOM）在拼接处可能缺内容：不是滚动位移错了，
//!   而是那一屏本身还没加载出来。缓解手段是 `settle_ms` 等久一点。

// ⚠️ 部分接口尚未接线（2026-09-17）
//
// 已接进 IPC 的是：`capture_screen` / `save_png` / `monitors`（`capture.screen`
// 与 `capture.monitors` 两条命令）。**滚动的长图拼接整条链路还没接线** ——
// 它需要"UI 滚一屏 → Rust 抓一帧"的往返协议，而那要先在 UI 侧有滚动驱动。
//
// 所以这里暂时允许 dead_code。**这是一个有明确终止条件的豁免，不是永久静音**：
// 一旦长图协议接上（见 local-docs 的待办），这一行必须删掉。
// 之所以不直接删掉未用的函数：它们有 13 个测试钉着（含表头干扰、到底判定、
// 内存上限），删了就把这些已经想清楚的边界条件一起丢了。
#![allow(dead_code)]

use std::path::Path;

pub type Result<T> = std::result::Result<T, String>;

// ============================================================
// 上限：每一个都有依据，不是拍脑袋
// ============================================================

/// 默认最多抓 50 帧。
///
/// 2560×1440 的一帧是 14 MB 原始像素，50 帧就是 700 MB —— 再往上加，
/// 用户机器上的内存压力比"再多截 200 行"重要得多。到顶时会以
/// `StopReason::MaxFrames` 提前停下并告诉界面"没截完"。
pub const MAX_FRAMES_DEFAULT: usize = 50;

/// 拼出来的长图的字节上限。256 MB ≈ 2560 宽的屏约 25000 行 ≈ 17 屏。
///
/// 为什么不设更大：`Vec` 是几何扩容的，峰值会到容量的 2 倍左右，
/// 256 MB 的账实际上可能占 512 MB。超限时以 `StopReason::MemoryCap` 停下，
/// 而不是让进程被 OOM 干掉。
pub const MAX_STITCH_BYTES: usize = 256 * 1024 * 1024;

/// 单帧的像素数上限（≈ 8192×4096）。
///
/// 挡的是"坐标算错导致申请几 GB"这类事故：`CreateCompatibleBitmap` 对
/// 荒谬的尺寸会返回 NULL，但 32 位色深的那块内存是先要的。
pub const MAX_FRAME_PIXELS: u64 = 8192 * 4096;

/// 每帧之间默认等待的毫秒数。
///
/// WebView 的滚动带平滑动画（惯性），动画没停就抓，拿到的是模糊的中间帧，
/// 位移会估错。250 ms 是"用户不觉得卡"与"动画基本停住"之间的折中。
pub const DEFAULT_SETTLE_MS: u64 = 250;

/// 单次滚动的默认步长占视口高度的比例（3/5）。
///
/// 必须留出重叠：位移估计要用两帧的重叠区做相关，重叠太少方差就大到不可用。
/// 3/5 意味着还剩 40% 的重叠，扣掉上下固定条（各 1/10）也够。
const SCROLL_STEP_NUM: u32 = 3;
const SCROLL_STEP_DEN: u32 = 5;

/// 位移搜索上限占视口高度的比例（3/4）。比步长略大：步长是理想值，
/// 实际位移会被小数像素、懒加载撑开的内容、页面自身的吸附行为带偏一点。
const MAX_SHIFT_NUM: u32 = 3;
const MAX_SHIFT_DEN: u32 = 4;

/// 亮度分桶数。见 `row_signatures` 里"为什么是 8"。
const BUCKETS: usize = 8;

/// 上下各排除的固定条占帧高的比例（1/N）。见 `estimate_scroll_offset`。
const GUARD_DIV: u32 = 10;

/// 参与相关计算的最少行数。少于这个数，相关性估计的方差大到没有意义 ——
/// 宁可返回 `None`（调用方会当作"没有位移"）也不要给一个瞎猜的位移。
const MIN_OVERLAP_ROWS: u32 = 16;

/// 判定"匹配上了"的最低零均值归一化互相关分数。
///
/// 同一个页面滚动出来的两帧，真实位移处的 ZNCC 通常在 0.95 以上（像素级相同）；
/// 0.6 这条线挡的是"两帧毫无关系但碰巧有 0.5 左右相关"的情况。
/// 注意 `Some(0)`（两帧一模一样）也要过这条线，只是它的分数是 1.0。
const MIN_MATCH_SCORE: f64 = 0.6;

// ============================================================
// 帧
// ============================================================

/// 一帧 BGRA 像素（每像素 4 字节，顺序 B、G、R、A，与 GDI 的 32bpp DIB 一致）。
#[derive(Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    /// 一帧需要多少字节。
    pub fn byte_len(&self) -> usize {
        self.w as usize * self.h as usize * 4
    }

    /// 一行多少字节。
    pub fn row_bytes(&self) -> usize {
        self.w as usize * 4
    }

    /// 新的空帧（全 0；alpha 也是 0，真正抓屏时 `grab_impl` 会补成 255）。
    pub fn new(w: u32, h: u32) -> Frame {
        Frame {
            w,
            h,
            bgra: vec![0u8; w as usize * h as usize * 4],
        }
    }

    /// 尺寸与实际数据是否自洽。后面每个入口都要先过这一关，
    /// 否则拼接时会按下标越界 panic —— 那是崩溃，不是错误处理。
    fn check(&self) -> Result<()> {
        let need = self.byte_len();
        if self.w == 0 || self.h == 0 {
            return Err("帧的宽高必须大于 0".into());
        }
        if self.bgra.len() < need {
            return Err(format!(
                "帧数据不完整：{}×{} 需要 {} 字节，实际只有 {}",
                self.w,
                self.h,
                need,
                self.bgra.len()
            ));
        }
        Ok(())
    }
}

/// 故意**手写** Debug，不 derive。
///
/// 这个结构里装的是用户屏幕上的全部内容。一旦 derive，任何一句
/// `log(format!("{frame:?}"))` 就会把整屏像素（可能含客户名单、身份证号）写进日志文件。
/// 手写成只打印尺寸，这条路就堵死了。
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}×{}, {} 字节)", self.w, self.h, self.bgra.len())
    }
}

/// 屏幕矩形（虚拟桌面坐标，**物理像素**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// 一台显示器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    /// 在 `monitors()` 返回的数组里的下标，可以直接传给 `capture_screen(Some(i))`
    pub index: usize,
    pub primary: bool,
    /// 虚拟桌面坐标，物理像素
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// 系统报告的 DPI（96 = 100%，120 = 125%，144 = 150%，192 = 200%）
    pub dpi: u32,
}

impl Monitor {
    /// 缩放系数：1.0 / 1.25 / 1.5 / 2.0
    pub fn scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }
}

// ============================================================
// Windows 原生绑定（只有这里用 unsafe，其余全是纯逻辑）
// ============================================================

#[cfg(target_os = "windows")]
mod win {
    use super::{Frame, Result};
    use std::ffi::c_void;

    pub type Hwnd = *mut c_void;
    pub type Hdc = *mut c_void;
    pub type Hbitmap = *mut c_void;
    pub type Hgdiobj = *mut c_void;
    pub type Hmonitor = *mut c_void;
    pub type Hmodule = *mut c_void;

    /// `BitBlt` 的"直接拷贝"光栅操作码。
    ///
    /// 不加 `CAPTUREBLT`（0x40000000）：那个位是为了抓分层窗口（老式的 tooltip）用的，
    /// 在 DWM 合成之后反而会引入闪烁与部分窗口的重复绘制。现代 Windows 上桌面 DC
    /// 给的已经是合成后的画面，`SRCCOPY` 就是我们要的。
    pub const SRCCOPY: u32 = 0x00CC_0020;

    pub const DIB_RGB_COLORS: u32 = 0;
    pub const BI_RGB: u32 = 0;

    pub const SM_XVIRTUALSCREEN: i32 = 76;
    pub const SM_YVIRTUALSCREEN: i32 = 77;
    pub const SM_CXVIRTUALSCREEN: i32 = 78;
    pub const SM_CYVIRTUALSCREEN: i32 = 79;

    pub const LOGPIXELSX: i32 = 88;
    pub const MONITOR_DEFAULTTONEAREST: u32 = 2;
    pub const MDT_EFFECTIVE_DPI: u32 = 0;
    pub const MONITORINFOF_PRIMARY: u32 = 1;

    /// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`
    pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    pub struct MonitorInfo {
        pub cb_size: u32,
        pub rc_monitor: Rect,
        pub rc_work: Rect,
        pub flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct BitmapInfoHeader {
        pub bi_size: u32,
        pub bi_width: i32,
        pub bi_height: i32,
        pub bi_planes: u16,
        pub bi_bit_count: u16,
        pub bi_compression: u32,
        pub bi_size_image: u32,
        pub bi_x_pels_per_meter: i32,
        pub bi_y_pels_per_meter: i32,
        pub bi_clr_used: u32,
        pub bi_clr_important: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct RgbQuad {
        pub blue: u8,
        pub green: u8,
        pub red: u8,
        pub reserved: u8,
    }

    #[repr(C)]
    pub struct BitmapInfo {
        pub header: BitmapInfoHeader,
        pub colors: [RgbQuad; 1],
    }

    pub type MonitorEnumProc =
        unsafe extern "system" fn(Hmonitor, Hdc, *mut Rect, *mut c_void) -> i32;

    // 静态导入的这几个都是 Windows 2000 起就存在的导出函数，导入表里一定有。
    // 后加的（DPI 相关的）走 GetProcAddress，见 `ensure_dpi_aware`。
    #[link(name = "user32")]
    extern "system" {
        pub fn GetDC(hwnd: Hwnd) -> Hdc;
        pub fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
        pub fn GetSystemMetrics(index: i32) -> i32;
        pub fn SetProcessDPIAware() -> i32;
        pub fn EnumDisplayMonitors(
            hdc: Hdc,
            clip: *const Rect,
            cb: MonitorEnumProc,
            data: *mut c_void,
        ) -> i32;
        pub fn GetMonitorInfoW(monitor: Hmonitor, info: *mut MonitorInfo) -> i32;
        pub fn MonitorFromPoint(pt: Point, flags: u32) -> Hmonitor;
    }

    #[link(name = "gdi32")]
    extern "system" {
        pub fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
        pub fn DeleteDC(hdc: Hdc) -> i32;
        pub fn CreateCompatibleBitmap(hdc: Hdc, w: i32, h: i32) -> Hbitmap;
        pub fn SelectObject(hdc: Hdc, obj: Hgdiobj) -> Hgdiobj;
        pub fn DeleteObject(obj: Hgdiobj) -> i32;
        pub fn BitBlt(
            dst: Hdc,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            src: Hdc,
            sx: i32,
            sy: i32,
            rop: u32,
        ) -> i32;
        pub fn GetDIBits(
            hdc: Hdc,
            bmp: Hbitmap,
            start: u32,
            lines: u32,
            bits: *mut c_void,
            info: *mut BitmapInfo,
            usage: u32,
        ) -> i32;
        pub fn GetDeviceCaps(hdc: Hdc, index: i32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn LoadLibraryW(name: *const u16) -> Hmodule;
        pub fn GetProcAddress(module: Hmodule, name: *const u8) -> *mut c_void;
    }

    /// 句柄的 RAII 守卫。
    ///
    /// 为什么不用 `let _ = ...` 手工释放：抓 50 帧意味着 150 个 GDI 句柄，
    /// 中间任何一条提前 `return` 漏掉释放，句柄就会在进程生命周期里累积
    /// （每个进程的 GDI 句柄配额是 10000）。Drop 保证出错路径也释放。
    pub struct ScreenDc(pub Hdc);
    impl ScreenDc {
        pub fn new() -> Result<Self> {
            let h = unsafe { GetDC(std::ptr::null_mut()) };
            if h.is_null() {
                Err("GetDC(整个桌面) 失败".into())
            } else {
                Ok(ScreenDc(h))
            }
        }
    }
    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe { ReleaseDC(std::ptr::null_mut(), self.0) };
        }
    }

    pub struct MemDc(pub Hdc);
    impl MemDc {
        pub fn new(src: Hdc) -> Result<Self> {
            let h = unsafe { CreateCompatibleDC(src) };
            if h.is_null() {
                Err("CreateCompatibleDC 失败".into())
            } else {
                Ok(MemDc(h))
            }
        }
    }
    impl Drop for MemDc {
        fn drop(&mut self) {
            unsafe { DeleteDC(self.0) };
        }
    }

    pub struct Bitmap(pub Hbitmap);
    impl Bitmap {
        pub fn new(src: Hdc, w: u32, h: u32) -> Result<Self> {
            let b = unsafe { CreateCompatibleBitmap(src, w as i32, h as i32) };
            if b.is_null() {
                Err(format!("CreateCompatibleBitmap({w}×{h}) 失败：显存/内存不足"))
            } else {
                Ok(Bitmap(b))
            }
        }
    }
    impl Drop for Bitmap {
        fn drop(&mut self) {
            unsafe { DeleteObject(self.0 as Hgdiobj) };
        }
    }

    /// C 字符串 → UTF-16（带结尾 0）
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 从已加载的模块里取一个函数地址。取不到就返回 null（调用方降级）。
    pub fn proc(module: &str, name: &str) -> *mut c_void {
        let mut buf: Vec<u8> = name.bytes().collect();
        buf.push(0);
        unsafe {
            let m = LoadLibraryW(wide(module).as_ptr());
            if m.is_null() {
                return std::ptr::null_mut();
            }
            // 故意不 FreeLibrary：模块要活到进程结束，否则缓存的函数指针会悬空
            GetProcAddress(m, buf.as_ptr())
        }
    }

    /// 真正抓一帧。这里是整个模块唯一碰屏幕的地方。
    pub fn grab(x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
        let screen = ScreenDc::new()?;
        let mem = MemDc::new(screen.0)?;
        let bmp = Bitmap::new(screen.0, w, h)?;

        unsafe {
            // 位图必须先选进 DC 才能当 BitBlt 的目标；出作用域前要换回来，
            // 否则 DeleteObject（Drop 里）对"仍被 DC 选中"的位图会失败
            let old = SelectObject(mem.0, bmp.0 as Hgdiobj);

            // 源坐标是**虚拟桌面坐标**，副屏在主屏左边时是负数 —— 这里不做任何
            // 假设，负数直接传给 BitBlt（GDI 本来就支持）。
            let copied = BitBlt(mem.0, 0, 0, w as i32, h as i32, screen.0, x, y, SRCCOPY);

            let out = if copied == 0 {
                Err(format!("BitBlt 失败（区域 {w}×{h} @ {x},{y}）"))
            } else {
                read_pixels(mem.0, bmp.0, w, h)
            };

            if !old.is_null() {
                SelectObject(mem.0, old);
            }
            out
        }
    }

    fn read_pixels(dc: Hdc, bmp: Hbitmap, w: u32, h: u32) -> Result<Frame> {
        unsafe {
            let mut info: BitmapInfo = std::mem::zeroed();
            info.header.bi_size = std::mem::size_of::<BitmapInfoHeader>() as u32;
            info.header.bi_width = w as i32;
            // 负高度 = 自上而下。用正数的话第 0 行是屏幕最下面那行，
            // 拼出来的长图会整张倒过来 —— 一个非常容易漏掉、又非常显眼的坑。
            info.header.bi_height = -(h as i32);
            info.header.bi_planes = 1;
            info.header.bi_bit_count = 32;
            info.header.bi_compression = BI_RGB;

            let mut bgra = vec![0u8; w as usize * h as usize * 4];
            let lines = GetDIBits(
                dc,
                bmp,
                0,
                h,
                bgra.as_mut_ptr() as *mut c_void,
                &mut info,
                DIB_RGB_COLORS,
            );
            if lines <= 0 {
                return Err("GetDIBits 失败".into());
            }
            if lines as u32 != h {
                return Err(format!("只取到 {lines} 行，期望 {h} 行"));
            }

            // GDI 的 32bpp DIB 不定义 alpha（实测通常是 0）。
            // 不补 255 的话，PNG 会是一张**全透明**的图：用户双击打开一片空白，
            // 还以为是软件坏了。这是这条路上最隐蔽的一个坑。
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }

            Ok(Frame { w, h, bgra })
        }
    }
}

// ============================================================
// DPI：不感知 DPI 的话，抓到的像素和逻辑坐标对不上
// ============================================================

#[cfg(target_os = "windows")]
/// 抓一帧（**公开入口**）。
///
/// 为什么不让外面直接调 `win::grab`：`win` 是平台实现细节，
/// 一旦有人从别处伸手进去，将来换实现（文档里说过"只有 grab_impl 需要换"）
/// 就得去改所有调用点。这里留一个**窄口子**，换实现只动这一行。
pub fn grab_frame(x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
    win::grab(x, y, w, h)
}
mod dpi {
    use super::win;

    /// 让本进程变成 **per-monitor DPI aware**。
    ///
    /// 为什么必须做：进程不感知 DPI 时，Windows 会对窗口做**位图拉伸**，
    /// `GetSystemMetrics` 返回的是缩放后的逻辑尺寸，`BitBlt` 抓到的是被系统插值过、
    /// 糊掉的画面。在 150% 的屏上，结果是"截出来的图又糊又缺一块"。
    ///
    /// 逐级降级的原因：`SetProcessDpiAwarenessContext` 是 Win10 1703 才有的导出函数，
    /// 静态导入会让导入表在更老的系统上直接启动失败；所以走 GetProcAddress，
    /// 取不到就退到 Win8.1 的 `shcore!SetProcessDpiAwareness`，再退到 Vista 的
    /// `SetProcessDPIAware()`。
    ///
    /// 三种都失败是**正常的**：只要进程已经设过 DPI 感知（比如 manifest 里写死，
    /// 或者 tao/wry 在启动时设过），这里就会全部失败，而行为正是我们想要的。
    pub fn ensure_aware() {
        // Once：这件事只该做一次，而且必须在**任何坐标计算之前**做
        // （DPI 感知一旦设定，调用线程的坐标空间就定了，不能再改）
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            let p = win::proc("user32.dll", "SetProcessDpiAwarenessContext");
            if !p.is_null() {
                let f: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(p);
                if f(win::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 {
                    return;
                }
            }
            let p = win::proc("shcore.dll", "SetProcessDpiAwareness");
            if !p.is_null() {
                // PROCESS_PER_MONITOR_DPI_AWARE = 2，返回 S_OK(0) 才算成功
                let f: unsafe extern "system" fn(u32) -> i32 = std::mem::transmute(p);
                if f(2) == 0 {
                    return;
                }
            }
            win::SetProcessDPIAware();
        });
    }

    /// 系统 DPI（每英寸像素数），拿不到时按 96 处理。
    fn system_dpi() -> u32 {
        unsafe {
            let dc = win::GetDC(std::ptr::null_mut());
            if dc.is_null() {
                return 96;
            }
            let v = win::GetDeviceCaps(dc, win::LOGPIXELSX);
            win::ReleaseDC(std::ptr::null_mut(), dc);
            if v > 0 {
                v as u32
            } else {
                96
            }
        }
    }

    /// 某个显示器（用它的 HMONITOR 句柄表示）上的 DPI。
    fn dpi_of_monitor(monitor: win::Hmonitor) -> u32 {
        let p = win::proc("shcore.dll", "GetDpiForMonitor");
        if !p.is_null() {
            let f: unsafe extern "system" fn(
                win::Hmonitor,
                u32,
                *mut u32,
                *mut u32,
            ) -> i32 = unsafe { std::mem::transmute(p) };
            let (mut dx, mut dy) = (96u32, 96u32);
            // 注意：进程不是 DPI 感知时，这个函数永远返回 96 并只写一条调试输出 ——
            // 所以调用方必须先 ensure_aware()
            let hr = unsafe { f(monitor, win::MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            if hr == 0 && dx > 0 {
                return dx;
            }
        }
        system_dpi()
    }

    /// 包含某个点的显示器上的 DPI。
    ///
    /// 用 MONITOR_DEFAULTTONEAREST 而不是"必须命中"：多显示器之间有一圈
    /// 虚拟桌面不覆盖的空隙，点落在那里时也要有个结果，而不是失败。
    pub fn dpi_at(x: i32, y: i32) -> u32 {
        unsafe {
            let pt = win::Point { x, y };
            let m = win::MonitorFromPoint(pt, win::MONITOR_DEFAULTTONEAREST);
            if m.is_null() {
                return system_dpi();
            }
            dpi_of_monitor(m)
        }
    }

    /// 枚举所有显示器。
    pub fn enumerate() -> super::Result<Vec<super::Monitor>> {
        unsafe extern "system" fn cb(
            monitor: win::Hmonitor,
            _dc: win::Hdc,
            _rect: *mut win::Rect,
            data: *mut std::ffi::c_void,
        ) -> i32 {
            let list = &mut *(data as *mut Vec<win::Hmonitor>);
            list.push(monitor);
            1 // 返回 1 = 继续枚举
        }

        let mut handles: Vec<win::Hmonitor> = Vec::new();
        let ok = unsafe {
            win::EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                cb,
                &mut handles as *mut Vec<win::Hmonitor> as *mut std::ffi::c_void,
            )
        };
        if ok == 0 {
            return Err("EnumDisplayMonitors 失败".into());
        }

        let mut out = Vec::with_capacity(handles.len());
        for (i, h) in handles.iter().enumerate() {
            let mut info: win::MonitorInfo = unsafe { std::mem::zeroed() };
            info.cb_size = std::mem::size_of::<win::MonitorInfo>() as u32;
            if unsafe { win::GetMonitorInfoW(*h, &mut info) } == 0 {
                continue; // 单个显示器查不到就跳过，不影响其余
            }
            let r = info.rc_monitor;
            out.push(super::Monitor {
                index: i,
                primary: info.flags & win::MONITORINFOF_PRIMARY != 0,
                x: r.left,
                y: r.top,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
                dpi: dpi_of_monitor(*h),
            });
        }
        // 下标重排：跳过失败的显示器后 index 要对得上数组位置
        for (i, m) in out.iter_mut().enumerate() {
            m.index = i;
        }
        Ok(out)
    }

    /// 虚拟桌面范围（所有显示器并起来的那块，副屏在主屏左边时 x 是负数）
    pub fn virtual_screen() -> super::Result<super::Rect> {
        unsafe {
            let x = win::GetSystemMetrics(win::SM_XVIRTUALSCREEN);
            let y = win::GetSystemMetrics(win::SM_YVIRTUALSCREEN);
            let w = win::GetSystemMetrics(win::SM_CXVIRTUALSCREEN);
            let h = win::GetSystemMetrics(win::SM_CYVIRTUALSCREEN);
            if w <= 0 || h <= 0 {
                // 远程桌面断开、会话被锁时会出现这种情况：没有可抓的桌面
                return Err(format!(
                    "拿不到虚拟桌面尺寸（{w}×{h}）—— 远程桌面断开或没有可用的显示输出时会出现"
                ));
            }
            Ok(super::Rect {
                x,
                y,
                w: w as u32,
                h: h as u32,
            })
        }
    }
}

// ============================================================
// 抓屏（公开 API）
// ============================================================

/// 抓一帧屏幕区域。
///
/// 内部走 GDI `BitBlt`（为什么不用 WGC/DXGI 见文件头的说明）。调用前会确保
/// 进程已经 DPI 感知，否则 125%/150% 缩放下拿到的是被系统插值过的糊图。
#[cfg(target_os = "windows")]
fn grab_impl(x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
    win::grab(x, y, w, h)
}

#[cfg(not(target_os = "windows"))]
fn grab_impl(_x: i32, _y: i32, _w: u32, _h: u32) -> Result<Frame> {
    Err("截屏目前只实现了 Windows（本项目的目标平台）".into())
}

/// 让本进程变成 DPI 感知的。幂等，可以随便调。
///
/// 必须在创建窗口/任何坐标换算**之前**至少调一次（`main.rs` 里在 tao 建窗口前调最稳）。
/// 本模块的其他入口自己也会调，但那是兜底，不是"不用管了"。
pub fn ensure_dpi_aware() {
    #[cfg(target_os = "windows")]
    dpi::ensure_aware();
}

/// 抓取主显示器（`None`）或第 `display` 台显示器的整个屏幕。
///
/// 下标来自 `monitors()`。传了不存在的下标会报错并带上本机的显示器数量 ——
/// 比默默抓主屏好排查。
pub fn capture_screen(display: Option<usize>) -> Result<Frame> {
    let all = monitors()?;
    let m = match display {
        None => all
            .iter()
            .find(|m| m.primary)
            .ok_or_else(|| "系统没有报告主显示器".to_string())?,
        Some(i) => all.get(i).ok_or_else(|| {
            format!("显示器 {i} 不存在：本机共 {} 台（下标 0..{}）", all.len(), all.len().saturating_sub(1))
        })?,
    };
    capture_region(m.x, m.y, m.w, m.h)
}

/// 抓取一个矩形区域。
///
/// **坐标是虚拟桌面的物理像素**：多显示器时副屏原点可能是负的（副屏在主屏左边
/// 就是负 x），这里不做任何"≥ 0"的假设。要在多显示器上全覆盖，就用
/// `virtual_screen_bounds()` 的结果去算。
///
/// 越界**不给黑图**，直接返回 Err 并把虚拟桌面的实际范围写进消息里 —— 原因见文件头。
pub fn capture_region(x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
    if w == 0 || h == 0 {
        return Err("抓屏区域的宽高必须大于 0".into());
    }
    let pixels = w as u64 * h as u64;
    if pixels > MAX_FRAME_PIXELS {
        let mb = pixels * 4 / 1024 / 1024;
        return Err(format!(
            "抓屏区域过大：{w}×{h} = {pixels} 像素（约 {mb} MB），上限 {MAX_FRAME_PIXELS} 像素"
        ));
    }

    // 先让进程 DPI 感知，再算坐标 —— 顺序反了的话 GetSystemMetrics 给的是逻辑值
    ensure_dpi_aware();

    let v = virtual_screen_bounds()?;
    let (x1, y1) = (x as i64 + w as i64, y as i64 + h as i64);
    let (vx1, vy1) = (v.x as i64 + v.w as i64, v.y as i64 + v.h as i64);
    if (x as i64) < v.x as i64 || (y as i64) < v.y as i64 || x1 > vx1 || y1 > vy1 {
        return Err(format!(
            "区域 {w}×{h} @ ({x},{y}) 超出虚拟桌面 ({},{}) {}×{} —— 多显示器时副屏在主屏左边原点是负的，\
             请用 virtual_screen_bounds() 的结果来算",
            v.x, v.y, v.w, v.h
        ));
    }

    grab_impl(x, y, w, h)
}

/// 枚举所有显示器（顺序与 `capture_screen(Some(i))` 的下标一致）。
pub fn monitors() -> Result<Vec<Monitor>> {
    #[cfg(target_os = "windows")]
    {
        ensure_dpi_aware();
        dpi::enumerate()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("枚举显示器目前只实现了 Windows".into())
    }
}

/// 虚拟桌面范围：(x, y, 宽, 高)。**x/y 可能是负的**，见 `capture_region`。
pub fn virtual_screen_bounds() -> Result<Rect> {
    #[cfg(target_os = "windows")]
    {
        dpi::virtual_screen()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("取虚拟桌面范围目前只实现了 Windows".into())
    }
}

/// 包含某个点的显示器上的缩放系数：1.0 / 1.25 / 1.5 / 2.0。
///
/// 用途：把 WebView 里 `getBoundingClientRect()` 给的 CSS 像素换成物理像素，
/// 见 `css_rect_to_physical`。
pub fn monitor_scale_at(x: i32, y: i32) -> f64 {
    #[cfg(target_os = "windows")]
    {
        ensure_dpi_aware();
        dpi::dpi_at(x, y) as f64 / 96.0
    }
    #[cfg(not(target_os = "windows"))]
    {
        1.0
    }
}

/// 把 CSS 像素矩形换算成 `capture_region` 需要的物理像素矩形。
///
/// 为什么需要它：`getBoundingClientRect()` 回来的是 CSS 像素，在 150% 缩放下与屏幕
/// 物理像素差 1.5 倍。直接把 CSS 坐标丢给 `capture_region`，截到的是窗口左上角
/// 的一部分，而且因为尺寸也不对，整张图是错的。
///
/// 用**左下与右上分别取整再相减**，而不是"起点取整 + 尺寸取整"：后者在 125% 这种
/// 非整数缩放下会系统性少一像素（100 CSS px → 125 物理 px，尺寸取整没错，
/// 但起点 12.5 取整后终点就对不上了）。
pub fn css_rect_to_physical(x: f64, y: f64, w: f64, h: f64, scale: f64) -> (i32, i32, u32, u32) {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let x0 = (x * s).round() as i32;
    let y0 = (y * s).round() as i32;
    let x1 = ((x + w) * s).round() as i32;
    let y1 = ((y + h) * s).round() as i32;
    (x0, y0, (x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32)
}

// ============================================================
// 位移估计
// ============================================================

/// 横向分桶的下标。测试与估计器共用这一个函数，避免两边各写一遍边界、
/// 结果测试和实现对不上（那种测试会一直绿，但绿得没有意义）。
fn bucket_index(x: u32, w: u32) -> usize {
    let b = (x as u64 * BUCKETS as u64) / (w as u64).max(1);
    b.min((BUCKETS - 1) as u64) as usize
}

/// 把每一行压成 8 个"横向分桶的灰度均值"，作为这一行的签名。
///
/// 为什么这么压：
/// · **纵向不降采样**。按步长抽行会让"非整数倍步长的位移"混叠 —— 位移估出来直接是错的，
///   而这是这个模块最容易悄悄出错的地方。所以纵向一行都不省。
/// · **横向分 8 桶而不是整行一个均值**。整行一个均值时，大段纯色（表格空白、纯背景）
///   会让很多行的签名几乎相同，位移就没有唯一解了。8 桶保留了"这一行的横向亮度分布"，
///   判别力够用，计算量也只是每像素一次加法。
/// · 亮度用 Rec.601 的整数近似（77/150/29 之和是 256，右移 8 位即可），
///   比转 f32 再乘系数快，精度对相关性来说绰绰有余。
fn row_signatures(f: &Frame) -> Vec<f32> {
    let w = f.w as usize;
    let h = f.h as usize;
    let stride = w * 4;
    let mut sig = vec![0f32; h * BUCKETS];

    // 桶宽度可能差一列（帧宽不是 8 的倍数时），所以先数一遍每桶有多少列，
    // 再按实际列数取平均 —— 否则不同桶的量级不一样，相关性会被这个假差异带偏
    let mut counts = [0u32; BUCKETS];
    for x in 0..w {
        counts[bucket_index(x as u32, f.w)] += 1;
    }
    let inv: Vec<f64> = counts.iter().map(|c| 1.0 / (*c).max(1) as f64).collect();

    for y in 0..h {
        let row = &f.bgra[y * stride..y * stride + stride];
        let mut sums = [0f64; BUCKETS];
        for x in 0..w {
            let i = x * 4;
            let luma =
                (77 * row[i + 2] as u32 + 150 * row[i + 1] as u32 + 29 * row[i] as u32) >> 8;
            sums[bucket_index(x as u32, f.w)] += luma as f64;
        }
        for b in 0..BUCKETS {
            sig[y * BUCKETS + b] = (sums[b] * inv[b]) as f32;
        }
    }
    sig
}

/// 两张帧之间垂直方向的位移估计（像素，内容向上滚了多少）。
///
/// 返回值：
/// · `Some(0)`  —— 两帧一模一样，没有滚动（包括"已经滚到底了"）
/// · `Some(d)`  —— 内容向上滚了 d 像素
/// · `None`     —— 找不到可信的对应关系（页面在动画中间、换了内容、相关分数不够）
///
/// **为什么不能只比较第一行**（题目里点名的坑）：滚动区域顶部常有一条固定表头
/// （粘性导航、工具栏），它在每一帧里都长在同一个位置。用"上一帧的第一行在下一帧
/// 出现在哪里"来估位移，会永远得到 0，也就是"没滚动"，长图就只有第一屏。
///
/// 这里的做法是**零均值归一化互相关（ZNCC）**，并且：
/// · 上下各排除 `h/10` 的固定条（粘性表头与底部状态栏的高发区），
///   于是表头既不参与也无法污染估计；
/// · 相关系数对整体亮度变化不敏感（归一化了），截图时窗口失焦变灰也不会估错；
/// · 搜索范围包含 d=0，所以"完全没动"会自然地返回 `Some(0)`，而不是靠一个
///   临时阈值去猜。
pub fn estimate_scroll_offset(prev: &Frame, next: &Frame, max_shift: u32) -> Option<u32> {
    if prev.w != next.w || prev.h != next.h || prev.check().is_err() || next.check().is_err() {
        return None;
    }
    let (w, h) = (prev.w, prev.h);
    if w < BUCKETS as u32 || h < 4 {
        return None;
    }
    let max_shift = max_shift.min(h - 1);
    if max_shift == 0 {
        return None;
    }

    // 上下固定条：1/10 的依据是常见粘性导航 64–96 px、视口高 800–1440 px；
    // 再往上调会把有效重叠区砍太多，信噪比反而变差。h/3 是上限保护，
    // 避免"很矮的帧"里把内容区也排掉。
    let guard = (h / GUARD_DIV).max(1).min(h / 3);
    let foot = guard;

    let a = row_signatures(prev);
    let b = row_signatures(next);

    let mut best: Option<(f64, u32)> = None;
    for d in 0..=max_shift {
        // 上一帧的行下标 y 必须满足：y 与 y-d 都落在内容区里（都排除掉固定条）。
        // 于是下界是 guard + d（y-d ≥ guard），上界是 h - foot。
        let lo = (guard + d) as usize;
        let hi = (h - foot) as usize;
        if hi <= lo + MIN_OVERLAP_ROWS as usize {
            // 重叠太少，估计不可信 —— 宁可没有结果也不给瞎猜的
            continue;
        }

        let (mut sa, mut sb, mut saa, mut sbb, mut sab, mut n) = (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
        for y in lo..hi {
            let ra = &a[y * BUCKETS..y * BUCKETS + BUCKETS];
            let rb = &b[(y - d as usize) * BUCKETS..(y - d as usize) * BUCKETS + BUCKETS];
            for k in 0..BUCKETS {
                let (va, vb) = (ra[k] as f64, rb[k] as f64);
                sa += va;
                sb += vb;
                saa += va * va;
                sbb += vb * vb;
                sab += va * vb;
                n += 1.0;
            }
        }
        if n <= 0.0 {
            continue;
        }
        let (ma, mb) = (sa / n, sb / n);
        let cov = sab - n * ma * mb;
        let vaa = saa - n * ma * ma;
        let vbb = sbb - n * mb * mb;
        // 任一边方差为 0（整片纯色）时相关系数没有定义，跳过这个候选
        if vaa <= 1e-9 || vbb <= 1e-9 {
            continue;
        }
        let score = cov / (vaa.sqrt() * vbb.sqrt());
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, d));
        }
    }

    match best {
        Some((score, d)) if score >= MIN_MATCH_SCORE => Some(d),
        _ => None,
    }
}

// ============================================================
// 拼接
// ============================================================

/// 把多帧按位移拼成一张长图，返回 `(图像, 有效高度)`。
///
/// `offsets[i]` 的含义是**第 i 帧相对第 i-1 帧向上滚了多少像素**；
/// `offsets[0]` 是基准帧、会被忽略（调用方常常填 0，也可能填别的，都不影响结果 ——
/// 这样调用方不用为了对齐下标而多做一次特判）。
///
/// 每帧只追加"新进来的那 d 行"，也就是它的**底部** d 行 —— 内容向上滚，
/// 新内容从下面进来。
///
/// 位移为 0 的帧不追加（追加 0 行本来也是空操作，但显式跳过更清楚，
/// 也避免了"接缝重复一行"的可能）。`offsets` 比 `frames` 短时缺的那些按 0 处理。
///
/// 超过 `MAX_STITCH_BYTES` 时会**截断**（不是报错也不是 OOM）：返回的高度就是
/// 真正拼出来的高度，调用方据此知道"没拼完"。
pub fn stitch(frames: &[Frame], offsets: &[u32]) -> Result<(Frame, u32)> {
    let first = frames.first().ok_or_else(|| "没有帧可以拼接".to_string())?;
    first.check()?;
    let row = first.row_bytes();

    // 先按位移算出总高度，一次分配 —— 逐帧扩容会让内存峰值到实际需求的 2 倍
    let mut extra: u64 = 0;
    for (k, f) in frames.iter().enumerate().skip(1) {
        if f.w != first.w || f.h != first.h {
            return Err(format!(
                "第 {k} 帧尺寸不一致：{}×{}，第一帧是 {}×{} —— 抓屏区域中途变了，拼起来会整体错行",
                f.w, f.h, first.w, first.h
            ));
        }
        f.check()?;
        let d = offsets.get(k).copied().unwrap_or(0);
        // 位移 ≥ 帧高说明两帧根本没有重叠，接缝一定错位。这里按"最多只剩 1 行重叠"
        // 处理，而不是 panic 或静默产出一张重复的图。
        extra += d.min(first.h.saturating_sub(1)) as u64;
    }

    let cap_rows = (MAX_STITCH_BYTES / row.max(1)) as u64;
    let want = first.h as u64 + extra;
    let out_h = want.min(cap_rows).max(1) as u32;

    let mut buf: Vec<u8> = Vec::with_capacity(out_h as usize * row);
    // 上限比一帧还小（截取区域很大而内存上限很小）时只保留放得下的那部分，
    // 而不是"复制了一帧却声称高度更小"
    let first_rows = first.h.min(out_h) as usize;
    buf.extend_from_slice(&first.bgra[..first_rows * row]);
    let mut valid = first_rows as u32;

    for (k, f) in frames.iter().enumerate().skip(1) {
        let d = offsets
            .get(k)
            .copied()
            .unwrap_or(0)
            .min(first.h.saturating_sub(1));
        if d == 0 {
            continue; // 没有新内容，不追加
        }
        if valid + d > out_h {
            break; // 触到内存上限：截断，由调用方通过返回的高度知道"没拼完"
        }
        let start = (first.h - d) as usize * row;
        buf.extend_from_slice(&f.bgra[start..start + d as usize * row]);
        valid += d;
    }

    // 触顶时 buf 就是 valid 行，直接返回
    Ok((Frame { w: first.w, h: valid, bgra: buf }, valid))
}

// ============================================================
// 长截图：抓 → 估位移 → 拼 → 滚，循环
// ============================================================

/// 为什么停下来的。**必须报给界面**：不报的话用户拿到一张短了一截的长图，
/// 只会以为"这软件截漏了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// 连着两帧都没有位移 —— 页面已经到底（或者根本没在滚）
    ReachedBottom,
    /// 到了帧数上限
    MaxFrames,
    /// 再加一帧就会超过内存上限
    MemoryCap,
}

impl StopReason {
    /// 给界面显示的一句话（中文，直接可用）
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::ReachedBottom => "已滚动到底，长图完整",
            StopReason::MaxFrames => "已达到帧数上限，长图可能不完整（可调大上限后重试）",
            StopReason::MemoryCap => "已达到内存上限，长图提前结束（可缩小截取区域后重试）",
        }
    }
}

/// 长截图的参数。
#[derive(Debug, Clone, Copy)]
pub struct LongShotOptions {
    /// 抓取区域（虚拟桌面物理像素）。先在界面上量好要截哪块，再传进来。
    pub region: Rect,
    /// 每次滚动的像素数。默认是区域高的 3/5，必须留在 `max_shift` 以内。
    pub scroll_step: u32,
    /// 位移搜索上限。默认是区域高的 3/4。
    pub max_shift: u32,
    /// 最多抓几帧
    pub max_frames: usize,
    /// 每帧之间等多久（毫秒），见 `DEFAULT_SETTLE_MS`
    pub settle_ms: u64,
    /// 位移估不出来时补等几次（页面还在动画里就会这样）
    pub settle_retries: u32,
    /// 长图的字节上限
    pub max_bytes: usize,
}

impl LongShotOptions {
    pub fn new(region: Rect) -> Self {
        let h = region.h.max(1);
        LongShotOptions {
            region,
            scroll_step: (h * SCROLL_STEP_NUM / SCROLL_STEP_DEN).max(1),
            max_shift: (h * MAX_SHIFT_NUM / MAX_SHIFT_DEN).max(1),
            max_frames: MAX_FRAMES_DEFAULT,
            settle_ms: DEFAULT_SETTLE_MS,
            settle_retries: 2,
            max_bytes: MAX_STITCH_BYTES,
        }
    }
}

/// 一次长截图的结果。
#[derive(Debug)]
pub struct LongShot {
    pub image: Frame,
    /// 有效高度（等于 `image.h`；单独给一份是为了调用方表达式写起来短一点，
    /// 也是"截断发生时图像里有多少行是有效的"这件事的显式出口）
    pub height: u32,
    pub frames_used: usize,
    /// 每帧相对前一帧的位移，`offsets[0]` 恒为 0。排查"接缝错了"时很有用。
    pub offsets: Vec<u32>,
    pub stop: StopReason,
}

/// 累加器的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Stop(StopReason),
}

/// 拼接累加器：把"还要不要继续抓"的判定与屏幕彻底解耦。
///
/// 为什么单独抽出来：帧数上限、内存上限、"连续两帧没动就算到底"这三条是本模块
/// 最需要被测到的逻辑，而它们本来会长在抓屏循环里 —— 那样的代码在没屏幕的 CI 上
/// 一行都测不了。抽出来之后，测试可以直接喂合成帧。
pub struct Stitcher {
    image: Frame,
    /// 抓进来的原始帧的尺寸。**不能拿 `image.h` 来比** —— 长图是越拼越高的，
    /// 用长图的高去校验下一帧，会把每一帧都当成"尺寸变了"而全部丢掉。
    src_w: u32,
    src_h: u32,
    offsets: Vec<u32>,
    no_move: u32,
    max_frames: usize,
    max_bytes: usize,
    stop: Option<StopReason>,
}

impl Stitcher {
    /// 第一帧即基准帧，它的高度先算进长图里。
    pub fn new(first: Frame, max_frames: usize, max_bytes: usize) -> Result<Self> {
        first.check()?;
        Ok(Stitcher {
            src_w: first.w,
            src_h: first.h,
            image: first,
            offsets: vec![0],
            no_move: 0,
            max_frames: max_frames.max(1),
            max_bytes,
            stop: None,
        })
    }

    pub fn frames_used(&self) -> usize {
        self.offsets.len()
    }

    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    pub fn stop(&self) -> Option<StopReason> {
        self.stop
    }

    /// 喂一帧。`shift` 来自 `estimate_scroll_offset`：
    /// `Some(0)` 或 `None` 都算"这一帧没有带来新内容"。
    pub fn push(&mut self, next: &Frame, shift: Option<u32>) -> Step {
        // 尺寸变了（用户中途改了窗口大小）：这一帧丢掉，但也要算进帧数，
        // 否则一个不断变化尺寸的窗口会让循环永远转下去
        if next.w != self.src_w || next.h != self.src_h || next.check().is_err() {
            self.no_move += 1;
            self.offsets.push(0);
            return self.finish_checks();
        }

        let d = match shift {
            None | Some(0) => {
                self.no_move += 1;
                self.offsets.push(0);
                return self.finish_checks();
            }
            Some(d) => d.min(self.src_h.saturating_sub(1)),
        };

        // 内存上限：先算账再追加，绝不"先拼上再说" —— 那正是 OOM 的来源
        let row = self.image.row_bytes().max(1);
        let cap_rows = self.max_bytes / row;
        if self.image.h as usize + d as usize > cap_rows {
            return self.stop_with(StopReason::MemoryCap);
        }

        let start = (self.src_h - d) as usize * row;
        self.image
            .bgra
            .extend_from_slice(&next.bgra[start..start + d as usize * row]);
        self.image.h += d;
        self.offsets.push(d);
        self.no_move = 0;
        self.finish_checks()
    }

    fn finish_checks(&mut self) -> Step {
        if self.frames_used() >= self.max_frames {
            return self.stop_with(StopReason::MaxFrames);
        }
        // 连续两次没有位移 = 到底了。为什么是两次而不是一次：单次"没动"可能是
        // 平滑滚动的动画还没停、或者页面在等异步内容，再给一次机会更稳。
        if self.no_move >= 2 {
            return self.stop_with(StopReason::ReachedBottom);
        }
        Step::Continue
    }

    fn stop_with(&mut self, r: StopReason) -> Step {
        self.stop = Some(r);
        Step::Stop(r)
    }

    pub fn finish(self) -> LongShot {
        let h = self.image.h;
        LongShot {
            image: self.image,
            height: h,
            frames_used: self.offsets.len(),
            offsets: self.offsets,
            stop: self.stop.unwrap_or(StopReason::ReachedBottom),
        }
    }
}

/// 抓长图。`scroll` 由调用方提供：给它一个像素数，它负责让页面向上滚那么多。
///
/// 为什么滚动不在这里做：WebView 里最稳的方式是注入 JS（`scrollTop += n`），
/// 而原生窗口则是 `SendMessage(WM_VSCROLL)` —— 这两件事都只有调用方知道该怎么做。
/// 更重要的是，本模块因此**不需要合成输入事件**（不碰 SendInput），
/// "能截屏"与"能操作用户键鼠"就成了两个分开的能力。
///
/// 调用方要先保证页面已经在顶部。
pub fn capture_long_region<F>(opts: &LongShotOptions, mut scroll: F) -> Result<LongShot>
where
    F: FnMut(u32) -> Result<()>,
{
    let r = opts.region;
    let first = capture_region(r.x, r.y, r.w, r.h)?;
    let mut st = Stitcher::new(first.clone(), opts.max_frames, opts.max_bytes)?;
    let mut prev = first;

    loop {
        scroll(opts.scroll_step)?;
        std::thread::sleep(std::time::Duration::from_millis(opts.settle_ms));

        let mut frame = capture_region(r.x, r.y, r.w, r.h)?;
        let mut shift = estimate_scroll_offset(&prev, &frame, opts.max_shift);

        // 估不出来时别急着当成"没动"：多半是平滑滚动的动画还没停。
        // 补等几次再下结论 —— 否则一个还没跑完的动画就会让长图提前结束。
        let mut tries = 0;
        while shift.is_none() && tries < opts.settle_retries {
            std::thread::sleep(std::time::Duration::from_millis(opts.settle_ms));
            frame = capture_region(r.x, r.y, r.w, r.h)?;
            shift = estimate_scroll_offset(&prev, &frame, opts.max_shift);
            tries += 1;
        }

        let step = st.push(&frame, shift);
        prev = frame;
        if let Step::Stop(_) = step {
            return Ok(st.finish());
        }
    }
}

// ============================================================
// PNG 编码（自己写，不引图像库）
// ============================================================

/// 把 BGRA 帧编码成 PNG 字节。
///
/// 为什么自己写：PNG 的"最小可用编码"只需要 zlib 流 + CRC32，加起来两百行；
/// 为此引一个图像库（image/png 都带一堆解码器与 unsafe 依赖）不划算，
/// 尤其是这个项目还要过依赖审计。输出格式是 8 位 RGBA（真彩色+alpha）。
///
/// 压缩用的是 **deflate 固定 Huffman + LZ77**（不是"存储"块）。截图里大片纯色、
/// 相邻行完全相同的情况极多，压缩通常能到 1/5 ~ 1/20；不压的话 2560 宽的长图
/// 动辄几百 MB，用户的数据目录会被一张图撑爆。
pub fn encode_png(frame: &Frame) -> Result<Vec<u8>> {
    frame.check()?;
    let w = frame.w as usize;
    let h = frame.h as usize;
    let stride = w * 4;

    // ---- 逐行滤波 ----
    // 只在 None(0) 与 Up(2) 之间二选一：屏幕截图里"这一行和上一行一样"极常见，
    // Up 会把它们整行变成 0，后面的 LZ77 几乎零成本存下。
    // 不用 Sub/Average/Paeth：它们要按 bpp（4 字节）偏移去算，收益在截图上很小，
    // 而实现错一个字节就会让解码器读出花屏。宁可选笨的。
    let mut raw: Vec<u8> = Vec::with_capacity((stride + 1) * h);
    let mut up_row = vec![0u8; stride];
    for y in 0..h {
        let row = &frame.bgra[y * stride..y * stride + stride];
        let mut use_up = false;
        if y > 0 {
            let prev = &frame.bgra[(y - 1) * stride..y * stride];
            let mut sum_up = 0u32;
            for i in 0..stride {
                let d = row[i].wrapping_sub(prev[i]);
                up_row[i] = d;
                // 规范附件的经验规则：把差分当有符号字节取绝对值求和
                sum_up += (d as i8).unsigned_abs() as u32;
            }
            let sum_none: u32 = row.iter().map(|b| (*b as i8).unsigned_abs() as u32).sum();
            use_up = sum_up < sum_none;
        }
        if use_up {
            raw.push(2); // 滤波器类型 Up
            raw.extend_from_slice(&up_row);
        } else {
            raw.push(0); // 滤波器类型 None（第一行只能用它）
            raw.extend_from_slice(row);
        }
    }

    // ---- zlib 包一层 ----
    let mut z = Vec::with_capacity(raw.len() / 4 + 64);
    z.extend_from_slice(&[0x78, 0x9C]); // CMF/FLG：deflate、32K 窗口、默认压缩级别
    z.extend_from_slice(&deflate_fixed(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    // ---- PNG 容器 ----
    let mut out = Vec::with_capacity(z.len() + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(frame.w).to_be_bytes());
    ihdr.extend_from_slice(&(frame.h).to_be_bytes());
    ihdr.push(8); // 位深
    ihdr.push(6); // 颜色类型 6 = 真彩色 + alpha（与 BGRA 一一对应）
    ihdr.push(0); // 压缩方法（固定 0）
    ihdr.push(0); // 滤波方法（固定 0）
    ihdr.push(0); // 非隔行
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &z);
    push_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// 标准 CRC-32（多项式 0xEDB88320，初值全 1、结果取反）。PNG 每个块都要。
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// zlib 流的校验和（用的是**未压缩**的数据）。
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 是 zlib 里的经典分块长度：保证 a、b 在 u32 里不会溢出
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

// ---- deflate：固定 Huffman + LZ77 ----

const WINDOW: usize = 32768; // deflate 允许的最大回溯距离
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
/// 哈希链上最多看几个候选。截图的行间重复很规律，看多了收益很小、耗时线性增长。
const MAX_CHAIN: usize = 24;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// deflate 的比特流是**低位先出**的，但 Huffman 码本身是**高位先写**的，
/// 所以写码之前要把码字反过来。这个函数就是干这个的。
fn reverse_bits(mut v: u32, n: u8) -> u32 {
    let mut r = 0;
    for _ in 0..n {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    buf: u32,
    n: u32,
}

impl BitWriter<'_> {
    /// 写 n 位"原始位"（低位先出）：额外位、块头都用它
    fn bits(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 16 && (n == 0 || v < (1u32 << n)));
        self.buf |= (v & ((1u32 << n) - 1)) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push((self.buf & 0xFF) as u8);
            self.buf >>= 8;
            self.n -= 8;
        }
    }
    /// 写一个已经反好序的 Huffman 码
    fn code(&mut self, reversed: u32, len: u8) {
        self.bits(reversed, len as u32);
    }
    fn flush(&mut self) {
        if self.n > 0 {
            self.out.push((self.buf & 0xFF) as u8);
            self.buf = 0;
            self.n = 0;
        }
    }
}

/// 固定 Huffman 表（RFC 1951 3.2.6）：字面量/长度符号 → (码字, 码长)
fn fixed_code(sym: u16) -> (u32, u8) {
    let (code, len) = if sym < 144 {
        (0x30 + sym as u32, 8)
    } else if sym < 256 {
        (0x190 + (sym as u32 - 144), 9)
    } else if sym < 280 {
        (sym as u32 - 256, 7)
    } else {
        (0xC0 + (sym as u32 - 280), 8)
    };
    (reverse_bits(code, len), len)
}

/// 三个字节的哈希（乘一个奇数再取高位，图的是分布均匀）
fn hash3(b: &[u8]) -> usize {
    let v = (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16);
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// 一个 deflate 块：BFINAL=1、BTYPE=01（固定 Huffman），内部贪心 LZ77。
fn deflate_fixed(src: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(src.len() / 4 + 1024);
    let n = src.len();
    let mut head = vec![u32::MAX; HASH_SIZE];
    let mut prev = vec![u32::MAX; WINDOW];

    {
        let mut bw = BitWriter {
            out: &mut out,
            buf: 0,
            n: 0,
        };
        bw.bits(1, 1); // BFINAL
        bw.bits(1, 2); // BTYPE = 01

        let mut i = 0usize;
        while i < n {
            let mut best_len = 0usize;
            let mut best_dist = 0usize;

            if i + MIN_MATCH <= n {
                let h = hash3(&src[i..]);
                let limit = i.saturating_sub(WINDOW);
                let max_len = (n - i).min(MAX_MATCH);
                let mut cand = head[h];
                let mut guard = 0;
                while cand != u32::MAX {
                    let c = cand as usize;
                    if c < limit || guard >= MAX_CHAIN {
                        break;
                    }
                    // 先比一个字节再决定要不要深入比，省掉大量无谓比较
                    if best_len < max_len && src[c + best_len] == src[i + best_len] {
                        let mut l = 0;
                        while l < max_len && src[c + l] == src[i + l] {
                            l += 1;
                        }
                        if l > best_len {
                            best_len = l;
                            best_dist = i - c;
                            if l == max_len {
                                break;
                            }
                        }
                    }
                    cand = prev[c & (WINDOW - 1)];
                    guard += 1;
                }
                // 当前位置必须挂进链里，否则后面的位置根本找不到它
                prev[i & (WINDOW - 1)] = head[h];
                head[h] = i as u32;
            }

            if best_len >= MIN_MATCH {
                write_match(&mut bw, best_len, best_dist);
                // 匹配区间里的位置也要挂链：不挂的话后续匹配会明显变差
                for k in 1..best_len {
                    let p = i + k;
                    if p + MIN_MATCH <= n {
                        let h = hash3(&src[p..]);
                        prev[p & (WINDOW - 1)] = head[h];
                        head[h] = p as u32;
                    }
                }
                i += best_len;
            } else {
                let (code, len) = fixed_code(src[i] as u16);
                bw.code(code, len);
                i += 1;
            }
        }

        let (eob, eob_len) = fixed_code(256);
        bw.code(eob, eob_len);
        bw.flush();
    }

    out
}

fn write_match(bw: &mut BitWriter<'_>, len: usize, dist: usize) {
    // 长度
    let mut li = 0usize;
    for k in 0..LENGTH_BASE.len() {
        if LENGTH_BASE[k] as usize <= len {
            li = k;
        }
    }
    let (code, bits) = fixed_code(257 + li as u16);
    bw.code(code, bits);
    if LENGTH_EXTRA[li] > 0 {
        bw.bits((len - LENGTH_BASE[li] as usize) as u32, LENGTH_EXTRA[li] as u32);
    }

    // 距离：5 位码，码字同样要反序
    let mut di = 0usize;
    for k in 0..DIST_BASE.len() {
        if DIST_BASE[k] as usize <= dist {
            di = k;
        }
    }
    bw.code(reverse_bits(di as u32, 5), 5);
    if DIST_EXTRA[di] > 0 {
        bw.bits((dist - DIST_BASE[di] as usize) as u32, DIST_EXTRA[di] as u32);
    }
}

/// 把长图写成 PNG 文件。
///
/// **路径由调用方给**，本函数不决定存哪里，也不落任何临时文件。产品的约定是存到
/// `<用户数据目录>/captures/` 下 —— 那是用户自己的目录，不是系统临时目录
/// （临时目录里别人/别的程序能读到，而且会被清理工具删掉）。
///
/// 与 xlsx 模块同一条策略：**只写新文件，绝不覆盖**。同名的文件很可能正是用户
/// 上一次要留的那张，覆盖等于替他做决定。
pub fn save_png(frame: &Frame, path: &Path) -> Result<()> {
    if path.exists() {
        return Err(format!("文件已存在，不覆盖：{}", path.display()));
    }
    let bytes = encode_png(frame)?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败：{e}"))?;
        }
    }
    std::fs::write(path, &bytes).map_err(|e| format!("写入失败：{e}"))
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试帧的宽（能被 8 整除，横向分桶的边界整齐）
    const TW: u32 = 640;
    /// 测试帧的高。要和 `GUARD_DIV` 配合：h/10 = 24 必须 > 表头行数，
    /// 否则"排除表头"这件事根本没被测到
    const TH: u32 = 240;
    /// 固定的表头占多少行（必须 < TH/GUARD_DIV = 24）
    const THEADER: usize = 20;
    /// 每次滚动多少行
    const TD: u32 = 17;
    /// 合成文档的总行数
    const TDOC: usize = 1400;

    /// 合成一页"文档"的灰度内容，保证**每一行的 8 桶签名几乎独一无二**：
    /// 除了与 x、y 都有关的纹理，还把行号 y 的低 8 位画成 8 个竖条
    /// （第 b 个竖条的亮/暗 = y 的第 b 位）。这样"位移 N 行"才有唯一解 ——
    /// 如果只是纯色或者整行同值，很多行长得一样，测试断言就等于没测。
    fn synth_doc() -> Vec<Vec<u8>> {
        (0..TDOC)
            .map(|y| {
                (0..TW)
                    .map(|x| {
                        let b = bucket_index(x, TW as u32);
                        let bit = (y >> b) & 1;
                        let tex = ((x.wrapping_mul(37) ^ (y as u32).wrapping_mul(101)) % 24) as u8;
                        if bit == 1 {
                            190 + tex
                        } else {
                            30 + tex
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// 固定不动的表头：每一行都长得一样，而且在文档里不会出现。
    /// 这正是真实页面上的粘性导航 —— 上半部分（前几行）做成醒目的渐变。
    fn synth_header() -> Vec<Vec<u8>> {
        (0..THEADER)
            .map(|_| (0..TW).map(|x| (20 + (x * 5 % 200)) as u8).collect())
            .collect()
    }

    /// 造一帧：顶部 `THEADER` 行是固定表头，下面是从文档第 `start` 行开始的内容。
    fn make_frame(doc: &[Vec<u8>], header: &[Vec<u8>], start: usize) -> Frame {
        let mut f = Frame::new(TW, TH);
        for y in 0..TH as usize {
            let row: &[u8] = if y < header.len() {
                &header[y]
            } else {
                &doc[start + y]
            };
            for x in 0..TW as usize {
                let v = row[x];
                let i = (y * TW as usize + x) * 4;
                f.bgra[i] = v;
                f.bgra[i + 1] = v;
                f.bgra[i + 2] = v;
                f.bgra[i + 3] = 255;
            }
        }
        f
    }

    /// 内容向上滚 `d` 行后的下一帧
    fn scrolled(doc: &[Vec<u8>], header: &[Vec<u8>], start: usize, d: u32) -> Frame {
        make_frame(doc, header, start + d as usize)
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * f.w as usize + x as usize) * 4;
        [f.bgra[i], f.bgra[i + 1], f.bgra[i + 2], f.bgra[i + 3]]
    }

    #[test]
    fn 位移估计等于真实滚动的行数() {
        let doc = synth_doc();
        let header = synth_header();
        for d in [1u32, 2, 17, 40, 63] {
            let a = make_frame(&doc, &header, 100);
            let b = scrolled(&doc, &header, 100, d);
            assert_eq!(
                estimate_scroll_offset(&a, &b, 64),
                Some(d),
                "内容向上滚了 {d} 行，估计出来的必须就是 {d}"
            );
        }
    }

    #[test]
    fn 顶部有固定表头时不被表头骗() {
        let doc = synth_doc();
        let header = synth_header();
        let d = 23u32;
        let a = make_frame(&doc, &header, 100);
        let b = scrolled(&doc, &header, 100, d);

        // 先证明这个陷阱是真的存在：两帧的第一行完全一样（表头没动），
        // 所以"找第一行在哪里重现"这种朴素做法只会得到 0（= 没滚动）
        assert_eq!(
            px(&a, 0, 0),
            px(&b, 0, 0),
            "表头是固定的，第一行在两帧里当然一样"
        );
        assert_eq!(px(&a, 0, (THEADER - 1) as u32), px(&b, 0, (THEADER - 1) as u32));

        let got = estimate_scroll_offset(&a, &b, 64);
        assert_ne!(got, Some(0), "被表头骗了：以为没滚动");
        assert_eq!(got, Some(d), "排除掉表头之后应该能估出真实位移");
    }

    #[test]
    fn 没有滚动时估计为零() {
        let doc = synth_doc();
        let header = synth_header();
        let a = make_frame(&doc, &header, 100);
        let b = make_frame(&doc, &header, 100); // 完全一样的一帧
        assert_eq!(estimate_scroll_offset(&a, &b, 64), Some(0));
    }

    #[test]
    fn 三帧拼接高度等于帧高加两倍位移() {
        let doc = synth_doc();
        let header = synth_header();

        // 帧序列：0, 17, 34 行处
        let frames = vec![
            make_frame(&doc, &header, 100),
            scrolled(&doc, &header, 100, TD),
            scrolled(&doc, &header, 100, TD * 2),
        ];

        // 位移由估计器给（不是硬编码），所以这条测试连"估计→拼接"整条链一起测了
        let d1 = estimate_scroll_offset(&frames[0], &frames[1], 64).expect("第 2 帧应该估得出位移");
        let d2 = estimate_scroll_offset(&frames[1], &frames[2], 64).expect("第 3 帧应该估得出位移");
        assert_eq!((d1, d2), (TD, TD));

        let offsets = vec![0, d1, d2];
        let (img, valid) = stitch(&frames, &offsets).unwrap();
        assert_eq!(valid, TH + 2 * TD);
        assert_eq!(img.h, TH + 2 * TD, "长图高度 = 帧高 + 2×位移");
        assert_eq!(img.w, TW);

        // 接缝两侧应当是连续的同一份内容：长图第 TH-1 行 = 第 2 帧的 (TH-1-d1) 行
        assert_eq!(px(&img, 5, TH - 1), px(&frames[1], 5, TH - d1 - 1));

        // 也走一遍累加器（驱动循环里用的就是它），结果必须一致
        let mut st = Stitcher::new(frames[0].clone(), 50, MAX_STITCH_BYTES).unwrap();
        assert_eq!(st.push(&frames[1], Some(d1)), Step::Continue);
        let shot = {
            let step = st.push(&frames[2], Some(d2));
            assert_eq!(step, Step::Continue, "两帧都在滚动，不该停");
            st.finish()
        };
        assert_eq!(shot.height, TH + 2 * TD);
        assert_eq!(shot.frames_used, 3);
        assert_eq!(shot.offsets, vec![0, TD, TD]);
    }

    #[test]
    fn 位移为零时不重复追加() {
        let doc = synth_doc();
        let header = synth_header();
        let frames = vec![
            make_frame(&doc, &header, 100),
            make_frame(&doc, &header, 100),
            make_frame(&doc, &header, 100),
        ];
        let (img, valid) = stitch(&frames, &[0, 0, 0]).unwrap();
        assert_eq!(valid, TH, "位移全 0 时长图就是一帧高");
        assert_eq!(img.h, TH);
        assert_eq!(img.bgra.len(), TH as usize * img.row_bytes());
        // 内容也必须和一帧完全一致（不是把同一帧叠了三遍）
        assert_eq!(px(&img, 7, 233), px(&frames[0], 7, 233));
    }

    #[test]
    fn 超过最大帧数时提前停止() {
        let doc = synth_doc();
        let header = synth_header();
        let max_frames = 5usize;

        let mut st = Stitcher::new(make_frame(&doc, &header, 0), max_frames, MAX_STITCH_BYTES)
            .unwrap();
        assert_eq!(st.frames_used(), 1, "基准帧先算一帧");

        let mut start = 0usize;
        let mut stopped = None;
        for _ in 1..=8 {
            start += TD as usize;
            let f = make_frame(&doc, &header, start);
            let step = st.push(&f, Some(TD)); // 每帧都在滚动，不会被"到底"提前结束
            if let Step::Stop(r) = step {
                stopped = Some(r);
                break;
            }
        }
        let reason = stopped.expect("抓够 5 帧就应该停下来");
        assert_eq!(st.frames_used(), max_frames, "停在帧数上限上，不多不少");
        assert_eq!(reason, StopReason::MaxFrames);

        let shot = st.finish();
        assert_eq!(shot.stop, StopReason::MaxFrames);
        assert_eq!(shot.frames_used, max_frames);
        assert_eq!(shot.height, TH + (max_frames as u32 - 1) * TD);
        assert!(shot.stop.as_str().contains("上限"));
    }

    #[test]
    fn 超过内存上限时提前停止并说明() {
        let doc = synth_doc();
        let header = synth_header();

        // 上限只够放 300 行（第一帧 240 行，每滚一次加 17 行）
        let row = TW as usize * 4;
        let cap_rows = 300usize;
        let max_bytes = row * cap_rows;

        let mut st = Stitcher::new(make_frame(&doc, &header, 0), 50, max_bytes).unwrap();
        let mut start = 0usize;
        let mut reason = None;
        for _ in 0..10 {
            start += TD as usize;
            let f = make_frame(&doc, &header, start);
            if let Step::Stop(r) = st.push(&f, Some(TD)) {
                reason = Some(r);
                break;
            }
        }
        assert_eq!(reason, Some(StopReason::MemoryCap));
        let shot = st.finish();
        assert_eq!(shot.stop, StopReason::MemoryCap);
        assert!(
            shot.height as usize <= cap_rows,
            "不能超过上限：{} > {cap_rows}",
            shot.height
        );
        // 到上限那一帧必须**没有**被拼进去（否则就超了）
        assert!(shot.height as usize + TD as usize > cap_rows);
        assert_eq!(shot.image.bgra.len(), shot.height as usize * row);
        assert!(shot.stop.as_str().contains("内存"));
    }

    #[test]
    fn 连续两帧没有位移判定为到底() {
        let doc = synth_doc();
        let header = synth_header();
        let mut st = Stitcher::new(make_frame(&doc, &header, 0), 50, MAX_STITCH_BYTES).unwrap();
        let a = make_frame(&doc, &header, 0);
        assert_eq!(st.push(&a, Some(0)), Step::Continue, "第一次没动还不该停");
        assert_eq!(
            st.push(&a, None),
            Step::Stop(StopReason::ReachedBottom),
            "连着两次没动就是到底了"
        );
        assert_eq!(st.finish().height, TH, "没位移的帧不追加任何行");
    }

    #[test]
    fn 尺寸不同的帧不能拼接() {
        let doc = synth_doc();
        let header = synth_header();
        let a = make_frame(&doc, &header, 0);
        let mut b = make_frame(&doc, &header, TD as usize);
        b.w -= 8; // 模拟抓屏中途区域变了
        let err = stitch(&[a, b], &[0, TD]).unwrap_err();
        assert!(err.contains("尺寸不一致"), "实际错误：{err}");
    }

    /// 测试用的最小 inflate：只认固定 Huffman 块（我们自己只写这一种）。
    ///
    /// 为什么要在测试里**再写一个解码器**：编码器里码表写错时，光看"魔数对不对"
    /// 是发现不了的 —— 得换个方向把数据读回来才知道码表对不对。CI 上没有浏览器、
    /// 也没有图像库，这是能做到的最强校验。（本机开发时另外用 Node 的 zlib 验过一遍。）
    fn inflate_fixed(src: &[u8]) -> Result<Vec<u8>> {
        struct Bits<'a> {
            d: &'a [u8],
            pos: usize,
            bit: u32,
        }
        impl Bits<'_> {
            fn bits(&mut self, n: u32) -> Result<u32> {
                let mut v = 0u32;
                for k in 0..n {
                    if self.pos >= self.d.len() {
                        return Err("比特流提前结束".into());
                    }
                    let b = (self.d[self.pos] >> self.bit) & 1;
                    v |= (b as u32) << k; // 低位先出
                    self.bit += 1;
                    if self.bit == 8 {
                        self.bit = 0;
                        self.pos += 1;
                    }
                }
                Ok(v)
            }
            /// 读一个固定 Huffman 符号（码字是高位先来的）
            fn symbol(&mut self) -> Result<u16> {
                let mut code = 0u32;
                for len in 1..=9u32 {
                    code = (code << 1) | self.bits(1)?;
                    match len {
                        7 if code <= 23 => return Ok(256 + code as u16),
                        8 if (48..=191).contains(&code) => return Ok((code - 48) as u16),
                        8 if (192..=199).contains(&code) => return Ok((280 + code - 192) as u16),
                        9 if code >= 400 => return Ok((144 + code - 400) as u16),
                        _ => {}
                    }
                }
                Err("非法 Huffman 码".into())
            }
        }

        if src.len() < 6 {
            return Err("zlib 流太短".into());
        }
        let mut br = Bits {
            d: &src[2..src.len() - 4], // 去掉 zlib 头与尾部 adler32
            pos: 0,
            bit: 0,
        };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let bfinal = br.bits(1)?;
            let btype = br.bits(2)?;
            if btype != 1 {
                return Err(format!("只支持固定 Huffman 块，遇到 BTYPE={btype}"));
            }
            loop {
                let sym = br.symbol()?;
                match sym {
                    0..=255 => out.push(sym as u8),
                    256 => break,
                    257..=285 => {
                        let li = (sym - 257) as usize;
                        let len = LENGTH_BASE[li] as u32 + br.bits(LENGTH_EXTRA[li] as u32)?;
                        let mut dsym = 0usize;
                        for _ in 0..5 {
                            dsym = (dsym << 1) | br.bits(1)? as usize;
                        }
                        let dist =
                            DIST_BASE[dsym] as u32 + br.bits(DIST_EXTRA[dsym] as u32)?;
                        if dist as usize > out.len() {
                            return Err(format!("距离 {dist} 超出已输出的 {} 字节", out.len()));
                        }
                        for _ in 0..len {
                            let b = out[out.len() - dist as usize];
                            out.push(b); // 逐字节复制，天然支持 dist < len 的重叠情况
                        }
                    }
                    _ => return Err(format!("保留的长度符号 {sym}")),
                }
            }
            if bfinal == 1 {
                break;
            }
        }
        Ok(out)
    }

    #[test]
    fn png_以魔数开头且能被解回原始像素() {
        let doc = synth_doc();
        let header = synth_header();
        let f = make_frame(&doc, &header, 100);

        let png = encode_png(&f).unwrap();
        assert_eq!(&png[..4], &[0x89, 0x50, 0x4E, 0x47], "PNG 魔数");

        // 块结构：IHDR → IDAT → IEND，且每个块的 CRC 都要对
        let mut pos = 8usize;
        let mut idat: Vec<u8> = Vec::new();
        let mut kinds: Vec<[u8; 4]> = Vec::new();
        while pos + 8 <= png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = png[pos + 4..pos + 8].try_into().unwrap();
            let data = &png[pos + 8..pos + 8 + len];
            let want = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            let mut crc_in = Vec::new();
            crc_in.extend_from_slice(&kind);
            crc_in.extend_from_slice(data);
            assert_eq!(crc32(&crc_in), want, "块 {kind:?} 的 CRC 不对");
            if &kind == b"IDAT" {
                idat.extend_from_slice(data);
            }
            kinds.push(kind);
            pos += 12 + len;
        }
        assert_eq!(kinds, vec![*b"IHDR", *b"IDAT", *b"IEND"]);

        let ihdr = &png[16..29];
        assert_eq!(u32::from_be_bytes(ihdr[0..4].try_into().unwrap()), TW);
        assert_eq!(u32::from_be_bytes(ihdr[4..8].try_into().unwrap()), TH);
        assert_eq!(ihdr[8], 8, "位深 8");
        assert_eq!(ihdr[9], 6, "颜色类型 6 = RGBA");

        // 解回来，去掉每行开头那个滤波字节，逐像素比对
        let raw = inflate_fixed(&idat).unwrap();
        let stride = TW as usize * 4;
        assert_eq!(raw.len(), (stride + 1) * TH as usize);
        let mut got = Vec::with_capacity(stride * TH as usize);
        let mut prev_row = vec![0u8; stride];
        for y in 0..TH as usize {
            let ft = raw[y * (stride + 1)];
            let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
            let mut cur = vec![0u8; stride];
            for i in 0..stride {
                cur[i] = match ft {
                    0 => line[i],
                    2 => line[i].wrapping_add(prev_row[i]),
                    other => panic!("不认识的滤波器类型 {other}"),
                };
            }
            got.extend_from_slice(&cur);
            prev_row = cur;
        }
        assert_eq!(got, f.bgra, "解回来的像素必须与原始帧逐字节相同");
    }

    #[test]
    fn 越界的抓屏区域被拒绝而不是给黑图() {
        // 宽高为 0 与"大到申请几 GB"这两类，必须在碰系统之前就被挡掉
        assert!(capture_region(0, 0, 0, 100).unwrap_err().contains("大于 0"));
        assert!(capture_region(0, 0, 100, 0).unwrap_err().contains("大于 0"));
        let err = capture_region(-100, -100, 60000, 60000).unwrap_err();
        assert!(err.contains("过大"), "实际错误：{err}");
    }

    #[test]
    fn css_像素按dpi换算成物理像素() {
        // 100%
        assert_eq!(css_rect_to_physical(10.0, 20.0, 100.0, 50.0, 1.0), (10, 20, 100, 50));
        // 125%：起点会落在半像素上，用"两个端点分别取整再相减"才不会系统性少 1 像素
        assert_eq!(css_rect_to_physical(10.0, 0.0, 100.0, 8.0, 1.25), (13, 0, 125, 10));
        // 150%
        assert_eq!(css_rect_to_physical(0.0, 0.0, 200.0, 100.0, 1.5), (0, 0, 300, 150));
        // 200%
        assert_eq!(css_rect_to_physical(3.0, 4.0, 10.0, 20.0, 2.0), (6, 8, 20, 40));
        // 缩放系数异常时按 1.0 处理，绝不返回 0 尺寸
        assert_eq!(css_rect_to_physical(0.0, 0.0, 5.0, 5.0, 0.0), (0, 0, 5, 5));
        assert_eq!(css_rect_to_physical(0.0, 0.0, 5.0, 5.0, f64::NAN), (0, 0, 5, 5));
    }

    #[test]
    fn png_不覆盖已有文件() {
        let dir = std::env::temp_dir().join("deskbase-capture-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exists.png");
        std::fs::write(&p, "占位").unwrap();
        let doc = synth_doc();
        let header = synth_header();
        let f = make_frame(&doc, &header, 0);
        let err = save_png(&f, &p).unwrap_err();
        assert!(err.contains("不覆盖"), "实际错误：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
