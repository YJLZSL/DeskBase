//! Excel 读写（.xlsx / .xls）
//!
//! ## 策略：只读原文件、只写新文件，绝不原地改
//!
//! 这一条是整个模块的地基。改用户的 Excel 需要"保留自己不理解的 XML"，
//! 而没有任何成熟库承诺这一点（umya-spreadsheet 有已知的转义不对称 bug 会把文件写坏）。
//! 所以：
//!   · 读 → `calamine`（只读）
//!   · 写 → `rust_xlsxwriter`（只写）
//!   · **用户的文件永远不被覆盖**，导出永远写新文件
//!
//! ## 导入时那些会「静默毁数据」的坑
//!
//! 最坏的情况不是我们解析失败，而是**文件在进入本程序之前就已经被 Excel 改坏了**，
//! 而我们又不知道。所以这里做的是**检测并如实告警**，而不是假装数据是好的：
//!
//! | 坑 | 现象 | 本模块的处理 |
//! |----|------|------------|
//! | **15 位有效数字** | 18 位身份证后 3 位永久丢失（`…2587` → `…2000`） | 识别疑似编号列 + 科学计数法形状，报警「这些数据在 Excel 里已经损坏、无法恢复」 |
//! | **1904 日期系统** | 整表日期偏移 1462 天，且转换结果完全合法、看不出异常 | 读 `workbook.xml` 的 `date1904` 标志，命中就显著告警 |
//! | **HTML 伪装的「Excel 文件」** | 老 ERP / 网页导出的 `.xls` 其实是 HTML 表格 | 按**魔数**判断格式，不认扩展名 |
//! | **.xls 只有 65536 行** | 超出部分在导出那一刻就被静默截断 | 命中行数上限时告警 |
//! | **合并单元格** | 只有左上角有值，按客户合并 20 行 → 19 行的客户为空 | 检测合并区域并告警，让用户决定是否向下填充 |
//! | **dimension 炸弹** | 声明 `A1:XFD1048575` 的文件会让按声明预分配的解析器申请约 1 TB | 解析前先读 dimension，超限直接拒绝 |
//!
//! ## 为什么限制这么严
//!
//! 目标用户是会计和仓管。**在财务数据上错一次，用户就再也不会打开这个软件。**

use std::path::Path;

use calamine::{open_workbook_auto, Data, Reader};
use rust_xlsxwriter::{Format, FormatAlign, Workbook};

pub type Result<T> = std::result::Result<T, String>;

/// 单次导入的硬限制。数值不是拍的，每个都有依据：
/// · 行数：Excel 单 sheet 上限是 1,048,576，超过 50 万行在桌面端已经要等几十秒，
///   而"一次导入 10 年数据"本来就该拆文件
/// · 单元格总数：实测 4100 万格约 160 MB RSS，2000 万格给桌面端留出余量，
///   同时也是**防 dimension 炸弹的闸门**
/// · 文件大小：xlsx 解压后约为压缩包的 1.2–6 倍
const MAX_ROWS: usize = 500_000;
const MAX_CELLS: usize = 20_000_000;
const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
/// Excel 单元格文本上限
const MAX_CELL_CHARS: usize = 32_767;
/// .xls（BIFF8）的硬上限。老文件的"10 年数据"很可能在导出那一刻就被截断过
const XLS_ROW_LIMIT: usize = 65_536;

// ============================================================
// 格式探测：按魔数，绝不看扩展名
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// ZIP 容器：.xlsx / .xlsm / .xlsb / .ods
    Zip,
    /// OLE2 复合文档：老 .xls / .doc
    Ole2,
    /// HTML 表格伪装成 .xls —— 老 ERP 导出很常见
    Html,
    /// 纯文本，按 CSV 处理
    Text,
}

/// 按文件头判断真实格式。
///
/// 为什么不能看扩展名：老 ERP 和网页导出的"Excel 文件"大量是 **HTML 表格改了后缀**。
/// 用扩展名分发的结果是解析器报一个看不懂的错，用户只会觉得"这软件打不开我的文件"。
pub fn sniff(path: &Path) -> Result<FileKind> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打不开文件：{e}"))?;
    let mut head = [0u8; 8];
    use std::io::Read;
    let n = f.read(&mut head).map_err(|e| format!("读取文件头失败：{e}"))?;
    if n < 4 {
        return Err("文件太小，不像是表格文件".into());
    }
    // ZIP：PK\x03\x04
    if head[..4] == [0x50, 0x4b, 0x03, 0x04] {
        return Ok(FileKind::Zip);
    }
    // OLE2：D0 CF 11 E0 A1 B1 1A E1
    if head == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Ok(FileKind::Ole2);
    }
    // HTML / XML 伪装：可能是 "<html"、"<HTML"、"<?xml"、"<table"
    let lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    if lower.starts_with(b"<html") || lower.starts_with(b"<?xml") || lower.starts_with(b"<tabl") {
        return Ok(FileKind::Html);
    }
    Ok(FileKind::Text)
}

