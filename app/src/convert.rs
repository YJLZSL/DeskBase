//! 文档 / 图片的格式转换（纯离线，零出网）
//!
//! ## 这个模块解决什么
//!
//! 用户说的"office 那些功能，比如说格式转换"，落到本地能真正做好的只有三类：
//!
//! | 转换 | 做什么 | 主要丢什么（见 `ConversionPlan::losses`） |
//! |------|--------|------------------------------------------|
//! | **表格** `xlsx → csv/tsv` | 一个工作簿拆成"每张工作表一个文件" | 公式、单元格格式、其余工作表、图片图表、合并单元格、宏、外部链接 |
//! | **表格** `csv/tsv → xlsx` | 多个文件合成一个工作簿（每源一张表） | 几乎没有，但"文本变成数字"必须提前说清 |
//! | **表格** `csv ⇄ tsv` | 换分隔符（真解析、真重写） | 引号规则会被规范化 |
//! | **文本编码** `GBK ⇄ UTF-8 ⇄ UTF-16` | 纯重编码，一个字节的内容都不动 | 目标编码装不下的字符（emoji 在 GBK 里会变成 `&#128512;`） |
//! | **图片** `png ⇄ jpg ⇄ bmp ⇄ webp` | 解码 → 应用 EXIF 方向 → 缩放/旋转/翻转 → 重新编码 | JPEG 有损、透明通道、16 位色深、EXIF 元数据 |
//!
//! **明确不做**（不是没时间，是做了也不会好）：
//!   · `.docx` / `.pptx` 的**排版级**转换 —— 那需要完整布局引擎（分页、字体度量、
//!     表格跨页、图形锚定）。任何"轻量库"做出来的结果都是"能打开但全乱"，
//!     比老实说不支持更伤用户。
//!   · PDF 生成 / 解析 —— 体积与复杂度都该单独排期。
//!   · 图片水印 —— 见 `apply_edits` 的注释（中文水印要字体光栅化，不划算）。
//!   · gif / tiff / heic —— gif 多半是动画（要处理多帧与调色板），后两者会让安装包
//!     明显变大而办公场景几乎用不到。遇到它们会给出"先用画图另存为 PNG"的替代做法。
//!   · 任何联网转换服务 —— 与「零出网、零遥测」直接冲突。
//!
//! ## 地基：只读 A、只写 B，绝不原地改（与 `xlsx.rs` 同一条策略）
//!
//!   · **源文件全程只被读**：不打开写句柄、不重命名、不改时间戳；
//!   · **目标文件已存在就直接报错**：不覆盖、不询问、不备份 —— 也没有"允许覆盖"
//!     这个开关，因为那就是给用户一个毁数据的按钮。`plan` 与 `run` 各查一次
//!     （从看计划到点确认之间，用户完全可能已经把那个名字的文件建出来了）；
//!   · 目标是**先写 `<名字>.part`、成功后才改名**：失败不会留下"半个文件"
//!     （写了一半的 xlsx 用 Excel 打开只会提示损坏，用户会以为是我们弄坏的）。
//!
//! ## 为什么必须有 `plan` 这一步
//!
//! 格式转换最坏的结果不是"失败"，而是**静默降级**：`xlsx → csv` 会丢掉公式、
//! 格式、多张工作表、图片、合并单元格，而用户以为"只是换了个后缀"。
//!
//! 所以 `plan()` 先做两件事：把「要做什么」翻译成人话（`steps`），把
//! 「这次会丢什么」**具体到数量和名字**（`losses`）摆出来 —— 让人有机会在读到
//! "会丢掉 2 张工作表（Sheet2、Sheet3）、18 个公式（如 C12 的 =SUM(C2:C11)）、
//! 4 张嵌入图片"之后再决定。
//!
//! `plan()` 只读到"够算清要丢什么"为止：工作表名、公式范围、ZIP 条目名、
//! 图片头里的宽高 —— **不解析全部单元格、不解码像素**，因为预览慢就没人看。
//! `run()` 再真正读一遍并写出来；两者最后在 `Report` 里对账。
//!
//! ## 上限与进度
//!
//! 每个转换都有硬上限（见下面的常量），超限是**报错**而不是"慢慢跑"：
//! 桌面应用卡 30 秒，用户会以为软件坏了。进度用 `run_with_progress` 的
//! `progress(done, total)` 回调报出去，`total` 一开始就是最终值（界面可直接画进度条）。
//!
//! ## 已知不足
//!
//!   · CSV 解析在这里是**第二份**实现。`csv_import.rs` 里那份是为"检查"写的
//!     （只取前 8 行、顺带统计告警），它的解析函数是私有且 `#[cfg(test)]` 的，
//!     而本模块按约束不能改那个文件。**唯一正确的收敛方向是把
//!     `csv_import::parse_limited` 提成 `pub(crate)` 让两边共用。**
//!   · **webp 是"只读友好"的**：解码完全支持（浏览器存下来的图就是 webp），
//!     但写出去只能无损（`image` 这套库只提供 `WebPEncoder::new_lossless`），
//!     所以照片转 webp 通常比 JPEG 大 —— 这一点在计划里会明说。
//!   · 图片一次只转一张（`plan_images` 会拒绝多张）。批量交给界面层循环调用 ——
//!     这样进度、失败与"哪一个文件出问题"才是一一对应的。
//!   · `plan` 与 `run` 各读一遍源文件。要准确报出"丢几个公式"就必须读一遍，
//!     要写出结果又必须再读一遍 —— 除非把整张表缓存在内存里（20 万格以上不划算）。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use calamine::{open_workbook_auto, Data, Reader};
use encoding_rs::GB18030;
use image::codecs::{
    bmp::BmpEncoder, jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder,
};
use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageReader, Limits, Rgb, RgbImage};
use rust_xlsxwriter::{Format, FormatAlign, Workbook};

use crate::csv_import::Encoding;

pub type Result<T> = std::result::Result<T, String>;

// ============================================================
// 硬上限
// ============================================================

/// 表格三道闸门，数值与 `xlsx.rs` / `csv_import.rs` **完全一致**。
/// 不一致的后果很具体：用户会看到"同一批数据换成 CSV 就能转、换成 xlsx 就不行"。
const MAX_TABLE_ROWS: usize = 500_000;
const MAX_TABLE_CELLS: usize = 20_000_000;
/// xlsx 是 ZIP，解压后会膨胀 1.2–6 倍，所以按压缩包大小卡
const MAX_XLSX_BYTES: u64 = 200 * 1024 * 1024;
/// CSV/TSV 不存在解压膨胀，代价只有内存
const MAX_DELIMITED_BYTES: u64 = 500 * 1024 * 1024;

/// 文本重编码：内存里同时存在「原始字节 + 解码后的 String + 编码后的字节」，
/// 最坏约 3 倍。256 MB 输入对应峰值约 800 MB，是桌面端能接受的上限。
const MAX_TEXT_BYTES: u64 = 256 * 1024 * 1024;

/// 图片：单文件 100 MB。手机长图也就几 MB，到这个量级基本是 TIFF/RAW，
/// 不是这个工具该处理的东西。
const MAX_IMAGE_BYTES: u64 = 100 * 1024 * 1024;
/// 像素上限：解码后每像素 4 字节，5000 万像素 = 200 MB，变换时还要一份副本。
/// 它同时是**解压炸弹闸门**：PNG 能声明一个几 GB 的尺寸而文件只有几 KB。
const MAX_PIXELS: u64 = 50_000_000;
/// 缩放后的最长边上限，防止"小图放大 100 倍"把内存瞬间吃光
const MAX_EDGE: u32 = 20_000;

/// ZIP 中央目录的读取上限。200 MB 的 xlsx 中央目录通常不到 1 MB；
/// 到 32 MB 说明这个包不正常，宁可放弃统计也不要把它读进内存。
const MAX_CENTRAL_DIR: u64 = 32 * 1024 * 1024;
/// 每条报告最多带几个样例（与 `csv_import.rs` 的 MAX_SAMPLES 一致）
const MAX_SAMPLES: usize = 5;
/// 计划阶段只读这么点头部：够探测编码/分隔符、够看出有没有长编号，
/// 但不用把 500 MB 读进来（用户可能只是想先看看会丢什么）
const PLAN_SNIFF_BYTES: usize = 64 * 1024;

// ============================================================
// 公共类型
// ============================================================

/// 转换中**会丢掉的东西**（不是"可能"）。
///
/// 每条都必须回答"丢了多少、叫什么"。写不出数量的东西（如"单元格格式"）
/// 不许写成 `Loss`，只能进 `warnings` —— 只说"可能丢失部分格式"等于没说，
/// 用户没法据此做任何决定。
///
/// 确实必然发生、但精确数量要解码后才知道的（如"多少透明像素被合成到白底"），
/// 计划里记 `count = 1`，实际报告里给准确数；对账只比种类不比数量（见 `run_with_progress`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Loss {
    /// 机器可读的分类（`sheets` / `formulas` / `images` / `alpha` / `exif` …）。
    /// 界面要按类型折叠或过滤时用它，**不要去解析中文 detail**。
    pub kind: String,
    /// 丢了几个
    pub count: usize,
    /// 人话描述，带数量、名字与样例："2 张工作表（Sheet2、Sheet3）"
    pub detail: String,
}

/// 一次转换的完整计划。**先给人看，再决定跑不跑。**
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversionPlan {
    /// 人话的步骤清单，按执行顺序，可直接显示在确认对话框里
    pub steps: Vec<String>,
    /// 要读的文件（原文件，**只读**）
    pub inputs: Vec<PathBuf>,
    /// 要写的文件（**全部是新文件**；任何一个已存在都会让 `plan` 直接报错）
    pub outputs: Vec<PathBuf>,
    /// 风险提示：体积可能变大、多文件输出不是事务的、不可计数的丢失项等
    pub warnings: Vec<String>,
    /// **这次会丢掉什么**，带数量与样例。空数组 = 无损转换
    pub losses: Vec<Loss>,
    /// 执行计划所需的一切（源/目标/格式/选项）。
    /// 私有：`run` 只执行本模块造出来的计划，外面手拼一个计划是编译不过的。
    #[serde(skip)]
    job: Job,
}

impl Report {
    /// 某个分类实际丢了多少（没丢返回 0）。给测试与界面用。
    pub fn loss_of(&self, kind: &str) -> usize {
        self.losses.iter().filter(|l| l.kind == kind).map(|l| l.count).sum()
    }
}

impl ConversionPlan {
    /// 计划的人话摘要。给"转换前确认"对话框和日志用 —— 省得界面自己拼。
    pub fn summary(&self) -> String {
        let mut s = String::new();
        s.push_str("将要做的：\n");
        for (i, step) in self.steps.iter().enumerate() {
            s.push_str(&format!("  {}. {}\n", i + 1, step));
        }
        if !self.losses.is_empty() {
            s.push_str("会丢掉的东西：\n");
            for l in &self.losses {
                s.push_str(&format!("  · {}\n", l.detail));
            }
        }
        if !self.warnings.is_empty() {
            s.push_str("要注意：\n");
            for w in &self.warnings {
                s.push_str(&format!("  · {w}\n"));
            }
        }
        s
    }

    /// 会不会丢东西。界面据此决定要不要弹"确认"。
    pub fn is_lossy(&self) -> bool {
        !self.losses.is_empty()
    }

    /// 某个分类丢了多少（没丢返回 0）。给测试与界面用。
    pub fn loss_of(&self, kind: &str) -> usize {
        self.losses.iter().filter(|l| l.kind == kind).map(|l| l.count).sum()
    }
}

/// 转换结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    /// 实际写出来的文件（顺序与 `plan.outputs` 一致）
    pub outputs: Vec<PathBuf>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// 表格：写出的数据行数合计（不含表头）；图片/文本为 0
    pub rows: usize,
    /// **实际**丢掉的东西。正常情况与 `plan.losses` 同种类，
    /// 种类不一致会在 `notes` 里点名 —— 那是我们预测错了，必须让用户知道。
    pub losses: Vec<Loss>,
    /// 人话总结：体积变化、方向标签、与计划的对账
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
}

/// 转换选项。全部有默认值，`Options::default()` 就是"最稳的那一组"。
///
/// 注意它**没有** derive `Serialize`：里面的 `Encoding` 来自 `csv_import`，
/// 那个类型没实现 Serialize（本模块不能改它）。要去界面的计划文本请用
/// `ConversionPlan::summary()` —— 那本来就是给人看的。
#[derive(Debug, Clone)]
pub struct Options {
    // ---------- 表格 ----------
    /// 只导出这一张工作表（`None` = 全部）。名字对不上会报错并列出现有名字。
    pub sheet: Option<String>,
    /// 源 CSV/TSV 的分隔符（`None` = 自动探测）
    pub src_delimiter: Option<char>,
    /// 目标 CSV/TSV 的分隔符（`None` = 按扩展名：.csv 逗号、.tsv 制表符）
    pub dst_delimiter: Option<char>,
    /// 输出编码。`None` = **保持源编码**（纯转码场景），
    /// xlsx → csv 这种"新文件"则用 UTF-8 带 BOM（Excel 双击打开不乱码）。
    pub out_encoding: Option<Encoding>,

    // ---------- 图片 ----------
    /// JPEG 质量 1–100
    pub jpeg_quality: u8,
    /// 等比缩放到最长边不超过这个像素（`None` = 不缩放）
    pub max_edge: Option<u32>,
    /// 顺时针旋转 90° 的次数（0–3）。EXIF 方向永远会被自动应用，这是额外的要求。
    pub rotate90: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sheet: None,
            src_delimiter: None,
            dst_delimiter: None,
            out_encoding: None,
            // 85 而不是 95：95 的体积差不多是 85 的两倍，而目标用户是"要把图发给客户"
            // 的人，不是要印刷的人。
            jpeg_quality: 85,
            max_edge: None,
            rotate90: 0,
            flip_h: false,
            flip_v: false,
        }
    }
}

impl Options {
    pub fn with_sheet(mut self, name: &str) -> Self {
        self.sheet = Some(name.to_string());
        self
    }
    pub fn with_encoding(mut self, enc: Encoding) -> Self {
        self.out_encoding = Some(enc);
        self
    }
    pub fn with_quality(mut self, q: u8) -> Self {
        self.jpeg_quality = q.clamp(1, 100);
        self
    }
    pub fn with_max_edge(mut self, edge: u32) -> Self {
        self.max_edge = Some(edge);
        self
    }
    pub fn with_rotate90(mut self, n: u8) -> Self {
        self.rotate90 = n % 4;
        self
    }

    /// 有没有要求对图片做"内容级"的改动。
    /// 用途：png → png 本来没意义，但有缩放/旋转时它就有意义了。
    fn has_image_edits(&self) -> bool {
        self.max_edge.is_some() || self.rotate90 % 4 != 0 || self.flip_h || self.flip_v
    }

    /// 校验一遍选项。**在 plan 的最前面调用**：宁可在用户点"转换"前就报错，
    /// 也不要跑到一半才发现参数不对。
    fn validate(&self, dst_fmt: Fmt) -> Result<()> {
        if let Some(edge) = self.max_edge {
            if edge == 0 {
                return Err("缩放的长边不能是 0 像素。".into());
            }
            if edge > MAX_EDGE {
                return Err(format!("缩放的长边最大 {MAX_EDGE} 像素（再大就不是缩，是放大了）。"));
            }
        }
        if let Some(Encoding::Unknown) = self.out_encoding {
            return Err(
                "输出编码不能是「未知」。可选：UTF-8 / UTF-8 带 BOM / GB18030 / UTF-16LE / UTF-16BE。"
                    .into(),
            );
        }
        if dst_fmt.is_image() {
            if self.out_encoding.is_some() {
                return Err("文本编码选项对图片没有意义（图片用什么编码由它自己的格式决定）。".into());
            }
            if self.sheet.is_some() || self.src_delimiter.is_some() || self.dst_delimiter.is_some() {
                return Err("表格选项（sheet / 分隔符）对图片没有意义，请去掉。".into());
            }
        } else if self.has_image_edits() {
            return Err("缩放 / 旋转 / 翻转只对图片有效，请去掉。".into());
        }
        Ok(())
    }
}

/// 能识别的文件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Fmt {
    /// .xlsx / .xlsm / .xlsb / .xls / .ods（都交给 calamine）
    Xlsx,
    Csv,
    Tsv,
    Txt,
    Png,
    Jpg,
    Bmp,
    Webp,
}

impl Fmt {
    /// 拼文件名用的扩展名（小写、不带点）
    pub fn ext(self) -> &'static str {
        match self {
            Fmt::Xlsx => "xlsx",
            Fmt::Csv => "csv",
            Fmt::Tsv => "tsv",
            Fmt::Txt => "txt",
            Fmt::Png => "png",
            Fmt::Jpg => "jpg",
            Fmt::Bmp => "bmp",
            Fmt::Webp => "webp",
        }
    }
    /// 是不是"纯文本表格"（csv/tsv/txt 都是文本，只是分隔符不同）
    pub fn is_delimited(self) -> bool {
        matches!(self, Fmt::Csv | Fmt::Tsv | Fmt::Txt)
    }
    pub fn is_image(self) -> bool {
        matches!(self, Fmt::Png | Fmt::Jpg | Fmt::Bmp | Fmt::Webp)
    }
}

/// 文件选择框用的扩展名清单。放在这里是为了**只有一处**知道支持什么，
/// 界面上的 filter 与实现不会走偏。
pub fn supported_extensions() -> &'static [&'static str] {
    &["xlsx", "xlsm", "xlsb", "xls", "ods", "csv", "tsv", "txt", "png", "jpg", "jpeg", "bmp", "webp"]
}

/// 按扩展名判定类型。
///
/// 为什么以扩展名为准：**"转成什么"是用户用后缀表达的意图**（他说转 csv 就是 csv）。
/// 魔数用来**发现不一致**：后缀是 `.xlsx` 其实是 HTML 表格时，我们要给人话错误，
/// 而不是把 calamine 的英文报错原样抛给用户（见 `check_src`）。
fn fmt_of(path: &Path) -> Result<Fmt> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    Ok(match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => Fmt::Xlsx,
        "csv" => Fmt::Csv,
        "tsv" | "tab" => Fmt::Tsv,
        "txt" => Fmt::Txt,
        "png" => Fmt::Png,
        "jpg" | "jpeg" => Fmt::Jpg,
        "bmp" | "dib" => Fmt::Bmp,
        "webp" => Fmt::Webp,
        "" => {
            return Err(format!(
                "「{}」没有扩展名，认不出是什么格式。目标格式由后缀决定，\
                 所以请把目标写成带后缀的名字。能转的是：{}。",
                path.display(),
                supported_extensions().join(" / ")
            ))
        }
        other => {
            return Err(format!(
                "认不出「.{other}」这种格式。能转的是：{}。\
                 （gif / tiff / heic 没有编进来：gif 多半是动画（要处理多帧），\
                 tiff / heic 会让安装包明显变大而办公场景几乎用不到。\
                 需要的话请先用画图或浏览器另存为 PNG / JPG。）",
                supported_extensions().join(" / ")
            ))
        }
    })
}

// ============================================================
// Job：plan 造它、run 执行它（私有，保证"只能执行计划过的转换"）
// ============================================================

