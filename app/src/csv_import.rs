//! CSV 导入（纯文本表格）
//!
//! ## 为什么单独一个模块，而不是塞进 `xlsx.rs`
//!
//! `xlsx.rs` 那条路走的是 ZIP / OLE2 二进制解析，难点在"文件被 Excel 提前改坏了"；
//! CSV 是**纯文本**，难点在"这段字节到底是什么编码"和"这一行怎么切"。
//! 两者的坑一点都不重叠，混在一个文件里只会让两边都难改。
//!
//! ## 本模块处理的坑（都是真实出现过的，不是想出来的）
//!
//! | 坑 | 现象 | 处理 |
//! |----|------|------|
//! | **中文 Excel「另存为 CSV」输出 GBK** | 整个文件在按 UTF-8 读时全是乱码 | `detect_encoding` 用「严格 UTF-8 校验」把 UTF-8 和 GB18030 分开 |
//! | **编码猜错 → U+FFFD「�」** | 屏幕上出现一串「�」，数字也可能被吃掉 | 解码后统计 U+FFFD，> 0 就告警并提示可以手动指定编码 |
//! | **字段内的换行**（RFC 4180 允许） | 按行 split 会把一条记录劈成两半、整表错位 | 解析器是**引号感知的状态机**，不按行 split |
//! | **全角数字 `１２３`** | `'１'.is_numeric() == true` 但 `'１'.to_digit(10) == None`，金额静默变成文本 | 逐格扫描并告警 |
//! | **空行 / 空列 / 行尾多余分隔符** | 空行变成一条空记录；多余分隔符凭空多出一列 | 空行跳过并计数；整列为空、行尾多分隔符分别告警 |
//! | **列数不一致的行** | 某个字段里有没转义的逗号 → 从那行起整表错位 | 与首行（表头）比对列数，单独计数并给出**行号**样例 |
//! | **15 位以上长编号** | 订单号/身份证被 Excel 改成科学计数法、后几位变 0 | 与 `xlsx.rs` 同样的判据，命中就告警 |
//! | **文件过大 / 行数过多** | 解析时 OOM，用户看到的是"闪退"而不是错误提示 | 500 MB、50 万行两道闸门，**在解析前/解析中**拦截 |
//!
//! ## 为什么这么在意"如实告警"
//!
//! 目标用户是会计和仓管。**在财务数据上错一次，用户就再也不会打开这个软件。**
//! 所以这里的原则是：宁可告警多一条让用户自己核对，也不静默猜一个"看起来对"的结果。
//!
//! ## 与 RFC 4180 的唯一一处有意偏离
//!
//! RFC 规定引号必须紧跟在字段开头，` , "abc,def"` 里的引号算普通字符。
//! 但真实导出里「逗号 + 空格 + 引号」非常常见，严格照 RFC 会让那个逗号被当成分隔符、
//! 从此整表错位 —— 这个后果比"丢掉引号前的空格"严重得多。
//! 所以本模块把「字段到目前为止只有空白 + 遇到 `"`」也当作字段开始，并丢掉那些空白。

use std::path::Path;

use encoding_rs::GB18030;

pub type Result<T> = std::result::Result<T, String>;

/// 单次导入的硬限制。
///
/// 行数与 `xlsx.rs` 的 `MAX_ROWS` 保持一致（50 万）—— 两边不一致的话，
/// 用户会看到"同一批数据换成 CSV 就能导、换成 xlsx 就不行"这种无法解释的行为。
///
/// 文件大小比 xlsx 宽（500 MB vs 200 MB）是有理由的：xlsx 是 ZIP，
/// 解压后能膨胀 1.2–6 倍，且有 dimension 炸弹，必须在文件层就卡死；
/// CSV 不存在解压膨胀，唯一代价是内存，500 MB 是"一次导入十年流水"的实际上限。
const MAX_ROWS: usize = 500_000;
const MAX_FILE_BYTES: u64 = 500 * 1024 * 1024;

/// 预览行数。与 `xlsx.rs` 的 8 行一致 —— 让用户自己确认表头在第几行，不猜。
const PREVIEW_ROWS: usize = 8;
/// 每条告警最多带几个样例：够用户自己去核对，又不至于把界面撑爆
const MAX_SAMPLES: usize = 5;
/// 候选分隔符。**顺序即优先级**：完全打平时取靠前的，因为逗号在任何场景下都最常见。
const DELIMS: [char; 4] = [',', '\t', ';', '|'];
/// 探测分隔符时只看前若干行：取全部行会让"某一行有个别逗号"这种噪声失去意义
const SAMPLE_LINES: usize = 20;
/// 长编号判据的关键字（等价于需求里的正则 `/编号|单号|身份证|账号|卡号|税号/`）
const LONG_ID_KEYWORDS: [&str; 6] = ["编号", "单号", "身份证", "账号", "卡号", "税号"];

// ============================================================
// 编码探测
// ============================================================

/// 编码探测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Gb18030,
    Utf16Le,
    Utf16Be,
    /// 看不出是文本（空文件、二进制、带 NUL 的怪文件）
    Unknown,
}

impl Encoding {
    /// 给界面显示的名字。UTF-8 带不带 BOM 都叫 "UTF-8" —— 是否带 BOM 属于
    /// 实现细节，写在 `encoding_confidence` 里解释就够了。
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => "UTF-8",
            Encoding::Gb18030 => "GB18030",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Unknown => "未知",
        }
    }
}