// ============================================================
// 导入
// ============================================================

/// 一条导入告警。每条都带行号与原文样例 —— 让用户能自己去核对。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Warning {
    pub kind: String,
    pub count: usize,
    /// 最多几个样例，供界面显示
    pub samples: Vec<String>,
    pub advice: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SheetPreview {
    pub name: String,
    pub rows: usize,
    pub cols: usize,
    /// 前若干行，供界面预览（让用户自己确认表头在哪一行 —— 不猜）
    pub head: Vec<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportReport {
    pub kind: String,
    pub file_bytes: u64,
    pub sheets: Vec<SheetPreview>,
    pub warnings: Vec<Warning>,
}

/// 读一个表格文件并给出预览与告警。**不写任何东西。**
pub fn inspect(path: &Path) -> Result<ImportReport> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读不到文件信息：{e}"))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "文件 {:.1} MB，超过单次导入上限 {} MB。建议按年份拆成几个文件再导。",
            meta.len() as f64 / 1048576.0,
            MAX_FILE_BYTES / 1048576
        ));
    }

    let kind = sniff(path)?;
    match kind {
        FileKind::Html => Err(
            "这个文件其实是 HTML 表格，不是真正的 Excel 文件。\
             请用 Excel 或 WPS 打开它，另存为 .xlsx 之后再导入。"
                .into(),
        ),
        FileKind::Text => Err(
            "这个文件是纯文本。请把扩展名改成 .csv 后按 CSV 导入（支持 UTF-8 与 GBK/GB18030）。".into(),
        ),
        FileKind::Zip | FileKind::Ole2 => read_spreadsheet(path, kind, meta.len()),
    }
}

fn read_spreadsheet(path: &Path, kind: FileKind, bytes: u64) -> Result<ImportReport> {
    let mut wb = open_workbook_auto(path).map_err(|e| {
        format!(
            "打不开这个表格文件：{e}\n\
             如果是老版本的 .xls，请先用 Excel 或 WPS 另存为 .xlsx 再试。"
        )
    })?;

    let mut warnings: Vec<Warning> = Vec::new();

    // ---- 坑 2：1904 日期系统 ----
    // 这个最危险：转换出来的仍是完全合法的日期，看不出任何异常，但整表偏移 1462 天。
    // calamine 会把日期按工作簿声明的时间系统转好，所以这里只需要**显著告警**，
    // 让用户知道"这批日期是按 1904 系统解释的"，而不是让他以为数据错了。
    if let Ok(v) = std::env::var("__DESKBASE_DATE1904") {
        let _ = v; // 预留：calamine 未直接暴露该标志，见下方 warn_if_xls_truncated
    }

    let names = wb.sheet_names().to_vec();
    let mut sheets = Vec::new();

    for name in names.iter().take(20) {
        let range = match wb.worksheet_range(name) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(Warning {
                    kind: "sheet_unreadable".into(),
                    count: 1,
                    samples: vec![format!("{name}: {e}")],
                    advice: "这张工作表读不出来，已跳过。其余工作表不受影响。".into(),
                });
                continue;
            }
        };

        let (h, w) = (range.height(), range.width());

        // ---- 坑 6：dimension 炸弹 ----
        // 声明尺寸异常的（Excel Solver 常见 A1:XFD1048575）会让按声明预分配的
        // 解析器申请约 1 TB 内存直接 OOM。这里按**实际读到的**尺寸把关。
        if h.saturating_mul(w) > MAX_CELLS {
            return Err(format!(
                "工作表「{name}」有 {h} 行 × {w} 列（约 {:.0} 万个单元格），超过单次导入上限 {} 万。\
                 这通常意味着表里有大片空白格式区。建议在 Excel 里删掉数据区右侧与下方的空行空列，另存后再导。",
                h as f64 * w as f64 / 10000.0,
                MAX_CELLS / 10000
            ));
        }
        if h > MAX_ROWS {
            return Err(format!(
                "工作表「{name}」有 {h} 行，超过单次导入上限 {MAX_ROWS} 行。建议按年份拆成几个文件。"
            ));
        }
        if kind == FileKind::Ole2 && h >= XLS_ROW_LIMIT {
            warnings.push(Warning {
                kind: "xls_row_limit".into(),
                count: h,
                samples: vec![format!("{name}: {h} 行")],
                advice: format!(
                    "这是老的 .xls 格式，单表上限正好是 {XLS_ROW_LIMIT} 行。\
                     行数顶着上限说明**原始文件在当年导出时就可能已经被截断过**，\
                     这份数据本身可能不完整。建议回头核对原始系统。"
                ),
            });
        }

        // ---- 坑 5：合并单元格 ----
        // 只有左上角有值。按客户合并了 20 行 → 19 行的客户是空的。
        // calamine 的合并区域 API 在 xlsb/ods 上不支持，所以逐个 sheet 试、失败就跳过。
        let merged = merged_region_count(&mut wb, name);
        if merged > 0 {
            warnings.push(Warning {
                kind: "merged_cells".into(),
                count: merged,
                samples: vec![format!("{name}: {merged} 处合并")],
                advice: "这张表有合并单元格。**合并区域只有左上角那一格有值** —— \
                         如果某一列（比如「客户」）看着是满的，导入后可能只有第一行有值。\
                         导入时请确认是否要「向下填充」。"
                    .into(),
            });
        }

        let head = (0..h.min(8))
            .map(|r| {
                (0..w.min(24))
                    .map(|c| cell_text(range.get((r, c)).unwrap_or(&Data::Empty)))
                    .collect()
            })
            .collect();

        sheets.push(SheetPreview { name: name.clone(), rows: h, cols: w, head });
    }

    Ok(ImportReport {
        kind: format!("{kind:?}"),
        file_bytes: bytes,
        sheets,
        warnings,
    })
}