#[derive(Debug, Clone)]
enum Job {
    Table(TableJob),
    /// 纯文本：`text` 是重编码（内容不动），`reshape` 是换分隔符（真解析重写）
    Text { text: Vec<TextUnit>, reshape: Vec<ReshapeUnit> },
    Image(Vec<ImageUnit>),
}

#[derive(Debug, Clone)]
enum TableJob {
    /// xlsx（多工作表）→ 每张表一个 csv/tsv 文件
    Split(Vec<SplitUnit>),
    /// 多个 csv/tsv/txt → 一个 xlsx（每源一张工作表）
    Merge(MergeJob),
}

#[derive(Debug, Clone)]
struct SplitUnit {
    src: PathBuf,
    sheet: String,
    out: PathBuf,
    delim: char,
    enc: Encoding,
}

#[derive(Debug, Clone)]
struct MergeJob {
    /// (路径, 源分隔符)
    srcs: Vec<(PathBuf, char)>,
    out: PathBuf,
    /// 每张工作表的名字（已清洗、已去重、≤31 字符）
    sheet_names: Vec<String>,
}

#[derive(Debug, Clone)]
struct ReshapeUnit {
    src: PathBuf,
    out: PathBuf,
    delim_in: char,
    delim_out: char,
    enc: Encoding,
}

#[derive(Debug, Clone)]
struct TextUnit {
    src: PathBuf,
    out: PathBuf,
    from: Encoding,
    to: Encoding,
}

#[derive(Debug, Clone)]
struct ImageUnit {
    src: PathBuf,
    out: PathBuf,
    /// 源格式（判断"这次是不是又压了一遍 JPEG"，它是会掉画质的）
    fmt: Fmt,
    /// 要应用的 EXIF 方向
    exif: Orientation,
    /// 源带透明通道（决定"要丢掉透明"这件事到底该不该报）
    has_alpha: bool,
    /// 源文件字节数（报告里算体积变化）
    bytes: u64,
    /// 换算好的编辑参数（存在计划里，run 不重新读选项：计划说什么就执行什么）
    edits: EditParams,
}

/// 图片编辑参数。存在 `ImageUnit` 里而不是从 `Options` 再读一次 ——
/// 计划里已经算过"缩放之后是多少"，执行时必须用同一组数。
#[derive(Debug, Clone, Copy)]
struct EditParams {
    max_edge: Option<u32>,
    rotate90: u8,
    flip_h: bool,
    flip_v: bool,
    jpeg_quality: u8,
}

// ============================================================
// plan
// ============================================================

/// 做一个转换计划。**只读源文件，不写任何东西。**
///
/// 目标文件已存在时直接报错（不覆盖）。
pub fn plan(src: &Path, dst: &Path, opts: &Options) -> Result<ConversionPlan> {
    plan_many(std::slice::from_ref(&src.to_path_buf()), dst, opts)
}

/// 多个源文件 → 一个目标**文件**（目标格式由目标后缀决定）。
/// 只有"合并"（多个 csv → 一个 xlsx）才会出现多源一目标。
pub fn plan_many(srcs: &[PathBuf], dst: &Path, opts: &Options) -> Result<ConversionPlan> {
    if srcs.is_empty() {
        return Err("没有选择任何要转换的文件。".into());
    }
    let dst_fmt = fmt_of(dst)?;
    opts.validate(dst_fmt)?;

    // 目标是已存在的目录时明确拒绝，不替用户猜后缀：
    // 猜错一个后缀的代价（用户拿到一堆 .csv 却以为自己是 .xlsx）比多问一句大得多。
    if dst.is_dir() {
        return Err(format!(
            "目标「{}」是一个目录。目标必须写成**带后缀的文件名**（例如 `导出.csv`），\
             因为目标格式由后缀决定，程序不替你猜。\
             一个工作簿要拆成多个文件时，其余文件会写成同目录里 `文件名-工作表名.csv` 这样的名字。",
            dst.display()
        ));
    }

    let src_fmts = srcs.iter().map(|p| fmt_of(p.as_path())).collect::<Result<Vec<_>>>()?;
    for (src, fmt) in srcs.iter().zip(&src_fmts) {
        check_src(src, *fmt)?;
        // 源和目标是同一个文件 —— 这是"改原件"唯一的可能入口，必须第一道拦。
        // 用 canonicalize 而不是字符串比较：`a.xlsx` 与 `.\a.xlsx`、大小写、
        // 8.3 短名都能绕过字符串比较。
        if same_file(src, dst) {
            return Err(format!(
                "源文件和目标文件是同一个文件：{}\n\
                 本程序只读原文件、只写新文件，不会原地修改任何东西，请换一个目标名。",
                src.display()
            ));
        }
    }

    // 一次只能转同一类。混着转的话，用户根本没法预判"哪个文件发生了什么"
    // （xlsx 会丢公式与格式，纯文本只换编码，图片又是另一套）。
    let dst_is_image = dst_fmt.is_image();
    if let Some((p, f)) = srcs
        .iter()
        .zip(&src_fmts)
        .find(|(_, f)| if dst_is_image { !f.is_image() } else { f.is_image() })
    {
        return Err(format!(
            "「{}」是{}，而目标是 .{} —— 表格和图片是两类东西。\
             本模块不做「表格转图片」（那需要排版渲染），也不做「图片转表格」（那需要 OCR）。",
            p.display(),
            if f.is_image() { "图片" } else { "表格" },
            dst_fmt.ext()
        ));
    }

    let mut plan = match dst_fmt {
        Fmt::Xlsx => plan_merge(srcs, &src_fmts, dst, opts)?,
        Fmt::Csv | Fmt::Tsv => plan_to_delimited(srcs, &src_fmts, dst, dst_fmt, opts)?,
        Fmt::Txt => plan_reencode(srcs, &src_fmts, dst, opts)?,
        Fmt::Png | Fmt::Jpg | Fmt::Bmp | Fmt::Webp => plan_images(srcs, dst, dst_fmt, opts)?,
    };

    // ---- 最后一道：目标已存在就整体失败 ----
    // 放在最后做，是因为分派逻辑要先把"到底会写出哪几个文件"算清楚
    // （xlsx → csv 会产出 N 个文件，每一个都不能已存在）。
    let exist: Vec<String> = plan
        .outputs
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !exist.is_empty() {
        return Err(format!(
            "目标文件已存在，本程序不覆盖任何已有文件：\n  {}\n\
             请改一个目标文件名（或者先把它移走），再试一次。",
            exist.join("\n  ")
        ));
    }
    for out in &plan.outputs {
        if let Some(dir) = out.parent() {
            if !dir.as_os_str().is_empty() && !dir.exists() {
                plan.warnings
                    .push(format!("目标目录还不存在，转换时会自动创建：{}", dir.display()));
            }
        }
    }
    if plan.outputs.len() > 1 {
        plan.warnings.push(format!(
            "这次会写出 {} 个文件。多文件输出**不是**一次事务：如果写到第 3 个失败，\
             前 2 个会留在磁盘上（都是新文件，不会破坏任何原件）。",
            plan.outputs.len()
        ));
    }
    plan.warnings
        .push("已有文件一律不覆盖：目标同名文件存在时，本次转换会失败并指出是哪一个。".into());
    Ok(plan)
}

// ---------------- 分派：csv/tsv/txt → xlsx（合并） ----------------

fn plan_merge(
    srcs: &[PathBuf],
    src_fmts: &[Fmt],
    dst: &Path,
    opts: &Options,
) -> Result<ConversionPlan> {
    if let Some((p, f)) = srcs.iter().zip(src_fmts).find(|(_, f)| !f.is_delimited()) {
        return Err(format!(
            "「{}」不能转成 .xlsx。能做的是 csv / tsv / txt → xlsx（把多个文本表格\
             合成一个工作簿，每个文件一张工作表）。{} 的转换请用对应的目标格式。",
            p.display(),
            f.ext().to_uppercase()
        ));
    }

    let mut steps =
        vec![format!("读取 {} 个文本表格，每个文件变成工作簿里的一张工作表", srcs.len())];
    let mut warnings = Vec::new();
    let mut losses = Vec::new();
    let mut sources: Vec<(PathBuf, char)> = Vec::new();
    let mut est_rows = 0u64;
    let mut est_cells = 0u64;
    let mut numeric = 0usize;
    let mut leading_zero = 0usize;
    let mut long_ids = 0usize;
    let mut unmappable = 0usize;
    let mut four_byte = 0usize;
    let out_enc = opts.out_encoding.unwrap_or(Encoding::Utf8Bom);

    for (src, fmt) in srcs.iter().zip(src_fmts) {
        let bytes = std::fs::metadata(src).map_err(|e| format!("读不到文件信息：{e}"))?.len();
        if bytes > MAX_DELIMITED_BYTES {
            return Err(too_big(src, bytes, MAX_DELIMITED_BYTES, "CSV / TSV"));
        }
        let head = read_head(src, PLAN_SNIFF_BYTES)?;
        let enc = crate::csv_import::detect_encoding(&head);
        if enc == Encoding::Unknown {
            return Err(format!(
                "「{}」里全是 NUL 之类的控制字节，不像是文本表格（可能是个二进制文件改了后缀）。",
                file_label(src)
            ));
        }
        let (text, _) = decode_text(&head, enc);
        let (delim, found) = match opts.src_delimiter {
            Some(d) => (d, true),
            None => sniff_delimiter(&text),
        };
        if !found {
            warnings.push(format!(
                "「{}」里没探测到分隔符（逗号 / Tab / 分号 / 竖线都没有一致的列数），\
                 会整行当成一格，转出来只有一列。如果它其实是用别的符号分列的，\
                 请在界面上手动指定分隔符。",
                file_label(src)
            ));
        }
        if *fmt == Fmt::Txt {
            warnings.push(format!(
                "「{}」是 .txt：按探测到的「{}」当分隔符解析。\
                 如果它本来就不该分列，请改用一个 .txt 目标（那样只换编码、不动内容）。",
                file_label(src),
                delim_label(delim)
            ));
        }

        // 头部扫描：够发现"列里有没有长编号""大概多少数字"，看不出总行数 ——
        // 总行数按"文件大小 ÷ 头部每行字节数"估，只用于总量闸门。
        let (head_rows, head_cols) = count_rows(&text, delim);
        let (num, lz, lid) = scan_cells(&text, delim, head_rows.max(1));
        numeric += num;
        leading_zero += lz;
        long_ids += lid;
        let e = encode_text(&text, out_enc);
        unmappable += e.unmappable;
        four_byte += e.four_byte;
        let per_row = (text.len() as u64 / head_rows.max(1) as u64).max(1);
        let this_rows = (bytes / per_row).max(head_rows as u64);
        est_rows += this_rows;
        est_cells += this_rows * head_cols.max(1) as u64;

        steps.push(format!(
            "「{}」：约 {} 行 × {} 列（按文件大小估算；头部 {head_rows} 行看过是 {} 分隔、{} 编码）→ 一张工作表",
            file_label(src),
            this_rows,
            head_cols,
            delim_label(delim),
            enc.label()
        ));
        sources.push((src.clone(), delim));
    }

    if est_cells > MAX_TABLE_CELLS as u64 {
        return Err(format!(
            "按文件大小估算约有 {:.0} 万个单元格，超过单次转换上限 {} 万个。建议拆成几个文件。",
            est_cells as f64 / 10000.0,
            MAX_TABLE_CELLS / 10000
        ));
    }
    if est_rows > MAX_TABLE_ROWS as u64 {
        return Err(format!(
            "按文件大小估算约有 {est_rows} 行，超过单次转换上限 {MAX_TABLE_ROWS} 行。\
             建议按年份拆成几个文件。"
        ));
    }

    let sheet_names = sheet_names_for(srcs);
    steps.push(format!(
        "写入 {}（{} 张工作表：{}）",
        dst.display(),
        sheet_names.len(),
        sheet_names.join("、")
    ));
    steps.push(
        "源文件是 GBK / UTF-16 也没关系：xlsx 内部一律是 UTF-8，这一步顺带把中文编码问题解决了"
            .into(),
    );

    // ---- "文本会变成数字"必须提前说清 ----
    // 这不是"丢东西"，但它会改变用户看到的单元格类型：金额能求和了，
    // 而"0012"这种编号若被当数字写，前导零就没了。
    if numeric > 0 {
        steps.push(format!(
            "头部看到的 {numeric} 个数字格子会按**数值**写入（Excel 里可以直接求和）"
        ));
    }
    if leading_zero > 0 {
        steps.push(format!(
            "保护 {leading_zero} 个带前导零的编号：按**文本**写，\
             因为当数字写会把「007」变成「7」，编号就废了"
        ));
    }
    if long_ids > 0 {
        warnings.push(format!(
            "头部看到 {long_ids} 处 15 位以上的长数字（订单号 / 身份证 / 卡号）。\
             它们会按**文本**写入，Excel 里不会变成科学计数法，但也**不能**当数字算 —— \
             这是刻意的取舍：算错可以重来，编号丢了找不回来。"
        ));
    }
    if unmappable > 0 {
        losses.push(Loss {
            kind: "encode_unmappable".into(),
            count: unmappable,
            detail: format!(
                "按前 64 KB 估算，有 {unmappable} 个字符在目标编码的映射表里没有对应\
                 （GB18030 覆盖了几乎全部 Unicode，只有极少数码位会这样），会被写成 `&#数字;` 文本。\
                 要保住它们只能用 UTF-8 输出。"
            ),
        });
    }
    if four_byte > 0 {
        warnings.push(format!(
            "按前 64 KB 估算，有 {four_byte} 个不在 GBK 范围内的字符（emoji / 生僻字）\
             会写成 GB18030 的四字节编码。文件是合法的，但按 GBK 读的软件会显示成乱码 —— \
             要保证到处都能看，请选 UTF-8 输出。"
        ));
    }

    Ok(ConversionPlan {
        steps,
        inputs: srcs.to_vec(),
        outputs: vec![dst.to_path_buf()],
        warnings,
        losses,
        job: Job::Table(TableJob::Merge(MergeJob {
            srcs: sources,
            out: dst.to_path_buf(),
            sheet_names,
        })),
    })
}

// ---------------- 分派：→ csv / tsv ----------------

fn plan_to_delimited(
    srcs: &[PathBuf],
    src_fmts: &[Fmt],
    dst: &Path,
    dst_fmt: Fmt,
    opts: &Options,
) -> Result<ConversionPlan> {
    let dst_delim = opts.dst_delimiter.unwrap_or(if dst_fmt == Fmt::Tsv { '\t' } else { ',' });

    // xlsx 与纯文本两个方向分开走：xlsx 要拆成多个文件并报出"丢了几张表"，
    // 纯文本只是重编码。混在一起会让丢失项没法归因。
    let has_workbook = src_fmts.iter().any(|f| *f == Fmt::Xlsx);
    let has_text = src_fmts.iter().any(|f| f.is_delimited());
    if has_workbook && has_text {
        return Err(
            "一次只能转同一类文件：Excel（.xlsx）和纯文本（.csv/.tsv/.txt）请分开转。\
             原因是两者的丢失项完全不同（Excel 会丢公式与格式，纯文本只换编码），\
             混在一个计划里，用户没法判断哪个文件发生了什么。"
                .into(),
        );
    }
    if has_workbook {
        return plan_split(srcs, dst, dst_fmt, dst_delim, opts);
    }
    plan_delimited_rewrite(srcs, dst, dst_fmt, dst_delim, opts)
}

/// xlsx（多工作表）→ 每张表一个 csv/tsv 文件。
fn plan_split(
    srcs: &[PathBuf],
    dst: &Path,
    dst_fmt: Fmt,
    dst_delim: char,
    opts: &Options,
) -> Result<ConversionPlan> {
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut losses = Vec::new();
    let mut units: Vec<SplitUnit> = Vec::new();
    // 默认输出编码：这些是**新文件**，而目标用户是"双击用 Excel 打开"的人，
    // 所以默认 UTF-8 带 BOM（Excel 打开中文不乱码）。想保持别的编码可以显式指定。
    let out_enc = opts.out_encoding.unwrap_or(Encoding::Utf8Bom);

    for src in srcs {
        let (bytes, sheets) = workbook_sheets(src)?;
        if bytes > MAX_XLSX_BYTES {
            return Err(too_big(src, bytes, MAX_XLSX_BYTES, "Excel"));
        }
        let selected: Vec<String> = match &opts.sheet {
            Some(want) => {
                if !sheets.iter().any(|s| s == want) {
                    return Err(format!(
                        "「{}」里没有叫「{want}」的工作表。现有的工作表是：{}",
                        file_label(src),
                        sheets.join("、")
                    ));
                }
                vec![want.clone()]
            }
            None => sheets.clone(),
        };

        // ---- 一条最该被看见的丢失：没被选中的工作表 ----
        let dropped: Vec<String> =
            sheets.iter().filter(|s| !selected.contains(s)).cloned().collect();
        if !dropped.is_empty() {
            losses.push(Loss {
                kind: "sheets".into(),
                count: dropped.len(),
                detail: format!(
                    "{} 张工作表（{}）不会被导出，它们的数据不会出现在这次转换的结果里。\
                     要一起导出，请去掉「只导出一张表」的选项。",
                    dropped.len(),
                    sample_list(&dropped)
                ),
            });
            steps.push(format!("只导出 {} 张工作表（这个工作簿一共 {} 张）", selected.len(), sheets.len()));
        } else {
            steps.push(format!("「{}」有 {} 张工作表，全部导出", file_label(src), sheets.len()));
        }

        // ---- ZIP 里的东西：图片 / 图表 / 宏 / 外部链接 ----
        push_zip_losses(&zip_facts(src), &mut losses, &mut warnings);

        for sheet in &selected {
            // 计划阶段就把这张表的公式与合并区域读出来 —— 这是唯一能让
            // "会丢 18 个公式"变成具体数字的办法，值得多读这一遍。
            let sf = sheet_facts(src, sheet)?;
            if sf.formulas > 0 {
                // 公式的"计算结果"其实来自文件里的**缓存值**。程序生成的工作簿
                // （很多 ERP 导出就是这样）常常没写缓存，读出来会是 0 或空 ——
                // 用户拿到一份"合计=0"的 CSV 会以为我们把数据弄丢了，所以必须提前说。
                warnings.push(format!(
                    "工作表「{sheet}」里的公式只有**缓存的计算结果**。如果这份文件不是 Excel / WPS \
                     保存的（比如是别的程序生成的），或者改了数据还没在 Excel 里保存过，\
                     缓存可能是 0 或空 —— 那样的数字是**不对的**。转完请抽查几个合计。"
                ));
                losses.push(Loss {
                    kind: "formulas".into(),
                    count: sf.formulas,
                    detail: format!(
                        "工作表「{sheet}」的 {} 个公式{} —— CSV 里只有**计算结果**，\
                         公式本身、以及它引用的其它格子都不在了。\
                         如果这些数是别人要拿来核对的，请留着原 xlsx。",
                        sf.formulas,
                        if sf.formula_samples.is_empty() {
                            String::new()
                        } else {
                            format!("（如 {}）", sf.formula_samples.join("、"))
                        }
                    ),
                });
            }
            if sf.merged > 0 {
                losses.push(Loss {
                    kind: "merged_cells".into(),
                    count: sf.merged,
                    detail: format!(
                        "工作表「{sheet}」的 {} 处合并单元格 —— 合并区只有左上角有值，\
                         CSV 里其它格子是空的。很多人以为「整列都有客户名」，\
                         其实合并之后只有第一行有值，导出来就只剩第一行了",
                        sf.merged
                    ),
                });
            }
            if sf.errors > 0 {
                losses.push(Loss {
                    kind: "error_values".into(),
                    count: sf.errors,
                    detail: format!(
                        "工作表「{sheet}」的 {} 个错误值（#REF! / #DIV/0! 等）会原样写成文本",
                        sf.errors
                    ),
                });
            }
            if sf.bools > 0 {
                losses.push(Loss {
                    kind: "booleans".into(),
                    count: sf.bools,
                    detail: format!(
                        "工作表「{sheet}」的 {} 个 TRUE/FALSE 会变成文本，Excel 里不再能参与逻辑判断",
                        sf.bools
                    ),
                });
            }
            if sf.blank_rows > 0 {
                warnings.push(format!(
                    "工作表「{sheet}」里有 {} 行整行是空的，CSV 里会保留成空行。\
                     注意：**本程序再导入 CSV 时会跳过空行**，所以导入后行号可能对不上。",
                    sf.blank_rows
                ));
            }
            if sf.long_numbers > 0 {
                warnings.push(format!(
                    "工作表「{sheet}」里有 {} 处 15 位以上的长数字。它们在 CSV 里是完好的，\
                     但**只要用 Excel 打开这个 CSV 再保存一次**，15 位以后就会变成 0。\
                     要核对请用记事本打开 CSV。",
                    sf.long_numbers
                ));
            }
            if sf.rows > MAX_TABLE_ROWS {
                return Err(format!(
                    "工作表「{sheet}」有 {} 行，超过单次转换上限 {MAX_TABLE_ROWS} 行。\
                     建议按年份拆成几个文件。",
                    sf.rows
                ));
            }

            let out = out_path_for(dst, Some(sheet), false);
            steps.push(format!("工作表「{sheet}」：{} 行 × {} 列", sf.rows, sf.cols));
            // 数据不从 A1 开始时必须说清楚：CSV 是"从左上角开始的一格格文本"，
            // 没有"从 B4 开始"这种表达，所以前面的空行空列会被**去掉**。
            if sf.start != (0, 0) {
                warnings.push(format!(
                    "工作表「{sheet}」的数据区从 {} 开始（左边/上边是空的）。\
                     CSV 没有「从第几格开始」这种表达，所以那些空行空列会被**去掉**，\
                     数据整体移到左上角。如果这个位置对你有意义，请改用 .xlsx。",
                    a1(sf.start.0 as usize, sf.start.1 as usize)
                ));
            }
            units.push(SplitUnit {
                src: src.clone(),
                sheet: sheet.clone(),
                out,
                delim: dst_delim,
                enc: out_enc,
            });
        }

        // 格式类的东西**数不出来**（散在 sheet XML 里，要整包解压才能数），
        // 所以不装作能报数量，只如实说"这类东西不会过去"。
        warnings.push(format!(
            "「{}」里的单元格格式（字体、颜色、边框、数字格式、条件格式、数据验证、批注、\
             冻结窗格、列宽）不会进入 CSV。这些没法按个数统计，但只要有非纯数据的东西，\
             就一定有丢的。原文件不会被改动，需要看格式时请直接打开它。",
            file_label(src)
        ));
    }

    // 只有一个输出时就用目标名本身（用户写了什么就是什么，不做二次加工）
    if units.len() == 1 {
        units[0].out = dst.to_path_buf();
    }
    let outputs: Vec<PathBuf> = units.iter().map(|u| u.out.clone()).collect();
    steps.push(format!(
        "写出 {} 个 .{} 文件（{} 编码，{} 分隔）：{}",
        outputs.len(),
        dst_fmt.ext(),
        out_enc.label(),
        delim_label(dst_delim),
        outputs.iter().map(|p| file_label(p)).collect::<Vec<_>>().join("、")
    ));

    Ok(ConversionPlan {
        steps,
        inputs: srcs.to_vec(),
        outputs,
        warnings,
        losses,
        job: Job::Table(TableJob::Split(units)),
    })
}