/// 探测编码。顺序很重要，每一步都在排除一种可能性：
///
/// 1. **有 BOM** → 按 BOM 判定。BOM 是文件自己写的声明，它是唯一"不需要猜"的证据。
/// 2. **有 NUL 字节** → 先分流（见下）
/// 3. **严格校验整个文件是不是合法 UTF-8**（`std::str::from_utf8`）
///    —— 合法就是 UTF-8。GBK 的中文双字节序列几乎必然产生非法 UTF-8
///    （首字节 ≥ 0x81，紧跟的第二字节常落在 0x40–0x7F 或 0x80 之后，凑不成合法续字节），
///    这一步非常可靠；纯 ASCII 文件两种解释结果相同，无所谓。
/// 4. **非法 UTF-8** → 判定为 GB18030。GB18030 是 GBK 的超集，简体场景一律用它解码，
///    两种都能覆盖，所以**不需要**区分 GB2312 / GBK / GB18030。
///
/// ## 为什么第 2 步必须插在 UTF-8 校验之前
///
/// 因为 **NUL（0x00）是合法的 UTF-8**。没有 BOM 的 UTF-16LE 文本
/// （`"ID"` → `49 00 44 00`）整个文件都是合法 UTF-8，会被第 3 步误判成 UTF-8，
/// 于是每个字符后面都拖一个看不见的 NUL。所以先按 NUL 的分布把 UTF-16 摘出去。
///
/// ## 已知不足
///
/// 没有 BOM、且**几乎没有 ASCII 字符**的 UTF-16 文件（例如整列中文的表）
/// 不产生 NUL，这里区分不出来，会落到 GB18030 分支 —— 但那种解码几乎必然产生
/// U+FFFD，`inspect` 的 replacement_char 告警会兜住，用户可以在界面手动指定编码。
pub fn detect_encoding(bytes: &[u8]) -> Encoding {
    // ---- 1. BOM ----
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Encoding::Utf8Bom;
    }
    // 注意顺序：UTF-32LE 的 BOM 也以 FF FE 开头，但办公场景没有 UTF-32 的 CSV，
    // 为它引入"还要再看两个字节"的复杂度不值得。
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }

    // ---- 2. 有 NUL 的先把 UTF-16 摘出来 ----
    if count_nul(bytes) >= 2 {
        return sniff_bomless_utf16(bytes);
    }

    // ---- 3. 严格 UTF-8 ----
    if std::str::from_utf8(bytes).is_ok() {
        return Encoding::Utf8;
    }

    // ---- 4. 剩下的按简体中文环境最常见的 GB18030 处理 ----
    Encoding::Gb18030
}

/// 只看前 4 KB 就够判断了：NUL 要么满地都是（UTF-16/二进制），要么一个没有。
/// 全量扫描 500 MB 只为数几个 0x00 不划算。
const SNIFF_BYTES: usize = 4096;

fn count_nul(bytes: &[u8]) -> usize {
    let n = bytes.len().min(SNIFF_BYTES);
    bytes[..n].iter().filter(|&&b| b == 0).count()
}

/// 没有 BOM 时靠 NUL 的位置判断是不是 UTF-16。
///
/// 原理：ASCII 字符在 UTF-16LE 里是 `XX 00`（NUL 落在**奇数**位），
/// 在 UTF-16BE 里是 `00 XX`（NUL 落在**偶数**位）。两边都有大量 NUL，
/// 说明这不是文本而是二进制（或者是我们不认识的东西），返回 `Unknown` 让上层拒收，
/// 而不是硬解出一堆「�」。
fn sniff_bomless_utf16(bytes: &[u8]) -> Encoding {
    let n = bytes.len().min(SNIFF_BYTES);
    let mut even = 0usize; // 0, 2, 4, ... 位置上的 NUL
    let mut odd = 0usize;
    for (i, &b) in bytes[..n].iter().enumerate() {
        if b == 0 {
            if i % 2 == 0 {
                even += 1;
            } else {
                odd += 1;
            }
        }
    }
    // 一边压倒性多才算数（×8 的余量是为了容忍夹杂的中文与标点：
    // 中文在 UTF-16 里两个字节都非 0，会稀释 NUL 的比例，但不会改变奇偶分布）
    if odd >= 2 && even * 8 < odd {
        Encoding::Utf16Le
    } else if even >= 2 && odd * 8 < even {
        Encoding::Utf16Be
    } else {
        Encoding::Unknown
    }
}

/// 按探测结果解码成 UTF-8 字符串。
///
/// 返回 `(文本, 有没有解码失败)`。第二个值专门用来区分
/// "文件里本来就有「�」"和"我们猜错了编码才产生「�」"。
fn decode(bytes: &[u8], enc: Encoding) -> (String, bool) {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            // BOM 必须在这里切掉：它不可见，留着就会变成第一个字段名的一部分，
            // 于是界面上的"客户"和数据库里的"\u{FEFF}客户"永远对不上 —— 这是 CSV 导入最常见的 bug。
            let body = if enc == Encoding::Utf8Bom && bytes.len() >= 3 {
                &bytes[3..]
            } else {
                bytes
            };
            match std::str::from_utf8(body) {
                Ok(s) => (s.to_string(), false),
                // 理论上到不了这里（detect_encoding 已经校验过），
                // 但真到了也不能 panic：宁可给用户带「�」的数据和一条告警。
                Err(_) => (String::from_utf8_lossy(body).into_owned(), true),
            }
        }
        // GB18030 覆盖 GB2312/GBK/GB18030 三级，简体中文场景一律用它。
        // 用 decode_without_bom_handling 而不是 decode：后者会自动嗅探 BOM 并按
        // 嗅探结果换编码解码，那种"静默换解码器"的行为在出问题时极难排查。
        Encoding::Gb18030 => {
            let (text, had_errors) = GB18030.decode_without_bom_handling(bytes);
            (text.into_owned(), had_errors)
        }
        Encoding::Utf16Le => decode_utf16(bytes, true),
        Encoding::Utf16Be => decode_utf16(bytes, false),
        Encoding::Unknown => (String::new(), true),
    }
}