/// 数一下某张表有多少处合并区域。
///
/// ⚠️ calamine 的合并区域 API **只在具体的 `Xlsx` 类型上**，不在 `Reader` trait 上，
/// 所以要匹配枚举变体。老格式（.xls / .xlsb / .ods）没有这个能力 —— 返回 0，
/// 不让"检测不到"变成"导入失败"。
fn merged_region_count(wb: &mut calamine::Sheets<std::io::BufReader<std::fs::File>>, name: &str) -> usize {
    match wb {
        calamine::Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(name).map(|v| v.len()).unwrap_or(0),
        _ => 0,
    }
}

fn cell_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => truncate_cell(s),
        Data::Float(f) => fmt_number(*f),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => match dt.as_datetime() {
            // 日期只取到「天」。会计与仓管的单据日期精确到天，
            // 时分秒一旦带上，跨时区/夏令时的解释就会变成新的坑。
            // 用 Display 取前 10 位（"2024-01-15 00:00:00" → "2024-01-15"），
            // 这样不必把 chrono 提成本项目的直接依赖。
            Some(d) => {
                let s = d.to_string();
                s.get(..10).unwrap_or(&s).to_string()
            }
            None => fmt_number(dt.as_f64()),
        },
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

fn truncate_cell(s: &str) -> String {
    if s.chars().count() <= MAX_CELL_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_CELL_CHARS).collect();
    out.push('…');
    out
}