/// csv/tsv/txt → csv/tsv：可能只换编码（安全），也可能要换分隔符（真重写）。
fn plan_delimited_rewrite(
    srcs: &[PathBuf],
    dst: &Path,
    dst_fmt: Fmt,
    dst_delim: char,
    opts: &Options,
) -> Result<ConversionPlan> {
    // 源里可能混着不同编码/不同分隔符的文件，所以逐个判定该走哪条路：
    // 只换编码 → 纯重编码（内容一个字节都不动）；换分隔符 → 真解析重写。
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut losses = Vec::new();
    let mut reencode: Vec<TextUnit> = Vec::new();
    let mut reshape: Vec<ReshapeUnit> = Vec::new();

    for src in srcs {
        let bytes = std::fs::metadata(src).map_err(|e| format!("读不到文件信息：{e}"))?.len();
        if bytes > MAX_DELIMITED_BYTES {
            return Err(too_big(src, bytes, MAX_DELIMITED_BYTES, "CSV / TSV"));
        }
        let head = read_head(src, PLAN_SNIFF_BYTES)?;
        let from = crate::csv_import::detect_encoding(&head);
        if from == Encoding::Unknown {
            return Err(format!("「{}」里全是 NUL 之类的控制字节，不像是文本表格。", file_label(src)));
        }
        let (text, _) = decode_text(&head, from);
        let (delim_in, found) = match opts.src_delimiter {
            Some(d) => (d, true),
            None => sniff_delimiter(&text),
        };
        if !found {
            warnings.push(format!("「{}」里没探测到分隔符，会整行当成一格。", file_label(src)));
        }
        // 默认**保持源编码**：用户说"csv → csv"时心里想的是"换个分隔符"，
        // 不是"顺手给我加个 BOM"。要改编码必须显式指定。
        let to = opts.out_encoding.unwrap_or(from);
        let same_ext = src.extension().map(|e| e.to_ascii_lowercase()) == Some(dst_fmt.ext().into());
        let out = out_path_for(dst, None, srcs.len() == 1);

        if delim_in == dst_delim {
            // 只换编码（或只换后缀）。这是最安全的一条路：内容一个字节都不动，
            // 连引号都不重新排 —— 用户手里的表可能就有没转义的引号，
            // 重排一次反而把列搞错位了。
            if from == to && same_ext {
                return Err(format!(
                    "「{}」已经是 {} 编码的 .{} 了，没有任何东西要转。\
                     如果只是想重新排版（比如修正引号），请换个目标名另存一份。",
                    file_label(src),
                    to.label(),
                    dst_fmt.ext()
                ));
            }
            steps.push(format!(
                "「{}」：{} → {}（**内容不动**，只重编码{}）",
                file_label(src),
                from.label(),
                to.label(),
                if same_ext { "" } else { " + 换后缀" }
            ));
            let e = encode_text(&text, to);
            if e.unmappable > 0 {
                losses.push(Loss {
                    kind: "encode_unmappable".into(),
                    count: e.unmappable,
                    detail: format!(
                        "「{}」里有约 {} 个字符（按前 64 KB 估算）在 {} 的映射表里没有对应{}，\
                         会被写成 `&#数字;` 文本 —— 内容已经变了，且不可逆。要保住只能用 UTF-8 输出。",
                        file_label(src),
                        e.unmappable,
                        to.label(),
                        if e.samples.is_empty() {
                            String::new()
                        } else {
                            format!("（如 {}）", e.samples.join("、"))
                        }
                    ),
                });
            }
            if e.four_byte > 0 {
                warnings.push(format!(
                    "「{}」里有约 {} 个字符不在 GBK 范围内{}，会写成 GB18030 的四字节编码。\
                     文件合法，但按 GBK 读的软件会显示成乱码 —— 请优先考虑 UTF-8 输出。",
                    file_label(src),
                    e.four_byte,
                    if e.samples.is_empty() {
                        String::new()
                    } else {
                        format!("（如 {}）", e.samples.join("、"))
                    }
                ));
            }
            reencode.push(TextUnit { src: src.clone(), out, from, to });
        } else {
            // 换分隔符：必须真解析再重写，于是引号会被规范化 —— 这算一次丢失，
            // 因为"某格里的逗号本来没转义"这种脏数据会被重新解释、列可能变。
            steps.push(format!(
                "「{}」：{} → {}（{} → {}）",
                file_label(src),
                delim_label(delim_in),
                delim_label(dst_delim),
                from.label(),
                to.label()
            ));
            losses.push(Loss {
                kind: "requote".into(),
                count: 1,
                detail: "换分隔符要重新排版：每个格子都会按 Excel 的规则重新加引号。\
                         如果原文件里有**没被引号包起来的逗号**（中文地址、商品名里很常见），\
                         从那一行起整表会错位一格 —— 在原文件里它已经是错的，重排只是让它显形。\
                         换完请对着原文件抽查几行。"
                    .into(),
            });
            reshape.push(ReshapeUnit { src: src.clone(), out, delim_in, delim_out: dst_delim, enc: to });
        }
    }

    let outputs =
        if srcs.len() == 1 { vec![dst.to_path_buf()] } else { reencode.iter().map(|u| u.out.clone()).chain(reshape.iter().map(|u| u.out.clone())).collect() };
    Ok(ConversionPlan {
        steps,
        inputs: srcs.to_vec(),
        outputs,
        warnings,
        losses,
        job: Job::Text { text: reencode, reshape },
    })
}

// ---------------- 分派：→ txt（纯重编码） ----------------

fn plan_reencode(
    srcs: &[PathBuf],
    src_fmts: &[Fmt],
    dst: &Path,
    opts: &Options,
) -> Result<ConversionPlan> {
    if let Some((p, _)) = srcs.iter().zip(src_fmts).find(|(_, f)| !f.is_delimited()) {
        return Err(format!(
            "「{}」不能转成 .txt。.txt 只承接纯文本（csv / tsv / txt）的重编码；\
             表格转文本请转成 .csv（那才是表格的文本形式）。",
            p.display()
        ));
    }

    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut losses = Vec::new();
    let mut units = Vec::new();

    for src in srcs {
        let bytes = std::fs::metadata(src).map_err(|e| format!("读不到文件信息：{e}"))?.len();
        if bytes > MAX_TEXT_BYTES {
            return Err(too_big(src, bytes, MAX_TEXT_BYTES, "文本"));
        }
        let head = read_head(src, PLAN_SNIFF_BYTES)?;
        let from = crate::csv_import::detect_encoding(&head);
        if from == Encoding::Unknown {
            return Err(format!("「{}」里全是 NUL 之类的控制字节，不像是文本文件。", file_label(src)));
        }
        let to = opts.out_encoding.unwrap_or(from);
        let same_ext = src.extension().map(|e| e.to_ascii_lowercase()) == Some("txt".into());
        if from == to && same_ext {
            return Err(format!(
                "「{}」已经是 {} 编码的 .txt 了，没有任何东西要转。",
                file_label(src),
                to.label()
            ));
        }
        let (text, _) = decode_text(&head, from);
        let e = encode_text(&text, to);
        steps.push(format!(
            "「{}」：{} → {}（纯重编码，内容一个字节都不动{}）",
            file_label(src),
            from.label(),
            to.label(),
            if same_ext { "" } else { "，并改后缀" }
        ));
        if from == to {
            warnings.push(format!(
                "「{}」的编码本来就是 {}，这次只是改后缀。\
                 如果目标是让 Excel 按列显示，光改后缀没有用 —— 请转成 .csv。",
                file_label(src),
                to.label()
            ));
        }
        if e.unmappable > 0 {
            losses.push(Loss {
                kind: "encode_unmappable".into(),
                count: e.unmappable,
                detail: format!(
                    "「{}」里有约 {} 个字符（按前 64 KB 估算）在 {} 的映射表里没有对应{}，\
                     会被写成 `&#数字;` 文本。要保住只能用 UTF-8 输出。",
                    file_label(src),
                    e.unmappable,
                    to.label(),
                    if e.samples.is_empty() {
                        String::new()
                    } else {
                        format!("（如 {}）", e.samples.join("、"))
                    }
                ),
            });
        }
        if e.four_byte > 0 {
            warnings.push(format!(
                "「{}」里有约 {} 个字符不在 GBK 范围内{}，会写成 GB18030 的四字节编码；\
                 按 GBK 读的软件会显示成乱码，建议改用 UTF-8 输出。",
                file_label(src),
                e.four_byte,
                if e.samples.is_empty() {
                    String::new()
                } else {
                    format!("（如 {}）", e.samples.join("、"))
                }
            ));
        }
        units.push(TextUnit { src: src.clone(), out: out_path_for(dst, None, srcs.len() == 1), from, to });
    }

    Ok(ConversionPlan {
        steps,
        inputs: srcs.to_vec(),
        outputs: if srcs.len() == 1 {
            vec![dst.to_path_buf()]
        } else {
            units.iter().map(|u| u.out.clone()).collect()
        },
        warnings,
        losses,
        job: Job::Text { text: units, reshape: Vec::new() },
    })
}

// ---------------- 分派：图片 ----------------

fn plan_images(srcs: &[PathBuf], dst: &Path, dst_fmt: Fmt, opts: &Options) -> Result<ConversionPlan> {
    // 一次一张。批量由界面层循环调用 —— 这样进度、失败与"哪一个文件出问题"
    // 才是一一对应的，而不是"批量转 20 张，第 13 张失败"这种没法处理的状态。
    if srcs.len() > 1 {
        return Err(
            "一次只转一张图片（请对每张图分别调用一次，界面层循环即可）。\
             这样进度、失败与「哪一个文件出问题」都是一一对应的。"
                .into(),
        );
    }
    let src = &srcs[0];
    let src_fmt = fmt_of(src)?;
    let bytes = std::fs::metadata(src).map_err(|e| format!("读不到文件信息：{e}"))?.len();
    if bytes > MAX_IMAGE_BYTES {
        return Err(too_big(src, bytes, MAX_IMAGE_BYTES, "图片"));
    }
    let probe = probe_image(src)?;

    // 同格式 + 没有任何编辑 = 没有意义（只是重新压一遍，还白掉一次画质）
    if src_fmt == dst_fmt && !opts.has_image_edits() {
        return Err(format!(
            "「{}」本来就是 {}，又没有任何缩放/旋转/翻转要求，没有任何东西要转。\
             （重新压一遍只会**再掉一次画质**：JPEG 每存一次都要重新量化。）",
            file_label(src),
            dst_fmt.ext().to_uppercase()
        ));
    }

    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    let mut losses = Vec::new();

    steps.push(format!(
        "读取 {}（{}×{}，{}，{:.1} KB）",
        file_label(src),
        probe.w,
        probe.h,
        color_label(probe.color),
        bytes as f64 / 1024.0
    ));

    // ---- EXIF 方向：手机拍的图不转正就会躺倒 ----
    if probe.exif != Orientation::NoTransforms {
        steps.push(format!(
            "应用 EXIF 方向标签（{}）—— 照片不会躺倒",
            orientation_label(probe.exif)
        ));
        steps.push(
            "方向标签本身不会写进新文件：它已经烧进像素了，再留一个标签会让别的软件又转一次"
                .into(),
        );
    }
    if probe.exif_extra {
        losses.push(Loss {
            kind: "exif".into(),
            count: 1,
            detail: "EXIF 元数据（相机型号、拍摄时间、光圈，可能还有 GPS 定位）不会写进新文件。\
                     GPS 那一项丢了反而是好事：把图发给别人不会连「在哪儿拍的」一起发出去。"
                .into(),
        });
    }

    // ---- 编辑 ----
    let (ow, oh) = oriented_dims(probe.w, probe.h, probe.exif);
    let (mut tw, mut th) = (ow, oh);
    if let Some(edge) = opts.max_edge {
        let longest = tw.max(th);
        if longest > edge {
            let ratio = f64::from(edge) / f64::from(longest);
            tw = ((f64::from(tw) * ratio).round() as u32).max(1);
            th = ((f64::from(th) * ratio).round() as u32).max(1);
            steps.push(format!("缩放到最长边 {edge} 像素（等比 → {tw}×{th}，Lanczos3 重采样）"));
        } else {
            warnings.push(format!(
                "图的最长边只有 {longest} 像素，比你要的 {edge} 还小，**不会放大** —— \
                 放大只会更糊，不会更清楚。"
            ));
        }
    }
    let r = opts.rotate90 % 4;
    if r != 0 {
        steps.push(format!("顺时针旋转 {}°", u32::from(r) * 90));
        if r % 2 == 1 {
            std::mem::swap(&mut tw, &mut th);
        }
    }
    if opts.flip_h || opts.flip_v {
        steps.push(format!(
            "翻转：{}",
            match (opts.flip_h, opts.flip_v) {
                (true, true) => "水平 + 垂直（等于转 180°）",
                (true, false) => "水平（左右镜像）",
                _ => "垂直（上下镜像）",
            }
        ));
    }

    // ---- 编码器侧的真实损失 ----
    if dst_fmt == Fmt::Jpg {
        if src_fmt != Fmt::Jpg || opts.has_image_edits() {
            losses.push(Loss {
                kind: "jpeg_lossy".into(),
                count: 1,
                detail: format!(
                    "JPEG 是**有损**格式（质量 {}）：画质会有轻微损失，换来的是体积通常只有 \
                     PNG 的 1/5 到 1/20。要无损请用 PNG。",
                    opts.jpeg_quality
                ),
            });
        }
        if probe.has_alpha {
            losses.push(Loss {
                kind: "alpha".into(),
                count: 1,
                detail: "JPEG 没有透明通道：透明区域会被合成到**白色**背景上\
                         （不这么做的话透明区会变成黑块）。具体多少个像素要解码后才数得准，\
                         报告里会给准确数。要保留透明请用 PNG。"
                    .into(),
            });
        }
        if probe.deep_color {
            losses.push(Loss {
                kind: "bit_depth".into(),
                count: 1,
                detail: "源图是 16 位色深，JPEG 只存 8 位，多出来的精度会丢掉（肉眼基本看不出）"
                    .into(),
            });
        }
        warnings.push("JPEG 不保存透明通道，也不保存图层/文字等编辑信息，只存最终的像素。".into());
    } else if dst_fmt == Fmt::Bmp {
        if probe.deep_color {
            losses.push(Loss {
                kind: "bit_depth".into(),
                count: 1,
                detail: "BMP 这一路只写 8 位色深，源图的 16 位精度会丢掉".into(),
            });
        }
        warnings.push(
            "BMP **完全不压缩**，体积通常是 PNG 的 3–10 倍、JPEG 的 10 倍以上。\
             它的用处是「某些老软件只认 BMP」，不是为了省空间。"
                .into(),
        );
    } else if dst_fmt == Fmt::Webp {
        // image 这套库只提供 **无损** WebP 编码（WebPEncoder::new_lossless），
        // 所以"转成 webp 能小很多"这件事在我们这里是**不成立**的，必须说清楚：
        // 无损 WebP 通常比 JPEG 大不少（比 PNG 小一些）。要省空间还是得用 JPEG。
        warnings.push(
            "WebP 这一路写的是**无损**格式（库只提供无损编码），所以照片转过去通常\
             **比 JPEG 大**、比 PNG 小。要发给客户、要省空间请用 JPEG；\
             想要 WebP 的小体积只能靠别的工具做**有损**编码。"
                .into(),
        );
        if probe.deep_color {
            losses.push(Loss {
                kind: "bit_depth".into(),
                count: 1,
                detail: "WebP 这一路只写 8 位色深，源图的 16 位精度会丢掉".into(),
            });
        }
    } else if dst_fmt == Fmt::Png {
        warnings.push(
            "PNG 是无损压缩，照片转过去通常比 JPEG 大 5–20 倍。\
             要发给人看、要省空间请用 JPEG；要截图、要透明背景才用 PNG。"
                .into(),
        );
    }

    if tw.max(th) > MAX_EDGE {
        return Err(format!("变换后的尺寸是 {tw}×{th}，最长边超过 {MAX_EDGE} 像素，请先缩小。"));
    }
    steps.push(format!("写出 {}（{tw}×{th}，{}）", dst.display(), dst_fmt.ext().to_uppercase()));

    let unit = ImageUnit {
        src: src.clone(),
        out: dst.to_path_buf(),
        fmt: dst_fmt,
        exif: probe.exif,
        has_alpha: probe.has_alpha,
        bytes,
        edits: EditParams {
            max_edge: opts.max_edge,
            rotate90: opts.rotate90 % 4,
            flip_h: opts.flip_h,
            flip_v: opts.flip_v,
            jpeg_quality: opts.jpeg_quality,
        },
    };
    Ok(ConversionPlan {
        steps,
        inputs: vec![src.clone()],
        outputs: vec![dst.to_path_buf()],
        warnings,
        losses,
        job: Job::Image(vec![unit]),
    })
}