/// UTF-16 解码用 `std::char::decode_utf16` 手写，而不是也用 `encoding_rs`。
///
/// 为什么：UTF-16 到 UTF-8 的转换在标准库里就是完备的（代理对、奇数字节都在
/// `decode_utf16` 里处理），为它多调一个库只会让"我们到底依赖了什么"变模糊。
/// `encoding_rs` 真正不可替代的是 GB18030 那张两万多条的映射表。
fn decode_utf16(bytes: &[u8], little: bool) -> (String, bool) {
    // 调用方已确认有 BOM，掐掉这两个字节
    let body = if bytes.len() >= 2 { &bytes[2..] } else { &[][..] };
    let chunks = body.chunks_exact(2);
    // 尾部多出的单个字节说明文件本身是坏的
    let mut bad = !chunks.remainder().is_empty();
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

// ============================================================
// 分隔符探测
// ============================================================

/// 探测分隔符：`,` `\t` `;` `|` 四种里选一个。
///
/// 判据是「**哪种分隔符让每行的字段数最一致**」，不是"谁出现次数多"。
/// 为什么不能用出现次数：中文地址里逗号极其常见，
/// 「北京市朝阳区,安贞路1号」这一格里的逗号会让逗号在计数法里直接胜出，
/// 而真正的分隔符（制表符）反而一次都不出现。
///
/// 选不出任何一个时返回 `','`（调用方 `inspect` 会另外给出"只有一列"的告警）。
///
/// 只有测试在用这个简单版：`inspect` 需要 `_ex` 额外返回的"到底探没探到"，
/// 拿不到它就无法区分「文件本来就是单列」和「压根没找到分隔符」——
/// 后者必须告警，前者不该。所以真正的调用路径走 `_ex`，这个包装留给测试表达意图。
#[cfg(test)]
pub fn detect_delimiter(text: &str) -> char {
    detect_delimiter_ex(text).0
}

/// 返回 `(分隔符, 是不是真的探测到了)`。分开返回是为了让 `inspect` 能区分
/// "文件就是用逗号的单列"和"压根没找到分隔符"—— 后者必须告警，前者不必。
fn detect_delimiter_ex(text: &str) -> (char, bool) {
    let lines = sample_lines(text, SAMPLE_LINES);
    if lines.is_empty() {
        return (',', false);
    }

    let mut best: Option<(char, i32, i32)> = None; // (分隔符, 一致度, 首行是否等于众数)
    for d in DELIMS {
        let counts: Vec<usize> = lines.iter().map(|l| count_fields(l, d)).collect();

        // 众数字段数：用"最多的那种列数"而不是首行的列数，
        // 因为首行可能是表格大标题（只有一格），不代表数据形状。
        let mut mode = 0usize;
        let mut freq = 0usize;
        for &c in &counts {
            let f = counts.iter().filter(|&&x| x == c).count();
            if f > freq {
                freq = f;
                mode = c;
            }
        }
        // 字段数必须 > 1：`|` 在一个没有分隔符的文件里也能让"每行都是 1 列"100% 一致，
        // 没有这道闸门，它就赢了。
        if mode <= 1 {
            continue;
        }
        let consistency = (freq * 1000 / counts.len()) as i32;
        // 首行（表头）通常最干净，作为次要判据：正文里可能混着没转义的分隔符
        let first_ok = if counts[0] == mode { 1 } else { 0 };
        let better = match best {
            None => true,
            // 严格大于 ⇒ 完全打平时保留 DELIMS 里靠前的那个（逗号优先）
            Some((_, bc, bf)) => (consistency, first_ok) > (bc, bf),
        };
        if better {
            best = Some((d, consistency, first_ok));
        }
    }

    match best {
        Some((d, _, _)) => (d, true),
        None => (',', false),
    }
}

/// 取前若干**逻辑**行（引号里的换行不算行边界）。
///
/// 按字节扫描是安全的：要匹配的 `"` `\n` `\r` 全是 ASCII，
/// 而 UTF-8 的多字节序列里不会出现 0x80 以下的字节，不会切坏字符。
fn sample_lines(text: &str, max_lines: usize) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut i = 0usize;
    while i < bytes.len() && out.len() < max_lines {
        let b = bytes[i];
        if in_quotes {
            if b == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2; // `""` 是转义，不是闭合
                    continue;
                }
                in_quotes = false;
            }
        } else if b == b'"' {
            in_quotes = true;
        } else if b == b'\n' || b == b'\r' {
            let line = &text[start..i];
            // 空行会让每种候选分隔符都多出一次"1 列"，把众数带偏
            if !line.trim().is_empty() {
                out.push(line);
            }
            i += 1;
            if b == b'\r' && bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            start = i;
            continue;
        }
        i += 1;
    }
    if out.len() < max_lines && start < text.len() {
        let line = &text[start..];
        if !line.trim().is_empty() {
            out.push(line);
        }
    }
    out
}

/// 数一行里有几个字段（= 引号外的分隔符数 + 1）。
///
/// 引号规则与 `Parser` 保持一致（含"引号前只有空白也算字段开始"），
/// 否则探测出的字段数和真正解析出来的对不上，预览就骗人了。
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

/// 分隔符给界面看的中文名。用户看到"逗号 (,)"比看到"Delimiter: 0x2C"有用得多。
fn delim_label(c: char) -> String {
    match c {
        ',' => "逗号 (,)".to_string(),
        '\t' => "制表符 (Tab)".to_string(),
        ';' => "分号 (;)".to_string(),
        '|' => "竖线 (|)".to_string(),
        other => format!("{other:?}"),
    }
}

// ============================================================
// 解析
// ============================================================

/// 解析成行/列。
///
/// 处理 RFC 4180 的引号规则：
///   · 字段被 `"` 包裹时，内部的 `,` 与换行都是字段的一部分
///   · `""` 表示一个字面量引号
///
/// **不要按行 split**：按行 split 是 CSV 解析最经典的错，
/// 一条含换行的记录会被劈成两行，从那以后整张表都错位。
///
/// 空行会被丢弃（不返回空记录）。
///
/// 只有测试在用：`inspect` 走的是 `parse_limited`，因为它需要顺手统计出
/// 空行数 / 行尾多分隔符 / 每条的物理行号（告警要报行号），
/// 而这些在状态机里是顺带得到的，事后从 `Vec<Vec<String>>` 反推做不到。
#[cfg(test)]
pub fn parse(text: &str, delim: char) -> Vec<Vec<String>> {
    parse_limited(text, delim, usize::MAX).0
}

/// 解析过程中顺手统计出来的信息。放在解析里是因为这些量
/// （空行数、行尾多分隔符、每条记录的行号）在状态机里是**顺手**得到的，
/// 事后从 `Vec<Vec<String>>` 反推则要么做不到（行号），要么要再扫一遍。
#[derive(Debug, Default, Clone)]
struct ParseStats {
    /// 被跳过的空行数
    empty_lines: usize,
    /// 行尾多出一个分隔符的行数（凭空多出一列空字段）
    trailing_delim: usize,
    /// 每条记录**起始**的物理行号（1 起），与返回的行一一对应
    record_lines: Vec<usize>,
    /// 行数超过上限，已提前中止（不中止的话 500 MB 的 `a\n` × 2.5 亿行会直接 OOM）
    over_limit: bool,
}

fn parse_limited(text: &str, delim: char, max_rows: usize) -> (Vec<Vec<String>>, ParseStats) {
    let mut p = Parser::new(text, delim, max_rows);
    p.run();
    (p.rows, p.stats)
}