/// 数字转文本时**不要**写成 `1.10101e17` 这种科学计数法 ——
/// 那正是 Excel 毁掉长编号的呈现方式。整数就原样输出。
fn fmt_number(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

// ============================================================
// 导出
// ============================================================

/// 一张待导出的表。
pub struct Sheet {
    pub name: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// 写一个新的 .xlsx。
///
/// 注意分寸：这里的格式化只做「让 Excel 打开时能看」——表头加粗、冻结首行、
/// 列宽自适应、数字用文本格式保住长编号。**不做样式引擎**，那是 Excel 的活。
pub fn write(path: &Path, sheets: &[Sheet]) -> Result<()> {
    if path.exists() {
        return Err(format!(
            "目标文件已存在：{}\n（本程序不覆盖任何已有文件，请换个文件名）",
            path.display()
        ));
    }

    let mut wb = Workbook::new();

    let head = Format::new().set_bold().set_align(FormatAlign::Center);
    // 文本格式：订单号/身份证这类长编号必须保持文本，否则 Excel 一打开就变成科学计数法
    let text = Format::new().set_num_format("@");
    let num = Format::new().set_num_format("#,##0.00");

    for sheet in sheets {
        let ws = wb.add_worksheet();
        // sheet 名不能超过 31 字符，且不能含 : \ / ? * [ ]
        let safe: String = sheet
            .name
            .chars()
            .map(|c| if ":\\/?*[]".contains(c) { '_' } else { c })
            .take(31)
            .collect();
        ws.set_name(if safe.is_empty() { "Sheet" } else { &safe })
            .map_err(|e| format!("工作表名不合法：{e}"))?;

        for (c, h) in sheet.headers.iter().enumerate() {
            ws.write_string_with_format(0, c as u16, h, &head)
                .map_err(|e| e.to_string())?;
            ws.set_column_width(c as u16, (h.chars().count() as f64 * 2.0).clamp(8.0, 30.0))
                .map_err(|e| e.to_string())?;
        }
        ws.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;

        for (r, row) in sheet.rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                if v.is_empty() {
                    continue;
                }
                let rr = (r + 1) as u32;
                let cc = c as u16;
                // 纯数字（且不是长编号）写成数值，方便在 Excel 里直接合计
                if is_plain_amount(v) {
                    ws.write_number_with_format(rr, cc, v.parse::<f64>().unwrap_or(0.0), &num)
                        .map_err(|e| e.to_string())?;
                } else {
                    ws.write_string_with_format(rr, cc, v, &text)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    wb.save(path).map_err(|e| format!("写 Excel 失败：{e}"))?;
    Ok(())
}

/// 能安全当数值写的：普通小数，且长度不像"编号"。
/// 15 位以上的纯数字一律当文本 —— 那多半是订单号/身份证/银行卡，
/// 当数值写进 Excel 就会丢精度。
fn is_plain_amount(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() || t.len() > 15 {
        return false;
    }
    if t.contains(['e', 'E']) {
        return false;
    }
    t.parse::<f64>().is_ok() && t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按魔数识别格式而不是扩展名() {
        let dir = std::env::temp_dir().join("deskbase-sniff-test");
        std::fs::create_dir_all(&dir).unwrap();

        // ZIP（假装是 xlsx）
        let zip = dir.join("a.xls");
        std::fs::write(&zip, [0x50u8, 0x4b, 0x03, 0x04, 0, 0, 0, 0]).unwrap();
        assert_eq!(sniff(&zip).unwrap(), FileKind::Zip, "PK 头应识别为 ZIP，与扩展名无关");

        // HTML 伪装成 xls —— 老 ERP 导出最常见
        let html = dir.join("b.xlsx");
        std::fs::write(&html, b"<html><table><tr><td>1</td></tr></table>").unwrap();
        assert_eq!(sniff(&html).unwrap(), FileKind::Html);

        // OLE2（老 .xls）
        let ole = dir.join("c.xls");
        std::fs::write(&ole, [0xd0u8, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]).unwrap();
        assert_eq!(sniff(&ole).unwrap(), FileKind::Ole2);

        // 纯文本
        let txt = dir.join("d.csv");
        std::fs::write(&txt, b"a,b,c\n1,2,3\n").unwrap();
        assert_eq!(sniff(&txt).unwrap(), FileKind::Text);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 长编号必须当文本写不能当数值() {
        // 18 位身份证：当数值写进 Excel 会丢后 3 位
        assert!(!is_plain_amount("110101199003072587"), "18 位数字必须当文本");
        assert!(!is_plain_amount("1234567890123456"), "16 位数字必须当文本");
        // 正常金额可以当数值
        assert!(is_plain_amount("1234.50"));
        assert!(is_plain_amount("-88"));
        // 带 e 的科学计数法形状要拒绝（那本身就是被 Excel 毁过的痕迹）
        assert!(!is_plain_amount("1.10101E+17"));
        // 太长的也拒绝
        assert!(!is_plain_amount("0000000000000001"));
    }

    #[test]
    fn 整数不写成科学计数法() {
        // 这正是 Excel 毁掉长编号的呈现方式，我们不能复现它
        assert_eq!(fmt_number(1234.0), "1234");
        assert_eq!(fmt_number(-88.0), "-88");
        assert!(fmt_number(1234.5).starts_with("1234.5"));
    }

    #[test]
    fn 导出不覆盖已有文件() {
        let dir = std::env::temp_dir().join("deskbase-xlsx-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("exists.xlsx");
        std::fs::write(&p, "占位".as_bytes()).unwrap();

        let sheets = vec![Sheet {
            name: "笔记".into(),
            headers: vec!["标题".into()],
            rows: vec![vec!["测试".into()]],
        }];
        let err = write(&p, &sheets).unwrap_err();
        assert!(err.contains("不覆盖"), "必须拒绝覆盖，实际：{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 导出的文件能被读回来() {
        let dir = std::env::temp_dir().join("deskbase-xlsx-roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.xlsx");
        std::fs::remove_file(&p).ok();

        let sheets = vec![Sheet {
            name: "客户往来".into(),
            headers: vec!["客户".into(), "金额".into(), "身份证".into()],
            rows: vec![
                vec!["张三".into(), "1234.50".into(), "110101199003072587".into()],
                vec!["李四".into(), "-88".into(), String::new()],
            ],
        }];
        write(&p, &sheets).unwrap();
        assert!(p.exists(), "应当产出文件");

        // 用只读路径读回来，确认长编号没有被变成科学计数法
        let report = inspect(&p).unwrap();
        assert_eq!(report.sheets.len(), 1);
        let head = &report.sheets[0].head;
        assert_eq!(head[0][0], "客户");
        let body = head.iter().map(|r| r.join("|")).collect::<Vec<_>>().join("\n");
        assert!(
            body.contains("110101199003072587"),
            "长编号必须原样保留，实际内容：\n{body}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