// ============================================================
// run
// ============================================================

/// 执行一个计划。失败时可能已经写出了前面的文件（见 plan 的警告），
/// 但**绝不会碰源文件**，也绝不会覆盖任何已有文件。
pub fn run(p: &ConversionPlan) -> Result<Report> {
    run_with_progress(p, |_, _| {})
}

/// 带进度的执行。`progress(done, total)` 在**每个可中断点**回调：
///   · 表格：单位是一张要写的工作表（xlsx → csv 就是 N 张）
///   · 图片：单位是"文件 × 3 个阶段"（解码 / 变换 / 编码）—— 慢的就是这三步
///   · 文本：单位是一个文件
///
/// `total` 一开始就是最终值，界面可以直接拿来画进度条。
///
/// `Report::losses` 以**计划里的清单**为底：那些丢失确实发生了（工作表没被导出、
/// 公式被丢掉了），而 run 阶段根本看不到它们（被丢的东西不在输出里）。
/// run 只做两件事：把能精确测量的项换成准确数字（如"多少个透明像素"），
/// 以及补上计划没预见到的项。
pub fn run_with_progress(p: &ConversionPlan, progress: impl Fn(usize, usize)) -> Result<Report> {
    let t0 = std::time::Instant::now();

    // 从 plan 到 run 之间，用户完全可能已经把目标文件建出来了 —— 再查一次。
    for out in &p.outputs {
        if out.exists() {
            return Err(format!(
                "目标文件已存在（可能是在你看计划的时候出现的）：{}\n本程序不覆盖任何已有文件。",
                out.display()
            ));
        }
    }

    let mut rep = Report {
        outputs: Vec::new(),
        bytes_in: 0,
        bytes_out: 0,
        rows: 0,
        losses: p.losses.clone(),
        notes: Vec::new(),
        elapsed_ms: 0,
    };

    match &p.job {
        Job::Table(job) => run_table(job, &progress, &mut rep)?,
        Job::Text { text, reshape } => run_text_job(text, reshape, &progress, &mut rep)?,
        Job::Image(units) => run_images(units, &progress, &mut rep)?,
    }

    // ---- 与计划对账：只查"计划没说、实际却丢了"这一个方向 ----
    // 反方向（计划里说会丢、实际没丢）是"更保守"，不值得打扰用户；
    // 而实际丢了计划没提的东西，才是必须点名的 —— 那说明我们漏看了。
    let mut planned: Vec<&str> = p.losses.iter().map(|l| l.kind.as_str()).collect();
    let mut actual: Vec<&str> = rep.losses.iter().map(|l| l.kind.as_str()).collect();
    planned.sort_unstable();
    planned.dedup();
    actual.sort_unstable();
    actual.dedup();
    let extra: Vec<&&str> = actual.iter().filter(|k| !planned.contains(*k)).collect();
    if !extra.is_empty() {
        rep.notes.push(format!(
            "注意：实际还丢了 {extra:?}，计划里没预告到 —— 以这里为准（计划只能看文件头与目录）。"
        ));
    }

    rep.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(rep)
}

/// 把某一类的丢失换成**更准确**的那条（同 kind 只留一条，不重复计数）。
fn set_loss(rep: &mut Report, loss: Loss) {
    rep.losses.retain(|l| l.kind != loss.kind);
    rep.losses.push(loss);
}

/// 某一类的丢失实际没有发生（如"以为有透明像素，其实没有"）。
fn clear_loss(rep: &mut Report, kind: &str) {
    rep.losses.retain(|l| l.kind != kind);
}

/// 把"编码时变了样的东西"报出去。表格与文本两条路都要用，所以提出来。
///
/// 两种情况必须分开，因为**严重程度不一样**：
///   · 真的装不下（写成 `&#数字;`）→ 是 `Loss`：内容已经变了，而且不可逆。
///   · 需要 GB18030 四字节形式（GBK 之外的字，如 emoji）→ 只是 `notes`：
///     一个字节都没丢，但中文 Windows 上很多软件按 GBK 读会显示成乱码。
fn report_encoding_issues(rep: &mut Report, what: &str, e: &Encoded, enc: Encoding) {
    if e.unmappable > 0 {
        set_loss(
            rep,
            Loss {
                kind: "encode_unmappable".into(),
                count: e.unmappable,
                detail: format!(
                    "{what} 里有 {} 个字符在 {} 的映射表里**没有对应**{}，\
                     已被写成 `&#数字;` 这样的文本。这类字符没法在这一编码里表示，\
                     要保住它们只能选 UTF-8 输出。",
                    e.unmappable,
                    enc.label(),
                    if e.samples.is_empty() {
                        String::new()
                    } else {
                        format!("（如 {}）", e.samples.join("、"))
                    }
                ),
            },
        );
    }
    if e.four_byte > 0 {
        rep.notes.push(format!(
            "{what} 里有 {} 个字符不在 GBK 范围内{}，写成了 GB18030 的**四字节**编码。\
             文件本身是合法的，但中文 Windows 上不少软件按 GBK 读，会把这几个字显示成乱码 —— \
             要让哪里都能正常显示，请选 UTF-8 输出。",
            e.four_byte,
            if e.samples.is_empty() {
                String::new()
            } else {
                format!("（如 {}）", e.samples.join("、"))
            }
        ));
    }
}

fn run_table(job: &TableJob, progress: &impl Fn(usize, usize), rep: &mut Report) -> Result<()> {
    match job {
        TableJob::Split(units) => {
            let total = units.len();
            for (i, u) in units.iter().enumerate() {
                progress(i, total);
                let (rows, bytes) = read_workbook_sheet(&u.src, &u.sheet)?;
                rep.bytes_in += bytes;
                rep.rows += rows.len().saturating_sub(1);
                let mut text = String::new();
                write_delimited(&mut text, &rows, u.delim);
                let (out_bytes, e) = write_text_atomic(&u.out, &text, u.enc)?;
                rep.bytes_out += out_bytes;
                rep.outputs.push(u.out.clone());
                report_encoding_issues(rep, &format!("工作表「{}」", u.sheet), &e, u.enc);
                progress(i + 1, total);
            }
        }
        TableJob::Merge(m) => {
            let total = m.srcs.len() + 1;
            let mut sheets: Vec<(String, Vec<Vec<String>>)> = Vec::new();
            for (i, (path, delim)) in m.srcs.iter().enumerate() {
                progress(i, total);
                let (text, bytes) = read_delimited(path)?;
                rep.bytes_in += bytes;
                let rows = parse_delimited(&text, *delim);
                if rows.len() > MAX_TABLE_ROWS {
                    return Err(format!(
                        "「{}」有 {} 行，超过单次转换上限 {MAX_TABLE_ROWS} 行。建议按年份拆分。",
                        file_label(path),
                        rows.len()
                    ));
                }
                rep.rows += rows.len().saturating_sub(1);
                sheets.push((m.sheet_names[i].clone(), rows));
            }
            progress(m.srcs.len(), total);
            let bytes = write_workbook(&m.out, &sheets)?;
            rep.bytes_out += bytes;
            rep.outputs.push(m.out.clone());
            progress(total, total);
        }
    }
    Ok(())
}

fn run_text_job(
    text: &[TextUnit],
    reshape: &[ReshapeUnit],
    progress: &impl Fn(usize, usize),
    rep: &mut Report,
) -> Result<()> {
    let total = text.len() + reshape.len();
    let mut done = 0usize;

    for u in reshape {
        progress(done, total);
        let (src_text, bytes) = read_delimited(&u.src)?;
        rep.bytes_in += bytes;
        let rows = parse_delimited(&src_text, u.delim_in);
        rep.rows += rows.len().saturating_sub(1);
        let mut out_text = String::new();
        write_delimited(&mut out_text, &rows, u.delim_out);
        let (out_bytes, e) = write_text_atomic(&u.out, &out_text, u.enc)?;
        rep.bytes_out += out_bytes;
        rep.outputs.push(u.out.clone());
        report_encoding_issues(rep, &format!("「{}」", file_label(&u.src)), &e, u.enc);
        rep.notes.push(format!(
            "「{}」的引号已按 Excel 规则重新排版（{} → {}）",
            file_label(&u.src),
            delim_label(u.delim_in),
            delim_label(u.delim_out)
        ));
        set_loss(
            rep,
            Loss {
                kind: "requote".into(),
                count: 1,
                detail: format!(
                    "「{}」的引号已按 Excel 规则重新排版（{} → {}）",
                    file_label(&u.src),
                    delim_label(u.delim_in),
                    delim_label(u.delim_out)
                ),
            },
        );
        done += 1;
        progress(done, total);
    }

    for u in text {
        progress(done, total);
        let bytes = std::fs::read(&u.src).map_err(|e| format!("读不出「{}」：{e}", file_label(&u.src)))?;
        rep.bytes_in += bytes.len() as u64;
        if bytes.is_empty() {
            return Err(format!("「{}」是 0 字节的空文件。", file_label(&u.src)));
        }
        let enc = crate::csv_import::detect_encoding(&bytes);
        if enc == Encoding::Unknown {
            return Err(format!("「{}」里全是 NUL 之类的控制字节。", file_label(&u.src)));
        }
        let (src_text, failed) = decode_text(&bytes, enc);
        if failed {
            rep.notes.push(format!(
                "「{}」按 {} 解码时有失败的位置（文件里可能本来就有坏字节），这些位置会变成「�」。",
                file_label(&u.src),
                enc.label()
            ));
        }
        // 源是 UTF-8 带 BOM、目标也是 UTF-8 时继续带 BOM：用户只是想换个编码，
        // 不是想改"记事本/Excel 靠 BOM 认编码"这件事。
        let to = if u.to == Encoding::Utf8 && u.from == Encoding::Utf8Bom {
            Encoding::Utf8Bom
        } else {
            u.to
        };
        let (out_bytes, e) = write_text_atomic(&u.out, &src_text, to)?;
        rep.bytes_out += out_bytes;
        rep.outputs.push(u.out.clone());
        report_encoding_issues(rep, &format!("「{}」", file_label(&u.src)), &e, to);
        rep.notes.push(format!(
            "「{}」：{} → {}，{} KB → {} KB（{}）",
            file_label(&u.src),
            u.from.label(),
            to.label(),
            bytes.len() / 1024,
            out_bytes / 1024,
            pct_change(bytes.len() as u64, out_bytes)
        ));
        done += 1;
        progress(done, total);
    }
    Ok(())
}

fn run_images(units: &[ImageUnit], progress: &impl Fn(usize, usize), rep: &mut Report) -> Result<()> {
    let total = units.len() * 3;
    for (i, u) in units.iter().enumerate() {
        let base = i * 3;
        progress(base, total);
        let (img, _) = read_image(&u.src)?;
        rep.bytes_in += u.bytes;
        progress(base + 1, total);

        let mut img = img;
        if u.exif != Orientation::NoTransforms {
            img.apply_orientation(u.exif);
            rep.notes
                .push(format!("已应用 EXIF 方向（{}）—— 照片不会躺倒", orientation_label(u.exif)));
        }
        let (w0, h0) = (img.width(), img.height());
        let img = apply_edits(img, &u.edits);
        let (w1, h1) = (img.width(), img.height());
        let (img, transparent) = prepare_for_encoder(img, u.fmt);
        progress(base + 2, total);

        let out_bytes = write_image(&u.out, img, u.fmt, u.edits.jpeg_quality)?;
        rep.bytes_out += out_bytes;
        rep.outputs.push(u.out.clone());

        // 透明像素数只有解完码才知道 —— 计划里记的是 1（"这件事会发生"），
        // 这里换成准确数字。数量对不上的情况（计划说 1、实际 12000）不是预测错误，
        // 所以对账只查"种类"。
        if u.fmt == Fmt::Jpg {
            if transparent > 0 {
                set_loss(
                    rep,
                    Loss {
                        kind: "alpha".into(),
                        count: transparent,
                        detail: format!(
                            "{transparent} 个透明像素被合成到了白色背景上（JPEG 存不下透明通道）"
                        ),
                    },
                );
            } else if u.has_alpha {
                // 有 alpha 通道但一个透明像素都没有（整图不透明）—— 那就不算丢
                clear_loss(rep, "alpha");
            }
        }

        if u.bytes > 0 {
            rep.notes.push(format!(
                "{}×{} → {}×{}，体积 {} KB → {} KB（{}）",
                w0,
                h0,
                w1,
                h1,
                u.bytes / 1024,
                out_bytes / 1024,
                pct_change(u.bytes, out_bytes)
            ));
        }
        let _ = &u.fmt;
        progress(base + 3, total);
    }
    Ok(())
}

// ============================================================
// 表格：读
// ============================================================

fn check_src(src: &Path, fmt: Fmt) -> Result<()> {
    let meta = std::fs::metadata(src).map_err(|e| format!("读不到「{}」：{e}", src.display()))?;
    if !meta.is_file() {
        return Err(format!("「{}」不是一个文件（是个目录？）。", src.display()));
    }
    if meta.len() == 0 {
        return Err(format!(
            "「{}」是 0 字节的空文件，没有可转换的内容。（空文件转出来还是空文件，\
             却会让人以为转换成功了。）",
            src.display()
        ));
    }
    // 表格源要做魔数交叉验证：后缀是 .xlsx 其实是 HTML 的"Excel 文件"在老 ERP
    // 导出里非常常见（`xlsx.rs` 的 sniff 就是为它写的）。
    if fmt == Fmt::Xlsx {
        match crate::xlsx::sniff(src)? {
            crate::xlsx::FileKind::Zip | crate::xlsx::FileKind::Ole2 => {}
            crate::xlsx::FileKind::Html => {
                return Err(format!(
                    "「{}」其实是 HTML 表格伪装成了 Excel 文件（老 ERP / 网页导出很常见）。\
                     请先用 Excel 或 WPS 打开它，另存为真正的 .xlsx 再转。",
                    file_label(src)
                ))
            }
            crate::xlsx::FileKind::Text => {
                return Err(format!(
                    "「{}」的后缀是 Excel，内容却是纯文本。请先确认它到底是什么格式\
                     （如果其实是逗号分隔的，把后缀改成 .csv 就能转）。",
                    file_label(src)
                ))
            }
        }
    }
    Ok(())
}

fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打不开「{}」：{e}", path.display()))?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf).map_err(|e| format!("读不出「{}」：{e}", path.display()))?;
    buf.truncate(read);
    Ok(buf)
}

/// (工作表名清单, 文件字节数)。只解包到 workbook.xml，不读单元格。
fn workbook_sheets(path: &Path) -> Result<(u64, Vec<String>)> {
    let bytes = std::fs::metadata(path).map_err(|e| format!("读不到文件信息：{e}"))?.len();
    let wb = open_workbook_auto(path).map_err(|e| {
        format!("打不开这个表格文件：{e}\n若是老版本的 .xls，请先用 Excel 或 WPS 另存为 .xlsx 再试。")
    })?;
    Ok((bytes, wb.sheet_names().to_vec()))
}

/// 读一张工作表 → 行/列文本 + 文件字节数。
fn read_workbook_sheet(path: &Path, sheet: &str) -> Result<(Vec<Vec<String>>, u64)> {
    let bytes = std::fs::metadata(path).map_err(|e| format!("读不到文件信息：{e}"))?.len();
    let mut wb = open_workbook_auto(path).map_err(|e| format!("打不开这个表格文件：{e}"))?;
    let range = wb.worksheet_range(sheet).map_err(|e| format!("读不出工作表「{sheet}」：{e}"))?;
    let (h, w) = (range.height(), range.width());
    if h.saturating_mul(w) > MAX_TABLE_CELLS {
        return Err(format!(
            "工作表「{sheet}」有 {h} 行 × {w} 列（约 {:.0} 万个单元格），超过上限 {} 万。\
             这通常意味着表里有大片空白格式区，请在 Excel 里删掉数据区右侧与下方的空行空列。",
            h as f64 * w as f64 / 10000.0,
            MAX_TABLE_CELLS / 10000
        ));
    }
    if h > MAX_TABLE_ROWS {
        return Err(format!(
            "工作表「{sheet}」有 {h} 行，超过单次转换上限 {MAX_TABLE_ROWS} 行。建议按年份拆分。"
        ));
    }
    let rows: Vec<Vec<String>> = (0..h)
        .map(|r| {
            (0..w).map(|c| cell_to_text(range.get((r, c)).unwrap_or(&Data::Empty))).collect()
        })
        .collect();
    Ok((rows, bytes))
}

fn read_delimited(path: &Path) -> Result<(String, u64)> {
    let bytes = std::fs::read(path).map_err(|e| format!("读不出「{}」：{e}", file_label(path)))?;
    if bytes.len() as u64 > MAX_DELIMITED_BYTES {
        return Err(too_big(path, bytes.len() as u64, MAX_DELIMITED_BYTES, "CSV / TSV"));
    }
    let enc = crate::csv_import::detect_encoding(&bytes);
    if enc == Encoding::Unknown {
        return Err(format!("「{}」里全是 NUL 之类的控制字节。", file_label(path)));
    }
    let (text, _) = decode_text(&bytes, enc);
    Ok((text, bytes.len() as u64))
}

/// calamine 的单元格 → 文本。
///
/// 与 `xlsx.rs` 的 `cell_text` 规则保持一致（那边是私有的，改不动，只能对齐）：
/// 日期只取到「天」（会计单据精确到天，一旦带上时分秒，跨时区解释就是新的坑），
/// 整数不写成科学计数法（那正是 Excel 毁掉长编号的样子）。
/// 唯一的差别是错误值：这里写成 Excel 自己的 `#REF!`（calamine 的 Display 就是
/// 这个），比 `xlsx.rs` 的 `#ERR:Ref` 更像用户手里的原件。
fn cell_to_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => fmt_number(*f),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
        Data::DateTime(dt) => match dt.as_datetime() {
            Some(d) => {
                let s = d.to_string();
                s.get(..10).unwrap_or(&s).to_string()
            }
            None => fmt_number(dt.as_f64()),
        },
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e}"),
    }
}