/// 引号感知的状态机。
///
/// 写成结构体而不是一堆局部变量：收记录的动作要在「遇到换行」和「文件末尾」
/// 两处调用，用闭包会和循环里的可变借用打架。
struct Parser<'a> {
    delim: char,
    max_rows: usize,
    it: std::iter::Peekable<std::str::Chars<'a>>,

    rows: Vec<Vec<String>>,
    stats: ParseStats,

    row: Vec<String>,
    field: String,
    in_quotes: bool,
    /// 当前字段出现过引号（用来区分"真空行"和 `""` 这种合法的空值）
    field_has_quote: bool,
    row_has_quote: bool,
    /// 当前字段到目前为止只有空白 —— 决定 `"` 算不算字段开始
    only_ws: bool,
    /// 上一个字符是分隔符（判断行尾多分隔符）
    last_was_delim: bool,
    /// 当前记录已经有内容（决定文件末尾要不要收尾）
    started: bool,
    /// 物理行号，1 起
    line: usize,
    /// 当前记录起始行号
    record_start: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str, delim: char, max_rows: usize) -> Self {
        Parser {
            delim,
            max_rows,
            it: text.chars().peekable(),
            rows: Vec::new(),
            stats: ParseStats::default(),
            row: Vec::new(),
            field: String::new(),
            in_quotes: false,
            field_has_quote: false,
            row_has_quote: false,
            only_ws: true,
            last_was_delim: false,
            started: false,
            line: 1,
            record_start: 1,
        }
    }

    fn run(&mut self) {
        while let Some(c) = self.it.next() {
            // ---- 引号内部：除了 `"` 之外一切都是普通字符 ----
            if self.in_quotes {
                if c == '"' {
                    let escaped = self.it.peek() == Some(&'"');
                    if escaped {
                        self.it.next();
                        self.field.push('"'); // `""` → 一个字面量引号
                    } else {
                        self.in_quotes = false; // 闭合
                    }
                } else {
                    // 引号里的换行是字段内容。行号还是要往前走，否则告警里的行号会偏。
                    if c == '\n' {
                        self.line += 1;
                    }
                    self.field.push(c);
                }
                continue;
            }

            // ---- 分隔符 ----
            if c == self.delim {
                self.push_field();
                self.only_ws = true;
                self.last_was_delim = true;
                self.started = true;
                continue;
            }

            // ---- 换行：\r\n / \n / 单个 \r（老 Mac 导出）都算一条记录结束 ----
            if c == '\n' || c == '\r' {
                let lf_after_cr = c == '\r' && self.it.peek() == Some(&'\n');
                if lf_after_cr {
                    self.it.next();
                }
                self.end_record();
                if self.stats.over_limit {
                    return;
                }
                self.line += 1;
                self.record_start = self.line;
                continue;
            }

            // ---- 引号开头：只有"这个字段还是空的/只有空白"时才算包裹 ----
            if c == '"' && self.only_ws {
                // 丢掉引号前的空白（见模块文档的"有意偏离"一节）
                self.field.clear();
                self.in_quotes = true;
                self.field_has_quote = true;
                self.row_has_quote = true;
                self.only_ws = false;
                self.started = true;
                self.last_was_delim = false;
                continue;
            }

            // ---- 普通字符（含字段中间的引号：那是字面量，Excel 也这么处理）----
            self.field.push(c);
            if !c.is_whitespace() {
                self.only_ws = false;
            }
            self.started = true;
            self.last_was_delim = false;
        }

        // 文件末尾没有换行时，最后一条记录还挂在手上
        if self.started {
            self.end_record();
        }
    }

    fn push_field(&mut self) {
        let f = std::mem::take(&mut self.field);
        self.row.push(f);
        self.row_has_quote |= self.field_has_quote;
        self.field_has_quote = false;
    }

    fn end_record(&mut self) {
        self.push_field();

        // 真空行：只有一格、那一格是空白、且没出现过引号。
        // 加"没出现过引号"是为了不误杀 `""` —— 那是用户明确写下的一个空值。
        let blank = self.row.len() == 1 && !self.row_has_quote && self.row[0].trim().is_empty();
        if blank {
            self.stats.empty_lines += 1;
            self.row.clear();
        } else {
            if self.last_was_delim && self.row.len() > 1 {
                self.stats.trailing_delim += 1;
            }
            self.stats.record_lines.push(self.record_start);
            self.rows.push(std::mem::take(&mut self.row));
            if self.rows.len() > self.max_rows {
                self.stats.over_limit = true;
            }
        }

        self.row_has_quote = false;
        self.field_has_quote = false;
        self.only_ws = true;
        self.last_was_delim = false;
        self.started = false;
    }
}

// ============================================================
// 检查（只读）
// ============================================================

/// 一条告警。结构与 `xlsx.rs` 的 `Warning` 保持一致（kind / count / samples / advice），
/// 这样界面可以用同一套组件渲染两种来源的告警。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Warning {
    pub kind: String,
    pub count: usize,
    /// 最多几个样例，供界面显示
    pub samples: Vec<String>,
    pub advice: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub encoding: String,
    /// 说明是靠 BOM、靠严格校验、还是靠启发 —— 让用户知道这个结论有多可信
    pub encoding_confidence: String,
    pub delimiter: String,
    /// 记录数，**含第一行**（那通常是表头）。本程序不猜表头在第几行，与 xlsx 一致。
    pub rows: usize,
    /// 第一行的列数。比这更长/更短的行由 ragged_rows 告警单独点名。
    pub cols: usize,
    /// 前 8 行，供界面预览
    pub head: Vec<Vec<String>>,
    pub warnings: Vec<Warning>,
}