fn fmt_number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// 一张工作表里"会丢的东西"。
struct SheetFacts {
    rows: usize,
    cols: usize,
    /// 数据区左上角在表里的位置。不是 A1 时要在计划里说清楚 ——
    /// 否则用户会发现"我表里的数据从 B4 开始，转出来的 CSV 却从第一行开始"。
    start: (u32, u32),
    formulas: usize,
    formula_samples: Vec<String>,
    merged: usize,
    errors: usize,
    bools: usize,
    blank_rows: usize,
    long_numbers: usize,
}

fn sheet_facts(path: &Path, sheet: &str) -> Result<SheetFacts> {
    let mut wb = open_workbook_auto(path).map_err(|e| format!("打不开这个表格文件：{e}"))?;
    let range = wb.worksheet_range(sheet).map_err(|e| format!("读不出工作表「{sheet}」：{e}"))?;
    let (h, w) = (range.height(), range.width());

    let mut f = SheetFacts {
        rows: h,
        cols: w,
        start: range.start().unwrap_or((0, 0)),
        formulas: 0,
        formula_samples: Vec::new(),
        merged: merged_count(&mut wb, sheet),
        errors: 0,
        bools: 0,
        blank_rows: 0,
        long_numbers: 0,
    };

    // 公式：calamine 能按格返回公式文本，所以"丢 18 个公式"是**数出来的**，不是猜的。
    // 这是本模块最想报准的一条。
    //
    // ⚠️ 公式 Range 的 (0,0) **不一定**是 A1：calamine 的 Range 是从"第一个有内容的
    // 格子"开始的，所以要用 start() 把偏移加回去，否则告警里的单元格号会指向别处 ——
    // 用户照着去找会找不到，那比不报还糟。
    if let Ok(fr) = wb.worksheet_formula(sheet) {
        let (fh, fw) = (fr.height(), fr.width());
        let (r0, c0) = fr.start().map(|(r, c)| (r as usize, c as usize)).unwrap_or((0, 0));
        for r in 0..fh {
            for c in 0..fw {
                let v = fr.get((r, c)).map(|s| s.as_str()).unwrap_or("");
                if v.is_empty() {
                    continue;
                }
                f.formulas += 1;
                if f.formula_samples.len() < MAX_SAMPLES {
                    f.formula_samples.push(format!(
                        "{} 的 {}{}",
                        a1(r + r0, c + c0),
                        if v.starts_with('=') { "" } else { "=" },
                        clip(v, 24)
                    ));
                }
            }
        }
    }

    let mut all_blank_run = 0usize;
    for r in 0..h {
        let mut blank = true;
        for c in 0..w {
            match range.get((r, c)).unwrap_or(&Data::Empty) {
                Data::Empty => {}
                Data::Bool(_) => {
                    blank = false;
                    f.bools += 1;
                }
                Data::Error(_) => {
                    blank = false;
                    f.errors += 1;
                }
                Data::Float(x) => {
                    blank = false;
                    if x.fract() == 0.0 && x.abs() >= 1e15 {
                        f.long_numbers += 1;
                    }
                }
                Data::Int(x) => {
                    blank = false;
                    if x.abs() >= 1_000_000_000_000_000 {
                        f.long_numbers += 1;
                    }
                }
                Data::String(s) => {
                    blank = false;
                    if is_long_number(s) {
                        f.long_numbers += 1;
                    }
                }
                _ => blank = false,
            }
        }
        if blank {
            all_blank_run += 1;
        } else {
            // 只有"最后一行非空之前"的空行才会真的变成 CSV 里的空行；
            // 尾部的空行是 calamine 的维度造成的，不算。
            f.blank_rows += all_blank_run;
            all_blank_run = 0;
        }
    }
    Ok(f)
}

/// 数合并区域。calamine 的合并 API **只在具体的 `Xlsx` 类型上**，不在 `Reader`
/// trait 上，所以要匹配枚举变体 —— 与 `xlsx.rs` 同样的写法。
fn merged_count(wb: &mut calamine::Sheets<std::io::BufReader<std::fs::File>>, name: &str) -> usize {
    match wb {
        calamine::Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(name).map(|v| v.len()).unwrap_or(0),
        _ => 0,
    }
}