/// 检查一个 CSV 文件：探测编码与分隔符、解析、给出告警与预览。
/// **只读，不写任何东西** —— 用户的原件永远安全。
pub fn inspect(path: &Path) -> Result<Report> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读不到文件信息：{e}"))?;
    check_size(meta.len())?;

    let bytes = std::fs::read(path).map_err(|e| format!("读不出文件内容：{e}"))?;
    if bytes.is_empty() {
        return Err("这个文件是空的（0 字节），没有可导入的内容。".into());
    }

    let enc = detect_encoding(&bytes);
    if enc == Encoding::Unknown {
        return Err("这个文件里全是 NUL 之类的控制字节，不像是文本表格。\
                    如果它确实是 CSV，请先用 Excel 或记事本打开，另存为「CSV UTF-8」再导入。"
            .into());
    }

    let (text, decode_failed) = decode(&bytes, enc);
    let confidence = confidence_text(enc);

    let (delim, delim_found) = detect_delimiter_ex(&text);
    let (mut rows, stats) = parse_limited(&text, delim, MAX_ROWS);

    // ---- 行数闸门 ----
    // 必须放在解析里卡（见 ParseStats::over_limit 的注释），这里只是把结果翻成人话。
    if stats.over_limit {
        return Err(format!(
            "文件超过 {MAX_ROWS} 行，超过单次导入上限。建议按年份或按月份拆成几个文件再导。"
        ));
    }
    if rows.is_empty() {
        return Err("这个文件里没有可用的数据行（只有空行）。".into());
    }

    let mut warnings: Vec<Warning> = Vec::new();

    // ---- 坑：编码猜错 → U+FFFD ----
    // 必须告警的理由：U+FFFD 是**不可逆**的，导入之后再去数据库里已经分不清
    // 哪一格原本是"中"、哪一格原本是"�"。所以只能在入口拦。
    let fffd = text.matches('\u{FFFD}').count();
    if fffd > 0 || decode_failed {
        let (cells, samples) = cells_matching(&rows, &stats.record_lines, |c| c.contains('\u{FFFD}'));
        warnings.push(Warning {
            kind: "replacement_char".into(),
            count: fffd.max(cells),
            samples,
            advice: format!(
                "解码后出现了 {fffd} 个「�」（U+FFFD）替换字符。\
                 这说明文件的编码可能不是{}。\
                 请在导入界面手动指定编码（UTF-8 或 GB18030）再试一次；\
                 如果换了编码还是「�」，那说明上游软件导出时就已经把它替换掉了，无法恢复。",
                enc.label()
            ),
        });
    }

    // ---- 坑：空行 ----
    if stats.empty_lines > 0 {
        warnings.push(Warning {
            kind: "empty_lines".into(),
            count: stats.empty_lines,
            samples: vec![format!("共 {} 行", stats.empty_lines)],
            advice: "这些行整行是空的，已跳过。如果它们本该有数据，\
                     说明上游软件在导出时就漏了。"
                .into(),
        });
    }

    // ---- 坑：没探测到分隔符 ----
    if !delim_found || rows[0].len() <= 1 {
        warnings.push(Warning {
            kind: "single_column".into(),
            count: 1,
            samples: rows.first().map(|r| vec![r.join("")]).unwrap_or_default(),
            advice: "没有识别出分隔符，整个文件被当成了一列。如果它确实有多列，\
                     可能是用的分隔符不在（逗号 / Tab / 分号 / 竖线）之内，\
                     建议用 Excel 打开后另存为「CSV UTF-8」再导入。"
                .into(),
        });
    }

    // ---- 坑：行尾多余分隔符 ----
    if stats.trailing_delim > 0 {
        warnings.push(Warning {
            kind: "trailing_delimiter".into(),
            count: stats.trailing_delim,
            samples: vec![format!("共 {} 行以分隔符结尾", stats.trailing_delim)],
            advice: "这些行以分隔符结尾，会凭空多出一个空的末列。\
                     如果每一行都这样，多半是导出软件的习惯，导入时忽略最后一列即可。"
                .into(),
        });
    }

    // ---- 坑：列数与表头不一致（错位的信号）----
    // 用**第一行**当表头基准。第一行是表格大标题（只占一格）时这里会报很多行，
    // 但告警里带了行号，用户看一眼预览就能判断，比我们擅自挑表头行安全。
    let cols = rows[0].len();
    let mut ragged = 0usize;
    let mut ragged_samples: Vec<String> = Vec::new();
    for (i, r) in rows.iter().enumerate().skip(1) {
        if r.len() != cols {
            ragged += 1;
            if ragged_samples.len() < MAX_SAMPLES {
                ragged_samples.push(format!(
                    "第 {} 行：{} 列（表头 {} 列）",
                    stats.record_lines.get(i).copied().unwrap_or(0),
                    r.len(),
                    cols
                ));
            }
        }
    }
    if ragged > 0 {
        warnings.push(Warning {
            kind: "ragged_rows".into(),
            count: ragged,
            samples: ragged_samples,
            advice: "这些行的列数和表头对不上。最常见的原因是某个字段里有**没被引号包起来的逗号**\
                     （中文地址、商品名里很常见），一旦发生，后面所有列都会整体错位一格。\
                     请对照上面的行号核对原文件；如果第一行是表格大标题、表头在第二行，\
                     这条告警可以直接忽略。"
                .into(),
        });
    }

    // ---- 坑：整列为空 ----
    if rows.len() >= 2 {
        let mut empty_count = 0usize;
        let mut empty_cols: Vec<String> = Vec::new();
        for c in 0..cols {
            let all_empty = rows[1..]
                .iter()
                .all(|r| r.get(c).map(|v| v.trim().is_empty()).unwrap_or(true));
            if !all_empty {
                continue;
            }
            empty_count += 1;
            // 计数要准，样例只留前几个（两者别混）
            if empty_cols.len() < MAX_SAMPLES {
                let name = rows[0].get(c).map(|s| s.trim()).unwrap_or("");
                empty_cols.push(if name.is_empty() {
                    format!("第 {} 列（无列名）", c + 1)
                } else {
                    format!("「{name}」")
                });
            }
        }
        if empty_count > 0 {
            warnings.push(Warning {
                kind: "empty_column".into(),
                count: empty_count,
                samples: empty_cols,
                advice: "这些列在所有数据行里都是空的。多半是导出时多带出来的空列，\
                         导入时可以不要它们；但如果它们本该有值，就说明上游数据有问题。"
                    .into(),
            });
        }
    }

    // ---- 坑：全角数字 ----
    // `'１'.is_numeric() == true` 但 `'１'.to_digit(10) == None`，
    // 于是"看起来是数字"的金额会在解析时静默变成文本、合计直接少一块。
    // 这是高频情况：中文输入法下打数字，手滑留在全角是常事。
    let (fw_count, fw_samples) = cells_matching(&rows, &stats.record_lines, has_fullwidth_digit);
    if fw_count > 0 {
        warnings.push(Warning {
            kind: "fullwidth_digits".into(),
            count: fw_count,
            samples: fw_samples,
            advice: "有格子用了全角数字（１２３）。它肉眼和半角几乎一样，\
                     但没法当数字参与计算和合计，Excel 里也常常是文本。\
                     建议在原文件里替换成半角数字后再导入，否则这些金额只能当文本存。"
                .into(),
        });
    }

    // ---- 坑：15 位以上长编号 ----
    // 判据与 xlsx.rs 对齐：列名命中关键字 且 值形如 `^\d{15,}$`。
    // 与 Excel 不同的是：**CSV 里的长编号此刻是完好的**，但我们仍要告警 ——
    // 因为用户下一手很可能是"用 Excel 打开看看"，那一存就再也回不来了。
    let mut id_samples: Vec<String> = Vec::new();
    let mut id_cols: Vec<String> = Vec::new();
    for (c, h) in rows[0].iter().enumerate() {
        if !LONG_ID_KEYWORDS.iter().any(|k| h.contains(k)) {
            continue;
        }
        let mut hit = 0usize;
        for (i, r) in rows.iter().enumerate().skip(1) {
            if let Some(v) = r.get(c) {
                if is_long_number(v) {
                    hit += 1;
                    if id_samples.len() < MAX_SAMPLES {
                        id_samples.push(format!(
                            "第 {} 行「{}」：{}",
                            stats.record_lines.get(i).copied().unwrap_or(0),
                            h.trim(),
                            v.trim()
                        ));
                    }
                }
            }
        }
        if hit > 0 {
            id_cols.push(format!("「{}」（{} 处）", h.trim(), hit));
        }
    }
    if !id_cols.is_empty() {
        warnings.push(Warning {
            kind: "long_number_id".into(),
            count: id_cols.len(),
            samples: id_samples,
            advice: "这些列里是 15 位以上的长编号（订单号 / 身份证 / 税号）。\
                     在 CSV 里它们现在是完好的，但**只要用 Excel 打开并保存一次**，\
                     15 位以后就会被静默改成 0（科学计数法存不下）。\
                     如果要核对，请直接用记事本看原文件。"
                .into(),
        });
    }

    // 预览只留前 8 行，剩下的（可能很大）在这里就丢掉
    let head: Vec<Vec<String>> = if rows.len() > PREVIEW_ROWS {
        rows.truncate(PREVIEW_ROWS);
        rows
    } else {
        rows
    };

    Ok(Report {
        encoding: enc.label().to_string(),
        encoding_confidence: confidence,
        delimiter: delim_label(delim),
        rows: stats.record_lines.len(),
        cols,
        head,
        warnings,
    })
}

/// 文件大小闸门。单独提出来是为了能直接测到 500 MB 这条线 ——
/// 真去造一个 500 MB 的测试文件太蠢了。
fn check_size(len: u64) -> Result<()> {
    if len > MAX_FILE_BYTES {
        return Err(format!(
            "文件 {:.0} MB，超过单次导入上限 {} MB。三个可行的做法：\
             ① 用 Excel/WPS 打开，按年份或月份拆成几个 CSV；\
             ② 另存为 .xlsx 再导入（xlsx 是压缩的，通常会小到五分之一）；\
             ③ 只导出你真正需要的那几列。",
            len as f64 / 1048576.0,
            MAX_FILE_BYTES / 1048576
        ));
    }
    Ok(())
}

/// 把"凭什么这么判"讲清楚。用户看到乱码时，唯一能自救的信息就是这句。
fn confidence_text(enc: Encoding) -> String {
    match enc {
        Encoding::Utf8Bom => "文件开头有 UTF-8 BOM（EF BB BF），按 BOM 判定，可以确定。".into(),
        Encoding::Utf8 => {
            "整个文件都通过了 UTF-8 严格校验（GBK 中文几乎必然通不过），可以确定。".into()
        }
        Encoding::Gb18030 => "不是合法的 UTF-8，按 GB18030 解码（简体中文环境最常见，\
                              它同时覆盖 GB2312 / GBK）。若显示为乱码，可在导入界面手动指定编码。"
            .into(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            "文件开头有 UTF-16 BOM，按 BOM 判定（Excel 的「Unicode 文本」就是这种）。".into()
        }
        Encoding::Unknown => "无法判断。".into(),
    }
}

/// 收集含某个特征的格子，顺便把行号和列名带上 —— 用户要能自己核对，
/// 只报一句"有 3 处异常"等于没告警。
///
/// 返回 `(命中总数, 样例)`。总数和样例要分开：界面上的计数必须是真数，
/// 样例只是前几个。
fn cells_matching<F: Fn(&str) -> bool>(
    rows: &[Vec<String>],
    lines: &[usize],
    pred: F,
) -> (usize, Vec<String>) {
    let head = rows.first().cloned().unwrap_or_default();
    let mut total = 0usize;
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate().skip(1) {
        for (c, v) in r.iter().enumerate() {
            if !pred(v) {
                continue;
            }
            total += 1;
            if out.len() >= MAX_SAMPLES {
                continue;
            }
            let name = head.get(c).map(|s| s.trim()).unwrap_or("");
            let label = if name.is_empty() {
                format!("第 {} 列", c + 1)
            } else {
                format!("「{name}」")
            };
            // 行号取物理行号：引号里有换行时，记录序号和用户看到的行号不是一回事
            let line = lines.get(i).copied().unwrap_or(i + 1);
            out.push(format!("第 {line} 行 {label}：{}", clip(v)));
        }
    }
    (total, out)
}

/// 全角数字 ０-９（U+FF10–U+FF19）。
///
/// 为什么不写 `c.is_numeric() && c.to_digit(10).is_none()`：
/// 那会把「壹贰叁」「一二三」这类中文数字也算进来，用户会收到一堆没用的告警。
/// 只认全角数字，判据窄但准。
fn has_fullwidth_digit(s: &str) -> bool {
    s.chars().any(|c| ('\u{FF10}'..='\u{FF19}').contains(&c))
}

/// 等价于正则 `^\d{15,}$`（本项目不引入 regex 依赖，这点判断手写就够）。
fn is_long_number(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 15 && t.bytes().all(|b| b.is_ascii_digit())
}