/// 行/列位置 → A1 记法（"C12"）。告警里写 A1，用户才能立刻定位到那一格。
fn a1(row: usize, col: usize) -> String {
    let mut letters = String::new();
    let mut c = col;
    loop {
        letters.insert(0, (b'A' + (c % 26) as u8) as char);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    format!("{letters}{}", row + 1)
}

// ============================================================
// 表格：ZIP 目录（数嵌入图片 / 图表 / 宏 / 外部链接）
// ============================================================

/// ZIP 里"额外东西"的计数。
///
/// 数的是**条目名**，不解析 XML —— 所以它是"至少这么多"，不是 Excel 界面里
/// 逐个数出来的精确值。这一点在措辞上讲清楚，别让用户以为我们数过。
#[derive(Debug, Default, Clone)]
struct ZipFacts {
    images: usize,
    charts: usize,
    drawings: usize,
    comments: usize,
    /// 宏（.xlsm 的 vbaProject.bin）
    macros: bool,
    external_links: usize,
    /// 读不出目录时的原因（不当作错误：数不出来就说数不出来）
    unknown: Option<String>,
}

/// 只读 ZIP 的**中央目录**：那里的文件名是**没有压缩**的，所以"这份 xlsx 里
/// 塞了几张图片"不用解压任何一个字节就能数出来。
///
/// 为什么不用现成的 ZIP 库：`zip` 只是 calamine 的传递依赖，本模块按约束不能改
/// `Cargo.toml`。而我们真的只需要文件名清单 —— 自己走一遍中央目录只有几十行，
/// 还顺带避免了 ZIP 炸弹（一个字节都不解压）。
fn zip_facts(path: &Path) -> ZipFacts {
    match zip_entry_names(path) {
        Ok(names) => {
            let mut f = ZipFacts::default();
            for n in &names {
                if n.starts_with("xl/media/") {
                    f.images += 1;
                } else if n.starts_with("xl/charts/") && n.ends_with(".xml") {
                    f.charts += 1;
                } else if n.starts_with("xl/drawings/drawing") && n.ends_with(".xml") {
                    f.drawings += 1;
                } else if n.starts_with("xl/comments") {
                    f.comments += 1;
                } else if n.starts_with("xl/externalLinks/") && n.ends_with(".xml") {
                    f.external_links += 1;
                } else if n == "xl/vbaProject.bin" {
                    f.macros = true;
                }
            }
            f
        }
        Err(e) => ZipFacts { unknown: Some(e), ..Default::default() },
    }
}

fn push_zip_losses(zf: &ZipFacts, losses: &mut Vec<Loss>, warnings: &mut Vec<String>) {
    if let Some(why) = &zf.unknown {
        warnings.push(format!(
            "读不出这个 Excel 文件的内部目录（{why}），所以**数不出**它里面有几张图片/图表。\
             转成 CSV 时这些东西一定会丢，只是说不清丢几个。"
        ));
        return;
    }
    if zf.images > 0 {
        losses.push(Loss {
            kind: "images".into(),
            count: zf.images,
            detail: format!("{} 张嵌在工作表里的图片（CSV 是纯文本，存不了图片）", zf.images),
        });
    }
    if zf.charts > 0 {
        losses.push(Loss {
            kind: "charts".into(),
            count: zf.charts,
            detail: format!(
                "{} 个图表（图表的形状与数据引用都在 xlsx 里，CSV 里没有它的位置）",
                zf.charts
            ),
        });
    }
    // 有图片时 Excel 也会为它写一个 drawing 条目，那时 drawing 数 ≠ 图形数，
    // 直接相减避免把同一张图算两遍（这里的精度上限：不做 XML 解析）。
    let shapes = zf.drawings.saturating_sub(zf.images);
    if shapes > 0 {
        losses.push(Loss {
            kind: "drawings".into(),
            count: shapes,
            detail: format!("{shapes} 处图形 / 文本框 / 箭头（画在表格上面的东西）"),
        });
    }
    if zf.comments > 0 {
        losses.push(Loss {
            kind: "comments".into(),
            count: zf.comments,
            detail: format!("{} 处批注（写在格子上的备注，CSV 没有这个结构）", zf.comments),
        });
    }
    if zf.external_links > 0 {
        losses.push(Loss {
            kind: "external_links".into(),
            count: zf.external_links,
            detail: format!(
                "{} 处外部链接（这个表引用了别的文件里的数据）。转成 CSV 后，\
                 那些「引来的值」会固定成当时看到的数字，之后源文件变了也不会跟着变。",
                zf.external_links
            ),
        });
    }
    if zf.macros {
        losses.push(Loss {
            kind: "macros".into(),
            count: 1,
            detail: "这个工作簿带有宏（VBA）。宏只存在于 xlsx/xlsm 里，CSV 里没有它的位置；\
                     而且**本程序从不执行宏**，只是把它当作一个会被丢掉的东西告诉你。"
                .into(),
        });
    }
}

/// 列出 ZIP 中央目录里的文件名。**不解压任何内容。**
fn zip_entry_names(path: &Path) -> std::result::Result<Vec<String>, String> {
    const EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    const CD_SIG: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    const EOCD_MIN: usize = 22;
    /// EOCD 后面最多跟 65535 字节的注释
    const TAIL_MAX: usize = EOCD_MIN + 0xffff;

    let mut f = std::fs::File::open(path).map_err(|e| format!("打不开文件：{e}"))?;
    let len = f.metadata().map_err(|e| format!("读不到文件信息：{e}"))?.len();
    if len < EOCD_MIN as u64 {
        return Err("文件太小，不是有效的 ZIP".into());
    }
    let tail_len = TAIL_MAX.min(len as usize);
    let mut tail = vec![0u8; tail_len];
    f.seek(SeekFrom::End(-(tail_len as i64))).map_err(|e| e.to_string())?;
    f.read_exact(&mut tail).map_err(|e| e.to_string())?;

    // 从后往前找 EOCD（注释里也可能碰巧出现这个签名，所以取最靠后的那个）
    let mut eocd = None;
    let mut i = tail.len() - EOCD_MIN;
    loop {
        if tail[i..i + 4] == EOCD_SIG {
            eocd = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let e = eocd.ok_or_else(|| "找不到 ZIP 的中央目录（文件可能被截断）".to_string())?;

    let u16le = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
    let u32le = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
    let count = u16le(&tail, e + 10);
    let cd_size = u32le(&tail, e + 12);
    let cd_off = u32le(&tail, e + 16);
    // ZIP64（>4 GB 或 >65535 个条目）要读额外的记录。办公表格到不了这个量级，
    // 与其写半套 ZIP64，不如老实说"数不出来"。
    if count == 0xffff || cd_size == 0xffff_ffff || cd_off == 0xffff_ffff {
        return Err("这是 ZIP64 格式的包，本程序不支持统计它的条目".into());
    }
    if cd_size > MAX_CENTRAL_DIR || cd_off > len {
        return Err("中央目录异常大，放弃统计".into());
    }

    let mut cd = vec![0u8; cd_size as usize];
    f.seek(SeekFrom::Start(cd_off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut cd).map_err(|e| e.to_string())?;

    let mut names = Vec::new();
    let mut p = 0usize;
    for _ in 0..count {
        if p + 46 > cd.len() || cd[p..p + 4] != CD_SIG {
            break; // 结构不对就停下，返回已经拿到的部分
        }
        let name_len = u16le(&cd, p + 28) as usize;
        let extra_len = u16le(&cd, p + 30) as usize;
        let comment_len = u16le(&cd, p + 32) as usize;
        let start = p + 46;
        if start + name_len > cd.len() {
            break;
        }
        names.push(String::from_utf8_lossy(&cd[start..start + name_len]).into_owned());
        p = start + name_len + extra_len + comment_len;
    }
    Ok(names)
}

// ============================================================
// 表格：写
// ============================================================

/// 写一个 csv/tsv 文本。
///
/// 引号规则与 Excel 一致：只有"会把结构搞坏"的字段才加引号（含分隔符 / 引号 /
/// 换行，或首尾有空格）。**不给每个字段都加引号** —— 那种文件用记事本看是一堆
/// 引号，会计会以为我们动了他的数据。
fn write_delimited(out: &mut String, rows: &[Vec<String>], delim: char) {
    for r in rows {
        for (i, cell) in r.iter().enumerate() {
            if i > 0 {
                out.push(delim);
            }
            if need_quote(cell, delim) {
                out.push('"');
                for ch in cell.chars() {
                    if ch == '"' {
                        out.push('"'); // 引号自己翻倍
                    }
                    out.push(ch);
                }
                out.push('"');
            } else {
                out.push_str(cell);
            }
        }
        // 统一 CRLF：Windows 上的「CSV」就是 CRLF，用记事本打开也不会连成一行。
        out.push_str("\r\n");
    }
}

fn need_quote(s: &str, delim: char) -> bool {
    s.contains(delim)
        || s.contains('"')
        || s.contains('\n')
        || s.contains('\r')
        || s.starts_with(' ')
        || s.ends_with(' ')
}

/// 写一个 xlsx。返回写出的字节数。
fn write_workbook(out: &Path, sheets: &[(String, Vec<Vec<String>>)]) -> Result<u64> {
    let mut cells = 0usize;
    for (_, rows) in sheets {
        if rows.len() > MAX_TABLE_ROWS {
            return Err(format!(
                "有一张表 {} 行，超过单次转换上限 {MAX_TABLE_ROWS} 行。建议按年份拆分。",
                rows.len()
            ));
        }
        for r in rows {
            cells += r.len();
        }
    }
    if cells > MAX_TABLE_CELLS {
        return Err(format!(
            "合计约 {:.0} 万个单元格，超过单次转换上限 {} 万个。建议拆成几个文件。",
            cells as f64 / 10000.0,
            MAX_TABLE_CELLS / 10000
        ));
    }

    write_atomically(out, |tmp| {
        let mut wb = Workbook::new();
        // 表头：加粗 + 居中 + 冻结首行。CSV 的第一行本来就是表头（这也是
        // csv_import 的既定理解）；万一它不是，那也**只是看着像表头**，数据一格没动。
        let head = Format::new().set_bold().set_align(FormatAlign::Center);
        // 文本格式：订单号/身份证这类长编号必须保持文本，否则 Excel 一打开
        // 就变成科学计数法（本项目最不能忍的一条，见 xlsx.rs）
        let text = Format::new().set_num_format("@");
        let num = Format::new().set_num_format("#,##0.00");

        for (name, rows) in sheets {
            let ws = wb.add_worksheet();
            ws.set_name(name).map_err(|e| format!("工作表名不合法：{e}"))?;
            if let Some(header) = rows.first() {
                for (c, h) in header.iter().enumerate() {
                    ws.write_string_with_format(0, c as u16, h, &head).map_err(|e| e.to_string())?;
                    ws.set_column_width(c as u16, (h.chars().count() as f64 * 2.0).clamp(8.0, 30.0))
                        .map_err(|e| e.to_string())?;
                }
                ws.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
            }
            for (r, row) in rows.iter().enumerate().skip(1) {
                for (c, v) in row.iter().enumerate() {
                    if v.is_empty() {
                        continue;
                    }
                    let rr = r as u32;
                    let cc = c as u16;
                    if is_plain_number(v) {
                        ws.write_number_with_format(rr, cc, v.trim().parse::<f64>().unwrap_or(0.0), &num)
                            .map_err(|e| e.to_string())?;
                    } else {
                        ws.write_string_with_format(rr, cc, v, &text).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        wb.save(tmp).map_err(|e| format!("写 Excel 失败：{e}"))?;
        std::fs::metadata(tmp).map(|m| m.len()).map_err(|e| e.to_string())
    })
}

/// 能不能安全当数字写。
///
/// 与 `xlsx.rs` 的 `is_plain_amount` 有**一处刻意的不同**：带前导零的一律拒绝。
/// `is_plain_amount("007")` 是 true（长度 ≤ 15、能 parse），于是"007"会被当数字
/// 写成 7 —— 而"007"这种编号在会计和仓管的表里到处都是。
/// 这里宁可多留一点文本，也不让编号悄悄变形。
fn is_plain_number(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() || t.len() > 15 {
        return false;
    }
    if t.contains(['e', 'E']) {
        return false; // 科学计数法形状本身就是被 Excel 毁过的痕迹
    }
    if !t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+') {
        return false;
    }
    let body = t.trim_start_matches(['+', '-']);
    if body.len() > 1 && body.starts_with('0') && !body.starts_with("0.") {
        return false; // 前导零 → 当文本，别把编号变成数字
    }
    t.parse::<f64>().is_ok()
}

// ============================================================
// 文本编码
// ============================================================

/// 按探测到的编码解码成 String。返回 `(文本, 有没有解码失败)`。
///
/// 为什么自己写而不是复用 `csv_import`：那边的解码函数是私有的，本模块改不动它。
/// 规则与它完全一致（BOM 必须切掉、GB18030 覆盖 GBK、UTF-16 交给标准库）。
fn decode_text(bytes: &[u8], enc: Encoding) -> (String, bool) {
    const BOM8: [u8; 3] = [0xEF, 0xBB, 0xBF];
    match enc {
        Encoding::Utf8 => match std::str::from_utf8(bytes) {
            Ok(s) => (s.to_string(), false),
            Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
        },
        Encoding::Utf8Bom => {
            // BOM 必须在这里切掉：它不可见，留着就会变成第一个字段名的一部分，
            // 于是界面上的"客户"和文件里的"\u{FEFF}客户"永远对不上。
            let body = bytes.strip_prefix(&BOM8[..]).unwrap_or(bytes);
            match std::str::from_utf8(body) {
                Ok(s) => (s.to_string(), false),
                Err(_) => (String::from_utf8_lossy(body).into_owned(), true),
            }
        }
        // 用 decode_without_bom_handling 而不是 decode：后者会自动嗅探 BOM 并按
        // 嗅探结果换编码，那种"静默换解码器"的行为出问题时极难排查。
        Encoding::Gb18030 => {
            let (t, err) = GB18030.decode_without_bom_handling(bytes);
            (t.into_owned(), err)
        }
        Encoding::Utf16Le => decode_utf16(bytes, true),
        Encoding::Utf16Be => decode_utf16(bytes, false),
        Encoding::Unknown => (String::new(), true),
    }
}

/// UTF-16 → String 用标准库手写，不调 `encoding_rs`。
///
/// 为什么：UTF-16 到 UTF-8 的转换在标准库里就是完备的（代理对、奇数字节都由
/// `decode_utf16` 处理），为它多调一个库只会让"我们到底依赖了什么"变模糊。
/// `encoding_rs` 真正不可替代的是 GB18030 那张两万多条的映射表。
fn decode_utf16(bytes: &[u8], little: bool) -> (String, bool) {
    let body = if bytes.len() >= 2 { &bytes[2..] } else { &[][..] }; // 切掉 BOM
    let chunks = body.chunks_exact(2);
    let mut bad = !chunks.remainder().is_empty(); // 尾巴多出单字节 = 文件本身坏了
    let units: Vec<u16> = chunks
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    let mut s = String::with_capacity(units.len());
    for r in std::char::decode_utf16(units) {
        match r {
            Ok(c) => s.push(c),
            Err(_) => {
                s.push('\u{FFFD}');
                bad = true;
            }
        }
    }
    (s, bad)
}

/// 一次编码的完整结果。**必须带上"有多少字符变了样"**。
#[derive(Debug, Clone)]
struct Encoded {
    bytes: Vec<u8>,
    /// 目标编码**真的装不下**的字符数（会被写成 `&#数字;` 这样的文本）。
    ///
    /// GB18030（encoding_rs 实现的是 GB18030-2022）覆盖了几乎全部 Unicode，
    /// 只有极少数码位（如 U+E5E5，官方表里就没有映射）会落到这里。
    /// 一旦发生就是"输出能打开、内容却变了"，所以必须计数并报警。
    unmappable: usize,
    /// 需要 **GB18030 四字节形式**的字符数（也就是 GBK 之外的字，比如 emoji）。
    ///
    /// 这类字符**没有丢**（GB18030 里它们是合法的四字节序列），但中文 Windows 上
    /// 大量软件（记事本、老 Excel、老 ERP）是按 GBK/ANSI 读文件的，看到四字节序列会
    /// 显示成乱码 —— 所以它必须被提前说出来，否则用户会以为我们写坏了文件。
    four_byte: usize,
    /// 前几个出问题的字符，供界面显示
    samples: Vec<String>,
}

/// 编码成字节。
///
/// UTF-8 / UTF-16 能装下任何字符；GB18030 装不下极少数码位、且会把 GBK 之外的字
/// 写成四字节。这两种情况都要如实报告 —— 见 `Encoded` 的字段说明。
fn encode_text(text: &str, enc: Encoding) -> Encoded {
    match enc {
        Encoding::Utf8 | Encoding::Unknown => Encoded {
            bytes: text.as_bytes().to_vec(),
            unmappable: 0,
            four_byte: 0,
            samples: Vec::new(),
        },
        Encoding::Utf8Bom => {
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(text.as_bytes());
            Encoded { bytes, unmappable: 0, four_byte: 0, samples: Vec::new() }
        }
        Encoding::Utf16Le => Encoded {
            bytes: encode_utf16(text, true),
            unmappable: 0,
            four_byte: 0,
            samples: Vec::new(),
        },
        Encoding::Utf16Be => Encoded {
            bytes: encode_utf16(text, false),
            unmappable: 0,
            four_byte: 0,
            samples: Vec::new(),
        },
        Encoding::Gb18030 => encode_gb18030(text),
    }
}

fn encode_utf16(text: &str, little: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() * 2 + 2);
    // BOM 不能省：Excel 的「Unicode 文本」和记事本都靠它认字节序，
    // 没有 BOM 的 UTF-16 文件在 Windows 上会被当成乱码。
    v.extend_from_slice(if little { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
    let mut buf = [0u16; 2];
    for c in text.chars() {
        for u in c.encode_utf16(&mut buf).iter() {
            v.extend_from_slice(&if little { u.to_le_bytes() } else { u.to_be_bytes() });
        }
    }
    v
}

/// GB18030 编码 + 精确统计"变了样的字符"。
///
/// 用 `encode_from_utf8_without_replacement` 自己走循环（而不是一次
/// `GB18030.encode()`），是为了能**精确数出**装不下的字符并留下样例 ——
/// 带 replacement 的那个 API 只给一个 bool，报不出数量就写不进 `Loss`。
/// 自己写 NCR（`&#数字;`）是为了与 encoding_rs 的 replacement 行为**逐字节一致**，
/// 这一点由测试 `gb18030_四字节字符与装不下的字符都被如实报告` 做对照来钉死。
fn encode_gb18030(text: &str) -> Encoded {
    let mut encoder = GB18030.new_encoder();
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len() + 16);
    let mut buf = [0u8; 8192];
    let mut src = text;
    let mut unmappable = 0usize;
    let mut samples: Vec<String> = Vec::new();
    // 防死循环：每一轮都必须消耗输入或写出输出，否则说明我们对 API 的理解错了。
    // 那时候宁可停下（返回已处理的部分），也不能让界面卡死。
    let guard_max = text.len() + 64;
    let mut guard = 0usize;
    loop {
        guard += 1;
        if guard > guard_max {
            break;
        }
        let (res, read, written) = encoder.encode_from_utf8_without_replacement(src, &mut buf, true);
        bytes.extend_from_slice(&buf[..written]);
        let mut consumed = read;
        if let encoding_rs::EncoderResult::Unmappable(c) = res {
            unmappable += 1;
            if samples.len() < MAX_SAMPLES {
                samples.push(format!("「{c}」"));
            }
            // encoding_rs 不消费这个字符（它不知道该怎么写），所以要自己跳过它，
            // 并写一个与它 replacement 行为一致的数字字符引用。
            if consumed == 0 {
                if let Some(ch) = src.chars().next() {
                    consumed = ch.len_utf8();
                }
            }
            bytes.extend_from_slice(format!("&#{};", c as u32).as_bytes());
        }
        src = &src[consumed..];
        if res == encoding_rs::EncoderResult::InputEmpty {
            break;
        }
    }
    let (four_byte, more_samples) = count_gb18030_four_byte(&bytes);
    for s in more_samples {
        if samples.len() >= MAX_SAMPLES {
            break;
        }
        samples.push(s);
    }
    Encoded { bytes, unmappable, four_byte, samples }
}

/// 数出 GB18030 里的**四字节序列**，并顺便把原文解回来当样例。
///
/// 四字节形式的结构是 `81–FE 30–39 81–FE 30–39`，而两字节形式的后一个字节**绝不会**
/// 落在 0x30–0x39（那是留给四字节的），所以这个判据不会误判。
fn count_gb18030_four_byte(bytes: &[u8]) -> (usize, Vec<String>) {
    let mut n = 0usize;
    let mut samples: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if (0x81..=0xFE).contains(&bytes[i]) && (0x30..=0x39).contains(&bytes[i + 1]) {
            n += 1;
            if samples.len() < MAX_SAMPLES {
                // 用解码器把原文取回来，样例里就能显示"是哪个字"，
                // 而不是给用户一串 94 39 DA 33 这样的字节。
                let (s, _, _) = GB18030.decode(&bytes[i..i + 4]);
                samples.push(format!("「{s}」"));
            }
            i += 4;
            continue;
        }
        i += 1;
    }
    (n, samples)
}

// ============================================================
// 图片
// ============================================================

struct Probe {
    w: u32,
    h: u32,
    color: ColorType,
    exif: Orientation,
    exif_extra: bool,
    deep_color: bool,
    has_alpha: bool,
}

/// 只读文件头：尺寸、颜色类型、EXIF 方向。**不解码像素。**
fn probe_image(path: &Path) -> Result<Probe> {
    let file = std::fs::File::open(path).map_err(|e| format!("打不开「{}」：{e}", file_label(path)))?;
    let mut reader = ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| format!("读不出「{}」的文件头：{e}", file_label(path)))?;
    reader.limits(decode_limits());
    let mut dec = reader
        .into_decoder()
        .map_err(|e| format!("「{}」不是能识别的图片，或者已经损坏：{e}", file_label(path)))?;
    let (w, h) = dec.dimensions();
    let color = dec.color_type();
    let exif = decoder_orientation(&mut dec);
    let exif_extra = dec.exif_metadata().map(|o| o.is_some()).unwrap_or(false);
    check_pixels(w, h)?;
    Ok(Probe {
        w,
        h,
        color,
        exif,
        exif_extra,
        deep_color: matches!(
            color,
            ColorType::Rgb16 | ColorType::Rgba16 | ColorType::L16 | ColorType::La16
        ),
        has_alpha: matches!(
            color,
            ColorType::Rgba8 | ColorType::Rgba16 | ColorType::La8 | ColorType::La16
        ),
    })
}

/// 取 EXIF 方向。
///
/// 先问解码器自己（JPEG / WebP / TIFF 都实现得对），拿不到再自己解析 EXIF 块 ——
/// PNG 的 `eXIf` 块里放的就是同一个 TIFF 结构，而 PNG 解码器没有实现
/// `orientation()`。两条路都失败就当"没有方向"，**绝不猜**。
fn decoder_orientation(dec: &mut impl ImageDecoder) -> Orientation {
    if let Ok(o) = dec.orientation() {
        if o != Orientation::NoTransforms {
            return o;
        }
    }
    dec.exif_metadata()
        .ok()
        .flatten()
        .and_then(|c| orientation_from_exif_chunk(&c))
        .unwrap_or(Orientation::NoTransforms)
}

/// 解析 EXIF 块里的 Orientation。
///
/// 有的解码器给的块**带** `Exif\0\0` 前缀（JPEG 的 APP1 原始内容），有的不带
/// （PNG 的 eXIf 块）。两种都认，免得因为一个 6 字节前缀就静默地不转正 ——
/// 那正是"手机拍的图转完躺倒"的原因。
fn orientation_from_exif_chunk(chunk: &[u8]) -> Option<Orientation> {
    if let Some(rest) = chunk.strip_prefix(b"Exif\0\0") {
        return Orientation::from_exif_chunk(rest);
    }
    Orientation::from_exif_chunk(chunk)
}

fn decode_limits() -> Limits {
    // `Limits` 是 #[non_exhaustive] 的，不能直接写字面量 —— 先拿默认值再改。
    let mut l = Limits::default();
    l.max_image_width = Some(MAX_EDGE);
    l.max_image_height = Some(MAX_EDGE);
    // 与 MAX_PIXELS 对应：5000 万像素 × 4 字节 = 200 MB，留一倍余量
    l.max_alloc = Some(400 * 1024 * 1024);
    l
}

fn check_pixels(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 {
        return Err("这张图的尺寸是 0，文件已经损坏。".into());
    }
    let px = u64::from(w) * u64::from(h);
    if px > MAX_PIXELS {
        return Err(format!(
            "这张图是 {w}×{h}（约 {:.0} 万像素），超过单次转换上限 {} 万像素（约需 {:.0} MB 内存）。\
             请先用别的工具缩小，或者裁到需要的部分。",
            px as f64 / 10000.0,
            MAX_PIXELS / 10000,
            px as f64 * 4.0 / 1048576.0
        ));
    }
    Ok(())
}

/// 完整解码。
fn read_image(path: &Path) -> Result<(DynamicImage, Probe)> {
    let probe = probe_image(path)?;
    let file = std::fs::File::open(path).map_err(|e| format!("打不开「{}」：{e}", file_label(path)))?;
    let mut reader = ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| format!("读不出「{}」的文件头：{e}", file_label(path)))?;
    reader.limits(decode_limits());
    let img = reader
        .decode()
        .map_err(|e| format!("解码「{}」失败：{e}（文件可能不完整或被改坏过）", file_label(path)))?;
    check_pixels(img.width(), img.height())?;
    Ok((img, probe))
}

/// EXIF 方向会把图转 90°，于是宽高互换。计划里报的尺寸必须是**转正之后**的，
/// 否则用户会发现"计划说 1920×1080，转出来是 1080×1920"。
fn oriented_dims(w: u32, h: u32, o: Orientation) -> (u32, u32) {
    match o {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (h, w),
        _ => (w, h),
    }
}

/// 缩放 / 旋转 / 翻转。
///
/// **不做水印**：中文水印需要字体光栅化（`ab_glyph` 之类 + 读系统字体）或者内嵌
/// 一套点阵字库。前者让"我们依赖了什么"变得不透明、还挑系统字体；后者为了几个字
/// 要带一份字库。而只支持 ASCII 的水印对中文用户基本没用 —— 三条路都不划算，
/// 真有需要就单独排期，不在这里凑合。
fn apply_edits(mut img: DynamicImage, p: &EditParams) -> DynamicImage {
    // 缩放：只缩不放。放大会让"我只想让它小一点"变成一张更糊的图，
    // 而且内存按面积增长。
    let (w, h) = (img.width(), img.height());
    if let Some(edge) = p.max_edge {
        let longest = w.max(h);
        if longest > edge {
            let ratio = f64::from(edge) / f64::from(longest);
            let nw = ((f64::from(w) * ratio).round() as u32).max(1);
            let nh = ((f64::from(h) * ratio).round() as u32).max(1);
            // Lanczos3：长图缩小时最怕摩尔纹，双线性会让小字糊成一片。
            // 代价是慢一些，但这是用户主动发起的操作 —— 慢比糊好。
            img = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
        }
    }
    for _ in 0..(p.rotate90 % 4) {
        img = img.rotate90();
    }
    if p.flip_h {
        img = img.fliph();
    }
    if p.flip_v {
        img = img.flipv();
    }
    img
}

/// 把图像整成目标编码器吃得下的样子，并数出"被压到白底的透明像素"。
fn prepare_for_encoder(img: DynamicImage, fmt: Fmt) -> (DynamicImage, usize) {
    match fmt {
        // JPEG 只存 8 位、没有透明通道 → 合成到白底，顺便数出透明像素个数。
        Fmt::Jpg => flatten_to_white(&img),
        // BMP / WebP 这两路都只写 8 位（16 位 BMP 连 Windows 画图都不一定认，
        // 而无损 WebP 的编码器只吃 8 位），所以深色深要在这里降下来。
        // 透明通道两边都保留 → 不动 alpha。
        Fmt::Bmp | Fmt::Webp => {
            if matches!(
                img.color(),
                ColorType::Rgb16 | ColorType::Rgba16 | ColorType::L16 | ColorType::La16
            ) {
                if matches!(img.color(), ColorType::Rgba16 | ColorType::La16) {
                    (DynamicImage::ImageRgba8(img.to_rgba8()), 0)
                } else {
                    (DynamicImage::ImageRgb8(img.to_rgb8()), 0)
                }
            } else {
                (img, 0)
            }
        }
        // PNG 什么色深都存得下，原样交给它
        _ => (img, 0),
    }
}

/// 把带透明通道的图合成到白底，并**精确数出**透明像素个数。
///
/// 为什么是白底：JPEG 存不下透明，直接丢 alpha 会让透明区变成黑色。浏览器和
/// Photoshop 的默认行为都是合成到白底，用户对白色有预期。这个数量必须报出来 ——
/// "有 12000 个透明像素变成了白色"比"可能丢失透明信息"有用得多。
fn flatten_to_white(img: &DynamicImage) -> (DynamicImage, usize) {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = RgbImage::new(w, h);
    let mut transparent = 0usize;
    for (dst, src) in out.pixels_mut().zip(rgba.pixels()) {
        let a = u32::from(src[3]);
        if a < 255 {
            transparent += 1;
        }
        let blend = |c: u8| (((u32::from(c) * a) + (255 * (255 - a)) + 127) / 255) as u8;
        *dst = Rgb([blend(src[0]), blend(src[1]), blend(src[2])]);
    }
    (DynamicImage::ImageRgb8(out), transparent)
}

/// 写图片。返回写出的字节数。
fn write_image(out: &Path, img: DynamicImage, fmt: Fmt, quality: u8) -> Result<u64> {
    let size = write_atomically(out, |tmp| {
        let mut f = std::fs::File::create(tmp).map_err(|e| format!("建不了文件：{e}"))?;
        match fmt {
            Fmt::Png => img
                .write_with_encoder(PngEncoder::new(&mut f))
                .map_err(|e| format!("写 PNG 失败：{e}"))?,
            Fmt::Jpg => img
                .write_with_encoder(JpegEncoder::new_with_quality(&mut f, quality))
                .map_err(|e| format!("写 JPEG 失败：{e}"))?,
            Fmt::Bmp => img
                .write_with_encoder(BmpEncoder::new(&mut f))
                .map_err(|e| format!("写 BMP 失败：{e}"))?,
            // WebP 只有无损编码（见 plan_images 的说明），所以这里没有质量参数。
            Fmt::Webp => img
                .write_with_encoder(WebPEncoder::new_lossless(&mut f))
                .map_err(|e| format!("写 WebP 失败：{e}"))?,
            _ => return Err("内部错误：目标不是图片格式".into()),
        }
        f.flush().map_err(|e| format!("落盘失败：{e}"))?;
        std::fs::metadata(tmp).map(|m| m.len()).map_err(|e| e.to_string())
    })?;
    Ok(size)
}

// ============================================================
// 通用：原子写、路径、小工具
// ============================================================

/// 先写 `<名字>.part`，成功后才改名成目标。
///
/// 为什么要多这一步：写 200 MB 的 xlsx 到一半失败（磁盘满、被拔 U 盘），
/// 直接写目标名就会留下一个**看起来存在、其实是坏的文件** —— 用户会以为转换
/// 成功了，拿它去交差。`.part` 这个名字也顺便告诉人"这不是最终文件"。
fn write_atomically<T>(dst: &Path, write: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    if let Some(dir) = dst.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建不了目录 {}：{e}", dir.display()))?;
        }
    }
    let tmp = temp_path(dst);
    // 上一次崩溃留下的 .part 会被覆盖：它不是用户的文件（用户不会自己造这种名字），
    // 而且我们只在写成功之后才改名。
    match write(&tmp) {
        Ok(v) => match std::fs::rename(&tmp, dst) {
            Ok(()) => Ok(v),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("改名失败（目标可能被别的程序占用）：{e}"))
            }
        },
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn temp_path(dst: &Path) -> PathBuf {
    let name = dst
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".into());
    dst.with_file_name(format!("{name}.part"))
}

/// 写文本文件。返回 `(字节数, 编码结果)` —— 编码结果里有"多少个字符变了样"，
/// 调用方要把它报出去（`Loss` 或 `notes`）。
fn write_text_atomic(out: &Path, text: &str, enc: Encoding) -> Result<(u64, Encoded)> {
    let e = encode_text(text, enc);
    let size = e.bytes.len() as u64;
    write_atomically(out, |tmp| {
        let mut f = std::fs::File::create(tmp).map_err(|e| format!("建不了文件：{e}"))?;
        f.write_all(&e.bytes).map_err(|e| format!("写文件失败：{e}"))?;
        f.flush().map_err(|e| format!("落盘失败：{e}"))?;
        Ok(())
    })?;
    Ok((size, e))
}

/// 推出一个输出路径。
///
/// 规则（也是给用户看的规则）：
///   · 只有 1 个输出 → 就用目标名本身（用户写了什么就是什么）；
///   · 一个工作簿拆成多张表 → `明细.csv` 派生出 `明细-客户.csv` 这样的同目录文件名。
fn out_path_for(dst: &Path, sheet: Option<&str>, single: bool) -> PathBuf {
    if single {
        return dst.to_path_buf();
    }
    let stem = dst
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".into());
    let ext = dst.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let suffix = sheet.map(sanitize_file_name).unwrap_or_else(|| "1".into());
    if ext.is_empty() {
        dst.with_file_name(format!("{stem}-{suffix}"))
    } else {
        dst.with_file_name(format!("{stem}-{suffix}.{ext}"))
    }
}

/// 工作表名 → 能当文件名的东西。**只换文件名里非法的字符**；
/// 工作表名本身（xlsx 里那个）保持原样，否则用户在 Excel 里找不到自己那张表。
fn sanitize_file_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) || (c as u32) < 0x20 { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').to_string();
    if trimmed.is_empty() {
        "Sheet".into()
    } else {
        trimmed
    }
}

/// 文件名（不含目录）→ 工作表名：清洗 + 去重 + 截到 31 字符。
/// Excel 要求工作表名唯一且 ≤31 字符，重名会让整个工作簿存不下来。
fn sheet_names_for(srcs: &[PathBuf]) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for s in srcs {
        let base = s.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let cleaned: String =
            base.chars().map(|c| if ":\\/?*[]".contains(c) { '_' } else { c }).collect();
        let trimmed: String = cleaned.chars().take(31).collect();
        let base = if trimmed.trim().is_empty() { "Sheet".to_string() } else { trimmed };
        let mut name = base.clone();
        let mut n = 2;
        while used.iter().any(|u| u.eq_ignore_ascii_case(&name)) {
            let suffix = format!("_{n}");
            let keep = 31usize.saturating_sub(suffix.len());
            name = format!("{}{}", base.chars().take(keep).collect::<String>(), suffix);
            n += 1;
        }
        used.push(name);
    }
    used
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        // 目标还不存在时 canonicalize 会失败，那就把父目录规范化后比文件名
        (Ok(x), Err(_)) => match (b.parent(), b.file_name()) {
            (Some(p), Some(n)) => {
                let p = if p.as_os_str().is_empty() { Path::new(".") } else { p };
                p.canonicalize().map(|cp| cp.join(n) == x).unwrap_or(false)
            }
            _ => false,
        },
        _ => false,
    }
}