/// 样例串太长会把界面撑坏
fn clip(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= 40 {
        return t.to_string();
    }
    let mut out: String = t.chars().take(40).collect();
    out.push('…');
    out
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 每个测试用自己独立的目录：cargo 是并行跑测试的，共用目录会互相踩
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deskbase-csv-{name}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_temp(dir: &Path, file: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(file);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// 走一层函数调用，避免 `invalid_from_utf8` lint 在编译期就把字面量判掉 ——
    /// "这段字节不是合法 UTF-8"正是下面要断言的事，不该被 lint 当成笔误。
    fn is_valid_utf8(b: &[u8]) -> bool {
        std::str::from_utf8(b).is_ok()
    }

    // ---------------- 编码探测 ----------------

    #[test]
    fn 编码探测_合法utf8的中文() {
        // 中文 UTF-8 字节 → 严格校验必然通过
        let s = "姓名,金额\n张三,100\n";
        assert_eq!(detect_encoding(s.as_bytes()), Encoding::Utf8);
        // 纯 ASCII：两种解释结果相同，判成 UTF-8 即可
        assert_eq!(detect_encoding(b"id,name\n1,x\n"), Encoding::Utf8);
    }

    #[test]
    fn 编码探测_非法的gbk中文() {
        // "中文" 的 GBK 是 D6 D0 CE C4 —— 这不是合法 UTF-8（第一步就失败）
        let gbk = b"\xD6\xD0\xCE\xC4";
        assert!(
            !is_valid_utf8(gbk),
            "前提：GBK 的中文双字节序列不该是合法 UTF-8"
        );
        assert_eq!(detect_encoding(gbk), Encoding::Gb18030);

        // 整行 GBK：中文只出现在值里，列名是 ASCII
        let line = b"name,note\n\xD6\xD0\xCE\xC4,GBK\n";
        assert_eq!(detect_encoding(line), Encoding::Gb18030);
    }

    #[test]
    fn 编码探测_四种编码与bom() {
        // UTF-8 BOM
        assert_eq!(detect_encoding(b"\xEF\xBB\xBFid\n1\n"), Encoding::Utf8Bom);
        // UTF-16LE BOM（Excel「Unicode 文本」的格式：'i' = 69 00）
        assert_eq!(detect_encoding(b"\xFF\xFEi\x00d\x00\n\x00"), Encoding::Utf16Le);
        // UTF-16BE BOM
        assert_eq!(detect_encoding(b"\xFE\xFF\x00i\x00d\x00\n"), Encoding::Utf16Be);
        // 空文件不算有效编码
        assert_eq!(detect_encoding(b""), Encoding::Utf8);
    }

    #[test]
    fn 没有bom的utf16靠nul分布认出来() {
        // 纯 ASCII 的 UTF-16LE 文件整个都是合法 UTF-8（0x00 是合法 UTF-8 字符），
        // 如果在 UTF-8 校验之后才看 NUL，这种文件会被误判成 UTF-8
        let le: Vec<u8> = "id,name\r\n1,a\r\n".bytes().flat_map(|b| [b, 0]).collect();
        assert_eq!(detect_encoding(&le), Encoding::Utf16Le);
        let be: Vec<u8> = "id,name\r\n1,a\r\n".bytes().flat_map(|b| [0, b]).collect();
        assert_eq!(detect_encoding(&be), Encoding::Utf16Be);
        // 两边都是 NUL 的是二进制，不是文本
        assert_eq!(detect_encoding(b"\x00\x00\x00\x00\x01\x00\x02\x00"), Encoding::Unknown);
    }

    #[test]
    fn 空的bom不能留在第一个字段里() {
        let d = tmp("bom");
        let p = write_temp(&d, "bom-utf8.csv", "\u{FEFF}姓名,金额\n张三,100\n".as_bytes());

        let r = inspect(&p).unwrap();
        assert_eq!(r.encoding, "UTF-8");
        assert!(r.encoding_confidence.contains("BOM"), "应说明是靠 BOM 判的");
        assert_eq!(r.head[0][0], "姓名", "BOM 必须被切掉，不能混进第一个字段");
        assert_eq!(r.cols, 2);

        // UTF-16LE 带 BOM 的也要能读，且 BOM 同样不能留下
        let le: Vec<u8> = "\u{FEFF}姓名,金额\n张三,100\n"
            .chars()
            .flat_map(|c| {
                let mut b = [0u16; 2];
                c.encode_utf16(&mut b).iter().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>()
            })
            .collect();
        let p2 = write_temp(&d, "bom-utf16.csv", &le);
        let r2 = inspect(&p2).unwrap();
        assert_eq!(r2.encoding, "UTF-16LE");
        assert_eq!(r2.head[0][0], "姓名", "UTF-16 的 BOM 也要切掉");
        assert_eq!(r2.head[1][0], "张三");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn gb18030文件能正确解码出中文() {
        let d = tmp("gbk");
        // 列名 ASCII、值 GBK 中文 —— 正是中文 Excel「另存为 CSV」的样子
        let p = write_temp(&d, "gbk.csv", b"name,note\n\xD6\xD0\xCE\xC4,GBK\n");

        let r = inspect(&p).unwrap();
        assert_eq!(r.encoding, "GB18030");
        assert_eq!(r.head[1][0], "中文", "GBK 中文必须被正确解码");
        assert_eq!(r.head[1][1], "GBK");
        assert!(
            !r.warnings.iter().any(|w| w.kind == "replacement_char"),
            "正常 GBK 文件不该出现替换字符告警"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 解码不了的字节会报替换字符() {
        let d = tmp("fffd");
        // 0xFF 0xFE 之后按 GB18030 是解不出东西的（这里没有 BOM，走 GB18030 分支）
        let p = write_temp(&d, "bad.csv", b"id,name\n1,\xFF\xFF\xFF\n");
        let r = inspect(&p).unwrap();
        let w = r
            .warnings
            .iter()
            .find(|w| w.kind == "replacement_char")
            .expect("产生了替换字符就必须告警");
        assert!(w.count > 0);
        assert!(w.advice.contains("手动指定编码"), "告警要给出可执行的下一步");
        std::fs::remove_dir_all(&d).ok();
    }

    // ---------------- 分隔符探测 ----------------

    #[test]
    fn 分隔符探测_逗号制表符分号() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3\n"), ',');
        assert_eq!(detect_delimiter("a\tb\tc\n1\t2\t3\n"), '\t');
        assert_eq!(detect_delimiter("a;b;c\n1;2;3\n"), ';');
        assert_eq!(detect_delimiter("a|b\n1|2\n"), '|');
    }

    #[test]
    fn 地址里的逗号不会把制表符文件带偏() {
        // 干扰案例：真正的分隔符是 Tab，但每一行的中文地址里都有逗号
        let tsv = "姓名\t地址\t金额\n\
                   张三\t北京市朝阳区,安贞路1号\t100\n\
                   李四\t上海市浦东新区,世纪大道2号\t200\n\
                   王五\t广州市天河区,体育西路3号\t300\n";
        assert_eq!(
            detect_delimiter(tsv),
            '\t',
            "按出现次数数的话逗号会赢，按字段数一致性判才判得对"
        );

        // 反向：真正的分隔符是逗号，地址里带分号
        let csv = "姓名,地址\n张三,朝阳区;安贞路\n李四,浦东新区;世纪大道\n";
        assert_eq!(detect_delimiter(csv), ',');
    }

    #[test]
    fn 单列文件不会硬凑出一个分隔符() {
        let (d, found) = detect_delimiter_ex("名称\n中文\n英文\n");
        assert!(!found, "没有任何分隔符时应当承认探测失败");
        assert_eq!(d, ',');

        let dir = tmp("onecol");
        let p = write_temp(&dir, "one.csv", "名称\n中文\n".as_bytes());
        let r = inspect(&p).unwrap();
        assert_eq!(r.cols, 1);
        assert!(r.warnings.iter().any(|w| w.kind == "single_column"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- 引号规则 ----------------

    #[test]
    fn 引号里的逗号和换行都是字段内容() {
        let text = "id,addr,note\n1,\"北京市,朝阳区\",ok\n2,\"第一行\n第二行\",ok\n";
        let rows = parse(text, ',');
        assert_eq!(rows.len(), 3, "引号里的换行不能把一条记录劈成两条");
        assert_eq!(rows[1][1], "北京市,朝阳区");
        assert_eq!(rows[2][1], "第一行\n第二行");
        assert_eq!(rows[2][2], "ok", "跨行字段之后的列不能错位");
    }

    #[test]
    fn 两个连续引号是一个字面量引号() {
        let rows = parse("id,note\n1,\"他说\"\"你好\"\"\"\n", ',');
        assert_eq!(rows[1][1], "他说\"你好\"");
        // 引号内的分隔符和 `""` 一起出现时也不能乱
        let rows2 = parse("a,b\n\"x,\"\"y\",z\n", ',');
        assert_eq!(rows2[1][0], "x,\"y");
        assert_eq!(rows2[1][1], "z");
    }

    #[test]
    fn 引号前有空格也当作字段开始() {
        // RFC 严格来说这里引号是字面量，但真实导出大量这么写。
        // 当成字面量的话那个逗号会被算成分隔符，从此整表错位 —— 后果严重得多。
        let rows = parse("a,b\n1, \"x,y\"\n", ',');
        assert_eq!(rows[1].len(), 2);
        assert_eq!(rows[1][1], "x,y");
    }

    // ---------------- 结构异常 ----------------

    #[test]
    fn 列数不一致的行会被计数并告警() {
        let d = tmp("ragged");
        let p = write_temp(
            &d,
            "ragged.csv",
            "客户,地址,金额\n张三,北京市朝阳区,100\n李四,上海市,浦东新区,200\n王五,广州市,300\n"
                .as_bytes(),
        );
        let r = inspect(&p).unwrap();
        let w = r
            .warnings
            .iter()
            .find(|w| w.kind == "ragged_rows")
            .expect("列数不一致必须告警");
        assert_eq!(w.count, 1, "只有第 3 行是 4 列");
        assert!(w.samples[0].contains("第 3 行"), "样例必须带行号：{:?}", w.samples);
        assert!(w.samples[0].contains("表头 3 列"), "样例要说明表头是几列");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 空行被跳过并计数() {
        let text = "a,b\n1,2\n\n\n3,4\n";
        let rows = parse(text, ',');
        assert_eq!(rows.len(), 3, "两条空行不能变成两条空记录");

        let d = tmp("blank");
        let p = write_temp(&d, "blank.csv", text.as_bytes());
        let r = inspect(&p).unwrap();
        assert_eq!(r.rows, 3);
        let w = r.warnings.iter().find(|w| w.kind == "empty_lines").unwrap();
        assert_eq!(w.count, 2);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 行尾多余分隔符与整列为空都被点名() {
        let d = tmp("trailing");
        let p = write_temp(
            &d,
            "t.csv",
            "客户,电话,备注\n张三,138,,\n李四,,,\n".as_bytes(),
        );
        let r = inspect(&p).unwrap();
        let t = r.warnings.iter().find(|w| w.kind == "trailing_delimiter").unwrap();
        assert_eq!(t.count, 2);
        let e = r.warnings.iter().find(|w| w.kind == "empty_column").unwrap();
        assert!(e.samples.iter().any(|s| s.contains("备注")), "空列要点出列名");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 全角数字被检测出来() {
        let d = tmp("fullwidth");
        let p = write_temp(&d, "fw.csv", "客户,金额\n张三,１２３.４５\n李四,200\n".as_bytes());
        let r = inspect(&p).unwrap();
        let w = r
            .warnings
            .iter()
            .find(|w| w.kind == "fullwidth_digits")
            .expect("全角数字必须告警");
        assert_eq!(w.count, 1);
        assert!(w.samples[0].contains("１２３"), "样例要带上原文：{:?}", w.samples);

        // 前提确认：全角数字 is_numeric 为真但 to_digit 拿不到值 —— 这就是它静默变文本的原因
        assert!('１'.is_numeric());
        assert_eq!('１'.to_digit(10), None);
        // 中文数字不该被误报（判据只认全角数字）
        assert!(!has_fullwidth_digit("一二三壹贰叁"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 长编号在编号列里被点名() {
        let d = tmp("longid");
        let p = write_temp(
            &d,
            "id.csv",
            "订单编号,金额\n110101199003072587,100\n1234,200\n".as_bytes(),
        );
        let r = inspect(&p).unwrap();
        let w = r
            .warnings
            .iter()
            .find(|w| w.kind == "long_number_id")
            .expect("18 位订单号必须告警");
        assert!(w.samples[0].contains("110101199003072587"));
        assert!(
            w.advice.contains("Excel"),
            "要讲清楚危险来自「用 Excel 打开」：{}",
            w.advice
        );
        std::fs::remove_dir_all(&d).ok();
    }

    // ---------------- 闸门 ----------------

    #[test]
    fn 文件过大与行数过多都被拒绝() {
        // 500 MB 这条线单独测；真造一个 500 MB 文件来测太蠢了
        assert!(check_size(MAX_FILE_BYTES).is_ok());
        let err = check_size(600 * 1024 * 1024).unwrap_err();
        assert!(err.contains("超过单次导入上限"), "实际：{err}");
        assert!(err.contains("拆"), "要给出可执行建议：{err}");

        // 行数上限是在解析里卡的：不卡的话 500 MB 的 "a\n" × 2.5 亿行会直接 OOM
        let text = "a\n1\n2\n3\n4\n";
        let (rows, stats) = parse_limited(text, ',', 3);
        assert!(stats.over_limit, "超过 3 行就该中止");
        assert!(rows.len() <= 4, "中止要早，不能把整个文件解析完再报错");

        // inspect 里走的是同一条闸门
        let d = tmp("rows");
        let big = "a,b\n".to_string() + &"1,2\n".repeat(MAX_ROWS + 1);
        let p = write_temp(&d, "big.csv", big.as_bytes());
        let err = inspect(&p).unwrap_err();
        assert!(err.contains("500000"), "错误信息要说清上限：{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn 报告的形状符合界面预期() {
        let d = tmp("shape");
        let mut text = String::from("客户,金额\n");
        for i in 0..20 {
            text.push_str(&format!("客户{i},{i}\n"));
        }
        let p = write_temp(&d, "shape.csv", text.as_bytes());

        let r = inspect(&p).unwrap();
        assert_eq!(r.rows, 21, "含表头一起数");
        assert_eq!(r.cols, 2);
        assert_eq!(r.head.len(), PREVIEW_ROWS, "预览只要前 8 行");
        assert_eq!(r.delimiter, "逗号 (,)");
        assert_eq!(r.encoding, "UTF-8");
        assert!(r.warnings.is_empty(), "干净的文件不该有告警：{:?}", r.warnings);

        // 能不能序列化给前端（IPC 走 JSON）
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"encoding\":\"UTF-8\""));
        std::fs::remove_dir_all(&d).ok();
    }
}