fn too_big(path: &Path, bytes: u64, limit: u64, kind: &str) -> String {
    format!(
        "「{}」{:.1} MB，超过{kind}单次转换上限 {} MB。建议拆成几个文件再转。",
        file_label(path),
        bytes as f64 / 1048576.0,
        limit / 1048576
    )
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn sample_list(items: &[String]) -> String {
    if items.len() <= MAX_SAMPLES {
        items.join("、")
    } else {
        format!("{} 等", items[..MAX_SAMPLES].join("、"))
    }
}

fn delim_label(c: char) -> String {
    match c {
        ',' => "逗号 (,)".into(),
        '\t' => "制表符 (Tab)".into(),
        ';' => "分号 (;)".into(),
        '|' => "竖线 (|)".into(),
        other => format!("{other:?}"),
    }
}

fn color_label(c: ColorType) -> String {
    match c {
        ColorType::L8 => "8 位灰度".into(),
        ColorType::La8 => "8 位灰度 + 透明".into(),
        ColorType::Rgb8 => "24 位彩色".into(),
        ColorType::Rgba8 => "24 位彩色 + 透明".into(),
        ColorType::L16 => "16 位灰度".into(),
        ColorType::La16 => "16 位灰度 + 透明".into(),
        ColorType::Rgb16 => "48 位彩色".into(),
        ColorType::Rgba16 => "48 位彩色 + 透明".into(),
        other => format!("{other:?}"),
    }
}

fn orientation_label(o: Orientation) -> String {
    match o {
        Orientation::NoTransforms => "无（本来就是正的）".into(),
        Orientation::Rotate90 => "第 6 号：顺时针 90°".into(),
        Orientation::Rotate180 => "第 3 号：180°".into(),
        Orientation::Rotate270 => "第 8 号：逆时针 90°".into(),
        Orientation::FlipHorizontal => "第 2 号：水平镜像".into(),
        Orientation::FlipVertical => "第 4 号：垂直镜像".into(),
        Orientation::Rotate90FlipH => "第 5 号：90° + 水平镜像".into(),
        Orientation::Rotate270FlipH => "第 7 号：270° + 水平镜像".into(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// 等价的 `^\d{15,}$`（本项目不引入 regex，这点判断手写就够）
fn is_long_number(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 15 && t.bytes().all(|b| b.is_ascii_digit())
}

/// "变了多少"的人话（给报告用）
fn pct_change(before: u64, after: u64) -> String {
    if before == 0 {
        return "—".into();
    }
    let d = after as f64 / before as f64;
    if d >= 1.0 {
        format!("+{:.0}%", (d - 1.0) * 100.0)
    } else {
        format!("-{:.0}%", (1.0 - d) * 100.0)
    }
}

/// 头部的行数与最宽列数。只用于计划里的估算与给人看的预估。
fn count_rows(text: &str, delim: char) -> (usize, usize) {
    let mut rows = 0usize;
    let mut cols = 0usize;
    for line in text.lines().take(10_000) {
        rows += 1;
        cols = cols.max(count_fields(line, delim));
    }
    (rows, cols)
}

/// 数一行里有几个字段（引号外的分隔符数 + 1），与解析器同一套引号规则。
fn count_fields(line: &str, delim: char) -> usize {
    let mut n = 1usize;
    let mut in_quotes = false;
    let mut only_ws = true;
    let mut it = line.chars();
    while let Some(c) = it.next() {
        if in_quotes {
            if c == '"' {
                if it.clone().next() == Some('"') {
                    it.next();
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        if c == delim {
            n += 1;
            only_ws = true;
            continue;
        }
        if c == '"' && only_ws {
            in_quotes = true;
            only_ws = false;
            continue;
        }
        if !c.is_whitespace() {
            only_ws = false;
        }
    }
    n
}

/// 探测分隔符：`,` `\t` `;` `|` 里选一个。
///
/// 判据是「**哪种分隔符让每行的字段数最一致**」，不是"谁出现次数多" ——
/// 中文地址里逗号极其常见，「北京市朝阳区,安贞路1号」会让逗号在计数法里直接胜出，
/// 而真正的分隔符（制表符）一次都不出现。
///
/// 与 `csv_import.rs` 的版本比，这里简化了一处：只看**物理行**（不做引号感知）。
/// 因为探测只看前 20 行、判据是"众数"，少数被引号里的换行劈开的行不影响众数。
fn sniff_delimiter(text: &str) -> (char, bool) {
    const DELIMS: [char; 4] = [',', '\t', ';', '|'];
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(20).collect();
    if lines.is_empty() {
        return (',', false);
    }
    let mut best: Option<(char, usize, usize)> = None; // (分隔符, 一致度, 首行是否等于众数)
    for d in DELIMS {
        let counts: Vec<usize> = lines.iter().map(|l| count_fields(l, d)).collect();
        let mut mode = 0usize;
        let mut freq = 0usize;
        for &c in &counts {
            let f = counts.iter().filter(|&&x| x == c).count();
            if f > freq {
                freq = f;
                mode = c;
            }
        }
        // 字段数必须 > 1：`|` 在一个没有分隔符的文件里也能让"每行都是 1 列"100% 一致
        if mode <= 1 {
            continue;
        }
        let first_ok = usize::from(counts[0] == mode);
        let better = match best {
            None => true,
            // 严格大于 ⇒ 打平时保留 DELIMS 里靠前的（逗号优先）
            Some((_, bf, bfo)) => (freq, first_ok) > (bf, bfo),
        };
        if better {
            best = Some((d, freq, first_ok));
        }
    }
    match best {
        Some((d, _, _)) => (d, true),
        None => (',', false),
    }
}

/// 引号感知的 CSV/TSV 解析（RFC 4180，含 `csv_import.rs` 文档里那条"有意偏离"：
/// 引号前只有空白也算字段开始 —— 真实导出里「逗号 + 空格 + 引号」太常见，
/// 严格照 RFC 会让那个逗号被当成分隔符，从此整表错位）。
///
/// 空行会被丢掉（与 `csv_import.rs` 一致，那边解释了为什么）。
fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut started = false;
    let mut field_has_quote = false;
    let mut row_has_quote = false;
    let mut only_ws = true;
    let mut it = text.chars().peekable();

    while let Some(c) = it.next() {
        if in_quotes {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    it.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == delim {
            row.push(std::mem::take(&mut field));
            row_has_quote |= field_has_quote;
            field_has_quote = false;
            only_ws = true;
            started = true;
            continue;
        }
        if c == '\n' || c == '\r' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            row.push(std::mem::take(&mut field));
            row_has_quote |= field_has_quote;
            field_has_quote = false;
            // 真空行（一格、空白、没出现过引号）不算一条记录 —— 与 csv_import 一致
            let blank = row.len() == 1 && !row_has_quote && row[0].trim().is_empty();
            if !blank {
                rows.push(std::mem::take(&mut row));
            }
            row.clear();
            row_has_quote = false;
            only_ws = true;
            started = false;
            continue;
        }
        if c == '"' && only_ws {
            field.clear();
            in_quotes = true;
            field_has_quote = true;
            row_has_quote = true;
            only_ws = false;
            started = true;
            continue;
        }
        field.push(c);
        if !c.is_whitespace() {
            only_ws = false;
        }
        started = true;
    }
    if started {
        row.push(std::mem::take(&mut field));
        row_has_quote |= field_has_quote;
        let blank = row.len() == 1 && !row_has_quote && row[0].trim().is_empty();
        if !blank {
            rows.push(row);
        }
    }
    rows
}

/// 头部扫描：数"看起来是数字的格子""带前导零的编号""15 位以上的长编号"。
/// 只用于计划里说清"会发生什么"，不参与真正的写入。
fn scan_cells(text: &str, delim: char, _head_rows: usize) -> (usize, usize, usize) {
    let mut numeric = 0usize;
    let mut leading_zero = 0usize;
    let mut long_ids = 0usize;
    for line in text.lines().take(2_000) {
        for cell in line.split(delim) {
            let t = cell.trim().trim_matches('"').trim();
            if t.is_empty() {
                continue;
            }
            if is_long_number(t) {
                long_ids += 1;
            } else if is_plain_number(t) {
                numeric += 1;
            } else if t.len() > 1 && t.chars().all(|c| c.is_ascii_digit()) && t.starts_with('0') {
                leading_zero += 1;
            }
        }
    }
    (numeric, leading_zero, long_ids)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用自己的目录：cargo 并行跑测试，共用目录会互相踩
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskbase-convert-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个测试用 xlsx。`sheets` = (工作表名, 行×列的字符串)
    fn make_xlsx(path: &Path, sheets: &[(&str, Vec<Vec<&str>>)]) {
        let mut wb = Workbook::new();
        for (name, rows) in sheets {
            let ws = wb.add_worksheet();
            ws.set_name(*name).unwrap();
            for (r, row) in rows.iter().enumerate() {
                for (c, v) in row.iter().enumerate() {
                    if !v.is_empty() {
                        ws.write_string(r as u32, c as u16, *v).unwrap();
                    }
                }
            }
        }
        wb.save(path).unwrap();
    }

    fn read_to_string(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let enc = crate::csv_import::detect_encoding(&bytes);
        decode_text(&bytes, enc).0
    }

    // ---------------- 表格 ----------------

    #[test]
    fn xlsx_split_to_csv_one_file_per_sheet() {
        let d = tmp("split");
        let xlsx = d.join("台账.xlsx");
        make_xlsx(
            &xlsx,
            &[
                ("明细", vec![vec!["客户", "金额"], vec!["张三", "100"], vec!["李四", "200"]]),
                ("备注", vec![vec!["说明"], vec!["仅供内部核对"]]),
            ],
        );

        let out = d.join("导出.csv");
        let p = plan(&xlsx, &out, &Options::default()).unwrap();
        assert_eq!(p.outputs.len(), 2, "两张工作表 → 两个文件");
        assert!(p.steps.iter().any(|s| s.contains("全部导出")), "步骤里要说清导了几张表");
        assert_eq!(p.loss_of("sheets"), 0, "全部导出时不该报丢工作表");

        let rep = run(&p).unwrap();
        assert_eq!(rep.outputs.len(), 2);
        assert_eq!(rep.rows, 3, "1+2 两行数据（不含表头）");

        // 文件名由目标名派生：导出-明细.csv / 导出-备注.csv
        let a = d.join("导出-明细.csv");
        let b = d.join("导出-备注.csv");
        assert!(a.exists() && b.exists(), "实际写出：{:?}", rep.outputs);
        // BOM 要直接看字节：read_to_string 会把 BOM 切掉，从字符串里看不出来
        let raw = std::fs::read(&a).unwrap();
        assert!(
            raw.starts_with(&[0xEF, 0xBB, 0xBF]),
            "默认 UTF-8 带 BOM，Excel 双击打开中文才不乱码"
        );
        let text = read_to_string(&a);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "客户,金额");
        assert_eq!(lines[1], "张三,100");
        assert_eq!(lines[2], "李四,200");
        assert!(read_to_string(&b).contains("仅供内部核对"));
    }

    #[test]
    fn exporting_one_sheet_plan_names_other_two() {
        let d = tmp("drop-sheets");
        let xlsx = d.join("多表.xlsx");
        make_xlsx(
            &xlsx,
            &[
                ("明细", vec![vec!["客户"], vec!["张三"]]),
                ("Sheet2", vec![vec!["a"], vec!["1"]]),
                ("Sheet3", vec![vec!["b"], vec!["2"]]),
            ],
        );

        let out = d.join("只要明细.csv");
        let p = plan(&xlsx, &out, &Options::default().with_sheet("明细")).unwrap();

        assert_eq!(p.outputs, vec![out.clone()], "只导一张表 → 就用目标名本身");
        assert_eq!(p.loss_of("sheets"), 2, "要报出丢了 2 张工作表");
        let loss = p.losses.iter().find(|l| l.kind == "sheets").unwrap();
        assert!(loss.detail.contains("Sheet2"), "要点名：{}", loss.detail);
        assert!(loss.detail.contains("Sheet3"), "要点名：{}", loss.detail);
        assert!(p.is_lossy());

        run(&p).unwrap();
        assert!(out.exists());
    }

    #[test]
    fn xlsx_formulas_merges_and_images_counted() {
        let d = tmp("count-losses");
        // 先造一张真 PNG 用来嵌进 xlsx（rust_xlsxwriter 会读它的文件头拿尺寸）
        let png = d.join("图.png");
        RgbImage::from_pixel(3, 2, Rgb([200, 30, 30])).save(&png).unwrap();

        let xlsx = d.join("带东西.xlsx");
        let mut wb = Workbook::new();
        {
            let ws = wb.add_worksheet();
            ws.set_name("明细").unwrap();
            ws.write_string(0, 0, "项目").unwrap();
            ws.write_string(1, 0, "收入").unwrap();
            ws.write_number(1, 1, 10.0).unwrap();
            ws.write_string(2, 0, "支出").unwrap();
            ws.write_number(2, 1, 20.0).unwrap();
            ws.write_string(3, 0, "合计").unwrap();
            ws.write_formula(3, 1, rust_xlsxwriter::Formula::new("=SUM(B2:B3)")).unwrap();
            ws.merge_range(0, 2, 0, 3, "客户往来", &Format::new().set_bold()).unwrap();
            ws.insert_image(5, 0, &rust_xlsxwriter::Image::new(&png).unwrap()).unwrap();
        }
        wb.add_worksheet().set_name("第二张").unwrap();
        wb.save(&xlsx).unwrap();

        let out = d.join("明细.csv");
        let p = plan(&xlsx, &out, &Options::default().with_sheet("明细")).unwrap();

        assert_eq!(p.loss_of("formulas"), 1, "1 个公式：{:?}", p.losses);
        let f = p.losses.iter().find(|l| l.kind == "formulas").unwrap();
        assert!(f.detail.contains("1 个公式"), "{}", f.detail);
        assert!(f.detail.contains("B4"), "公式要点出单元格：{}", f.detail);
        assert!(f.detail.contains("SUM"), "公式要点出原文：{}", f.detail);
        assert_eq!(p.loss_of("merged_cells"), 1, "1 处合并单元格");
        assert_eq!(p.loss_of("images"), 1, "1 张嵌入图片：{:?}", p.losses);
        assert_eq!(p.loss_of("sheets"), 1, "另一张工作表不导出");

        let rep = run(&p).unwrap();
        let text = read_to_string(&out);
        assert!(text.contains("合计"), "{text}");
        // 公式那一格拿到的是文件里的**缓存值**：这份测试文件是库生成的、没有缓存，
        // 所以这里是 0 —— 现实里也要把这件事告诉用户（见 plan 的 warning）。
        assert!(
            p.warnings.iter().any(|w| w.contains("缓存")),
            "要提醒「公式只有缓存值、可能不准」：{:?}",
            p.warnings
        );
        // Report 必须自包含：只拿到 Report 的界面也要能看到"丢了什么"
        assert!(rep.loss_of("formulas") >= 1, "报告里也要有公式丢失：{:?}", rep.losses);
        assert!(rep.loss_of("images") >= 1, "报告里也要有图片丢失：{:?}", rep.losses);
    }

    #[test]
    fn csv_to_xlsx_readable_by_calamine() {
        let d = tmp("csv2xlsx");
        let csv = d.join("客户.csv");
        // 中文 + 金额 + 18 位订单号（长编号必须当文本）
        std::fs::write(
            &csv,
            "\u{FEFF}客户,金额,订单编号\n张三,1234.50,110101199003072587\n李四,88,\n".as_bytes(),
        )
        .unwrap();

        let out = d.join("客户.xlsx");
        let p = plan(&csv, &out, &Options::default()).unwrap();
        assert!(p.losses.is_empty(), "csv → xlsx 不该报丢失：{:?}", p.losses);
        run(&p).unwrap();
        assert!(out.exists(), "目标必须真的产出");

        // 用 calamine 读回来：真往返
        let mut wb = open_workbook_auto(&out).unwrap();
        assert_eq!(wb.sheet_names(), &["客户".to_string()]);
        let r = wb.worksheet_range("客户").unwrap();
        assert_eq!(r.height(), 3);
        assert_eq!(r.width(), 3);
        assert_eq!(r.get((0, 0)), Some(&Data::String("客户".into())));
        assert_eq!(r.get((1, 0)), Some(&Data::String("张三".into())));
        // 金额应当是**数值**（Excel 里能直接求和）
        match r.get((1, 1)) {
            Some(Data::Float(f)) => assert!((f - 1234.50).abs() < 1e-9, "实际 {f}"),
            other => panic!("金额应当是数值，实际 {other:?}"),
        }
        // 18 位订单号必须是**文本**，不能在 Excel 里变成科学计数法
        match r.get((1, 2)) {
            Some(Data::String(s)) => assert_eq!(s, "110101199003072587"),
            other => panic!("长编号必须是文本，实际 {other:?}"),
        }
    }

    #[test]
    fn leading_zero_id_stays_text_in_xlsx() {
        let d = tmp("leading-zero");
        let csv = d.join("编号.csv");
        std::fs::write(&csv, "编号,数量\n007,3\n0012,4\n".as_bytes()).unwrap();
        let out = d.join("编号.xlsx");
        let p = plan(&csv, &out, &Options::default()).unwrap();
        // 计划里就要讲清"会保护前导零"
        assert!(
            p.steps.iter().any(|s| s.contains("前导零")),
            "步骤里要提到前导零保护：{:?}",
            p.steps
        );
        run(&p).unwrap();

        let mut wb = open_workbook_auto(&out).unwrap();
        let r = wb.worksheet_range("编号").unwrap();
        assert_eq!(r.get((1, 0)), Some(&Data::String("007".into())), "007 不能变成 7");
        assert_eq!(r.get((2, 0)), Some(&Data::String("0012".into())), "0012 不能变成 12");
        match r.get((1, 1)) {
            Some(Data::Float(f)) => assert!((f - 3.0).abs() < 1e-9),
            other => panic!("数量没前导零，应当写成数值，实际 {other:?}"),
        }
    }

    #[test]
    fn many_csv_merged_into_one_xlsx_one_sheet_each() {
        let d = tmp("merge");
        let a = d.join("一月.csv");
        let b = d.join("二月.csv");
        std::fs::write(&a, "客户,金额\n张三,100\n".as_bytes()).unwrap();
        std::fs::write(&b, "客户,金额\n李四,200\n".as_bytes()).unwrap();

        let out = d.join("季度.xlsx");
        let p = plan_many(&[a.clone(), b.clone()], &out, &Options::default()).unwrap();
        assert_eq!(p.outputs.len(), 1, "两个源合成一个工作簿");
        assert!(p.steps.iter().any(|s| s.contains("一月") && s.contains("二月")));
        let rep = run(&p).unwrap();
        assert_eq!(rep.rows, 2);

        let mut wb = open_workbook_auto(&out).unwrap();
        assert_eq!(wb.sheet_names(), &["一月".to_string(), "二月".to_string()]);
        let r = wb.worksheet_range("二月").unwrap();
        assert_eq!(r.get((1, 0)), Some(&Data::String("李四".into())));
    }

    #[test]
    fn csv_to_tsv_keeps_commas_in_fields() {
        let d = tmp("reshape");
        let csv = d.join("地址.csv");
        std::fs::write(&csv, "客户,地址\n张三,\"北京市朝阳区,安贞路1号\"\n".as_bytes()).unwrap();
        let out = d.join("地址.tsv");
        let p = plan(&csv, &out, &Options::default()).unwrap();
        assert_eq!(p.loss_of("requote"), 1, "换分隔符要报「引号会重新排版」");
        run(&p).unwrap();

        let text = read_to_string(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "客户\t地址");
        assert_eq!(lines[1], "张三\t北京市朝阳区,安贞路1号", "字段里的逗号不是分隔符");
    }

    // ---------------- 覆盖保护 ----------------

    #[test]
    fn existing_target_plan_errors_not_overwrite() {
        let d = tmp("no-overwrite");
        let xlsx = d.join("源.xlsx");
        make_xlsx(&xlsx, &[("表", vec![vec!["a"], vec!["1"]])]);
        let out = d.join("已存在.csv");
        std::fs::write(&out, "这是用户自己的文件").unwrap();

        let err = plan(&xlsx, &out, &Options::default()).unwrap_err();
        assert!(err.contains("已存在"), "要说明原因：{err}");
        assert!(err.contains("不覆盖"), "要讲清策略：{err}");
        // 用户的文件必须原样还在
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "这是用户自己的文件");
    }

    #[test]
    fn target_appearing_between_plan_and_run_errors() {
        let d = tmp("race");
        let csv = d.join("源.csv");
        std::fs::write(&csv, "a,b\n1,2\n".as_bytes()).unwrap();
        let out = d.join("新.csv");

        let p = plan(&csv, &out, &Options::default().with_encoding(Encoding::Gb18030)).unwrap();
        // 模拟"用户看完计划后，目标文件被别人建出来了"
        std::fs::write(&out, "别人的文件").unwrap();

        let err = run(&p).unwrap_err();
        assert!(err.contains("已存在"), "run 也要再查一遍：{err}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "别人的文件", "绝不能覆盖");
        // .part 临时文件不该留下
        assert!(!temp_path(&out).exists(), "失败后不能留下 .part");
    }

    #[test]
    fn same_source_and_target_blocked() {
        let d = tmp("same-file");
        let f = d.join("同一个.csv");
        std::fs::write(&f, "a\n1\n".as_bytes()).unwrap();
        let err = plan(&f, &f, &Options::default().with_encoding(Encoding::Gb18030)).unwrap_err();
        assert!(err.contains("同一个文件"), "{err}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\n1\n", "源文件不能被碰");
    }

    // ---------------- 编码转换 ----------------

    #[test]
    fn gbk_to_utf8_roundtrip_loses_nothing() {
        let d = tmp("gbk");
        let text = "客户,金额\n张三,100\n李四,200\n（备注：中文标点。";
        let e = encode_text(text, Encoding::Gb18030);
        assert_eq!(e.unmappable, 0, "这段文本在 GB18030 里全都装得下");
        assert_eq!(e.four_byte, 0, "全是 GBK 范围内的字，不该出现四字节编码");
        let gbk = e.bytes;
        let src = d.join("gbk.csv"); // 后缀是 .csv，但内容是 GBK —— 中文 Excel 的默认输出
        std::fs::write(&src, &gbk).unwrap();
        assert_eq!(crate::csv_import::detect_encoding(&gbk), Encoding::Gb18030);

        // GBK → UTF-8
        let utf8 = d.join("utf8.txt");
        let p = plan(&src, &utf8, &Options::default().with_encoding(Encoding::Utf8)).unwrap();
        run(&p).unwrap();
        let back = std::fs::read(&utf8).unwrap();
        assert_eq!(String::from_utf8(back.clone()).unwrap(), text, "UTF-8 往返不丢字");
        assert!(!back.starts_with(&[0xEF, 0xBB, 0xBF]), "显式选 UTF-8 就不该带 BOM");

        // UTF-8 → GBK：字节必须与最初那份完全一致（真往返）
        let back_to_gbk = d.join("回到gbk.txt");
        let p2 = plan(&utf8, &back_to_gbk, &Options::default().with_encoding(Encoding::Gb18030)).unwrap();
        run(&p2).unwrap();
        assert_eq!(std::fs::read(&back_to_gbk).unwrap(), gbk, "GBK → UTF-8 → GBK 必须逐字节相同");
    }

    #[test]
    fn same_encoding_and_delimiter_rejected() {
        let d = tmp("nothing-todo");
        let f = d.join("已经是了.csv");
        std::fs::write(&f, "a,b\n1,2\n".as_bytes()).unwrap();
        let err = plan(&f, &d.join("另一个.csv"), &Options::default()).unwrap_err();
        assert!(err.contains("没有任何东西要转"), "{err}");
    }

    #[test]
    fn gb18030_四字节字符与装不下的字符都被如实报告() {
        // ---- (1) emoji：GB18030 覆盖**全部** Unicode，所以它没有丢，
        //      但会写成四字节序列 —— 而按 GBK 读的软件会把这几个字节显示成乱码。
        let text = "中文😀ok🎉结束";
        let e = encode_text(text, Encoding::Gb18030);
        assert_eq!(e.unmappable, 0, "GB18030 覆盖全部 Unicode，emoji 不该算「装不下」");
        assert_eq!(e.four_byte, 2, "两个 emoji 都是四字节形式：{:?}", e.samples);
        assert!(e.samples.iter().any(|s| s.contains("😀")), "样例要能看出是哪个字：{:?}", e.samples);

        // 与官方一次性接口逐字节一致（自己写的循环不能改变输出）
        let (official, _, had_errors) = GB18030.encode(text);
        assert!(!had_errors);
        assert_eq!(e.bytes, official.to_vec());
        // 四字节序列的位置上必须能解回原来的字（结构是 81–FE 30–39 81–FE 30–39）
        let i = e
            .bytes
            .windows(4)
            .position(|w| (0x81..=0xFE).contains(&w[0]) && (0x30..=0x39).contains(&w[1]))
            .expect("emoji 必须写成四字节序列");
        let (back, _, _) = GB18030.decode(&e.bytes[i..i + 4]);
        assert_eq!(back, "😀", "四字节序列必须能解回原字：{:02X?}", &e.bytes[i..i + 4]);

        // ---- (2) U+E5E5：GB18030-2022 的官方映射表里就没有它 → 真的装不下，
        //      会被写成 `&#58853;`。这条路径虽然罕见，但一旦发生就是内容变形，
        //      所以必须有精确计数（用官方接口做对照钉死）。
        let e2 = encode_text("\u{E5E5}", Encoding::Gb18030);
        assert_eq!(e2.unmappable, 1, "这个码位确实装不下：{:?}", e2.samples);
        let (official2, _, had2) = GB18030.encode("\u{E5E5}");
        assert!(had2, "前提：官方接口也认为它装不下");
        assert_eq!(e2.bytes, official2.to_vec(), "自己写的 NCR 必须与官方 replacement 完全一致");
        assert_eq!(String::from_utf8_lossy(&e2.bytes), "&#58853;");
    }

    // ---------------- 图片 ----------------

    #[test]
    fn png_to_jpg_to_png_size_unchanged() {
        let d = tmp("img-roundtrip");
        let png = d.join("原.png");
        RgbImage::from_fn(41, 23, |x, y| {
            Rgb([(x * 5) as u8, (y * 9) as u8, 128])
        })
        .save(&png)
        .unwrap();

        let jpg = d.join("中间.jpg");
        let p = plan(&png, &jpg, &Options::default()).unwrap();
        assert_eq!(p.loss_of("jpeg_lossy"), 1, "jpg 是有损的，必须报");
        run(&p).unwrap();

        let back = d.join("回来.png");
        let p2 = plan(&jpg, &back, &Options::default()).unwrap();
        run(&p2).unwrap();

        let img = image::open(&back).unwrap();
        assert_eq!((img.width(), img.height()), (41, 23), "尺寸必须原样");
        // 颜色允许有损压缩的偏差，但必须"还是那个颜色"
        let c = img.to_rgb8().get_pixel(20, 11).0;
        assert!(c[0] > 60 && c[0] < 140, "红色分量跑掉了：{c:?}");
    }

    #[test]
    fn exif_orientation_applied_else_photo_rotated() {
        let d = tmp("exif");
        // 造一张 41×23 的 jpg，然后在 SOI 之后插一段 EXIF：Orientation = 6（顺时针 90°）
        let base = d.join("竖着拍的.jpg");
        RgbImage::from_fn(41, 23, |x, _| Rgb([(x * 5) as u8, 10, 10])).save(&base).unwrap();
        let rotated = d.join("带方向.jpg");
        let bytes = std::fs::read(&base).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "前提：JPEG 以 SOI 开头");
        let app1 = exif_app1(6);
        let mut out_bytes = Vec::with_capacity(bytes.len() + app1.len());
        out_bytes.extend_from_slice(&bytes[..2]);
        out_bytes.extend_from_slice(&app1);
        out_bytes.extend_from_slice(&bytes[2..]);
        std::fs::write(&rotated, out_bytes).unwrap();

        let out = d.join("转正.png");
        let p = plan(&rotated, &out, &Options::default()).unwrap();
        assert!(
            p.steps.iter().any(|s| s.contains("EXIF 方向")),
            "计划里要说会应用方向：{:?}",
            p.steps
        );
        let rep = run(&p).unwrap();
        assert!(
            rep.notes.iter().any(|n| n.contains("EXIF 方向")),
            "报告里要说已经应用：{:?}",
            rep.notes
        );

        let img = image::open(&out).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (23, 41),
            "方向 6 = 顺时针 90°，宽高必须互换（否则手机拍的照片是躺着的）"
        );
    }

    #[test]
    fn transparent_pixels_counted_and_flattened_to_white() {
        let d = tmp("alpha");
        // 整张图都是透明的：转成 JPEG 之后必须是纯白，不能是黑块
        let png = d.join("透明.png");
        image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 0])).save(&png).unwrap();

        let jpg = d.join("白底.jpg");
        let p = plan(&png, &jpg, &Options::default()).unwrap();
        assert_eq!(p.loss_of("alpha"), 1, "计划要先说明会丢透明通道：{:?}", p.losses);
        let rep = run(&p).unwrap();

        let loss = rep
            .losses
            .iter()
            .find(|l| l.kind == "alpha")
            .expect("运行后必须有精确计数");
        assert_eq!(loss.count, 16 * 16, "256 个透明像素：{}", loss.detail);

        // 透明处应当是白色，不是黑块
        let img = image::open(&jpg).unwrap().to_rgb8();
        let c = img.get_pixel(8, 8).0;
        assert!(c[0] > 230 && c[1] > 230 && c[2] > 230, "透明处应当是白色，实际 {c:?}");
    }

    #[test]
    fn lossless_webp_roundtrip_pixel_identical() {
        let d = tmp("webp");
        let png = d.join("原.png");
        RgbImage::from_fn(37, 19, |x, y| Rgb([(x * 7) as u8, (y * 13) as u8, 200]))
            .save(&png)
            .unwrap();

        // png → webp：计划里要说清"只能无损、照片会比 jpg 大"
        let webp = d.join("中间.webp");
        let p = plan(&png, &webp, &Options::default()).unwrap();
        assert!(
            p.warnings.iter().any(|w| w.contains("无损")),
            "要说明 WebP 这一路是无损的：{:?}",
            p.warnings
        );
        run(&p).unwrap();
        assert!(webp.exists());

        // webp → png：无损格式往返必须**一个像素都不差**
        let back = d.join("回来.png");
        let p2 = plan(&webp, &back, &Options::default()).unwrap();
        run(&p2).unwrap();
        let a = image::open(&png).unwrap().to_rgb8();
        let b = image::open(&back).unwrap().to_rgb8();
        assert_eq!(a.dimensions(), b.dimensions(), "尺寸必须一致");
        assert_eq!(a.as_raw(), b.as_raw(), "无损格式往返不能有任何像素差");
    }

    #[test]
    fn scale_down_only_rotate_swaps_dimensions() {
        let d = tmp("resize");
        let png = d.join("长图.png");
        RgbImage::from_pixel(100, 50, Rgb([10, 200, 10])).save(&png).unwrap();

        // 只缩不放
        let small = d.join("小.png");
        let p = plan(&png, &small, &Options::default().with_max_edge(20)).unwrap();
        assert!(p.steps.iter().any(|s| s.contains("20×10")), "计划里要给出目标尺寸：{:?}", p.steps);
        run(&p).unwrap();
        let img = image::open(&small).unwrap();
        assert_eq!((img.width(), img.height()), (20, 10));

        // 本来就更小 → 不放大（只警告）
        let bigger = d.join("更大.png");
        let p2 = plan(&png, &bigger, &Options::default().with_max_edge(400)).unwrap();
        assert!(
            p2.warnings.iter().any(|w| w.contains("不会放大")),
            "要提示不会放大：{:?}",
            p2.warnings
        );
        run(&p2).unwrap();
        let img2 = image::open(&bigger).unwrap();
        assert_eq!((img2.width(), img2.height()), (100, 50), "不会放大");

        // 旋转 90°：宽高互换
        let rot = d.join("旋转.png");
        let p3 = plan(&png, &rot, &Options::default().with_rotate90(1)).unwrap();
        run(&p3).unwrap();
        let img3 = image::open(&rot).unwrap();
        assert_eq!((img3.width(), img3.height()), (50, 100));

        // 缩放 + 旋转一起
        let both = d.join("又缩又转.png");
        let p4 = plan(
            &png,
            &both,
            &Options::default().with_max_edge(20).with_rotate90(1),
        )
        .unwrap();
        run(&p4).unwrap();
        let img4 = image::open(&both).unwrap();
        assert_eq!((img4.width(), img4.height()), (10, 20), "先缩到 20×10 再转 90°");
    }

    // ---------------- 错误路径 ----------------

    #[test]
    fn empty_missing_and_same_format_error_not_panic() {
        let d = tmp("errors");

        // 不存在的文件
        let err = plan(&d.join("没有这个.csv"), &d.join("出.csv"), &Options::default()).unwrap_err();
        assert!(err.contains("读不到"), "{err}");

        // 空文件
        let empty = d.join("空.csv");
        std::fs::write(&empty, b"").unwrap();
        let err = plan(&empty, &d.join("出.csv"), &Options::default()).unwrap_err();
        assert!(err.contains("0 字节"), "{err}");

        // 同格式且没有编辑 → 没有意义
        let png = d.join("图.png");
        RgbImage::from_pixel(2, 2, Rgb([1, 2, 3])).save(&png).unwrap();
        let err = plan(&png, &d.join("又一张.png"), &Options::default()).unwrap_err();
        assert!(err.contains("没有任何东西要转"), "{err}");

        // xlsx → xlsx
        let xlsx = d.join("表.xlsx");
        make_xlsx(&xlsx, &[("表", vec![vec!["a"], vec!["1"]])]);
        let err = plan(&xlsx, &d.join("又一份.xlsx"), &Options::default()).unwrap_err();
        assert!(err.contains("不能转成 .xlsx"), "{err}");

        // 认不出来的后缀（gif 没编进来，要给替代做法）
        let err = plan(&png, &d.join("出.gif"), &Options::default()).unwrap_err();
        assert!(err.contains("gif"), "要讲清 gif 的情况：{err}");
        assert!(err.contains("PNG"), "要给出可执行的替代做法：{err}");

        // 图片和表格不能互转
        let csv = d.join("表.csv");
        std::fs::write(&csv, "a,b\n1,2\n".as_bytes()).unwrap();
        let err = plan(&csv, &d.join("出.png"), &Options::default()).unwrap_err();
        assert!(err.contains("两类东西"), "{err}");

        // 空文件也不能让 run 崩（run 前目标已存在的检查会先过）
        let p2 = plan(&png, &d.join("出.jpg"), &Options::default()).unwrap();
        assert!(run(&p2).is_ok());
    }

    #[test]
    fn reconcile_reports_kind_not_count() {
        let d = tmp("reconcile");
        // png 带透明 → jpg：计划里 alpha 记 1（具体数量要解码后才知道），
        // 实际是 1 个像素。两者数量不同但种类相同，**不该**报"与计划不符"。
        let png = d.join("一个透明点.png");
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 255]));
        img.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        img.save(&png).unwrap();

        let p = plan(&png, &d.join("出.jpg"), &Options::default()).unwrap();
        let rep = run(&p).unwrap();
        assert!(
            !rep.notes.iter().any(|n| n.contains("与计划不符")),
            "同种类（只是数量更精确）不该报不符：{:?}",
            rep.notes
        );
    }

    #[test]
    fn plan_summary_is_display_ready() {
        let d = tmp("summary");
        let xlsx = d.join("两张表.xlsx");
        make_xlsx(
            &xlsx,
            &[
                ("明细", vec![vec!["客户"], vec!["张三"]]),
                ("Sheet2", vec![vec!["a"], vec!["1"]]),
            ],
        );
        let p = plan(&xlsx, &d.join("出.csv"), &Options::default().with_sheet("明细")).unwrap();
        let s = p.summary();
        assert!(s.contains("将要做的"), "{s}");
        assert!(s.contains("会丢掉的东西"), "有丢失时摘要里必须写出来：{s}");
        assert!(s.contains("Sheet2"), "{s}");
        assert!(s.contains("要注意"), "{s}");
        assert!(p.is_lossy(), "丢了工作表就是有损转换");

        // 图片质量选项也要能用（界面上会给一个质量滑杆）
        let png = d.join("图.png");
        RgbImage::from_pixel(4, 4, Rgb([9, 9, 9])).save(&png).unwrap();
        let p2 = plan(&png, &d.join("图.jpg"), &Options::default().with_quality(60)).unwrap();
        assert!(p2.steps.iter().any(|s| s.contains("读取")), "{:?}", p2.steps);
    }

    /// 造一段最小可用的 EXIF APP1：`Exif\0\0` + TIFF 头 + 一个只含 Orientation 的 IFD0。
    /// 相机就是这么写的，所以用它来测"方向到底有没有被应用"最接近真实。
    fn exif_app1(orientation: u16) -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(&[0x49, 0x49, 42, 0]); // 小端 + 42
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 从偏移 8 开始
        tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 个条目
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag = Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // type = SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        let mut val = [0u8; 4];
        val[..2].copy_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&val);
        tiff.extend_from_slice(&0u32.to_le_bytes()); // 没有下一个 IFD

        let mut app1 = vec![0xFF, 0xE1];
        let payload = 2 + 6 + tiff.len();
        app1.extend_from_slice(&(payload as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        app1
    }
}
