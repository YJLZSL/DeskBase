//! 用户库表：结构、DDL、分页与单元格读写（见 docs/06 §2.2 / §2.3 / §5 / §9 / §11）
//!
//! 这一层存在的理由：**渲染层只会给字符串，而 SQLite 的参数绑定不能用于标识符**。
//! 所以每个表名/字段名都必须先过 [`validate_identifier`]，再一律用双引号包起来拼进 SQL；
//! 值则永远走参数绑定，绝不拼字符串（docs/06 §11 的第一条红线）。
//!
//! 四条贯穿全模块的约定（都是踩过的坑，不是风格偏好）：
//!
//! 1. **一切靠 rowid**。翻页、改单元格、删行都按 `rowid` 定位，因此
//!    `WITHOUT ROWID` 表被明确拒绝，字段名也不允许叫 `rowid`/`_rowid_`/`oid`
//!    —— 它们会遮蔽 SQLite 的内部行号，遮蔽之后的"编辑"会改到别的行上去。
//!    [`Page::rows`] 的每一行第 0 个值就是 `_rowid`（见 [`ROWID_COLUMN`]），
//!    界面拿它当行标识，不要显示给用户。
//! 2. **翻页用 keyset，不用 OFFSET**。`OFFSET` 是 O(N²)，大表实测冷缓存几十秒；
//!    而且排序键有并列值时 OFFSET 会漏行/重复行 —— 那是数据正确性问题，不只是慢。
//!    见 [`page_rows`]。
//! 3. **不数总数**。`COUNT(*)` 在大表上要全表扫；统计信息不存表元数据是 SQLite
//!    刻意的设计决定。要显示行数就用 [`TableInfo::row_estimate`]（估计值，界面要标"约"）。
//! 4. **应用自己的表一律 `_db_` 前缀**，且不出现在 [`list_tables`] 里，跟用户的业务表
//!    分开（docs/adr/0012 的结论：同库同文件、靠前缀分命名空间）。
//!
//! 本模块**不记录任何 SQL 日志**（docs/06 §9.3：查询日志默认关闭，记录执行的 SQL
//! 可能泄露业务信息）；审计由上层按需接入。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value as Json;

/// 本模块的错误一律是「给人看的中文」，不需要在界面层再翻译一遍。
/// 与 `db.rs` 的 `Result` 保持一致，接线时不用做类型转换。
pub type Result<T> = std::result::Result<T, String>;

// ---------------- 常量 ----------------

/// SQLite 内部表（`sqlite_sequence`、`sqlite_stat1`…）的后缀：用户表列表里要排除。
const SQLITE_PREFIX: &str = "sqlite_";
/// 应用自己的元数据表前缀。用户表不许用这个前缀，否则会被列表隐藏（等于数据"消失"）。
const META_PREFIX: &str = "_db_";
/// 表注释（表级）。
const TABLE_COMMENT_TABLE: &str = "_db_table_comment";
/// 字段注释 + 字段语义类型。
const COLUMN_COMMENT_TABLE: &str = "_db_column_comment";
/// 内置行号的表达式。用 `_rowid_` 而不是 `rowid`：字段名叫 `rowid` 的（外部建的表）
/// 会遮蔽 `rowid`，但不会遮蔽 `_rowid_`，这样能少一类坑。
const ROWID_EXPR: &str = "_rowid_";
/// [`Page::columns`] 的第一列名。**这是对界面的约定**：第 0 列永远是行号。
pub const ROWID_COLUMN: &str = "_rowid";
/// 标识符长度上限。SQLite 自己不限长，这是给界面的合理上限（也为避免误粘一整段文本当表名）。
const MAX_IDENT_CHARS: usize = 64;
/// 单页行数上限。界面一次要再多也没有意义，而且会把 IPC 和渲染拖死。
pub const MAX_PAGE_LIMIT: usize = 10_000;
/// `DEFAULT` 子句长度上限（DEFAULT 不能被参数化绑定，长文本本身就是可疑信号）。
const MAX_DEFAULT_CHARS: usize = 200;
/// 金额的小数位：人民币最小单位是分（docs/06 §2.3 把"金额"归到小数类型下，
/// 但 SQLite 没有真正的十进制类型，见 [`ColType::declared_type`] 的说明）。
pub const MONEY_SCALE: u32 = 2;
/// 一次 `DELETE ... WHERE _rowid_ IN (...)` 的批量大小。
/// 为什么要分批：SQLite 的参数个数有上限（默认 32766），而且一条 SQL 里几百个占位符
/// 之后的解析成本开始明显。
const DELETE_CHUNK: usize = 400;

// ---------------- 字段类型 ----------------

/// 字段类型。对应用户能理解的语义，不是 SQLite 的存储类。
///
/// 与 docs/06 §2.3 的映射（**照文档来，不自己发明**）：
///
/// | 本枚举 | 界面中文 | 声明类型 |
/// |-------|---------|---------|
/// | `Text` | 文本 / 长文本 | `TEXT` |
/// | `Integer` | 数字（整数） | `INTEGER` |
/// | `Real` | 数字（小数） | `REAL` |
/// | `Money` | 金额 | `INTEGER`（**单位：分**，见下） |
/// | `Boolean` | 是/否 | `BOOLEAN`（存 0/1） |
/// | `Date` | 日期 | `DATE`（存 `YYYY-MM-DD` 文本） |
/// | `DateTime` | 日期时间 | `DATETIME`（存 `YYYY-MM-DD HH:MM:SS` 文本） |
/// | `Json` | JSON（进阶） | `TEXT` |
/// | `Blob` | 二进制（进阶） | `BLOB` |
///
/// 需要说明的两处偏离，都在注释里写清理由：
///   - **`Money` 不用 `REAL`**：SQLite 的 `NUMERIC`/`DECIMAL` 没有真正的十进制语义，
///     小数会落到 IEEE-754 双精度上（0.1+0.2≠0.3），账目上不能接受。所以金额按
///     **最小单位（分）的整数**存：比较、求和都是精确整数运算，排序也正确。
///     为了不让"元"悄悄变成"分"，建表时会给金额列加一条 `CHECK (typeof(...) IN
///     ('integer','null'))` —— 谁（包括界面之外的 SQL）想往金额列写小数，都会当场报错，
///     而不是把错误数据记进账里。原值/显示值的换算只用 [`money_parse`] / [`money_display`]。
///   - **`Json` 用 `TEXT`**：SQLite 里 `JSON` 这个类型名只有 NUMERIC 亲和性，没有任何
///     JSON 语义，反而会把 `123` 这类合法 JSON 文本转成整数。声明成 `TEXT` 才是"原样存
///     字符串"，JSON 合法性由写入时的校验负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColType {
    Text,
    Integer,
    Real,
    Money,
    Boolean,
    Date,
    DateTime,
    Json,
    Blob,
}

impl ColType {
    /// 拼进 `CREATE TABLE` 的声明类型。
    pub fn declared_type(self) -> &'static str {
        match self {
            ColType::Text => "TEXT",
            ColType::Integer => "INTEGER",
            ColType::Real => "REAL",
            // 为什么不是 REAL：见 ColType 的文档
            ColType::Money => "INTEGER",
            // SQLite 没有布尔类型，声明 BOOLEAN 是为了让第三方工具与导出的 DDL 看得出来意图，
            // 值一律写 0/1 整数
            ColType::Boolean => "BOOLEAN",
            ColType::Date => "DATE",
            ColType::DateTime => "DATETIME",
            ColType::Json => "TEXT",
            ColType::Blob => "BLOB",
        }
    }

    /// 从声明类型反推语义类型，只用于**不是本工具建的表**（导入的 DDL、老库）。
    ///
    /// 为什么要反推：这些表没有 `_db_column_comment` 里的语义记录，但我们仍然希望
    /// 界面上的编辑有基本的类型校验（比如往整数列里写中文要报错，而不是存成一串文本）。
    /// 认不出来的类型**不做任何强转** —— 猜别人的列语义比不猜更危险。
    /// 特别是外部建的 `MONEY` 列：它很可能存的是"元"的浮点，我们不去猜，原样存。
    fn from_declared(decl: &str) -> Option<ColType> {
        let head = decl
            .trim()
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match head.as_str() {
            "TEXT" | "VARCHAR" | "NVARCHAR" | "CHAR" | "NCHAR" | "CLOB" | "STRING" => {
                Some(ColType::Text)
            }
            "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" | "MEDIUMINT" | "INT2"
            | "INT8" => Some(ColType::Integer),
            "REAL" | "DOUBLE" | "FLOAT" | "NUMERIC" | "DECIMAL" => Some(ColType::Real),
            "BOOLEAN" | "BOOL" => Some(ColType::Boolean),
            "DATE" => Some(ColType::Date),
            "DATETIME" | "TIMESTAMP" => Some(ColType::DateTime),
            "JSON" | "JSONB" => Some(ColType::Json),
            "BLOB" | "BINARY" | "VARBINARY" => Some(ColType::Blob),
            _ => None,
        }
    }
}

/// 建表/加列时的字段定义（建表向导第 2 步的产物）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColType,
    pub not_null: bool,
    /// **原样的 SQL 片段**，不是值：`0`、`''`、`NULL`、`CURRENT_TIMESTAMP` …
    /// 为什么是字符串：`DEFAULT` 子句不允许绑定参数，只能拼进 DDL。
    /// 所以这里按白名单校验（见 `normalize_default`）—— 它是一条真实的注入面。
    pub default: Option<String>,
    pub primary_key: bool,
    /// 中文注释：给用户看的字段说明（docs/06 §2.2「表注释」）。
    pub comment: Option<String>,
}

/// 一张表的完整定义（建表向导第 1–3 步的产物）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableSpec {
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ColumnDef>,
}

/// 读回来的字段信息（`PRAGMA table_info` 的样子）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// 声明类型原文。可能是空串（外部建的无类型列）。
    pub decl_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub pk: bool,
}

/// 字段的附加元数据（注释 + 语义类型）。`ColumnInfo` 只描述 SQLite 认识的模式，
/// 这里是本工具额外记住的东西：注释，以及声明类型表达不出来的语义（金额、JSON）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ColumnMeta {
    pub name: String,
    pub comment: Option<String>,
    /// 语义类型。已建的表按注释表里记的来；老库按声明类型反推（可能为 None）。
    pub semantic: Option<ColType>,
}

/// 表信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TableInfo {
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ColumnInfo>,
    /// **行数估计值，不是精确值**。来源见 `row_estimate`。
    /// 界面显示时必须带"约"字；`-1` 表示无法估计（要显示"未知"而不是 0）。
    pub row_estimate: i64,
}

/// 一页数据。
///
/// `columns[0]` 恒为 [`ROWID_COLUMN`]，`rows[i][0]` 就是对应该行的 rowid
/// —— 界面要用它调 [`update_cell`] / [`delete_rows`]。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub rows: Vec<Vec<Json>>,
    pub columns: Vec<String>,
    pub has_more: bool,
    /// 不透明游标：界面原样回传给下一次调用即可，不要解析、不要自己拼。
    /// 最后一页为 `None`。
    pub next_cursor: Option<String>,
}

/// 查询结果。`columns` 为空表示这是一条不返回结果集的写操作，
/// 影响行数看 `affected`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
    pub truncated: bool,
    pub elapsed_ms: i64,
    pub affected: usize,
}

// ---------------- 标识符 ----------------

/// 校验表名/字段名/列名。合法：字母、数字、下划线、中文（中文属 Unicode 字母），
/// 不能以数字开头，不能超过 [`MAX_IDENT_CHARS`] 个字符，不能为空。
///
/// 为什么必须有这一层：SQLite 的参数绑定**不能**用在标识符上，表名/字段名只能拼进
/// SQL 字符串（`PRAGMA table_info("x")` 连引号都不能省）。于是"校验 + 转义"是唯一
/// 可行的防线：这里挡掉引号、分号、注释符等一切能改变语句结构的字符。
pub fn validate_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err("名字不能为空".to_string());
    }
    let n = name.chars().count();
    if n > MAX_IDENT_CHARS {
        return Err(format!("名字太长了（{n} 个字符，最多 {MAX_IDENT_CHARS} 个）"));
    }
    if name.chars().next().is_some_and(|c| c.is_numeric()) {
        return Err(format!("「{name}」不能以数字开头"));
    }
    for c in name.chars() {
        if c == '_' || c.is_alphanumeric() {
            continue;
        }
        return Err(format!(
            "「{name}」里有不允许的字符「{c}」：表名与字段名只能用字母、数字、下划线、中文"
        ));
    }
    Ok(())
}

/// 字段名的额外限制：不许叫 `rowid` / `_rowid_` / `oid`。
///
/// 这三个名字在 SQLite 里是**内部行号的别名**，用户拿它当字段名会遮蔽真正的 rowid，
/// 之后按 rowid 定位的分页与编辑就会改到别的行上去 —— 那是静默的数据损坏。
fn validate_column_name(name: &str) -> Result<()> {
    validate_identifier(name)?;
    let lower = name.to_lowercase();
    if matches!(lower.as_str(), "rowid" | "_rowid_" | "oid" | ROWID_COLUMN) {
        return Err(format!(
            "「{name}」是 SQLite 的内部行号名，不能当字段名（本工具靠行号分页与定位记录）"
        ));
    }
    Ok(())
}

/// 用户表名的额外限制：不许用 `_db_`（应用元数据）与 `sqlite_`（SQLite 保留）前缀。
fn validate_user_table_name(name: &str) -> Result<()> {
    validate_identifier(name)?;
    let lower = name.to_lowercase();
    if lower.starts_with(META_PREFIX) {
        return Err(format!(
            "「{name}」用了保留前缀 {META_PREFIX}（应用自己的元数据表用），请换一个名字"
        ));
    }
    if lower.starts_with(SQLITE_PREFIX) {
        return Err(format!("「{name}」用了 SQLite 保留前缀 {SQLITE_PREFIX}，请换一个名字"));
    }
    Ok(())
}

/// 把标识符包成 `"名字"`，内部的 `"` 转义成两个。
///
/// 校验已经挡掉了 `"`，这里仍然转义是"纵深防御"：将来若有人放宽校验，
/// 拼接处不会立刻变成注入点。
fn quote_ident(name: &str) -> String {
    let mut s = String::with_capacity(name.len() + 2);
    s.push('"');
    for c in name.chars() {
        if c == '"' {
            s.push('"');
        }
        s.push(c);
    }
    s.push('"');
    s
}

// ---------------- 金额：分 <-> 元 ----------------

/// 中文输入法下用户很容易打出全角数字/符号，直接当"非法输入"拒绝太不友好。
fn to_halfwidth(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '０'..='９' => {
                char::from_u32('0' as u32 + (c as u32 - '０' as u32)).unwrap_or(c)
            }
            '．' => '.',
            '－' => '-',
            '＋' => '+',
            '：' => ':',
            '　' => ' ',
            _ => c,
        })
        .collect()
}

/// 数字类输入：半角化之后，再去掉货币符号、千分位与空白
/// （从网页、Excel、聊天记录里粘出来的数字常带这些）。
fn normalize_numeric_input(raw: &str) -> String {
    to_halfwidth(raw)
        .chars()
        .filter(|c| !matches!(c, '¥' | '￥' | '$' | ',' | '，' | ' '))
        .collect()
}

/// 把用户输入的金额（元）解析成最小单位（分）。
///
/// 为什么对外暴露：界面要按同一套规则把"分"显示回"元"，两处各写一遍必然漂移，
/// 所以换算只有这一份实现。
///
/// 接受：`12.34`、`-0.05`、`¥1,234.50`、`12元`、全角数字、`.5`（= 0.50）。
/// 拒绝：超过 [`MONEY_SCALE`] 位小数（**不四舍五入**）、非数字、超出 i64 范围。
/// 为什么超小数位要拒绝而不是四舍五入：静默改动金额比报错严重得多，
/// docs/06 §5.2 的处理方式也是"校验失败就提示"，不是"悄悄帮用户改"。
pub fn money_parse(raw: &str) -> Result<i64> {
    let norm = normalize_numeric_input(raw);
    let s = norm.strip_suffix('元').unwrap_or(&norm).trim();
    if s.is_empty() {
        return Err("金额不能为空".to_string());
    }
    let (neg, body) = if let Some(r) = s.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else {
        (false, s)
    };
    let (int_s, frac_s) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if int_s.is_empty() && frac_s.is_empty() {
        return Err(format!("「{raw}」不是金额"));
    }
    if !int_s.bytes().all(|b| b.is_ascii_digit()) || !frac_s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("「{raw}」不是金额（只能是数字，可带正负号与小数点）"));
    }
    if frac_s.len() > MONEY_SCALE as usize {
        return Err(format!(
            "金额「{raw}」有 {} 位小数，超过 {MONEY_SCALE} 位（金额按分记账，不能存下厘或更小）",
            frac_s.len()
        ));
    }
    // 用 i128 中间计算：避免 "92233720368547758.07" 这类边界值在乘法时溢出
    let int_v: i128 = if int_s.is_empty() {
        0
    } else {
        int_s
            .parse::<i128>()
            .map_err(|_| format!("金额「{raw}」超出可表示的范围"))?
    };
    let frac_v: i128 = match frac_s.len() {
        0 => 0,
        1 => frac_s.parse::<i128>().unwrap_or(0) * 10,
        _ => frac_s.parse::<i128>().unwrap_or(0),
    };
    let mut cents = int_v
        .checked_mul(100)
        .and_then(|v| v.checked_add(frac_v))
        .ok_or_else(|| format!("金额「{raw}」超出可表示的范围"))?;
    if neg {
        cents = -cents;
    }
    i64::try_from(cents).map_err(|_| format!("金额「{raw}」超出可表示的范围"))
}

/// 把最小单位（分）显示成"元"的字符串，固定两位小数。
///
/// 当前界面（db.js）用同一条规则在 JS 侧做了换算（centsToYuan），
/// Rust 侧暂无调用方。**终止条件**：导入 / 导出管线（xlsx / csv）开始
/// 携带金额列时必须改用本函数，不得在那些模块里再写第二套换算。
#[allow(dead_code)]
pub fn money_display(cents: i64) -> String {
    let neg = cents < 0;
    let v = (cents as i128).abs();
    let s = format!("{}.{:02}", v / 100, v % 100);
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

// ---------------- 值的转换与校验（docs/06 §5.2 数据校验） ----------------

fn col_err(column: &str, e: String) -> String {
    format!("字段「{column}」：{e}")
}

/// 整数：允许 `12`、`12.0`（Excel/CSV 里很常见）、千分位、全角数字。
/// `12.5` 这种真的不是整数的要报错，不能截断。
fn parse_integer(raw: &str) -> Result<i64> {
    let s = normalize_numeric_input(raw);
    if let Ok(v) = s.parse::<i64>() {
        return Ok(v);
    }
    if let Ok(f) = s.parse::<f64>() {
        if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
            return Ok(f as i64);
        }
        return Err(format!("「{raw}」不是整数"));
    }
    Err(format!("「{raw}」不是整数"))
}

/// 小数：拒绝 NaN / Inf —— SQLite 存不下它们（NaN 会被写成 NULL），
/// 与其静默变成 NULL，不如在这里报错。
fn parse_real(raw: &str) -> Result<f64> {
    let s = normalize_numeric_input(raw);
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(f),
        Ok(_) => Err(format!("「{raw}」不是有效数字")),
        Err(_) => Err(format!("「{raw}」不是有效数字")),
    }
}

/// 是/否：统一成 1/0（docs/06 §2.3 的"复选框"）。
/// 中文写法一并接受，因为用户就是在中文界面里填表。
fn parse_bool(raw: &str) -> Result<i64> {
    let t = raw.trim().to_lowercase();
    match t.as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" | "是" | "真" | "对" | "有" | "√" | "✓" => Ok(1),
        "0" | "false" | "f" | "no" | "n" | "off" | "否" | "假" | "错" | "无" | "×" | "✗" => {
            Ok(0)
        }
        _ => Err(format!(
            "「{raw}」不是「是/否」值（可以填：是、否、有、无、true、false、1、0）"
        )),
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn date_num(s: &str) -> Result<i64> {
    let t = s.trim();
    if t.is_empty() {
        return Err("缺少数字".to_string());
    }
    t.parse::<i64>().map_err(|_| format!("「{t}」不是数字"))
}

/// 日期归一化成 `YYYY-MM-DD`（补零）。
///
/// 为什么要归一化：日期列里混着 `2026-9-7` 和 `2026-10-05` 时，字符串排序会把
/// 9 月排在 10 月后面 —— 界面点表头排序就错了。归一化成定长 ISO 之后，
/// 排序、比较、以及第三方工具打开都对。
///
/// 只认明确的数字格式（`2026-9-7` / `2026/9/7` / `2026.9.7` / `2026年9月7日` / `20260907`）；
/// 认不出来就报错让用户确认，不做"猜测性解析"（美式 `9/17/2026`、Excel 序列号
/// 这类要靠导入向导先做映射，见 docs/06 §6.1）。
/// 带时间部分的（Excel 导出常见 `2026/9/7 0:00`）只取日期，因为列类型就是"只有日期"。
fn normalize_date(raw: &str) -> Result<String> {
    let s = to_halfwidth(raw);
    let date_part = s
        .split(|c: char| c == 'T' || c == ' ')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if date_part.is_empty() {
        return Err(format!("「{raw}」不是日期"));
    }
    let (y, m, d) = if date_part.contains('年') || date_part.contains('月') || date_part.contains('日')
    {
        let t = date_part
            .replace('年', "-")
            .replace('月', "-")
            .replace('日', "");
        let parts: Vec<&str> = t.split('-').filter(|p| !p.trim().is_empty()).collect();
        if parts.len() != 3 {
            return Err(format!("「{raw}」不是日期（要写成 2026-09-07 或 2026年9月7日）"));
        }
        (date_num(parts[0])?, date_num(parts[1])?, date_num(parts[2])?)
    } else if date_part.len() == 8 && date_part.bytes().all(|b| b.is_ascii_digit()) {
        (
            date_num(&date_part[0..4])?,
            date_num(&date_part[4..6])?,
            date_num(&date_part[6..8])?,
        )
    } else {
        let parts: Vec<&str> = date_part.split(['-', '/', '.']).collect();
        if parts.len() != 3 {
            return Err(format!("「{raw}」不是日期（要写成 2026-09-07 或 2026/9/7）"));
        }
        (date_num(parts[0])?, date_num(parts[1])?, date_num(parts[2])?)
    };
    if !(1000..=9999).contains(&y) {
        return Err(format!("「{raw}」的年份要写四位，例如 2026"));
    }
    if !(1..=12).contains(&m) {
        return Err(format!("「{raw}」的月份不对"));
    }
    if d < 1 || d > days_in_month(y, m) {
        return Err(format!("「{raw}」这一年的 {m} 月只有 {} 天", days_in_month(y, m)));
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

/// 日期时间归一化成 `YYYY-MM-DD HH:MM:SS`。没写时间就当 00:00:00；
/// 小数秒会被丢掉（办公场景不需要毫秒，保留反而让值变得难以阅读与比较）。
fn normalize_datetime(raw: &str) -> Result<String> {
    let s = to_halfwidth(raw).replace('T', " ");
    let s = s.trim();
    let mut it = s.split_whitespace();
    let date_part = it.next().unwrap_or("");
    let time_part = it.next().unwrap_or("");
    if it.next().is_some() {
        return Err(format!("「{raw}」不是日期时间（要写成 2026-09-07 08:05）"));
    }
    let date = normalize_date(date_part)?;
    if time_part.is_empty() {
        return Ok(format!("{date} 00:00:00"));
    }
    let time_head = time_part.split('.').next().unwrap_or("");
    let parts: Vec<&str> = time_head.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(format!("「{raw}」的时间部分不对（要写成 08:05 或 08:05:30）"));
    }
    let h = date_num(parts[0])?;
    let mi = if parts.len() > 1 { date_num(parts[1])? } else { 0 };
    let se = if parts.len() > 2 { date_num(parts[2])? } else { 0 };
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=59).contains(&se) {
        return Err(format!("「{raw}」的时间超出范围"));
    }
    Ok(format!("{date} {h:02}:{mi:02}:{se:02}"))
}

/// 把界面/导入传来的字符串按列语义转换成一个可绑定的值。
///
/// 约定：
///   - `None`（界面里"没填"）→ `NULL`。
///   - 空串：`Text`/`Json` 保留空串；其余类型视为"清空" → `NULL`
///     （用户在表格里把单元格删空，意思就是"这一项没有值"）。
///   - 类型未知（不是本工具建的表、且声明类型认不出来）→ 原样存字符串，不强转。
///   - `Blob` 一律拒绝：docs/06 §2.3 把二进制列为进阶能力，界面上没有它的表示形式，
///     硬塞一个字符串进去只会得到谁都不认识的字节。
fn coerce_value(ty: Option<ColType>, column: &str, raw: Option<&str>) -> Result<Value> {
    let raw = match raw {
        None => return Ok(Value::Null),
        Some(v) => v,
    };
    let empty = raw.trim().is_empty();
    match ty {
        None | Some(ColType::Text) => Ok(Value::Text(raw.to_string())),
        Some(ColType::Json) => {
            if empty {
                return Ok(Value::Null);
            }
            serde_json::from_str::<Json>(raw)
                .map_err(|e| col_err(column, format!("不是合法的 JSON：{e}")))?;
            // 校验通过后按原文存，不改写用户的排版
            Ok(Value::Text(raw.to_string()))
        }
        Some(ColType::Integer) => {
            if empty {
                Ok(Value::Null)
            } else {
                parse_integer(raw)
                    .map(Value::Integer)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::Real) => {
            if empty {
                Ok(Value::Null)
            } else {
                parse_real(raw)
                    .map(Value::Real)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::Money) => {
            if empty {
                Ok(Value::Null)
            } else {
                money_parse(raw)
                    .map(Value::Integer)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::Boolean) => {
            if empty {
                Ok(Value::Null)
            } else {
                parse_bool(raw)
                    .map(Value::Integer)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::Date) => {
            if empty {
                Ok(Value::Null)
            } else {
                normalize_date(raw)
                    .map(Value::Text)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::DateTime) => {
            if empty {
                Ok(Value::Null)
            } else {
                normalize_datetime(raw)
                    .map(Value::Text)
                    .map_err(|e| col_err(column, e))
            }
        }
        Some(ColType::Blob) => Err(col_err(
            column,
            "是二进制字段，不能在表格里直接编辑（docs/06 §2.3 把二进制列为进阶能力）：\
             请用 SQL 编辑器或导入流程写入"
                .to_string(),
        )),
    }
}

/// 把结果集里的值转成 IPC 能带走的 JSON。
///
/// 三处刻意的处理：
///   - 实数：JSON 表示不了 NaN/Inf（serde_json 会拒绝），转成字符串保留信息，
///     不静默变 NULL —— 变成 NULL 等于悄悄丢数据。
///   - 文本：按 UTF-8 lossy 转。库里的非法字节序列不该让整个结果集取不出来。
///   - 二进制：只给一个长度占位符。把二进制塞进 JSON 会污染整个表格的渲染，
///     真正的二进制查看/导出要走单独的通道（本模块不做）。
fn value_to_json(v: ValueRef<'_>) -> Json {
    match v {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(i) => Json::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Json::Number)
            .unwrap_or_else(|| Json::String(f.to_string())),
        ValueRef::Text(b) => Json::String(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Blob(b) => Json::String(format!("〔二进制 {} 字节〕", b.len())),
    }
}

/// SQLite 的报错翻译：只翻译有明确界面含义的几种，其余原样透出
/// （原文里有表名/字段名，比我们瞎猜更有用；docs/06 §3.3 的"通俗解释"由编辑器层做）。
fn sqlite_msg(e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::MultipleStatement => {
            "一次只能执行一条 SQL 语句（多条请逐条执行）".to_string()
        }
        rusqlite::Error::ExecuteReturnedResults => {
            "该语句会返回数据行，请用查询的方式执行".to_string()
        }
        other => other.to_string(),
    }
}

// ---------------- 元数据表（表/字段注释、字段语义） ----------------

const META_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS _db_table_comment (
    table_name TEXT PRIMARY KEY,
    comment    TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS _db_column_comment (
    table_name  TEXT NOT NULL,
    column_name TEXT NOT NULL,
    comment     TEXT NOT NULL DEFAULT '',
    semantic    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (table_name, column_name)
);
"#;

/// 建元数据表。只在写路径调用（读路径不该因为一个查询就让只读库变成可写）。
fn ensure_meta_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(META_DDL)
        .map_err(|e| format!("建立元数据表失败：{}", sqlite_msg(e)))
}

/// 元数据表是否已经存在。**读路径必须先问这一句**：
/// 老库、只读打开的库、刚 import 进来的库都可能还没有这两张表。
fn meta_tables_ready(conn: &Connection) -> Result<bool> {
    for t in [TABLE_COMMENT_TABLE, COLUMN_COMMENT_TABLE] {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [t],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_msg)?;
        if found.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn upsert_table_comment(conn: &Connection, table: &str, comment: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO _db_table_comment (table_name, comment) VALUES (?1, ?2)
         ON CONFLICT(table_name) DO UPDATE SET comment = excluded.comment",
        params![table, comment],
    )
}

fn upsert_column_meta(
    conn: &Connection,
    table: &str,
    column: &str,
    comment: &str,
    semantic: Option<ColType>,
) -> rusqlite::Result<usize> {
    let sem = semantic
        .map(|t| serde_json::to_string(&t).unwrap_or_default())
        .unwrap_or_default();
    let sem = sem.trim_matches('"').to_string();
    conn.execute(
        "INSERT INTO _db_column_comment (table_name, column_name, comment, semantic)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(table_name, column_name) DO UPDATE SET
             comment = excluded.comment,
             semantic = CASE WHEN excluded.semantic = '' THEN semantic ELSE excluded.semantic END",
        params![table, column, comment, sem],
    )
}

/// 字段的语义类型，只有声明类型表达不出来的才需要记（金额、JSON）。
fn semantic_for(ty: ColType) -> Option<ColType> {
    match ty {
        ColType::Money | ColType::Json => Some(ty),
        _ => None,
    }
}

#[allow(dead_code)]
fn upsert_column_comment(conn: &Connection, table: &str, column: &str, comment: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO _db_column_comment (table_name, column_name, comment, semantic)
         VALUES (?1, ?2, ?3, '')
         ON CONFLICT(table_name, column_name) DO UPDATE SET comment = excluded.comment",
        params![table, column, comment],
    )
}

/// 一次取完所有表注释（`list_tables` 用，避免每张表一次查询）。
fn load_table_comments(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if !meta_tables_ready(conn)? {
        return Ok(out);
    }
    let mut stmt = conn
        .prepare("SELECT table_name, comment FROM _db_table_comment")
        .map_err(sqlite_msg)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(sqlite_msg)?;
    for r in rows {
        let (t, c) = r.map_err(sqlite_msg)?;
        out.insert(t.to_lowercase(), c);
    }
    Ok(out)
}

/// 一张表的字段附加元数据（注释 + 语义类型），按字段名小写索引。
fn load_column_meta(conn: &Connection, table: &str) -> Result<HashMap<String, (String, Option<ColType>)>> {
    let mut out = HashMap::new();
    if !meta_tables_ready(conn)? {
        return Ok(out);
    }
    let mut stmt = conn
        .prepare("SELECT column_name, comment, semantic FROM _db_column_comment WHERE table_name = ?1")
        .map_err(sqlite_msg)?;
    let rows = stmt
        .query_map([table], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(sqlite_msg)?;
    for r in rows {
        let (name, comment, sem) = r.map_err(sqlite_msg)?;
        let sem_ty: Option<ColType> = if sem.trim().is_empty() {
            None
        } else {
            serde_json::from_str::<ColType>(&format!("\"{}\"", sem.trim())).ok()
        };
        out.insert(name.to_lowercase(), (comment, sem_ty));
    }
    Ok(out)
}

fn table_comment(conn: &Connection, table: &str) -> Result<Option<String>> {
    if !meta_tables_ready(conn)? {
        return Ok(None);
    }
    let c: Option<String> = conn
        .query_row(
            "SELECT comment FROM _db_table_comment WHERE table_name = ?1",
            [table],
            |r| r.get(0),
        )
        .optional()
        .map_err(sqlite_msg)?;
    Ok(c.filter(|s| !s.trim().is_empty()))
}

/// 删除一张表的所有元数据。**删表时必须一起删**，否则下次建同名表会继承旧注释，
/// 那是会误导人的（"我新表怎么自带说明"）。
fn drop_table_meta(conn: &Connection, table: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM _db_table_comment WHERE table_name = ?1", [table])?;
    conn.execute("DELETE FROM _db_column_comment WHERE table_name = ?1", [table])?;
    Ok(())
}

// ---------------- 表结构读取 ----------------

/// 对象类型（table/view/index/trigger）+ 库里的真实写法。
/// 表名大小写不敏感，所以查回来的是"库里存的那个名字"，界面显示用它最准。
fn object_type(conn: &Connection, name: &str) -> Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT name, type FROM sqlite_master WHERE name = ?1 COLLATE NOCASE LIMIT 1",
        [name],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(sqlite_msg)
}

/// 表必须存在、必须是表（不是视图/索引/触发器）、且是用户表。
/// 返回库里的真实名字。
fn ensure_user_table(conn: &Connection, name: &str) -> Result<String> {
    match object_type(conn, name)? {
        Some((real, t)) if t == "table" => {
            validate_user_table_name(&real)?;
            Ok(real)
        }
        Some((real, _)) => Err(format!(
            "「{real}」不是表（是视图/索引之类的对象），表结构的操作请选一张表；\
             视图的数据用查询看"
        )),
        None => Err(format!("表「{name}」不存在（可能已被改名或删除）")),
    }
}

/// `PRAGMA table_info` 的结果。注意这里**必须**把表名拼进 SQL ——
/// PRAGMA 的参数不支持绑定，这正是 `validate_identifier` + `quote_ident` 必有的原因。
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql).map_err(sqlite_msg)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ColumnInfo {
                name: r.get(1)?,
                decl_type: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                not_null: r.get::<_, i64>(3)? != 0,
                default: r.get::<_, Option<String>>(4)?,
                pk: r.get::<_, i64>(5)? > 0,
            })
        })
        .map_err(sqlite_msg)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(sqlite_msg)?);
    }
    Ok(out)
}

/// 是不是 `WITHOUT ROWID` 表。用 `PRAGMA table_list`（3.37+）判断，
/// 不去 grep `sqlite_master.sql` 的文本 —— 表名里恰好含这四个词就会误判。
fn is_without_rowid(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_list").map_err(sqlite_msg)?;
    let mut rows = stmt.query([]).map_err(sqlite_msg)?;
    while let Some(r) = rows.next().map_err(sqlite_msg)? {
        let schema: String = r.get(0).map_err(sqlite_msg)?;
        let name: String = r.get(1).map_err(sqlite_msg)?;
        if schema == "main" && name.eq_ignore_ascii_case(table) {
            let wr: i64 = r.get(4).map_err(sqlite_msg)?;
            return Ok(wr != 0);
        }
    }
    Ok(false)
}

/// 分页/编辑前的守门：必须是用户表、必须有可用的 rowid。
///
/// 为什么把"字段名遮蔽 rowid"也挡掉：如果表里有一个叫 `rowid` 的字段，
/// `_rowid_` 仍指向内部行号（这是 SQLite 的规则），但分页读出来的 rowid 与界面
/// 看到的"rowid 字段"不是一回事，混淆成本太高。宁可明确拒绝。
fn ensure_rowid_table(conn: &Connection, table: &str) -> Result<String> {
    let real = ensure_user_table(conn, table)?;
    if is_without_rowid(conn, &real)? {
        return Err(format!(
            "「{real}」是 WITHOUT ROWID 表：分页与单元格编辑都靠 rowid 定位记录，\
             它没有 rowid，本工具无法安全支持（请用 SQL 编辑器操作）"
        ));
    }
    for c in table_columns(conn, &real)? {
        if matches!(c.name.to_lowercase().as_str(), "rowid" | "_rowid_" | "oid") {
            return Err(format!(
                "「{real}」有一个叫「{}」的字段，它遮蔽了 SQLite 的内部行号，\
                 分页与编辑无法安全进行",
                c.name
            ));
        }
    }
    Ok(real)
}

/// 行数**估计**。绝不 `COUNT(*)`（官方不存行数元数据是有意的设计决定，
/// 4 亿行实测几百秒）。
///
/// 顺序：
///   1. `sqlite_stat1` —— 跑过 `ANALYZE` 的话，里面有当时的行数（**可能过期**）；
///   2. `SELECT max(rowid)` —— rowid 的上界，删过行就会偏大（只多不少）；
///   3. `-1` —— 估不出来（`WITHOUT ROWID` 且没 ANALYZE）。界面显示"未知"，别显示 0。
fn row_estimate(conn: &Connection, table: &str) -> i64 {
    if let Some(n) = stat1_rows(conn, table) {
        return n;
    }
    let sql = format!("SELECT max({ROWID_EXPR}) FROM {}", quote_ident(table));
    match conn.query_row(&sql, [], |r| r.get::<_, Option<i64>>(0)) {
        Ok(Some(n)) => n,
        // 空表：max 返回 NULL，0 是准确的
        Ok(None) => 0,
        // WITHOUT ROWID 表取不到 rowid（"no such column"）：给"未知"而不是撒谎说 0
        Err(_) => -1,
    }
}

fn stat1_rows(conn: &Connection, table: &str) -> Option<i64> {
    // sqlite_stat1 只有 ANALYZE 之后才存在；没有这张表就说明没分析过
    conn.query_row(
        "SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 COLLATE NOCASE AND idx IS NULL",
        [table],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|stat| {
        stat.split_whitespace()
            .next()
            .and_then(|n| n.parse::<i64>().ok())
    })
}

/// `DEFAULT` 的安全校验。
///
/// `DEFAULT` 子句**不能绑定参数**，只能把用户的字符串拼进 DDL —— 这是一个真实的
/// 注入面（docs/06 §11 禁止字符串拼接用户输入，这里是唯一没法参数化的例外地方，
/// 所以用白名单把它堵上）。
///
/// 允许：数字字面量、`'文本'`/`"文本"`（成对引号）、`NULL`、`TRUE`/`FALSE`、
/// `CURRENT_DATE`/`CURRENT_TIME`/`CURRENT_TIMESTAMP`、以及括号包起来的表达式
/// （表达式里仍然禁止 `;`、`--`、`/*`、`*/` —— 没有分号就拼不出第二条语句，
/// 没有注释符就吞不掉我们自己写的收尾括号）。
fn normalize_default(raw: &str) -> Result<Option<String>> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("默认值不能是空白（要留空就不要填默认值）".to_string());
    }
    if v.chars().count() > MAX_DEFAULT_CHARS {
        return Err(format!("默认值太长了（最多 {MAX_DEFAULT_CHARS} 个字符）"));
    }
    let upper = v.to_ascii_uppercase();
    // 自动编号：`INTEGER PRIMARY KEY` 本身就是自增的（rowid 别名），
    // 再加 AUTOINCREMENT 只会多一张 sqlite_sequence 表、写得更慢，没有任何收益
    if upper == "AUTOINCREMENT" {
        return Ok(None);
    }
    if upper == "NULL" {
        return Ok(Some("NULL".to_string()));
    }
    if matches!(
        upper.as_str(),
        "TRUE" | "FALSE" | "CURRENT_DATE" | "CURRENT_TIME" | "CURRENT_TIMESTAMP"
    ) {
        return Ok(Some(upper));
    }
    if v.contains(';') || v.contains("--") || v.contains("/*") || v.contains("*/") {
        return Err(format!(
            "默认值「{raw}」里有 SQL 分隔符或注释符号。默认值不能被参数化绑定，只能用字面量，\
             所以这类内容不接受"
        ));
    }
    if is_number_literal(&normalize_numeric_input(v)) {
        return Ok(Some(v.to_string()));
    }
    if whole_quoted(v, '\'') || whole_quoted(v, '"') {
        return Ok(Some(v.to_string()));
    }
    if v.starts_with('(') && v.ends_with(')') && expr_is_safe(v) {
        return Ok(Some(v.to_string()));
    }
    Err(format!(
        "默认值「{raw}」不是能识别的字面量（可用：数字、'文本'、NULL、TRUE/FALSE、\
         CURRENT_TIMESTAMP，或用括号包起来的表达式）"
    ))
}

/// 是不是一个数字字面量（不依赖正则，避免为一个判断引入依赖）。
fn is_number_literal(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - st;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == st {
            return false;
        }
    }
    i == b.len()
}

/// 整串是不是一个完整的引号字面量（内部引号必须成对转义，且正好在末尾闭合）。
fn whole_quoted(s: &str, q: char) -> bool {
    let cs: Vec<char> = s.chars().collect();
    if cs.len() < 2 || cs[0] != q {
        return false;
    }
    let mut i = 1;
    loop {
        if i >= cs.len() {
            return false;
        }
        if cs[i] == q {
            if i == cs.len() - 1 {
                return true;
            }
            if cs[i + 1] == q {
                i += 2;
                continue;
            }
            return false;
        }
        i += 1;
    }
}

/// 括号是否配平、引号是否闭合（配合"禁止分号与注释符"，足以保证它拼出来的
/// 仍然只是一条语句里的一个表达式）。
fn expr_is_safe(s: &str) -> bool {
    let mut depth = 0i32;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\'' | '"' => {
                let q = c;
                let mut closed = false;
                while let Some(c2) = it.next() {
                    if c2 == q {
                        if it.peek() == Some(&q) {
                            it.next();
                        } else {
                            closed = true;
                            break;
                        }
                    }
                }
                if !closed {
                    return false;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// 金额列的默认值：界面按「元」输入，存储按「分」—— 这里做唯一一次换算。
///
/// 为什么不能把 `DEFAULT 12.34` 直接放进 DDL：`column_ddl` 会给金额列钉一条
/// `CHECK (typeof(x) IN ('integer','null'))`，而 `DEFAULT 12.34` 往 INTEGER 列里
/// 写的是 REAL —— **建表能过，第一次插入才报错**，而报错信息完全指不到"默认值"
/// 这三个字上。这类"离现场很远"的报错正是本项目最想避免的。
///
/// 界面把用户填的「元」包成字符串字面量（`'12.34'`）交过来，这里剥壳后交给
/// [`money_parse`] —— 顺带白拿了它全部的校验（超 2 位小数、非数字、超范围）。
fn money_default_to_cents(literal: &str) -> Result<String> {
    let v = literal.trim();
    let upper = v.to_ascii_uppercase();
    // 关键字原样放行：`NULL` 与 `CURRENT_*`（时间戳当金额没意义，但不该由这里拦）
    if upper == "NULL" || upper.starts_with("CURRENT_") {
        return Ok(v.to_string());
    }
    let inner = strip_sql_string_literal(v).unwrap_or(v);
    let cents = money_parse(inner)?;
    Ok(cents.to_string())
}

/// 剥掉 SQL 字符串字面量的外层引号（`'12.34'` → `12.34`），并把 `''` 还原成 `'`。
/// 不是完整的字面量（缺一侧引号、长度不足）时返回 `None`，交给调用方按原样处理。
fn strip_sql_string_literal(v: &str) -> Option<&str> {
    let b = v.as_bytes();
    if b.len() < 2 {
        return None;
    }
    let q = b[0];
    if (q != b'\'' && q != b'"') || b[b.len() - 1] != q {
        return None;
    }
    // 单引号转义在 SQL 里是翻倍写法，这里不展开 —— money_parse 只认数字，
    // 真有人往金额默认值里塞引号，让它在下面报"不是金额"更好。
    Some(&v[1..v.len() - 1])
}

/// 单列的 DDL 片段。
///
/// `inline_pk`：主键列在只有一列主键时写成列级 `PRIMARY KEY`；多列主键走表级约束
/// （SQLite 不允许一张表出现两个列级 `PRIMARY KEY`）。
fn column_ddl(col: &ColumnDef, inline_pk: bool) -> Result<String> {
    validate_column_name(&col.name)?;
    let mut s = format!("{} {}", quote_ident(&col.name), col.ty.declared_type());
    // 主键与"必填"是两件事，这里不替用户合并：
    // SQLite 只有在主键是 INTEGER（rowid 别名）时才隐式非空，其它类型的主键在普通表里
    // **允许 NULL**（历史兼容行为）。所以界面上的「必填」要单独勾选，别指望勾了主键就非空。
    if col.primary_key && inline_pk {
        s.push_str(" PRIMARY KEY");
    }
    if col.not_null {
        s.push_str(" NOT NULL");
    }
    if let Some(d) = &col.default {
        if let Some(d) = normalize_default(d)? {
            // 金额列的默认值按「元」写、按「分」存 —— 与界面录入、Excel 导入
            // 是同一套约定。换算只走 money_parse 这一份实现（D-034）。
            let d = if col.ty == ColType::Money {
                money_default_to_cents(&d)?
            } else {
                d
            };
            s.push_str(" DEFAULT ");
            s.push_str(&d);
        }
    }
    if col.ty == ColType::Money {
        // 金额必须是整数（分）。把这条规则钉在模式层：以后不管谁写数据
        // （界面、导入、SQL 编辑器、第三方工具）写进小数都会当场报错，
        // 而不是把 IEEE-754 的误差记进账里。
        s.push_str(&format!(
            " CONSTRAINT {} CHECK (typeof({}) IN ('integer','null'))",
            quote_ident(&format!("ck_{}_money_int", col.name)),
            quote_ident(&col.name)
        ));
    }
    Ok(s)
}

/// 生成建表 SQL（不执行）。
///
/// 单独暴露的理由是 docs/06 §2.2 的建表向导第 4 步：「预览 DDL → 创建」，
/// 以及 §1 的"可解释"原则 —— 向导建的每一步都要能看到等价的 SQL。
pub fn create_table_sql(spec: &TableSpec) -> Result<String> {
    validate_user_table_name(&spec.name)?;
    if spec.columns.is_empty() {
        return Err("表至少要有一个字段".to_string());
    }
    // 字段名大小写不敏感，重复在这里挡掉，报错比 SQLite 的原文好懂
    let mut seen: HashSet<String> = HashSet::new();
    for c in &spec.columns {
        validate_column_name(&c.name)?;
        if !seen.insert(c.name.to_lowercase()) {
            return Err(format!("字段名「{}」重复了", c.name));
        }
    }
    let pk_count = spec.columns.iter().filter(|c| c.primary_key).count();
    let mut parts: Vec<String> = Vec::with_capacity(spec.columns.len() + 1);
    for c in &spec.columns {
        parts.push(column_ddl(c, pk_count == 1)?);
    }
    if pk_count > 1 {
        let names: Vec<String> = spec
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| quote_ident(&c.name))
            .collect();
        parts.push(format!("PRIMARY KEY ({})", names.join(", ")));
    }
    Ok(format!(
        "CREATE TABLE {} (\n  {}\n);",
        quote_ident(&spec.name),
        parts.join(",\n  ")
    ))
}

// ---------------- 库表管理 ----------------

/// 列出用户表。
///
/// 隐藏两类表：`sqlite_*`（SQLite 自己的：`sqlite_sequence`、`sqlite_stat1`…）与
/// `_db_*`（本工具的元数据）。视图不在这里返回 —— docs/06 §4.2 要求视图单独分组，
/// 那是另一条路径（视图没有 rowid，分页/编辑的语义也不一样）。
pub fn list_tables(conn: &Connection) -> Result<Vec<TableInfo>> {
    let comments = load_table_comments(conn)?;
    let sql = format!(
        "SELECT name FROM sqlite_master
         WHERE type = 'table'
           AND name NOT GLOB '{SQLITE_PREFIX}*'
           AND name NOT GLOB '{META_PREFIX}*'
         ORDER BY name"
    );
    let mut stmt = conn.prepare(&sql).map_err(sqlite_msg)?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sqlite_msg)?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(sqlite_msg)?;
    drop(stmt);

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let columns = table_columns(conn, &name)?;
        out.push(TableInfo {
            comment: comments.get(&name.to_lowercase()).cloned(),
            columns,
            row_estimate: row_estimate(conn, &name),
            name,
        });
    }
    Ok(out)
}

/// 取一张表的结构。表不存在/是个视图都会给可读的中文错误。
pub fn get_table(conn: &Connection, name: &str) -> Result<TableInfo> {
    validate_identifier(name)?;
    let real = match object_type(conn, name)? {
        Some((real, t)) if t == "table" => real,
        Some((real, _)) => {
            return Err(format!(
                "「{real}」不是表（是视图/索引之类的对象）：视图的数据请用查询看"
            ))
        }
        None => return Err(format!("表「{name}」不存在（可能已被改名或删除）")),
    };
    Ok(TableInfo {
        comment: table_comment(conn, &real)?,
        columns: table_columns(conn, &real)?,
        row_estimate: row_estimate(conn, &real),
        name: real,
    })
}

/// 字段的注释与语义类型（界面渲染"金额/JSON"要用语义类型，
/// 光看 `decl_type` 分不出"金额的分"和"普通整数"）。
pub fn column_meta(conn: &Connection, table: &str) -> Result<Vec<ColumnMeta>> {
    validate_identifier(table)?;
    let real = match object_type(conn, table)? {
        Some((real, t)) if t == "table" => real,
        Some((real, _)) => return Err(format!("「{real}」不是表")),
        None => return Err(format!("表「{table}」不存在")),
    };
    let metas = load_column_meta(conn, &real)?;
    let mut out = Vec::new();
    for c in table_columns(conn, &real)? {
        let (comment, sem) = metas
            .get(&c.name.to_lowercase())
            .cloned()
            .unwrap_or((String::new(), None));
        out.push(ColumnMeta {
            semantic: sem.or_else(|| ColType::from_declared(&c.decl_type)),
            comment: if comment.trim().is_empty() {
                None
            } else {
                Some(comment)
            },
            name: c.name,
        });
    }
    Ok(out)
}

/// 建表。
///
/// 不用 `IF NOT EXISTS`：建表是用户的显式动作，同名表存在时必须**明确报错**，
/// 静默跳过会让人以为表建好了。整个建表（DDL + 注释元数据）在一个事务里 ——
/// SQLite 支持事务内 DDL，失败可以整块回滚（docs/06 §2.2「安全改结构」，
/// 结构变更与 `meta_column` 更新必须同一事务，见 ADR-0012）。
///
/// 注意：**本函数不创建快照**。docs/06 §11 要求"改结构先快照"，那需要文件级备份，
/// 属上层职责（`backup` 模块），这里不越权。
pub fn create_table(conn: &Connection, spec: &TableSpec) -> Result<()> {
    let ddl = create_table_sql(spec)?;
    ensure_meta_tables(conn)?;
    if let Some((real, t)) = object_type(conn, &spec.name)? {
        return Err(format!("已经有一个叫「{real}」的{t}了，请换一个名字"));
    }
    let tx = conn.unchecked_transaction().map_err(sqlite_msg)?;
    tx.execute_batch(&ddl)
        .map_err(|e| format!("建表失败：{}", sqlite_msg(e)))?;
    if let Some(c) = &spec.comment {
        if !c.trim().is_empty() {
            upsert_table_comment(&tx, &spec.name, c).map_err(sqlite_msg)?;
        }
    }
    for c in &spec.columns {
        let comment = c.comment.as_deref().unwrap_or("");
        let sem = semantic_for(c.ty);
        if !comment.trim().is_empty() || sem.is_some() {
            upsert_column_meta(&tx, &spec.name, &c.name, comment, sem).map_err(sqlite_msg)?;
        }
    }
    tx.commit().map_err(sqlite_msg)
}

/// 改表名。数据不动，注释元数据跟着搬。
///
/// 为什么必须显式搬注释：SQLite 不知道我们这两张注释表跟用户表有什么关系，
/// `ALTER TABLE ... RENAME` 不会替我们改（ADR-0012 说的"结构变更与元数据
/// 必须同一事务"就是这件事）。
// 【未接线 API】表结构编辑器（改表名 / 加列 / 改注释）的界面尚未实现，
// 以下函数在二进制里暂时没有调用方。按本项目规矩（D-044），豁免必须写明
// 终止条件：**建表向导二期（表结构编辑）接线时，这里一个 allow 都不能留**。
// 它们各自带着完整的校验与测试，删掉等于丢掉已经想清楚的边界条件。

#[allow(dead_code)]
pub fn rename_table(conn: &Connection, from: &str, to: &str) -> Result<()> {
    validate_identifier(from)?;
    validate_user_table_name(to)?;
    let real_from = ensure_user_table(conn, from)?;
    if real_from.eq_ignore_ascii_case(to) {
        return Err("新名字与原名字一样".to_string());
    }
    if let Some((real, t)) = object_type(conn, to)? {
        return Err(format!("已经有一个叫「{real}」的{t}了，请换一个名字"));
    }
    ensure_meta_tables(conn)?;
    let tx = conn.unchecked_transaction().map_err(sqlite_msg)?;
    tx.execute_batch(&format!(
        "ALTER TABLE {} RENAME TO {}",
        quote_ident(&real_from),
        quote_ident(to)
    ))
    .map_err(|e| format!("改名失败：{}", sqlite_msg(e)))?;
    // 目标名字上若有残留元数据（理论上不该有，删表时会清），先删掉再搬，避免主键冲突
    drop_table_meta(&tx, to).map_err(sqlite_msg)?;
    tx.execute(
        "UPDATE _db_table_comment SET table_name = ?1 WHERE table_name = ?2",
        params![to, real_from],
    )
    .map_err(sqlite_msg)?;
    tx.execute(
        "UPDATE _db_column_comment SET table_name = ?1 WHERE table_name = ?2",
        params![to, real_from],
    )
    .map_err(sqlite_msg)?;
    tx.commit().map_err(sqlite_msg)
}

/// 删表。**要求逐字符输入表名做二次确认**（docs/06 §2.2「删除（需输入表名确认）」、
/// §11「破坏性操作必须二次确认并要求输入对象名」）。
///
/// 比较是严格的逐字符相等：不做 trim、不忽略大小写。这道闸的意义就在于"手抄一遍"，
/// 放宽任何一点都会削弱它。多删一张表是不可逆的，而多敲几个字符是可逆的。
pub fn drop_table(conn: &Connection, name: &str, confirm_name: &str) -> Result<()> {
    validate_identifier(name)?;
    if confirm_name != name {
        return Err(format!(
            "确认名不匹配：要删除的是表「{name}」，请在确认框里逐字输入这个表名"
        ));
    }
    let real = ensure_user_table(conn, name)?;
    ensure_meta_tables(conn)?;
    let tx = conn.unchecked_transaction().map_err(sqlite_msg)?;
    tx.execute_batch(&format!("DROP TABLE {}", quote_ident(&real)))
        .map_err(|e| format!("删表失败：{}", sqlite_msg(e)))?;
    drop_table_meta(&tx, &real).map_err(sqlite_msg)?;
    tx.commit().map_err(sqlite_msg)
}

/// 加一列。DDL 与注释元数据同一事务。
///
/// 两处提前拦下的 SQLite 限制（报错原文不好懂，这里给人话）：
///   - 不能加主键列；
///   - 要求非空就必须给默认值（已有的行要用它填值）。
#[allow(dead_code)]
pub fn add_column(conn: &Connection, table: &str, col: &ColumnDef) -> Result<()> {
    validate_identifier(table)?;
    let real = ensure_user_table(conn, table)?;
    validate_column_name(&col.name)?;
    if table_columns(conn, &real)?
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(&col.name))
    {
        return Err(format!("表「{real}」已经有字段「{}」了", col.name));
    }
    if col.primary_key {
        return Err(
            "SQLite 不允许给已有的表加主键列：主键要在建表时定（可以新建一张表再把数据迁过去）"
                .to_string(),
        );
    }
    if col.not_null && col.default.is_none() {
        return Err(
            "新增字段要求非空时必须给一个默认值 —— 表里已有的那些行要用它来填这个字段"
                .to_string(),
        );
    }
    let ddl = format!(
        "ALTER TABLE {} ADD COLUMN {}",
        quote_ident(&real),
        column_ddl(col, false)?
    );
    ensure_meta_tables(conn)?;
    let tx = conn.unchecked_transaction().map_err(sqlite_msg)?;
    tx.execute_batch(&ddl)
        .map_err(|e| format!("加字段失败：{}", sqlite_msg(e)))?;
    let comment = col.comment.as_deref().unwrap_or("");
    let sem = semantic_for(col.ty);
    if !comment.trim().is_empty() || sem.is_some() {
        upsert_column_meta(&tx, &real, &col.name, comment, sem).map_err(sqlite_msg)?;
    }
    tx.commit().map_err(sqlite_msg)
}

/// 改表注释。
#[allow(dead_code)]
pub fn set_table_comment(conn: &Connection, table: &str, comment: &str) -> Result<()> {
    validate_identifier(table)?;
    let real = ensure_user_table(conn, table)?;
    ensure_meta_tables(conn)?;
    upsert_table_comment(conn, &real, comment).map_err(sqlite_msg)?;
    Ok(())
}

/// 改字段注释。
#[allow(dead_code)]
pub fn set_column_comment(conn: &Connection, table: &str, column: &str, comment: &str) -> Result<()> {
    validate_identifier(table)?;
    validate_column_name(column)?;
    let real = ensure_user_table(conn, table)?;
    let cols = table_columns(conn, &real)?;
    let Some(c) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(column)) else {
        return Err(format!("表「{real}」没有字段「{column}」"));
    };
    ensure_meta_tables(conn)?;
    upsert_column_comment(conn, &real, &c.name, comment).map_err(sqlite_msg)?;
    Ok(())
}

// ---------------- 分页：keyset ----------------

/// 翻页游标。对界面不透明（就是个字符串），这里定的是内部结构。
struct Cursor {
    key: Value,
    rowid: i64,
}

fn encode_cursor(key: &Json, rowid: i64, key_col: Option<&str>, desc: bool) -> String {
    let mut m = serde_json::Map::new();
    m.insert("v".to_string(), key.clone());
    m.insert("r".to_string(), Json::from(rowid));
    m.insert(
        "o".to_string(),
        match key_col {
            Some(c) => Json::String(c.to_string()),
            None => Json::Null,
        },
    );
    m.insert("d".to_string(), Json::Bool(desc));
    Json::Object(m).to_string()
}

/// 解游标。**必须校验排序方式是否与游标一致**：用户翻到第 3 页时点了表头换个排序，
/// 若还拿旧游标去查，就会得到"看起来正常、其实是错的"一页数据 —— 这类静默错误
/// 比直接报错危险得多，所以这里宁可让他从第一页重来。
fn decode_cursor(s: &str, key_col: Option<&str>, desc: bool) -> Result<Cursor> {
    let v: Json = serde_json::from_str(s)
        .map_err(|_| "翻页游标无法识别（可能被改动过），请重新从第一页加载".to_string())?;
    let key_matches = match (v.get("o"), key_col) {
        (Some(Json::Null), None) => true,
        (Some(Json::String(a)), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    if !key_matches || v.get("d").and_then(Json::as_bool) != Some(desc) {
        return Err(
            "翻页游标与当前排序方式不一致（排序在翻页途中被改过），请重新从第一页加载"
                .to_string(),
        );
    }
    let rowid = v
        .get("r")
        .and_then(Json::as_i64)
        .ok_or_else(|| "翻页游标无法识别（缺少行号），请重新从第一页加载".to_string())?;
    let key = match v.get("v") {
        Some(Json::Null) => Value::Null,
        Some(Json::Number(n)) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Real(n.as_f64().unwrap_or(0.0)),
        },
        Some(Json::String(s)) => Value::Text(s.clone()),
        _ => return Err("翻页游标无法识别（排序值类型不对），请重新从第一页加载".to_string()),
    };
    Ok(Cursor { key, rowid })
}

/// 取一页数据（keyset 分页）。
///
/// **不用 `OFFSET`**：`OFFSET n` 要求 SQLite 先数过前 n 行，代价是 O(N²)，
/// 1 亿行实测冷缓存 30–109 秒；更要命的是当排序列有并列值时，OFFSET 的分页边界
/// 不稳定（同一批并列值的相对顺序可能变），会出现"翻页时某行重复出现、另一行再也看不到"。
///
/// 这里用 keyset：`WHERE 排序键 > 上一页最后一个键 ORDER BY 排序键, rowid LIMIT n`。
/// 三条要点：
///   1. **必须带唯一的 tiebreaker**（`rowid`）。并列值只靠排序键比较，"
///      等于"的那一批会整批跳过或整批重来。rowid 唯一，所以边界稳定。
///   2. tiebreaker 恒为 `rowid ASC`（不随升降序翻转）：并列值的相对顺序固定成
///      "录入顺序"，用户来回切换排序时看到的顺序更稳定。
///   3. 排序键可能为 `NULL`，而 `NULL` 与任何值比较都是 NULL（假），
///      直接用 `> ` 比较会漏掉整个 NULL 组 —— 所以 WHERE 里对 NULL 单独处理
///      （升序 NULL 在前、降序 NULL 在后，与 SQLite 的默认排序一致）。
///
/// `has_more` 的处理：多取一行来判断还有没有下一页（不数总数，见 `row_estimate`）。
/// 多取的那一行不会返回给调用方。
pub fn page_rows(
    conn: &Connection,
    table: &str,
    order_by: Option<&str>,
    desc: bool,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    page_rows_filtered(conn, table, order_by, desc, cursor, limit, &[])
}

/// 带**列关键词筛选**的分页（数据网格筛选行的后端）。
///
/// `filters` 是 (字段名, 关键词) 列表，语义是"这一列的值**包含**该关键词
/// （大小写不敏感）"，多个条件之间是 AND；空关键词被忽略。
///
/// 两个刻意的决定：
///   - **不数总数**：与 [`page_rows`] 同一个理由，筛选后的行数只能靠
///     "取到没有更多为止"得知，界面显示已加载行数即可。
///   - **游标不编码筛选条件**：筛选变化时界面必须回到第一页重新开始
///     （网格的 reload() 正是这么做的）。把筛选编进游标会让人误以为
///     "换个筛选还能接着翻旧游标"，那翻出来的是错误数据。
pub fn page_rows_filtered(
    conn: &Connection,
    table: &str,
    order_by: Option<&str>,
    desc: bool,
    cursor: Option<&str>,
    limit: usize,
    filters: &[(String, String)],
) -> Result<Page> {
    validate_identifier(table)?;
    if limit == 0 {
        return Err("每页行数至少为 1".to_string());
    }
    let limit = limit.min(MAX_PAGE_LIMIT);
    let real = ensure_rowid_table(conn, table)?;
    let cols = table_columns(conn, &real)?;
    if cols.is_empty() {
        return Err(format!("表「{real}」没有任何字段"));
    }

    // 排序键：不指定就按 rowid（= 录入顺序）
    let key_col: Option<String> = match order_by {
        Some(c) => {
            validate_column_name(c)?;
            match cols.iter().find(|x| x.name.eq_ignore_ascii_case(c)) {
                Some(x) => Some(x.name.clone()),
                None => return Err(format!("表「{real}」没有字段「{c}」")),
            }
        }
        None => None,
    };
    let key_expr = match &key_col {
        Some(c) => quote_ident(c),
        None => ROWID_EXPR.to_string(),
    };
    // 排序列在返回行里的下标（第 0 个是 rowid）
    let key_pos = match &key_col {
        Some(c) => 1 + cols
            .iter()
            .position(|x| x.name.eq_ignore_ascii_case(c))
            .unwrap_or(0),
        None => 0,
    };
    // 排序列就是 rowid 时不用再写一遍 tiebreaker
    let dir = if desc { "DESC" } else { "ASC" };
    let order_clause = match &key_col {
        Some(c) => format!("ORDER BY {} {dir}, {ROWID_EXPR} ASC", quote_ident(c)),
        None => format!("ORDER BY {ROWID_EXPR} {dir}"),
    };

    // ---- WHERE 的装配：筛选在前，游标在后 ----
    // 绑定值全部收进一个 Vec，占位符用**带编号**的 ?n（n 从 1 数起）。
    // 筛选参数排前面，所以游标占位符的编号依赖筛选的个数 —— 这也是为什么
    // 筛选必须先装配。顺序错了整条查询就错了，新增参数时必须跟着改编号。
    let mut binds: Vec<Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();

    for (col, kw) in filters {
        let kw = kw.trim();
        if kw.is_empty() {
            continue;
        }
        validate_column_name(col)?;
        let real_col = cols
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(col))
            .ok_or_else(|| format!("表「{real}」没有字段「{col}」"))?;
        // CAST 成文本再做包含匹配：数字列也能按"123"筛。
        // NULL 行自然被排除（CAST(NULL) 还是 NULL，instr 返回 NULL，不满足 > 0）。
        let idx = binds.len() + 1;
        binds.push(Value::Text(kw.to_lowercase()));
        clauses.push(format!(
            "instr(lower(CAST({} AS TEXT)), lower(?{idx})) > 0",
            quote_ident(&real_col.name)
        ));
    }

    let cur = match cursor {
        Some(s) => Some(decode_cursor(s, key_col.as_deref(), desc)?),
        None => None,
    };
    let p1 = binds.len() + 1;
    let p2 = binds.len() + 2;
    match (&cur, &key_col) {
        (None, _) => {}
        (Some(c), None) => {
            let op = if desc { "<" } else { ">" };
            clauses.push(format!("{ROWID_EXPR} {op} ?{p1}"));
            binds.push(Value::Integer(c.rowid));
        }
        (Some(c), Some(_)) => {
            let cond = if desc {
                // 降序时 NULL 排在最后：游标在 NULL 组里，就只剩同组的 rowid 更大的行
                format!(
                    "(?{p1} IS NULL AND {key_expr} IS NULL AND {ROWID_EXPR} > ?{p2})
                        OR (?{p1} IS NOT NULL AND ({key_expr} IS NULL
                            OR {key_expr} < ?{p1}
                            OR ({key_expr} = ?{p1} AND {ROWID_EXPR} > ?{p2})))"
                )
            } else {
                // 升序时 NULL 排在最前：游标在 NULL 组里，剩下的是同组更大的 rowid + 全部非 NULL 行
                format!(
                    "(?{p1} IS NULL AND ({key_expr} IS NULL AND {ROWID_EXPR} > ?{p2}
                            OR {key_expr} IS NOT NULL))
                        OR (?{p1} IS NOT NULL AND ({key_expr} > ?{p1}
                            OR ({key_expr} = ?{p1} AND {ROWID_EXPR} > ?{p2})))"
                )
            };
            clauses.push(cond);
            binds.push(c.key.clone());
            binds.push(Value::Integer(c.rowid));
        }
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let select_list = std::iter::once(format!("{ROWID_EXPR} AS {}", quote_ident(ROWID_COLUMN)))
        .chain(cols.iter().map(|c| quote_ident(&c.name)))
        .collect::<Vec<String>>()
        .join(", ");
    // LIMIT 也走参数绑定；用带编号的位置参数，编号与上面的 WHERE 对齐
    let limit_idx = binds.len() + 1;
    let sql = format!(
        "SELECT {select_list} FROM {} {where_clause} {order_clause} LIMIT ?{limit_idx}",
        quote_ident(&real)
    );
    let mut all_binds = binds;
    all_binds.push(Value::Integer(limit as i64 + 1)); // 多取一行：只为判断还有没有下一页

    let mut stmt = conn.prepare(&sql).map_err(sqlite_msg)?;
    let mut rows = stmt
        .query(params_from_iter(all_binds.iter()))
        .map_err(sqlite_msg)?;

    let ncol = cols.len() + 1;
    let mut out: Vec<Vec<Json>> = Vec::new();
    let mut last_key = Json::Null;
    let mut last_rowid = 0i64;
    let mut has_more = false;
    while let Some(r) = rows.next().map_err(sqlite_msg)? {
        if out.len() >= limit {
            has_more = true;
            break;
        }
        let mut row = Vec::with_capacity(ncol);
        for i in 0..ncol {
            row.push(value_to_json(r.get_ref(i).map_err(sqlite_msg)?));
        }
        last_rowid = row[0].as_i64().unwrap_or(0);
        last_key = row[key_pos].clone();
        out.push(row);
    }
    drop(rows);
    drop(stmt);

    let next_cursor = if has_more {
        Some(encode_cursor(&last_key, last_rowid, key_col.as_deref(), desc))
    } else {
        None
    };
    let columns = std::iter::once(ROWID_COLUMN.to_string())
        .chain(cols.iter().map(|c| c.name.clone()))
        .collect();
    Ok(Page {
        rows: out,
        columns,
        has_more,
        next_cursor,
    })
}

// ---------------- 数据编辑 ----------------

/// 一列的引用：真实名字 + 语义类型（元数据优先，其次按声明类型反推）。
struct ColRef {
    name: String,
    ty: Option<ColType>,
}

/// 按字段名（小写）索引的列信息，用于写入时的校验与类型强转。
fn column_index(conn: &Connection, table: &str) -> Result<HashMap<String, ColRef>> {
    let metas = load_column_meta(conn, table)?;
    let mut out = HashMap::new();
    for c in table_columns(conn, table)? {
        let ty = metas
            .get(&c.name.to_lowercase())
            .and_then(|(_, sem)| *sem)
            .or_else(|| ColType::from_declared(&c.decl_type));
        out.insert(c.name.to_lowercase(), ColRef { name: c.name, ty });
    }
    Ok(out)
}

/// 批量插入。
///
/// 整批放在**一个事务**里：这是 docs/06 §7.2「事务批量提交」的做法 ——
/// 逐行自动提交时每行一次 fsync（本库是 `synchronous=FULL`），1 万行的导入会慢到不可用。
/// 语句只预编译一次，循环里只换绑定值。
///
/// 长任务（100 万行级）请由调用方分批多次调用，别指望一次调用扛完 ——
/// 那样内存里会堆着一整个 `rows`，而且失败要整批重来。
pub fn insert_rows(
    conn: &mut Connection,
    table: &str,
    columns: &[String],
    rows: &[Vec<String>],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    validate_identifier(table)?;
    let real = ensure_user_table(conn, table)?;
    let idx = column_index(conn, &real)?;

    // 列必须存在、且不重复（字段名大小写不敏感）
    let mut seen: HashSet<String> = HashSet::new();
    let mut cols: Vec<&ColRef> = Vec::with_capacity(columns.len());
    for c in columns {
        validate_column_name(c)?;
        if !seen.insert(c.to_lowercase()) {
            return Err(format!("列「{c}」重复出现了"));
        }
        match idx.get(&c.to_lowercase()) {
            Some(ci) => cols.push(ci),
            None => return Err(format!("表「{real}」没有字段「{c}」")),
        }
    }

    let tx = conn.transaction().map_err(sqlite_msg)?;
    let inserted: usize = if cols.is_empty() {
        // 一个字段都没给：界面上的"空白行"就是这样，插一行全默认值
        let sql = format!("INSERT INTO {} DEFAULT VALUES", quote_ident(&real));
        let mut n = 0usize;
        for row in rows {
            if !row.is_empty() {
                return Err("没有指定任何列，却给了值".to_string());
            }
            n += tx.execute(&sql, []).map_err(sqlite_msg)?;
        }
        n
    } else {
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote_ident(&real),
            cols.iter()
                .map(|c| quote_ident(&c.name))
                .collect::<Vec<String>>()
                .join(", "),
            (1..=cols.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<String>>()
                .join(", ")
        );
        let mut stmt = tx.prepare(&sql).map_err(sqlite_msg)?;
        let mut n = 0usize;
        for (ri, row) in rows.iter().enumerate() {
            if row.len() != cols.len() {
                return Err(format!(
                    "第 {} 行给了 {} 个值，但有 {} 列 —— 数量必须一致",
                    ri + 1,
                    row.len(),
                    cols.len()
                ));
            }
            let mut vals: Vec<Value> = Vec::with_capacity(cols.len());
            for (v, c) in row.iter().zip(cols.iter()) {
                vals.push(
                    coerce_value(c.ty, &c.name, Some(v))
                        .map_err(|e| format!("第 {} 行{e}", ri + 1))?,
                );
            }
            n += stmt
                .execute(params_from_iter(vals.iter()))
                .map_err(|e| format!("第 {} 行插入失败：{}", ri + 1, sqlite_msg(e)))?;
        }
        n
    };
    tx.commit().map_err(sqlite_msg)?;
    Ok(inserted)
}

/// 改一个单元格。按 rowid 定位，所以不会误伤别的行。
///
/// 行不存在时报错而不是静默成功：界面上的"改了一格却没生效"是最难排查的一类问题
/// （多端/多窗口下常见），宁可让用户刷新。
pub fn update_cell(
    conn: &Connection,
    table: &str,
    rowid: i64,
    column: &str,
    value: Option<&str>,
) -> Result<()> {
    validate_identifier(table)?;
    validate_column_name(column)?;
    let real = ensure_rowid_table(conn, table)?;
    let idx = column_index(conn, &real)?;
    let Some(c) = idx.get(&column.to_lowercase()) else {
        return Err(format!("表「{real}」没有字段「{column}」"));
    };
    let v = coerce_value(c.ty, &c.name, value)?;
    let sql = format!(
        "UPDATE {} SET {} = ?1 WHERE {ROWID_EXPR} = ?2",
        quote_ident(&real),
        quote_ident(&c.name)
    );
    let n = conn
        .execute(&sql, params![v, rowid])
        .map_err(sqlite_msg)?;
    if n == 0 {
        return Err("没有找到要修改的那一行（可能已被删除），请刷新后再试".to_string());
    }
    Ok(())
}

/// 按 rowid 批量删行，返回真正删掉的条数。
///
/// 为什么返回条数而不是布尔：界面要在"删了 5 条，其中 3 条早就不在了"这种情况下
/// 给用户一个准确交代（docs/06 §5.1「删除前显示影响行数」）。
/// 整批一个事务：要么都删掉，要么一条都不删。
pub fn delete_rows(conn: &Connection, table: &str, rowids: &[i64]) -> Result<usize> {
    if rowids.is_empty() {
        return Ok(0);
    }
    validate_identifier(table)?;
    let real = ensure_rowid_table(conn, table)?;
    let tx = conn.unchecked_transaction().map_err(sqlite_msg)?;
    let mut n = 0usize;
    for chunk in rowids.chunks(DELETE_CHUNK) {
        let holders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "DELETE FROM {} WHERE {ROWID_EXPR} IN ({holders})",
            quote_ident(&real)
        );
        n += tx
            .execute(&sql, params_from_iter(chunk.iter()))
            .map_err(sqlite_msg)?;
    }
    tx.commit().map_err(sqlite_msg)?;
    Ok(n)
}

// ---------------- 查询执行 ----------------

/// 语句类别。**只按首关键字判**，不解析 SQL —— 见 `classify` 的说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    Write,
}

/// 跳过前导空白与注释，取第一个关键字。
///
/// 判据（刻意保持简单）：首关键字是 `SELECT` / `PRAGMA` / `EXPLAIN` / `WITH` 视为"读"，
/// 其余视为"写"。**它的局限**（写在注释里，因为界面的确认流程要依赖这个结论）：
///   - `WITH ... DELETE/UPDATE/INSERT`（带 CTE 的写操作）会被当成读；
///   - `VALUES (1),(2)` 这类只读语句会被当成写；
///   - 注释里的关键字不影响判断（已经跳过），但字符串字面量里的分号/关键字不参与判断。
///
/// 因此 `run_query` 里另外用了 SQLite 自己的 `sqlite3_stmt_readonly` 做**兜底**：
/// 判据说"读"、但 SQLite 说"这条语句会写库"时，也不会提前截断它（见下）。
/// 真正拦住破坏性操作的防线是 docs/06 §11 的界面二次确认，不是这个判据。
fn classify(sql: &str) -> Result<(Mode, String)> {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
                Some(i) => s = rest[i + 1..].trim_start(),
                None => return Err("这条 SQL 只有注释，没有可执行的语句".to_string()),
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = rest[i + 2..].trim_start(),
                None => return Err("块注释没有闭合（缺少 */）".to_string()),
            }
        } else {
            break;
        }
    }
    let word: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if word.is_empty() {
        return Err("看不懂这条语句的开头（不是字母开头）".to_string());
    }
    let upper = word.to_ascii_uppercase();
    let mode = match upper.as_str() {
        "SELECT" | "PRAGMA" | "EXPLAIN" | "WITH" => Mode::Read,
        _ => Mode::Write,
    };
    Ok((mode, upper))
}

/// 破坏性操作的**粗判**，给界面在执行前弹二次确认用（docs/06 §3.3 危险操作拦截、
/// §11 破坏性操作确认）。返回一句给人看的理由，`None` 表示没看出危险。
///
/// 它是**提示不是保险**：只看首关键字与有没有 `WHERE`，看不懂 CTE、子查询、
/// 以及一次给多条语句的情况。真正的闸门是"要求用户输入对象名"那一步。
pub fn needs_confirm(sql: &str) -> Option<&'static str> {
    let (_, head) = classify(sql).ok()?;
    match head.as_str() {
        "DROP" => Some("DROP 会连表带数据一起删掉，删完无法恢复"),
        "TRUNCATE" => Some("TRUNCATE 会清空整张表"),
        "DELETE" if !has_where(sql) => Some("DELETE 没有 WHERE 条件，会删掉表里的全部记录"),
        "UPDATE" if !has_where(sql) => Some("UPDATE 没有 WHERE 条件，会改写表里的全部记录"),
        _ => None,
    }
}

/// 语句里有没有出现在字符串/注释之外、**括号深度 0** 的 `WHERE`，且这个 `WHERE`
/// 后面跟的是**真正的过滤条件**（而不是 `WHERE 1=1` 这类恒真谓词）。
///
/// 两道防线（都是踩过的坑，不是风格偏好）：
///
/// 1. 只看**顶层** WHERE（括号深度 0）。`WHERE` 藏在子查询里时，对外层语句**没有**
///    过滤条件 —— 上一版实现只要在语句里找到 WHERE 这个词就算"有条件"，恰好漏掉这一类
///    （第十二轮交接清单里点名的那条确认漏网）。
/// 2. 顶层 WHERE 之后若只是恒真谓词（`1`、`true`、`1=1`、`(1=1)`，以及它们用 AND
///    连接的纯恒真组合），一律当作"没有真正的过滤条件"——`needs_confirm` 据此继续弹
///    确认，堵住 `DELETE FROM t WHERE 1=1` / `WHERE true` 这类**绕过确认、实则改/删全表**
///    的写法。判断不了的形态（子查询、派生表）按"需确认"处理，**宁多勿少**。
///
/// ```sql
/// UPDATE 客户 SET 电话 = (SELECT 1 WHERE 1=1);   -- 外层 UPDATE 仍改全表：子查询里的 WHERE 不算
/// DELETE FROM t WHERE 1=1;                       -- 顶层 WHERE 但恒真：仍改全表，必须弹确认
/// DELETE FROM t WHERE 1=1 AND a=2;               -- 含真实条件 a=2：这才算真正过滤，不弹确认
/// ```
fn has_where(sql: &str) -> bool {
    // 1) 找到顶层 WHERE，取出它后面的谓词
    let Some(pred) = top_level_where_predicate(sql) else {
        return false; // 根本没有顶层 WHERE
    };
    // 2) 谓词里是否含有"真正的条件"（只要有一个非恒真的合取项就算）
    eval_predicate(pred) == PredKind::Real
}

/// 跳过字符串/注释、跟踪括号深度，返回首个**顶层**（深度 0）`WHERE` 之后的谓词子串。
/// 找不到顶层 WHERE 时返回 `None`（`WHERE` 在子查询里、或在字符串/注释里都算没有）。
fn top_level_where_predicate(sql: &str) -> Option<&str> {
    let mut rest = sql;
    let mut depth = 0usize;
    while !rest.is_empty() {
        // 行注释
        if let Some(r) = rest.strip_prefix("--") {
            rest = match r.find('\n') {
                Some(i) => &r[i + 1..],
                None => "",
            };
            continue;
        }
        // 块注释
        if let Some(r) = rest.strip_prefix("/*") {
            rest = match r.find("*/") {
                Some(i) => &r[i + 2..],
                None => "",
            };
            continue;
        }
        let c = rest.chars().next().unwrap_or(' ');
        // 引号 / 方括号 / 反引号字符串：整段跳过（字符串里出现 where 1=1 不能当真）
        if c == '\'' || c == '"' || c == '[' || c == '`' {
            let close = if c == '[' { ']' } else { c };
            let mut it = rest.char_indices();
            it.next();
            let mut end = None;
            let mut skip = false;
            for (i, ch) in it {
                if skip {
                    skip = false;
                    continue;
                }
                if ch == close {
                    end = Some(i + ch.len_utf8());
                    break;
                }
                if ch == c && c != '[' {
                    skip = true;
                }
            }
            rest = match end {
                Some(e) => &rest[e..],
                None => "",
            };
            continue;
        }
        // 括号深度：子查询里的关键字与顶层关键字必须分得开
        if c == '(' {
            depth += 1;
            rest = &rest[1..];
            continue;
        }
        if c == ')' {
            depth = depth.saturating_sub(1);
            rest = &rest[1..];
            continue;
        }
        // 关键字
        if c.is_ascii_alphabetic() || c == '_' {
            let word: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let upper = word.to_ascii_uppercase();
            let after = &rest[word.len()..];
            if upper == "WHERE" && depth == 0 {
                // 谓词从 WHERE 词后开始，到语句末尾（DELETE/UPDATE 的 WHERE 都是末子句）
                return Some(after.trim_start());
            }
            rest = after;
            continue;
        }
        rest = &rest[c.len_utf8()..];
    }
    None
}

/// 谓词的分类结果。
/// - `Real`：含有明确的条件（列名 + 比较/操作符、裸列名、IN/LIKE/IS/BETWEEN …）。
/// - `Taut`：纯恒真（只有恒真原子 AND 连接）。
/// - `Unknown`：判断不了（子查询、派生表等），按"需确认"处理。
#[derive(PartialEq)]
enum PredKind {
    Real,
    Taut,
    Unknown,
}

/// 评估一个顶层 WHERE 谓词：其中是否含有"真正的条件"。
/// 合取（AND）里只要出现一个 Real 项，整条就当 Real；
/// 全为 Taut 才算 Taut；出现 Unknown 且无 Real 时算 Unknown（宁多勿少）。
fn eval_predicate(pred: &str) -> PredKind {
    let conjuncts = split_on_logical(pred, "AND");
    if conjuncts.is_empty() {
        return PredKind::Unknown;
    }
    let mut any_real = false;
    let mut any_unknown = false;
    for conj in conjuncts {
        match eval_disjunction(conj.trim()) {
            PredKind::Real => any_real = true,
            PredKind::Unknown => any_unknown = true,
            PredKind::Taut => {}
        }
    }
    if any_real {
        PredKind::Real
    } else if any_unknown {
        PredKind::Unknown
    } else {
        PredKind::Taut
    }
}

/// 析取（OR）：任一析取项为 Taut 则整体 Taut（恒真）；
/// 否则有 Real 即 Real；否则 Unknown。
fn eval_disjunction(disj: &str) -> PredKind {
    let terms = split_on_logical(disj, "OR");
    if terms.is_empty() {
        return PredKind::Unknown;
    }
    let mut any_taut = false;
    let mut any_real = false;
    for t in terms {
        match classify_atom(t.trim()) {
            PredKind::Taut => any_taut = true,
            PredKind::Real => any_real = true,
            // Unknown 不需要单独记：下面的兜底分支就是它（没有 Taut 也没有 Real → Unknown）
            PredKind::Unknown => {}
        }
    }
    if any_taut {
        PredKind::Taut
    } else if any_real {
        PredKind::Real
    } else {
        PredKind::Unknown
    }
}

/// 把一个原子（不含顶层 AND/OR 的谓词片段）分类。
fn classify_atom(atom: &str) -> PredKind {
    // 反复剥掉最外层成对的圆括号，再判断
    let inner = strip_outer_parens(atom);
    let s = inner.trim();
    if s.is_empty() {
        return PredKind::Unknown;
    }
    // 直接子查询 / 派生表（`(SELECT …)`、`(WITH …)`）：不在外层做恒真判定，按"需确认"
    {
        let head = s.trim_start();
        if head.starts_with("SELECT") || head.starts_with("WITH") {
            return PredKind::Unknown;
        }
    }
    // 恒真原子（忽略空白）：1 / true / 1=1 / true=true
    let compact: String = s.split_whitespace().collect();
    if compact == "1" || compact.eq_ignore_ascii_case("true") {
        return PredKind::Taut;
    }
    if compact == "1=1" || compact.eq_ignore_ascii_case("true=true") {
        return PredKind::Taut;
    }
    // 含顶层比较操作符或关系关键字 → 真实条件
    if has_top_level_op_or_relkw(s) {
        return PredKind::Real;
    }
    // 单个裸词（列名，按真值参与过滤）→ 真实条件
    if !s.contains(char::is_whitespace) {
        return PredKind::Real;
    }
    PredKind::Unknown
}

/// 剥掉最外层成对的圆括号（可嵌套），返回剩下的部分。
fn strip_outer_parens(s: &str) -> &str {
    let mut cur = s.trim();
    loop {
        if let Some(stripped) = cur.strip_prefix('(') {
            if let Some(body) = stripped.strip_suffix(')') {
                // 校验括号是否真的成对，避免 `(a(b)` 这类误剥
                if parens_balanced(body) {
                    cur = body.trim();
                    continue;
                }
            }
        }
        return cur;
    }
}

/// `s` 里左右圆括号是否成对（用于安全剥外层括号）。
fn parens_balanced(s: &str) -> bool {
    let mut d = 0i32;
    for c in s.chars() {
        if c == '(' {
            d += 1;
        } else if c == ')' {
            d -= 1;
            if d < 0 {
                return false;
            }
        }
    }
    d == 0
}

/// `s` 里（跳过字符串/注释、按括号深度）是否出现顶层的比较操作符或关系关键字。
fn has_top_level_op_or_relkw(s: &str) -> bool {
    let relkw = ["IN", "LIKE", "IS", "BETWEEN"];
    let mut rest = s;
    let mut depth = 0usize;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("--") {
            rest = match r.find('\n') {
                Some(i) => &r[i + 1..],
                None => "",
            };
            continue;
        }
        if let Some(r) = rest.strip_prefix("/*") {
            rest = match r.find("*/") {
                Some(i) => &r[i + 2..],
                None => "",
            };
            continue;
        }
        let c = rest.chars().next().unwrap_or(' ');
        if c == '\'' || c == '"' || c == '[' || c == '`' {
            let close = if c == '[' { ']' } else { c };
            let mut it = rest.char_indices();
            it.next();
            let mut end = None;
            let mut skip = false;
            for (i, ch) in it {
                if skip {
                    skip = false;
                    continue;
                }
                if ch == close {
                    end = Some(i + ch.len_utf8());
                    break;
                }
                if ch == c && c != '[' {
                    skip = true;
                }
            }
            rest = match end {
                Some(e) => &rest[e..],
                None => "",
            };
            continue;
        }
        if c == '(' {
            depth += 1;
            rest = &rest[1..];
            continue;
        }
        if c == ')' {
            depth = depth.saturating_sub(1);
            rest = &rest[1..];
            continue;
        }
        if c == '=' || c == '<' || c == '>' || c == '!' {
            if depth == 0 {
                return true;
            }
            rest = &rest[1..];
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let word: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if depth == 0 && relkw.contains(&word.to_ascii_uppercase().as_str()) {
                return true;
            }
            rest = &rest[word.len()..];
            continue;
        }
        rest = &rest[c.len_utf8()..];
    }
    false
}

/// 按顶层（深度 0）的逻辑关键字切分谓词。`kw` 为 `"AND"` 时会跳过 `BETWEEN … AND …`
/// 里的那个 AND（它属于 BETWEEN 结构，不是合取分隔符）。
fn split_on_logical(sql: &str, kw: &str) -> Vec<String> {
    let kw = kw.to_ascii_uppercase();
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut between_depth: Option<usize> = None; // 期望在哪个深度消耗一个 AND
    let mut cur = String::new();
    let mut rest = sql;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("--") {
            rest = match r.find('\n') {
                Some(i) => &r[i + 1..],
                None => "",
            };
            continue;
        }
        if let Some(r) = rest.strip_prefix("/*") {
            rest = match r.find("*/") {
                Some(i) => &r[i + 2..],
                None => "",
            };
            continue;
        }
        let c = rest.chars().next().unwrap_or(' ');
        if c == '\'' || c == '"' || c == '[' || c == '`' {
            let close = if c == '[' { ']' } else { c };
            let mut it = rest.char_indices();
            it.next();
            let mut end = None;
            let mut skip = false;
            for (i, ch) in it {
                if skip {
                    skip = false;
                    continue;
                }
                if ch == close {
                    end = Some(i + ch.len_utf8());
                    break;
                }
                if ch == c && c != '[' {
                    skip = true;
                }
            }
            if let Some(e) = end {
                cur.push_str(&rest[..e]);
                rest = &rest[e..];
            } else {
                cur.push_str(rest);
                rest = "";
            }
            continue;
        }
        if c == '(' {
            depth += 1;
            cur.push(c);
            rest = &rest[1..];
            continue;
        }
        if c == ')' {
            depth = depth.saturating_sub(1);
            cur.push(c);
            rest = &rest[1..];
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let word: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let upper = word.to_ascii_uppercase();
            if upper == kw && depth == 0 {
                // 顶层分隔符：可能是 BETWEEN 的收尾 AND（不切分），否则切分
                if between_depth == Some(depth) {
                    between_depth = None;
                } else {
                    parts.push(std::mem::take(&mut cur));
                }
            } else if upper == "BETWEEN" && kw == "AND" {
                // 记住：接下来的那个顶层 AND 是 BETWEEN 的收尾，不切分
                between_depth = Some(depth);
            }
            cur.push_str(&word);
            rest = &rest[word.len()..];
            continue;
        }
        cur.push(c);
        rest = &rest[c.len_utf8()..];
    }
    parts.push(cur);
    // 丢完全空白的片段
    parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

/// 执行一条 SQL。
///
/// 读（`SELECT`/`PRAGMA`/`EXPLAIN`/`WITH`）走结果集路径：最多取 `max_rows` 行，
/// 多一行用来判断有没有被截断（`truncated`）。写走执行路径：返回 `affected`。
///
/// 几件必须做的事：
///   - **必须有行数上限**：`SELECT * FROM 千万行表` 不加限制会让界面直接卡死。
///     上限到了就停手，由界面决定要不要继续取（继续取请用 [`page_rows`]）。
///   - 多条语句被拒绝（rusqlite 的 `prepare` 会挡，这里翻译成人话）。
///   - 判据说"读"但 SQLite 说会写库时，**不做提前截断**：截断会把一条写语句执行一半。
///   - 不记录 SQL 日志（docs/06 §9.3）。
///
/// 不做的三件事（都是上层的活）：参数化查询的 `:参数名` 填值、慢查询标记、
/// 结果集导回文件。**超时与中断已经在 IPC 层接上了**（`main.rs` 的
/// `run_query_async`：独立线程 + `get_interrupt_handle`，超时哨兵与"执行中
/// 再点一次"共用同一个中断句柄，见 Q-042）。
pub fn run_query(conn: &Connection, sql: &str, max_rows: usize) -> Result<QueryResult> {
    let (mode, _) = classify(sql)?;
    let cap = max_rows.max(1);
    let started = Instant::now();
    // 影响行数用 total_changes 的**差值**算。
    // 为什么不用 `changes()`：它只反映"最近一条 DML"，DDL 不改动它 —— 于是
    // `CREATE INDEX` 之后 `changes()` 仍然是上一条 DELETE 的行数，用户会看到
    // "建索引影响了 1 行"这种胡话。total_changes 是单调累加的，差值永远对应
    // 这一条语句。代价是触发器造成的写入也计入 —— 对"这次操作动了多少行"这个问题，
    // 含触发器更接近实情（docs/06 §4.4 提醒过触发器有"看不见的写入"）。
    let before = conn.total_changes();
    let mut stmt = conn.prepare(sql).map_err(sqlite_msg)?;
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    // 纯写（没有结果列）：走 execute 路径
    if mode == Mode::Write && columns.is_empty() {
        stmt.execute([]).map_err(sqlite_msg)?;
        return Ok(QueryResult {
            columns,
            rows: Vec::new(),
            truncated: false,
            elapsed_ms: started.elapsed().as_millis() as i64,
            affected: (conn.total_changes() - before) as usize,
        });
    }

    // SQLite 自己的判断：这条语句会不会改库。它比首关键字判据准，所以用它决定
    // "能不能提前停"（提前停 = 语句没跑完）。
    let readonly = stmt.readonly();
    let stop_early = mode == Mode::Read && readonly;

    let ncol = columns.len();
    let mut out: Vec<Vec<Json>> = Vec::new();
    let mut truncated = false;
    {
        let mut rows = stmt.query([]).map_err(sqlite_msg)?;
        while let Some(r) = rows.next().map_err(sqlite_msg)? {
            if out.len() >= cap {
                truncated = true;
                if stop_early {
                    break;
                }
                continue;
            }
            let mut row = Vec::with_capacity(ncol);
            for i in 0..ncol {
                row.push(value_to_json(r.get_ref(i).map_err(sqlite_msg)?));
            }
            out.push(row);
        }
    }
    // 影响行数：只读语句自然是 0（它不改行），写语句取上面记的差值。
    // 注意这一条对"被误判成读的写"（`WITH ... DELETE`）同样成立 ——
    // 那种语句不会提前截断（见 stop_early），也会如实报出影响行数。
    let affected = (conn.total_changes() - before) as usize;
    Ok(QueryResult {
        columns,
        rows: out,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as i64,
        affected,
    })
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        // 与 db.rs 的持久性参数保持一致（内存库上 WAL 无所谓，外键约束要在）
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        c
    }

    fn c(name: &str, ty: ColType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty,
            not_null: false,
            default: None,
            primary_key: false,
            comment: None,
        }
    }

    fn pk(name: &str, ty: ColType) -> ColumnDef {
        ColumnDef {
            primary_key: true,
            ..c(name, ty)
        }
    }

    fn spec(name: &str, columns: Vec<ColumnDef>) -> TableSpec {
        TableSpec {
            name: name.to_string(),
            comment: None,
            columns,
        }
    }

    /// 便捷插入：`&str` 值转成 String
    fn ins(conn: &mut Connection, table: &str, columns: &[&str], rows: &[Vec<&str>]) -> usize {
        let cols: Vec<String> = columns.iter().map(|s| s.to_string()).collect();
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect();
        insert_rows(conn, table, &cols, &rows).unwrap()
    }

    fn rowids_of(page: &Page) -> Vec<i64> {
        page.rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
    }

    /// 用 keyset 分页把整张表取完，返回每行的 rowid 序列
    fn drain(conn: &Connection, table: &str, order: Option<&str>, desc: bool, page: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let p = page_rows(conn, table, order, desc, cursor.as_deref(), page).unwrap();
            assert!(p.rows.len() <= page);
            out.extend(rowids_of(&p));
            match p.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
            assert!(out.len() < 100_000, "分页没有终止");
        }
        out
    }

    /// 参照答案：直接让 SQLite 排序，拿 rowid 序列
    fn reference_order(conn: &Connection, table: &str, order: Option<&str>, desc: bool) -> Vec<i64> {
        let dir = if desc { "DESC" } else { "ASC" };
        let sql = match order {
            Some(c) => format!(
                "SELECT {} FROM {} ORDER BY {} {dir}, {} ASC",
                ROWID_EXPR,
                quote_ident(table),
                quote_ident(c),
                ROWID_EXPR
            ),
            None => format!(
                "SELECT {} FROM {} ORDER BY {} {dir}",
                ROWID_EXPR,
                quote_ident(table),
                ROWID_EXPR
            ),
        };
        let r = run_query(conn, &sql, 100_000).unwrap();
        r.rows.iter().map(|row| row[0].as_i64().unwrap()).collect()
    }

    // ---------- 标识符 ----------

    #[test]
    fn 合法标识符可以通过_含中文() {
        for name in ["客户", "客户_2026", "a1", "_x", "订单明细表", "金额2", "ABC_def"] {
            assert!(validate_identifier(name).is_ok(), "{name} 应该被接受");
        }
    }

    #[test]
    fn 非法标识符被拒绝() {
        let cases = [
            "",
            "1abc",
            "a b",
            "a\"b",
            "a;b",
            "a-b",
            "a'b",
            "a--b",
            "a/*b",
            "客户 表",
            "a\"; DROP TABLE x; --",
        ];
        for name in cases {
            assert!(validate_identifier(name).is_err(), "{name:?} 应该被拒绝");
        }
        // 超长（65 个字符）
        let long = "表".repeat(65);
        assert!(validate_identifier(&long).is_err());
        // 64 个字符是上限，应当通过
        assert!(validate_identifier(&"表".repeat(64)).is_ok());
    }

    #[test]
    fn 注入式表名不会破坏已有表() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        let evil = [r#"客户"; DROP TABLE 客户; --"#, "客户;DROP TABLE 客户"];
        for name in evil {
            let e = create_table(&conn, &spec(name, vec![c("a", ColType::Text)])).unwrap_err();
            assert!(e.contains("不允许的字符") || e.contains("不能"), "错误信息要能看懂：{e}");
        }
        // 原表还在，而且能读写
        let t = get_table(&conn, "客户").unwrap();
        assert_eq!(t.columns.len(), 1);
    }

    #[test]
    fn 字段名不能遮蔽_rowid() {
        for name in ["rowid", "ROWID", "_rowid_", "oid", "_rowid"] {
            assert!(
                validate_column_name(name).is_err(),
                "{name} 是内部行号名，必须拒绝"
            );
        }
        assert!(validate_column_name("行号").is_ok());
    }

    #[test]
    fn 用户表名不能用保留前缀() {
        assert!(validate_user_table_name("_db_x").is_err());
        assert!(validate_user_table_name("sqlite_x").is_err());
        assert!(validate_user_table_name("客户").is_ok());
    }

    // ---------- 建表 / 列表 / 改名 / 删表 ----------

    #[test]
    fn 建表后能在列表里看到_列信息正确() {
        let conn = mem();
        let mut s = spec(
            "客户",
            vec![
                pk("id", ColType::Integer),
                ColumnDef {
                    not_null: true,
                    ..c("姓名", ColType::Text)
                },
                c("余额", ColType::Money),
            ],
        );
        s.comment = Some("客户主表".to_string());
        create_table(&conn, &s).unwrap();

        let list = list_tables(&conn).unwrap();
        assert_eq!(list.len(), 1);
        let t = &list[0];
        assert_eq!(t.name, "客户");
        assert_eq!(t.comment.as_deref(), Some("客户主表"));
        assert_eq!(t.row_estimate, 0);
        assert_eq!(
            t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["id", "姓名", "余额"]
        );
        assert!(t.columns[0].pk);
        assert!(t.columns[1].not_null);
        assert!(!t.columns[1].pk);
        assert_eq!(t.columns[2].decl_type, "INTEGER");
        // 单个主键的列级 PRIMARY KEY 只出现一次
        assert_eq!(get_table(&conn, "客户").unwrap().columns.len(), 3);
    }

    #[test]
    fn 每种字段类型的建表语句按文档映射() {
        let s = spec(
            "类型测试",
            vec![
                c("a文本", ColType::Text),
                c("b整数", ColType::Integer),
                c("c小数", ColType::Real),
                c("d金额", ColType::Money),
                c("e是否", ColType::Boolean),
                c("f日期", ColType::Date),
                c("g日期时间", ColType::DateTime),
                c("h_json", ColType::Json),
                c("i二进制", ColType::Blob),
            ],
        );
        let ddl = create_table_sql(&s).unwrap();
        assert!(ddl.contains("\"a文本\" TEXT"), "{ddl}");
        assert!(ddl.contains("\"b整数\" INTEGER"), "{ddl}");
        assert!(ddl.contains("\"c小数\" REAL"), "{ddl}");
        assert!(ddl.contains("\"e是否\" BOOLEAN"), "{ddl}");
        assert!(ddl.contains("\"f日期\" DATE"), "{ddl}");
        assert!(ddl.contains("\"g日期时间\" DATETIME"), "{ddl}");
        assert!(ddl.contains("\"h_json\" TEXT"), "{ddl}");
        assert!(ddl.contains("\"i二进制\" BLOB"), "{ddl}");
        // 金额：整数存储 + 模式层的整数约束，绝不能是浮点类型
        let money_line = ddl
            .lines()
            .find(|l| l.contains("\"d金额\""))
            .expect("金额列应该在 DDL 里");
        assert!(money_line.contains("INTEGER"), "{money_line}");
        assert!(money_line.contains("CHECK"), "{money_line}");
        assert!(!money_line.contains("REAL"), "{money_line}");
        assert!(!money_line.contains("DECIMAL"), "{money_line}");
        assert!(!money_line.contains("FLOAT"), "{money_line}");
        // 能真的建出来并读回声明类型
        let conn = mem();
        create_table(&conn, &s).unwrap();
        let t = get_table(&conn, "类型测试").unwrap();
        let decl: Vec<&str> = t.columns.iter().map(|c| c.decl_type.as_str()).collect();
        assert_eq!(
            decl,
            vec![
                "TEXT", "INTEGER", "REAL", "INTEGER", "BOOLEAN", "DATE", "DATETIME", "TEXT", "BLOB"
            ]
        );
    }

    #[test]
    fn 多列主键走表级约束() {
        let s = spec(
            "订单明细",
            vec![
                pk("订单号", ColType::Text),
                pk("行号", ColType::Integer),
                c("数量", ColType::Integer),
            ],
        );
        let ddl = create_table_sql(&s).unwrap();
        assert_eq!(ddl.matches("PRIMARY KEY").count(), 1, "{ddl}");
        assert!(ddl.contains("PRIMARY KEY (\"订单号\", \"行号\")"), "{ddl}");
        let conn = mem();
        create_table(&conn, &s).unwrap();
        let t = get_table(&conn, "订单明细").unwrap();
        assert!(t.columns[0].pk && t.columns[1].pk && !t.columns[2].pk);
    }

    #[test]
    fn 默认值与自动编号的处理() {
        let mut s = spec("t", vec![pk("id", ColType::Integer), c("n", ColType::Integer)]);
        s.columns[1].default = Some("0".to_string());
        s.columns[0].default = Some("AUTOINCREMENT".to_string()); // 应当被忽略（INTEGER PRIMARY KEY 本来就自增）
        let ddl = create_table_sql(&s).unwrap();
        assert!(!ddl.contains("AUTOINCREMENT"), "{ddl}");
        assert!(ddl.contains("\"n\" INTEGER DEFAULT 0"), "{ddl}");
        let conn = mem();
        create_table(&conn, &s).unwrap();
        let mut conn = conn;
        // 不给任何列 = 全默认值（界面上"底部空白行"直接提交就是这个）
        ins(&mut conn, "t", &[], &[vec![]]);
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][1].as_i64(), Some(1), "INTEGER PRIMARY KEY 自动编号");
        assert_eq!(p.rows[0][2].as_i64(), Some(0), "默认值生效");
        // 显式给空串 = 用户把这个字段清空了，写 NULL，**不是**回落到默认值
        ins(&mut conn, "t", &["n"], &[vec![""]]);
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[1][2], Json::Null);
    }

    #[test]
    fn 默认值里的注入尝试被拒绝() {
        let mut s = spec("t", vec![c("a", ColType::Text)]);
        s.columns[0].default = Some("0); DROP TABLE t; --".to_string());
        assert!(create_table_sql(&s).is_err());
        // 合法字面量仍然可以
        for ok in ["0", "-1.5", "''", "'未填写'", "\"x\"", "NULL", "TRUE", "CURRENT_TIMESTAMP", "(1+2)"] {
            let mut s2 = spec("t2", vec![c("a", ColType::Text)]);
            s2.columns[0].default = Some(ok.to_string());
            assert!(create_table_sql(&s2).is_ok(), "{ok} 应该被接受");
        }
    }

    #[test]
    fn 表已存在时报可读错误() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        let e = create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap_err();
        assert!(e.contains("客户") && e.contains("已经"), "{e}");
        // 视图占用了同名也说得清楚
        conn.execute_batch("CREATE VIEW v1 AS SELECT 1 AS x").unwrap();
        let e = create_table(&conn, &spec("v1", vec![c("a", ColType::Text)])).unwrap_err();
        assert!(e.contains("view") || e.contains("视图"), "{e}");
    }

    #[test]
    fn 重命名表会带上数据与注释() {
        let conn = mem();
        let mut s = spec(
            "客户",
            vec![pk("id", ColType::Integer), c("姓名", ColType::Text)],
        );
        s.comment = Some("客户主表".to_string());
        s.columns[1].comment = Some("客户姓名".to_string());
        create_table(&conn, &s).unwrap();
        let mut conn = conn;
        ins(&mut conn, "客户", &["姓名"], &[vec!["张三"], vec!["李四"]]);

        rename_table(&conn, "客户", "客户_2026").unwrap();
        assert!(get_table(&conn, "客户").is_err());
        let t = get_table(&conn, "客户_2026").unwrap();
        assert_eq!(t.comment.as_deref(), Some("客户主表"));
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.row_estimate, 2);
        // 注释跟着搬了
        let metas = column_meta(&conn, "客户_2026").unwrap();
        assert_eq!(metas[1].comment.as_deref(), Some("客户姓名"));
        let names = list_tables(&conn)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["客户_2026"]);
    }

    #[test]
    fn 重命名到已存在的名字会被拒() {
        let conn = mem();
        create_table(&conn, &spec("a", vec![pk("id", ColType::Integer)])).unwrap();
        create_table(&conn, &spec("b", vec![pk("id", ColType::Integer)])).unwrap();
        let e = rename_table(&conn, "a", "b").unwrap_err();
        assert!(e.contains("b"), "{e}");
        // 原来的两张表都还在
        assert!(get_table(&conn, "a").is_ok());
        assert!(get_table(&conn, "b").is_ok());
        // 改名到自己的名字也不行
        assert!(rename_table(&conn, "a", "a").is_err());
    }

    #[test]
    fn 删表必须逐字确认表名() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        for bad in ["", "客户 ", " 客户", "客户表", "kehu"] {
            let e = drop_table(&conn, "客户", bad).unwrap_err();
            assert!(e.contains("确认名不匹配"), "{e}");
            assert!(get_table(&conn, "客户").is_ok(), "不匹配时表必须还在");
        }
        drop_table(&conn, "客户", "客户").unwrap();
        assert!(get_table(&conn, "客户").is_err());
        assert!(list_tables(&conn).unwrap().is_empty());
    }

    #[test]
    fn 删表会把注释一起清掉_重建同名表不继承旧注释() {
        let conn = mem();
        let mut s = spec("客户", vec![pk("id", ColType::Integer), c("姓名", ColType::Text)]);
        s.comment = Some("旧注释".to_string());
        s.columns[1].comment = Some("旧字段注释".to_string());
        create_table(&conn, &s).unwrap();
        drop_table(&conn, "客户", "客户").unwrap();
        create_table(&conn, &spec("客户", vec![c("姓名", ColType::Text)])).unwrap();
        let t = get_table(&conn, "客户").unwrap();
        assert_eq!(t.comment, None);
        let metas = column_meta(&conn, "客户").unwrap();
        assert_eq!(metas[0].comment, None);
    }

    #[test]
    fn 用户表列表不包含内部表与sqlite内部表() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        conn.execute_batch("ANALYZE;").unwrap(); // 让 sqlite_stat1 出现
        let names = list_tables(&conn)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["客户"], "元数据表与 sqlite_* 都不该出现");
        // 但它们确实存在于库里
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('_db_table_comment','_db_column_comment','sqlite_stat1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn 加列成功_旧行读取为新列的空值() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        let mut conn = conn;
        ins(&mut conn, "客户", &["id"], &[vec!["1"], vec!["2"]]);
        let mut col = c("手机号", ColType::Text);
        col.comment = Some("11 位".to_string());
        add_column(&conn, "客户", &col).unwrap();
        assert_eq!(get_table(&conn, "客户").unwrap().columns.len(), 2);

        let p = page_rows(&conn, "客户", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::Null, "旧行在新列上是空值");
        // 新列能写入
        let rid = p.rows[0][0].as_i64().unwrap();
        update_cell(&conn, "客户", rid, "手机号", Some("13800000000")).unwrap();
        let metas = column_meta(&conn, "客户").unwrap();
        assert_eq!(metas[1].comment.as_deref(), Some("11 位"));

        // 重复列名要拒绝（大小写不敏感）
        assert!(add_column(&conn, "客户", &c("手机号", ColType::Text)).is_err());
        assert!(add_column(&conn, "客户", &c("手机号", ColType::Text)).is_err());
    }

    #[test]
    fn 加列的两条限制给人话() {
        let conn = mem();
        create_table(&conn, &spec("客户", vec![pk("id", ColType::Integer)])).unwrap();
        // 主键列
        let e = add_column(&conn, "客户", &pk("新主键", ColType::Integer)).unwrap_err();
        assert!(e.contains("主键"), "{e}");
        // 非空但没默认值
        let mut col = c("必填", ColType::Text);
        col.not_null = true;
        let e = add_column(&conn, "客户", &col).unwrap_err();
        assert!(e.contains("默认值"), "{e}");
        // 失败的改动不应该留下半成品
        assert_eq!(get_table(&conn, "客户").unwrap().columns.len(), 1);
        // 给了默认值就可以加
        col.default = Some("''".to_string());
        add_column(&conn, "客户", &col).unwrap();
        assert_eq!(get_table(&conn, "客户").unwrap().columns.len(), 2);
    }

    #[test]
    fn 取不存在的表给可读错误() {
        let conn = mem();
        let e = get_table(&conn, "没有这张表").unwrap_err();
        assert!(e.contains("不存在"), "{e}");
        assert!(page_rows(&conn, "没有这张表", None, false, None, 10).is_err());
        assert!(update_cell(&conn, "没有这张表", 1, "a", Some("x")).is_err());
        // 视图不是表
        conn.execute_batch("CREATE VIEW v AS SELECT 1 AS x").unwrap();
        let e = get_table(&conn, "v").unwrap_err();
        assert!(e.contains("不是表"), "{e}");
    }

    #[test]
    fn row_estimate是估计值_不数总数() {
        let conn = mem();
        // 故意不建 INTEGER PRIMARY KEY（那是 rowid 的别名），
        // 这样 rowid 由 SQLite 自增，1..=500
        create_table(&conn, &spec("t", vec![c("v", ColType::Text)])).unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=500).map(|i| vec![format!("第{i}行")]).collect();
        insert_rows(&mut conn, "t", &["v".to_string()], &rows).unwrap();
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 500);
        // 删掉一半：max(rowid) 只多不少，估计值会偏大 —— 这就是"估计"的含义
        conn.execute_batch("DELETE FROM t WHERE rowid % 2 = 0").unwrap();
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 499);
        // ANALYZE 之后改走 sqlite_stat1（ANALYZE 当时的行数 250）
        conn.execute_batch("ANALYZE").unwrap();
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 250);
        // 空表是 0（准确值，不是估计）
        create_table(&conn, &spec("空表", vec![pk("id", ColType::Integer)])).unwrap();
        assert_eq!(get_table(&conn, "空表").unwrap().row_estimate, 0);
        // WITHOUT ROWID 表估不出来 → -1，界面该显示"未知"而不是 0
        conn.execute_batch("CREATE TABLE wr (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID")
            .unwrap();
        assert_eq!(get_table(&conn, "wr").unwrap().row_estimate, -1);
    }

    // ---------- 分页 ----------

    #[test]
    fn keyset分页取完不重不漏() {
        let conn = mem();
        create_table(
            &conn,
            &spec(
                "t",
                vec![
                    pk("id", ColType::Integer),
                    c("名称", ColType::Text),
                    c("备注", ColType::Text),
                    c("标签", ColType::Text),
                ],
            ),
        )
        .unwrap();
        let mut conn = conn;
        let mut data: Vec<Vec<String>> = Vec::new();
        for i in 1..=1000 {
            let mut row = vec![String::new(); 4];
            row[0] = i.to_string();
            row[1] = format!("名称{i}");
            row[2] = "含中文与emoji🙂".to_string();
            row[3] = if i % 7 == 0 { String::new() } else { "x".to_string() };
            data.push(row);
        }
        insert_rows(
            &mut conn,
            "t",
            &["id".into(), "名称".into(), "备注".into(), "标签".into()],
            &data,
        )
        .unwrap();

        for page in [1usize, 7, 97, 999, 1000, 1001] {
            let got = drain(&conn, "t", None, false, page);
            assert_eq!(got, (1..=1000).collect::<Vec<i64>>(), "每页 {page} 行时顺序与完整性都要对");
            let got_desc = drain(&conn, "t", None, true, page);
            assert_eq!(got_desc, (1..=1000).rev().collect::<Vec<i64>>(), "倒序也要一致");
        }
    }

    #[test]
    fn 并列排序键分页不重不漏() {
        let conn = mem();
        create_table(
            &conn,
            &spec(
                "订单",
                vec![pk("id", ColType::Integer), c("状态", ColType::Text), c("金额", ColType::Money)],
            ),
        )
        .unwrap();
        let mut conn = conn;
        // 状态只有 4 种取值 → 大量并列；金额同样重复
        let mut data: Vec<Vec<String>> = Vec::new();
        for i in 1..=1000 {
            let st = ["待付款", "已付款", "已发货", "已完成"][i % 4];
            let amt = format!("{}.00", i % 5);
            data.push(vec![i.to_string(), st.to_string(), amt]);
        }
        insert_rows(
            &mut conn,
            "订单",
            &["id".into(), "状态".into(), "金额".into()],
            &data,
        )
        .unwrap();

        for page in [1usize, 13, 37, 333] {
            for (col, desc) in [("状态", false), ("状态", true), ("金额", true)] {
                let got = drain(&conn, "订单", Some(col), desc, page);
                let want = reference_order(&conn, "订单", Some(col), desc);
                assert_eq!(
                    got, want,
                    "按 {col} desc={desc} 每页 {page} 行时必须与 SQLite 的排序完全一致"
                );
                // 不重不漏
                let mut uniq = got.clone();
                uniq.sort_unstable();
                uniq.dedup();
                assert_eq!(uniq.len(), 1000);
            }
        }
    }

    #[test]
    fn 排序键含空值时分页不重不漏() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("k", ColType::Integer)]),
        )
        .unwrap();
        let mut conn = conn;
        let mut data: Vec<Vec<String>> = Vec::new();
        for i in 1..=300 {
            let k = if i % 3 == 0 { String::new() } else { (i % 11).to_string() };
            data.push(vec![i.to_string(), k]);
        }
        insert_rows(&mut conn, "t", &["id".into(), "k".into()], &data).unwrap();

        for desc in [false, true] {
            for page in [1usize, 7, 29, 300] {
                let got = drain(&conn, "t", Some("k"), desc, page);
                let want = reference_order(&conn, "t", Some("k"), desc);
                assert_eq!(got, want, "空值参与排序时 desc={desc} 每页 {page} 行也要对");
                assert_eq!(got.len(), 300);
            }
        }
    }

    #[test]
    fn has_more在多取一行时正确() {
        let conn = mem();
        create_table(&conn, &spec("t", vec![pk("id", ColType::Integer)])).unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=10).map(|i| vec![i.to_string()]).collect();
        insert_rows(&mut conn, "t", &["id".to_string()], &rows).unwrap();

        // 正好取完：has_more=false 且没有游标（多余的一次查询就省了）
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows.len(), 10);
        assert!(!p.has_more);
        assert!(p.next_cursor.is_none());
        // 差一行：有下一页
        let p = page_rows(&conn, "t", None, false, None, 9).unwrap();
        assert_eq!(p.rows.len(), 9);
        assert!(p.has_more);
        assert!(p.next_cursor.is_some());
        // 空表
        create_table(&conn, &spec("空", vec![pk("id", ColType::Integer)])).unwrap();
        let p = page_rows(&conn, "空", None, false, None, 10).unwrap();
        assert!(p.rows.is_empty() && !p.has_more && p.next_cursor.is_none());
        assert_eq!(p.columns, vec![ROWID_COLUMN.to_string(), "id".to_string()]);
    }

    #[test]
    fn 游标与排序方式不一致时报错() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("k", ColType::Text)]),
        )
        .unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=20)
            .map(|i| vec![i.to_string(), format!("v{i}")])
            .collect();
        insert_rows(&mut conn, "t", &["id".into(), "k".into()], &rows).unwrap();

        let p = page_rows(&conn, "t", Some("k"), false, None, 5).unwrap();
        let cur = p.next_cursor.unwrap();
        // 换个排序键
        assert!(page_rows(&conn, "t", Some("id"), false, Some(&cur), 5).is_err());
        // 方向反了
        assert!(page_rows(&conn, "t", Some("k"), true, Some(&cur), 5).is_err());
        // 按 rowid 排序
        assert!(page_rows(&conn, "t", None, false, Some(&cur), 5).is_err());
        // 乱改游标
        assert!(page_rows(&conn, "t", Some("k"), false, Some("不是json"), 5).is_err());
        assert!(page_rows(&conn, "t", Some("k"), false, Some(r#"{"o":"k","d":false}"#), 5).is_err());
        // 一致就正常
        assert!(page_rows(&conn, "t", Some("k"), false, Some(&cur), 5).is_ok());
    }

    #[test]
    fn without_rowid表被明确拒绝() {
        let conn = mem();
        conn.execute_batch("CREATE TABLE wr (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID")
            .unwrap();
        let mut conn = conn;
        ins(&mut conn, "wr", &["k", "v"], &[vec!["a", "1"]]);
        let e = page_rows(&conn, "wr", None, false, None, 10).unwrap_err();
        assert!(e.contains("WITHOUT ROWID"), "{e}");
        assert!(update_cell(&conn, "wr", 1, "v", Some("2")).is_err());
        assert!(delete_rows(&conn, "wr", &[1]).is_err());
        // 但读结构、改名、删表这些不依赖 rowid 的操作仍然可以
        assert_eq!(get_table(&conn, "wr").unwrap().columns.len(), 2);
        assert_eq!(run_query(&conn, "SELECT count(*) FROM wr", 10).unwrap().rows[0][0], Json::from(1));
    }

    #[test]
    fn 字段遮蔽rowid的表也拒绝分页() {
        let conn = mem();
        // 外部建的表（我们的 DDL 不允许这种字段名）
        conn.execute_batch("CREATE TABLE shadow (rowid INTEGER, v TEXT)")
            .unwrap();
        let e = page_rows(&conn, "shadow", None, false, None, 10).unwrap_err();
        assert!(e.contains("遮蔽"), "{e}");
    }

    // ---------- 数据读写 ----------

    #[test]
    fn 插入含空值_中文_emoji_超长文本() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("内容", ColType::Text)]),
        )
        .unwrap();
        let mut conn = conn;
        let long = "长".repeat(100_000);
        let n = insert_rows(
            &mut conn,
            "t",
            &["内容".to_string()],
            &[
                vec![String::new()],          // 空串（文本列保留空串）
                vec!["张三🙂李四".to_string()],
                vec![long.clone()],
                vec!["删\n行\t符".to_string()],
            ],
        )
        .unwrap();
        assert_eq!(n, 4);

        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows.len(), 4);
        assert_eq!(p.rows[0][2], Json::String(String::new()));
        assert_eq!(p.rows[1][2], Json::String("张三🙂李四".to_string()));
        assert_eq!(p.rows[2][2].as_str().unwrap().chars().count(), 100_000);
        assert_eq!(p.rows[3][2], Json::String("删\n行\t符".to_string()));

        // NULL 和空串是两回事，都能写进去
        update_cell(&conn, "t", 1, "内容", None).unwrap();
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::Null);
        update_cell(&conn, "t", 1, "内容", Some("")).unwrap();
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::String(String::new()));
    }

    #[test]
    fn 列表值不匹配时整批回滚() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("v", ColType::Text)]),
        )
        .unwrap();
        let mut conn = conn;
        let e = insert_rows(
            &mut conn,
            "t",
            &["v".to_string()],
            &[vec!["好的".to_string()], vec!["坏的".to_string(), "多出来的".to_string()]],
        )
        .unwrap_err();
        assert!(e.contains("第 2 行"), "{e}");
        // 第一行也不该留下
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 0);
        // 列不存在
        let e = insert_rows(&mut conn, "t", &["没有这列".to_string()], &[vec!["x".to_string()]])
            .unwrap_err();
        assert!(e.contains("没有字段"), "{e}");
    }

    #[test]
    fn 金额按分存整数且拒绝浮点() {
        let conn = mem();
        create_table(
            &conn,
            &spec("账", vec![pk("id", ColType::Integer), c("金额", ColType::Money)]),
        )
        .unwrap();
        let mut conn = conn;
        ins(
            &mut conn,
            "账",
            &["金额"],
            &[vec!["12.34"], vec!["¥1,234.50"], vec!["-0.05"], vec![".5"], vec![""], vec!["12元"]],
        );
        let p = page_rows(&conn, "账", None, false, None, 10).unwrap();
        let vals: Vec<&Json> = p.rows.iter().map(|r| &r[2]).collect();
        assert_eq!(vals[0].as_i64(), Some(1234));
        assert_eq!(vals[1].as_i64(), Some(123450));
        assert_eq!(vals[2].as_i64(), Some(-5));
        assert_eq!(vals[3].as_i64(), Some(50));
        assert_eq!(*vals[4], Json::Null, "空串按清空处理");
        assert_eq!(vals[5].as_i64(), Some(1200));
        // 存的是整数（不是浮点）
        for v in &vals[..4] {
            assert!(v.is_i64() || v.is_u64(), "{v:?} 必须是整数");
            assert!(!v.is_f64());
        }
        // SQLite 侧也确实是整数，求和是精确整数运算
        let r = run_query(&conn, "SELECT typeof(金额), sum(金额) FROM 账", 10).unwrap();
        assert_eq!(r.rows[0][0], Json::String("integer".to_string()));
        assert_eq!(r.rows[0][1].as_i64(), Some(1234 + 123450 - 5 + 50 + 1200));

        // 超过两位小数：报错，不四舍五入
        let e = update_cell(&conn, "账", 1, "金额", Some("12.345")).unwrap_err();
        assert!(e.contains("小数"), "{e}");
        assert!(update_cell(&conn, "账", 1, "金额", Some("12.3")).is_ok());
        // 非数字
        assert!(update_cell(&conn, "账", 1, "金额", Some("一百块")).is_err());
        // 模式层的 CHECK：绕过界面直接写小数也会被拦住
        let e = conn
            .execute("UPDATE 账 SET 金额 = 12.5", [])
            .unwrap_err()
            .to_string();
        assert!(e.contains("CHECK"), "{e}");
        // 元换算的显示函数
        assert_eq!(money_display(1234), "12.34");
        assert_eq!(money_display(-5), "-0.05");
        assert_eq!(money_display(0), "0.00");
        assert_eq!(money_parse("0.05").unwrap(), 5);

        // 给已有的表加金额列：CHECK 也要一起加上（SQLite 的 ADD COLUMN 允许列级 CHECK）
        let mut col = c("手续费", ColType::Money);
        col.not_null = true;
        col.default = Some("0".to_string());
        add_column(&conn, "账", &col).unwrap();
        let metas = column_meta(&conn, "账").unwrap();
        assert_eq!(metas[2].semantic, Some(ColType::Money), "加列时语义类型要记下来");
        assert_eq!(metas[2].comment, None);
        // 旧行拿到默认值 0，新行仍然按"分"写入
        let p = page_rows(&conn, "账", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][3].as_i64(), Some(0));
        update_cell(&conn, "账", 1, "手续费", Some("0.5")).unwrap();
        let p = page_rows(&conn, "账", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][3].as_i64(), Some(50));
        // 新加的金额列同样挡浮点
        assert!(conn.execute("UPDATE 账 SET 手续费 = 1.5", []).is_err());
    }

    #[test]
    fn 是否列写入被规范化成01() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("开着", ColType::Boolean)]),
        )
        .unwrap();
        let mut conn = conn;
        ins(
            &mut conn,
            "t",
            &["开着"],
            &[vec!["是"], vec!["否"], vec!["TRUE"], vec!["false"], vec!["1"], vec!["0"], vec!["有"], vec![""]],
        );
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        let got: Vec<Option<i64>> = p.rows.iter().map(|r| r[2].as_i64()).collect();
        assert_eq!(
            got,
            vec![Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), None]
        );
        assert!(update_cell(&conn, "t", 1, "开着", Some("也许")).is_err());
    }

    #[test]
    fn 日期与日期时间会归一化() {
        let conn = mem();
        create_table(
            &conn,
            &spec(
                "t",
                vec![pk("id", ColType::Integer), c("日", ColType::Date), c("时刻", ColType::DateTime)],
            ),
        )
        .unwrap();
        let mut conn = conn;
        ins(
            &mut conn,
            "t",
            &["日", "时刻"],
            &[
                vec!["2026/9/7", "2026-09-07T08:05"],
                vec!["2026年9月7日", "2026/9/7 8:05:30"],
                vec!["20260907", "2026-09-07"],
                vec!["2026-9-7 0:00", "2026-09-07 08:05:30.123"],
            ],
        );
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::String("2026-09-07".to_string()));
        assert_eq!(p.rows[0][3], Json::String("2026-09-07 08:05:00".to_string()));
        assert_eq!(p.rows[1][2], Json::String("2026-09-07".to_string()));
        assert_eq!(p.rows[1][3], Json::String("2026-09-07 08:05:30".to_string()));
        assert_eq!(p.rows[2][2], Json::String("2026-09-07".to_string()));
        assert_eq!(p.rows[2][3], Json::String("2026-09-07 00:00:00".to_string()));
        assert_eq!(p.rows[3][2], Json::String("2026-09-07".to_string()));
        assert_eq!(p.rows[3][3], Json::String("2026-09-07 08:05:30".to_string()));

        // 定长 ISO 之后，字符串排序 = 日期排序
        assert!(normalize_date("2026-9-7").unwrap() < normalize_date("2026-10-05").unwrap());

        // 不存在的日期、识别不了的格式要报错
        assert!(update_cell(&conn, "t", 1, "日", Some("2026-02-30")).is_err());
        assert!(update_cell(&conn, "t", 1, "日", Some("2026-13-01")).is_err());
        assert!(update_cell(&conn, "t", 1, "日", Some("9/17/2026")).is_err());
        assert!(update_cell(&conn, "t", 1, "日", Some("今天")).is_err());
        assert!(update_cell(&conn, "t", 1, "时刻", Some("2026-09-07 25:00")).is_err());
        // 闰年
        assert!(normalize_date("2024-02-29").is_ok());
        assert!(normalize_date("2026-02-29").is_err());
    }

    #[test]
    fn json列写入前会校验() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("配置", ColType::Json)]),
        )
        .unwrap();
        let mut conn = conn;
        ins(&mut conn, "t", &["配置"], &[vec![r#"{"a": 1, "b": [2,3]}"#]]);
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        // 原样存（不改写用户的排版）
        assert_eq!(p.rows[0][2], Json::String(r#"{"a": 1, "b": [2,3]}"#.to_string()));
        // 非法 JSON 报错
        let e = update_cell(&conn, "t", 1, "配置", Some("{不是 json")).unwrap_err();
        assert!(e.contains("JSON"), "{e}");
        // 空串按清空处理
        update_cell(&conn, "t", 1, "配置", Some("")).unwrap();
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::Null);
    }

    #[test]
    fn 二进制列不能从表格写入() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("图", ColType::Blob)]),
        )
        .unwrap();
        let mut conn = conn;
        let e = insert_rows(&mut conn, "t", &["图".to_string()], &[vec!["abc".to_string()]])
            .unwrap_err();
        assert!(e.contains("二进制"), "{e}");
        // SQL 路径可以写，但读回来只给长度占位符（不把二进制塞进 JSON）
        conn.execute("INSERT INTO t (图) VALUES (x'00ff10')", [])
            .unwrap();
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::String("〔二进制 3 字节〕".to_string()));
        assert!(update_cell(&conn, "t", 1, "图", Some("x")).is_err());
    }

    #[test]
    fn 整数与小数列的强转() {
        let conn = mem();
        create_table(
            &conn,
            &spec(
                "t",
                vec![pk("id", ColType::Integer), c("n", ColType::Integer), c("x", ColType::Real)],
            ),
        )
        .unwrap();
        let mut conn = conn;
        ins(
            &mut conn,
            "t",
            &["n", "x"],
            &[vec!["123", "1.5"], vec!["1,000", "-0.25"], vec!["12.0", "1e3"], vec!["", ""]],
        );
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2].as_i64(), Some(123));
        assert_eq!(p.rows[1][2].as_i64(), Some(1000), "千分位要能识别");
        assert_eq!(p.rows[2][2].as_i64(), Some(12), "12.0 是整数");
        assert_eq!(p.rows[0][3].as_f64(), Some(1.5));
        assert_eq!(p.rows[2][3].as_f64(), Some(1000.0));
        assert_eq!(p.rows[3][2], Json::Null);

        for bad in ["abc", "12.5", "1e400", "NaN"] {
            let e = update_cell(&conn, "t", 1, "n", Some(bad)).unwrap_err();
            assert!(e.contains("整数"), "{bad} → {e}");
        }
        assert!(update_cell(&conn, "t", 1, "x", Some("abc")).is_err());
    }

    #[test]
    fn 不是本工具建的表不做类型强转() {
        let conn = mem();
        // 声明类型认不出来 → 原样存
        conn.execute_batch("CREATE TABLE ext (k TEXT, v WEIRDTEXT)").unwrap();
        let mut conn = conn;
        ins(&mut conn, "ext", &["k", "v"], &[vec!["abc", "123"]]);
        let p = page_rows(&conn, "ext", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::String("123".to_string()));
        // 认得的声明类型仍然按语义强转（VARCHAR → 文本）
        conn.execute_batch("CREATE TABLE ext2 (v VARCHAR(20))").unwrap();
        ins(&mut conn, "ext2", &["v"], &[vec!["中文"]]);
        let p = page_rows(&conn, "ext2", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][1], Json::String("中文".to_string()));
    }

    #[test]
    fn 更新单元格_行不存在时给可读错误() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("v", ColType::Text)]),
        )
        .unwrap();
        let mut conn = conn;
        ins(&mut conn, "t", &["v"], &[vec!["一"]]);
        update_cell(&conn, "t", 1, "v", Some("二")).unwrap();
        let p = page_rows(&conn, "t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][2], Json::String("二".to_string()));
        // 行不存在
        let e = update_cell(&conn, "t", 999, "v", Some("三")).unwrap_err();
        assert!(e.contains("没有找到"), "{e}");
        // 列不存在
        let e = update_cell(&conn, "t", 1, "没有这列", Some("三")).unwrap_err();
        assert!(e.contains("没有字段"), "{e}");
    }

    #[test]
    fn 删除多行返回条数() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("v", ColType::Text)]),
        )
        .unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=10).map(|i| vec![i.to_string()]).collect();
        insert_rows(&mut conn, "t", &["v".to_string()], &rows.iter().cloned().collect::<Vec<_>>())
            .unwrap();
        assert_eq!(delete_rows(&conn, "t", &[]).unwrap(), 0);
        assert_eq!(delete_rows(&conn, "t", &[2, 4, 6]).unwrap(), 3);
        // 已经删掉的行再删不会报错，但也不计数
        assert_eq!(delete_rows(&conn, "t", &[2, 99]).unwrap(), 0);
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 10);
        let left = drain(&conn, "t", None, false, 3);
        assert_eq!(left, vec![1, 3, 5, 7, 8, 9, 10]);
        // 大批量删除会分批（不会撞上参数个数上限）
        let ids: Vec<i64> = (1..=1000).collect();
        let mut conn2 = mem();
        create_table(
            &conn2,
            &spec("big", vec![pk("id", ColType::Integer), c("v", ColType::Text)]),
        )
        .unwrap();
        let rows: Vec<Vec<String>> = ids.iter().map(|i| vec![i.to_string()]).collect();
        insert_rows(&mut conn2, "big", &["v".to_string()], &rows).unwrap();
        assert_eq!(delete_rows(&conn2, "big", &ids).unwrap(), 1000);
    }

    // ---------- 查询执行 ----------

    #[test]
    fn 查询判据_读走结果集_写回报影响行数() {
        let conn = mem();
        create_table(
            &conn,
            &spec("t", vec![pk("id", ColType::Integer), c("v", ColType::Text)]),
        )
        .unwrap();

        // 写：INSERT / UPDATE / DELETE 都返回 affected
        let r = run_query(&conn, "INSERT INTO t (v) VALUES ('a'), ('b')", 100).unwrap();
        assert_eq!(r.affected, 2);
        assert!(r.columns.is_empty() && r.rows.is_empty());
        let r = run_query(&conn, "UPDATE t SET v = 'c' WHERE id = 1", 100).unwrap();
        assert_eq!(r.affected, 1);
        let r = run_query(&conn, "DELETE FROM t WHERE id = 2", 100).unwrap();
        assert_eq!(r.affected, 1);
        // DDL 也是写，影响 0 行
        let r = run_query(&conn, "CREATE INDEX idx_t_v ON t(v)", 100).unwrap();
        assert_eq!(r.affected, 0);

        // 读：SELECT / PRAGMA / EXPLAIN / WITH 都走结果集
        let r = run_query(&conn, "SELECT id, v FROM t", 100).unwrap();
        assert_eq!(r.columns, vec!["id".to_string(), "v".to_string()]);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.affected, 0);
        let r = run_query(&conn, "PRAGMA table_info(t)", 100).unwrap();
        assert!(!r.columns.is_empty(), "PRAGMA 应该走结果集路径");
        let r = run_query(&conn, "EXPLAIN SELECT 1", 100).unwrap();
        assert!(!r.rows.is_empty());
        let r = run_query(&conn, "WITH x AS (SELECT 2 AS n) SELECT n FROM x", 100).unwrap();
        assert_eq!(r.rows[0][0], Json::from(2));
        // 带前导注释与块注释也能认出首关键字
        let r = run_query(&conn, "-- 说明\n/* 也是注释 */SELECT 7 AS n", 100).unwrap();
        assert_eq!(r.rows[0][0], Json::from(7));
        assert!(run_query(&conn, "-- 只有注释", 100).is_err());
        // 不返回结果集的 PRAGMA（判据说"读"，但一行数据都没有）不该报错
        let r = run_query(&conn, "PRAGMA foreign_keys = ON", 100).unwrap();
        assert!(r.rows.is_empty() && r.columns.is_empty());
        assert_eq!(r.affected, 0, "PRAGMA 不改数据行");
        // 带 RETURNING 的写语句：既有结果行，也要如实报影响行数
        let r = run_query(&conn, "INSERT INTO t (v) VALUES ('z') RETURNING id", 100).unwrap();
        assert_eq!(r.columns, vec!["id".to_string()]);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.affected, 1);

        // elapsed_ms 有值
        assert!(r.elapsed_ms >= 0);
    }

    #[test]
    fn 查询会被行数上限截断() {
        let conn = mem();
        create_table(&conn, &spec("t", vec![pk("id", ColType::Integer)])).unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=200).map(|i| vec![i.to_string()]).collect();
        insert_rows(&mut conn, "t", &["id".to_string()], &rows).unwrap();

        let r = run_query(&conn, "SELECT * FROM t", 50).unwrap();
        assert_eq!(r.rows.len(), 50);
        assert!(r.truncated, "被截断时必须给出信号");
        // 上限之内不截断
        let r = run_query(&conn, "SELECT * FROM t", 1000).unwrap();
        assert_eq!(r.rows.len(), 200);
        assert!(!r.truncated);
        // 正好等于上限：没有第 201 行，所以不算截断
        let r = run_query(&conn, "SELECT * FROM t", 200).unwrap();
        assert_eq!(r.rows.len(), 200);
        assert!(!r.truncated);
        // 0 当作 1（避免"0 = 不限制"这种危险约定）
        let r = run_query(&conn, "SELECT * FROM t", 0).unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn 多语句被拒绝且不会破坏数据() {
        let conn = mem();
        create_table(&conn, &spec("t", vec![pk("id", ColType::Integer)])).unwrap();
        let e = run_query(&conn, "SELECT 1; DROP TABLE t", 10).unwrap_err();
        assert!(e.contains("一条"), "{e}");
        assert!(get_table(&conn, "t").is_ok(), "第二条语句绝不能被执行");
        // 末尾分号是允许的
        assert!(run_query(&conn, "SELECT 1;", 10).is_ok());
        // 认不出开头
        assert!(run_query(&conn, "1 + 1", 10).is_err());
        // 语法错误原样透出（带表名/字段名，比我们瞎猜有用）
        assert!(run_query(&conn, "SELECT * FROM 没有这张表", 10).is_err());
    }

    #[test]
    fn with开头的写操作也会跑完并回报影响行数() {
        let conn = mem();
        create_table(&conn, &spec("t", vec![pk("id", ColType::Integer)])).unwrap();
        let mut conn = conn;
        let rows: Vec<Vec<String>> = (1..=10).map(|i| vec![i.to_string()]).collect();
        insert_rows(&mut conn, "t", &["id".to_string()], &rows).unwrap();
        // 首关键字是 WITH → 判据说"读"，但 SQLite 知道它会写库：
        // 不能提前截断，影响行数要如实回报
        let r = run_query(
            &conn,
            "WITH d AS (SELECT id FROM t WHERE id <= 4) DELETE FROM t WHERE id IN (SELECT id FROM d)",
            1000,
        )
        .unwrap();
        assert_eq!(r.affected, 4);
        assert_eq!(get_table(&conn, "t").unwrap().row_estimate, 10);
        let left = drain(&conn, "t", None, false, 100);
        assert_eq!(left, vec![5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn 危险操作粗判() {
        assert!(needs_confirm("DROP TABLE t").is_some());
        assert!(needs_confirm("DELETE FROM t").is_some());
        assert!(needs_confirm("UPDATE t SET a = 1").is_some());
        assert!(needs_confirm("DELETE FROM t WHERE id = 1").is_none());
        assert!(needs_confirm("UPDATE t SET a = 1 WHERE id = 2").is_none());
        assert!(needs_confirm("SELECT * FROM t WHERE id = 1").is_none());
        assert!(needs_confirm("INSERT INTO t VALUES (1)").is_none());
        // 字符串里的 where 不算
        assert!(needs_confirm("UPDATE t SET a = 'where'").is_some());
    }

    #[test]
    fn where_藏在子查询里不算过滤条件() {
        // 第十二轮交接点名的确认漏网：词面有 WHERE，但它在括号里，
        // 约束的是子查询 —— 外层 UPDATE 仍然改全表，必须弹确认。
        assert!(needs_confirm("UPDATE t SET a = (SELECT 1 WHERE 1=1);").is_some());
        assert!(needs_confirm("UPDATE 客户 SET 电话=(SELECT 1 WHERE 1=1);").is_some());
        // 顶层 WHERE 才算数；子查询和顶层 WHERE 并存时也认得出
        assert!(needs_confirm("DELETE FROM t WHERE id IN (SELECT id FROM x)").is_none());
        // 注释与字符串里的括号不影响深度计数
        assert!(needs_confirm("UPDATE t SET a = '(where' -- (where\n").is_some());
        assert!(needs_confirm("UPDATE t SET a = 1 /* ( */ WHERE id = 1").is_none());
    }

    #[test]
    fn 危险语句闸门不认恒真谓词() {
        // P2 加固：顶层 WHERE 之后若只是恒真谓词，仍当作"没有真正的过滤条件"，
        // 必须弹确认，堵住 `WHERE 1=1` 这类绕过确认、实则改/删全表的写法。
        // 恒真的几种写法都要报确认：
        assert!(
            needs_confirm("DELETE FROM t WHERE 1=1").is_some(),
            "WHERE 1=1 必须弹确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE 1 = 1").is_some(),
            "WHERE 1 = 1 必须弹确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE 1").is_some(),
            "WHERE 1 必须弹确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE true").is_some(),
            "WHERE true 必须弹确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE (1=1)").is_some(),
            "WHERE (1=1) 必须弹确认"
        );
        assert!(
            needs_confirm("UPDATE t SET a=1 WHERE 1=1 AND true").is_some(),
            "纯恒真 AND 组合必须弹确认"
        );
        // 带真实条件就不算恒真：不报确认
        assert!(
            needs_confirm("DELETE FROM t WHERE 1=1 AND a=2").is_none(),
            "WHERE 1=1 AND a=2 含真实条件，不报确认"
        );
        assert!(
            needs_confirm("UPDATE t SET a=1 WHERE id=5 AND 1=1").is_none(),
            "真实条件在前、恒真在后，不报确认"
        );
        // 字符串里的恒真不算：字符串内容不会凭空造出过滤条件
        assert!(
            needs_confirm("DELETE FROM t WHERE x = '1=1'").is_none(),
            "字符串里的 1=1 是字面量比较，算真实过滤"
        );
        // 子查询里的恒真不影响外层判定
        assert!(
            needs_confirm("DELETE FROM t WHERE (SELECT 1 WHERE 1=1)").is_some(),
            "子查询里的恒真不替外层兜底，外层无真实过滤要弹确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE id IN (SELECT id FROM x WHERE 1=1)").is_none(),
            "子查询恒真不影响外层真实过滤"
        );
        // BETWEEN … AND … 里的 AND 不是合取分隔符
        assert!(
            needs_confirm("DELETE FROM t WHERE a BETWEEN 1 AND 2").is_none(),
            "BETWEEN 1 AND 2 是真实范围条件，不报确认"
        );
        assert!(
            needs_confirm("DELETE FROM t WHERE 1=1 OR a=2").is_some(),
            "1=1 OR a=2 整条恒真，要弹确认"
        );
    }

    // ---------- 筛选分页（page_rows_filtered） ----------

    #[test]
    fn 筛选按包含匹配且大小写不敏感() {
        let mut conn = mem();
        create_table(
            &conn,
            &spec(
                "客户",
                vec![c("名称", ColType::Text), c("金额", ColType::Money)],
            ),
        )
        .unwrap();
        let rows: Vec<Vec<String>> = vec![
            vec!["华为手机".into(), "1234.56".into()],
            vec!["小米电视".into(), "99.00".into()],
            vec!["Huawei Pad".into(), "5.00".into()],
        ];
        insert_rows(&mut conn, "客户", &["名称".into(), "金额".into()], &rows).unwrap();

        let f = |kw: &str| vec![("名称".to_string(), kw.to_string())];
        let page = page_rows_filtered(&conn, "客户", None, false, None, 100, &f("华为")).unwrap();
        assert_eq!(page.rows.len(), 1, "中文关键词命中中文行");
        let page = page_rows_filtered(&conn, "客户", None, false, None, 100, &f("HUA")).unwrap();
        assert_eq!(page.rows.len(), 1, "关键词大小写不敏感（ASCII）");
        // 数字列按文本筛：金额 1234.56 能被 "1234" 命中
        let page = page_rows_filtered(
            &conn,
            "客户",
            None,
            false,
            None,
            100,
            &[("金额".to_string(), "1234".to_string())],
        )
        .unwrap();
        assert_eq!(page.rows.len(), 1);
        // 空关键词被忽略 = 全量
        let page = page_rows_filtered(&conn, "客户", None, false, None, 100, &f("")).unwrap();
        assert_eq!(page.rows.len(), 3);
        // 不存在的字段要报错，不能静默不过滤（注意这里必须直接给"地址"这个列名）
        assert!(
            page_rows_filtered(
                &conn,
                "客户",
                None,
                false,
                None,
                100,
                &[("地址".to_string(), "x".to_string())]
            )
            .is_err()
        );
    }

    /// 分页应答的字段名是**界面依赖的契约**：Rust 的 snake_case 与 JS 读取的名字必须逐字对上。
    ///
    /// 为什么值得专门钉一个测试：v0.2.0-beta.1 出过一个 P1 —— `db.js` 里写成
    /// `page.hasMore`，而 serde 序列化出来的是 `has_more`，值恒为 `undefined`，
    /// 于是超过一页的表永远翻不动。**测试全绿、界面自检全绿，只有真的去翻页才会发现。**
    /// 这里把两头都钉住：改字段名而不改界面，这个测试就会红。
    #[test]
    fn 分页应答的字段名与界面读取的名字一致() {
        // ① Rust 侧序列化出来的键（snake_case，本模块没有 rename_all）
        let page = Page {
            rows: vec![vec![Json::from(1)]],
            columns: vec![ROWID_COLUMN.to_string()],
            has_more: true,
            next_cursor: Some("x".to_string()),
        };
        let obj = serde_json::to_value(&page)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        for k in ["rows", "columns", "has_more", "next_cursor"] {
            assert!(obj.contains_key(k), "分页应答缺少字段 {k}：界面会读到 undefined");
        }
        assert!(
            !obj.contains_key("hasMore"),
            "分页应答不该出现驼峰字段 —— 界面要的是 has_more"
        );

        // ② 界面侧必须按这些名字读（源码级核对，防止再写错）
        let js = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/db.js"),
        )
        .expect("读不到 app/ui/db.js —— 这个测试要核对界面读取的字段名");
        for name in ["has_more", "next_cursor"] {
            assert!(
                js.contains(&format!("page.{name}")),
                "app/ui/db.js 里没有 `page.{name}`：字段名对不上时不会报错，只会静默拿到 \
                 undefined（这个坑已经踩过一次，见 BUG_HUNT-2026-09-18.md P1-1）"
            );
        }
    }

    /// 金额列的默认值必须换算成整数分。
    ///
    /// 反例说明为什么值得测：`DEFAULT 12.34` 在 INTEGER 列上写的是 REAL，
    /// 撞上建表时钉的 `CHECK (typeof(x) IN ('integer','null'))` ——
    /// **建表会成功，第一次插入才报错**，而报错信息完全指不到"默认值"上。
    #[test]
    fn 金额列的默认值按元换算成整数分() {
        // 界面会把用户填的「元」包成字符串字面量
        assert_eq!(money_default_to_cents("'12.34'").unwrap(), "1234");
        assert_eq!(money_default_to_cents("12.34").unwrap(), "1234");
        assert_eq!(money_default_to_cents("-0.05").unwrap(), "-5");
        assert_eq!(money_default_to_cents("0").unwrap(), "0");
        // 关键字原样放行
        assert_eq!(money_default_to_cents("NULL").unwrap(), "NULL");
        assert_eq!(
            money_default_to_cents("CURRENT_TIMESTAMP").unwrap(),
            "CURRENT_TIMESTAMP"
        );
        // 校验白拿了 money_parse 的那一套
        assert!(money_default_to_cents("'12.345'").is_err(), "三位小数必须被拒");
        assert!(money_default_to_cents("'未结清'").is_err(), "非数字必须被拒");

        // DDL 层面确认真的写成了整数，且原来的"12.34"字样不再出现
        let col = ColumnDef {
            default: Some("'12.34'".to_string()),
            ..c("金额", ColType::Money)
        };
        let ddl = column_ddl(&col, true).unwrap();
        assert!(ddl.contains("DEFAULT 1234"), "DDL 应为 DEFAULT 1234：{ddl}");
        assert!(!ddl.contains("12.34"), "DDL 里不该出现实数：{ddl}");

        // 端到端：建表后不给金额列赋值，默认值要能落成 1234 分
        let mut conn = mem();
        create_table(&conn, &spec("报销", vec![c("事项", ColType::Text), col])).unwrap();
        ins(&mut conn, "报销", &["事项"], &[vec!["打车"]]);
        let page = page_rows(&conn, "报销", None, false, None, 10).unwrap();
        // rows[i] = [_rowid, 事项, 金额]
        assert_eq!(
            page.rows[0][2].as_i64(),
            Some(1234),
            "默认值应落成 1234 分，实际：{:?}",
            page.rows[0][2]
        );
    }

    /// 中断句柄能真的打断一条正在跑的查询（Q-042 的底层能力）。
    ///
    /// 为什么在**这里**测：IPC 那一层（`main.rs` 的 `run_query_async`）跑不了单测
    /// —— 它需要一个事件循环与 WebView。但它成立的前提只有一条：另一个线程
    /// 按住中断句柄能不能真的让 statement 停下来。这一条测住了，剩下的
    /// （超时哨兵、执行中再点一次）就只是调度问题。
    #[test]
    fn 中断句柄能打断正在执行的查询() {
        let conn = mem();
        let long = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000000) \
                    SELECT sum(x) FROM c";
        // 先确认这个 SQL 本身合法：同一个形状把上限调小，必须能跑完。
        // （否则"报错了"可能只是语法错，测不出中断到底有没有用。）
        let short = long.replace("3000000", "1000");
        assert!(
            run_query(&conn, &short, 10).is_ok(),
            "参照查询应当能跑完 —— 跑不完说明这条 SQL 本身有问题"
        );

        let handle = conn.get_interrupt_handle();
        let killer = std::thread::spawn(move || {
            // 给查询一点时间真正跑起来，再按中断
            std::thread::sleep(std::time::Duration::from_millis(150));
            handle.interrupt();
        });
        let r = run_query(&conn, long, 10);
        killer.join().unwrap();
        assert!(
            r.is_err(),
            "被中断的查询必须返回错误，而不是继续跑到底 —— 说明 interrupt 没接上"
        );
    }

    /// 读一个界面脚本的源码（字段名核对用）。
    fn ui_src(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ui")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 app/ui/{name}：{e}"))
    }

    /// 去掉 JS 的**行注释**（`//` 到行尾），字符串里的 `//` 不算。
    ///
    /// 为什么要去注释：注释里为了讲清这个坑，会**写出错误的字段名**
    /// （"不能写成 `elapsedMs`"）。不去掉，门禁会被自己的说明文字弄红 ——
    /// 那等于逼着后来者删掉说明，是本项目的红线（D-040 的精神）。
    fn strip_line_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for line in src.lines() {
            let b: Vec<char> = line.chars().collect();
            let mut quote: Option<char> = None;
            let mut cut = b.len();
            let mut i = 0;
            while i < b.len() {
                let c = b[i];
                match quote {
                    Some(q) => {
                        if c == '\\' {
                            i += 2;
                            continue;
                        }
                        if c == q {
                            quote = None;
                        }
                    }
                    None => {
                        if c == '"' || c == '\'' || c == '`' {
                            quote = Some(c);
                        } else if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
                            cut = i;
                            break;
                        }
                    }
                }
                i += 1;
            }
            out.extend(b[..cut].iter());
            out.push('\n');
        }
        out
    }

    /// `has_more` → `hasMore`
    fn to_lower_camel(field: &str) -> String {
        let mut out = String::new();
        let mut up = false;
        for ch in field.chars() {
            if ch == '_' {
                up = true;
                continue;
            }
            if up {
                out.extend(ch.to_uppercase());
                up = false;
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// 跨语言字段名的**通用**门禁：凡是用 serde 默认（snake_case）序列化的结构体字段，
    /// 界面里都不允许出现它的驼峰写法。
    ///
    /// 上面那个 `分页应答的字段名…` 测试只钉住 `Page` 的两个字段；这一条把
    /// 「凡是跨语言边界传字段都要逐个核对」变成机器可查。它已经被踩过两次：
    ///   · `Page.has_more` 写成 `hasMore` → 超过一页的表永远翻不动
    ///   · `QueryResult.elapsed_ms` 写成 `elapsedMs` → 每条语句的耗时与总耗时永远不显示
    /// 两次都是"测试全绿、自检全绿、界面看起来正常"。
    ///
    /// 只查 `db.js` / `sql.js`（直接读 IPC 应答的两个文件）。`grid.js` 不查：
    /// 它吃的是 `db.js` 转好的**驼峰**对象，那是刻意的内部契约，不是 Rust 那侧的名字。
    #[test]
    fn 跨语言字段名不得在界面里写成驼峰() {
        const FIELDS: &[&str] = &[
            "has_more",
            "next_cursor",
            "elapsed_ms",
            "row_estimate",
            "decl_type",
            "not_null",
        ];

        for file in ["db.js", "sql.js"] {
            let code = strip_line_comments(&ui_src(file));
            for f in FIELDS {
                let camel = to_lower_camel(f);
                if camel == *f {
                    continue; // 没有下划线的字段不存在这个问题
                }
                let bad = format!(".{camel}");
                assert!(
                    !code.contains(&bad),
                    "app/ui/{file} 里出现了 `{bad}` —— Rust 侧序列化出来的是 `{f}`。\
                     写成驼峰不会报错，只会静默拿到 undefined（这个坑踩过两次，\
                     见 BUG_HUNT-2026-09-18.md）"
                );
            }
        }

        // 反向：Rust 侧序列化出来的键必须就是这些名字（防止有人给结构体加 rename_all）
        let q = QueryResult {
            columns: vec!["a".into()],
            rows: Vec::new(),
            truncated: false,
            elapsed_ms: 1,
            affected: 0,
        };
        let qv = serde_json::to_value(&q).unwrap();
        let qo = qv.as_object().unwrap();
        for k in ["columns", "rows", "truncated", "elapsed_ms", "affected"] {
            assert!(qo.contains_key(k), "QueryResult 缺少字段 {k}：界面会读到 undefined");
        }
        let t = TableInfo {
            name: "t".into(),
            comment: None,
            columns: Vec::new(),
            row_estimate: -1,
        };
        let tv = serde_json::to_value(&t).unwrap();
        assert!(tv.as_object().unwrap().contains_key("row_estimate"));

        // 该按 snake 读的地方确实读了 —— 否则上面两条只要把读法删掉就能过
        let db = strip_line_comments(&ui_src("db.js"));
        for probe in ["page.has_more", "page.next_cursor", "t.row_estimate"] {
            assert!(db.contains(probe), "app/ui/db.js 里没有 `{probe}`");
        }
        let sql = strip_line_comments(&ui_src("sql.js"));
        assert!(
            sql.contains("res.elapsed_ms"),
            "app/ui/sql.js 里没有 `res.elapsed_ms`：耗时徽章会永远不显示"
        );
    }

    #[test]
    fn 筛选与游标分页可以叠加() {
        let mut conn = mem();
        create_table(&conn, &spec("t", vec![c("名", ColType::Text)])).unwrap();
        let rows: Vec<Vec<String>> = (1..=10)
            .map(|i| vec![if i % 2 == 0 { format!("偶{i}") } else { format!("奇{i}") }])
            .collect();
        insert_rows(&mut conn, "t", &["名".into()], &rows).unwrap();

        let f = vec![("名".to_string(), "偶".to_string())];
        // 筛选后只剩 5 行，一页取 2 行，翻完应恰好取到这 5 行、不重不漏
        let mut seen: Vec<i64> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let page =
                page_rows_filtered(&conn, "t", None, false, cursor.as_deref(), 2, &f).unwrap();
            for r in &page.rows {
                seen.push(r[0].as_i64().unwrap());
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        seen.sort();
        assert_eq!(seen, vec![2, 4, 6, 8, 10], "筛选 + keyset 翻页必须不重不漏");
    }

    // ---------- 注释元数据 ----------

    #[test]
    fn 表注释与字段注释可以写入与读回() {
        let conn = mem();
        let mut s = spec("客户", vec![pk("id", ColType::Integer), c("金额", ColType::Money)]);
        s.comment = Some("客户主表".to_string());
        s.columns[0].comment = Some("客户编号".to_string());
        create_table(&conn, &s).unwrap();

        assert_eq!(get_table(&conn, "客户").unwrap().comment.as_deref(), Some("客户主表"));
        let metas = column_meta(&conn, "客户").unwrap();
        assert_eq!(metas[0].comment.as_deref(), Some("客户编号"));
        // 金额列的语义类型记在元数据里（声明类型是 INTEGER，光看 DDL 看不出来）
        assert_eq!(metas[1].semantic, Some(ColType::Money));
        // 普通列的语义类型按声明类型反推
        assert_eq!(metas[0].semantic, Some(ColType::Integer));

        // 改注释
        set_table_comment(&conn, "客户", "改过的说明").unwrap();
        set_column_comment(&conn, "客户", "id", "改过的字段说明").unwrap();
        assert_eq!(get_table(&conn, "客户").unwrap().comment.as_deref(), Some("改过的说明"));
        let metas = column_meta(&conn, "客户").unwrap();
        assert_eq!(metas[0].comment.as_deref(), Some("改过的字段说明"));
        // 改字段注释不会把语义类型弄丢（金额仍然是金额）
        assert_eq!(metas[1].semantic, Some(ColType::Money));
        // 给不存在的字段写注释要报错
        assert!(set_column_comment(&conn, "客户", "没有这列", "x").is_err());
        // 空注释等于没注释
        set_table_comment(&conn, "客户", "").unwrap();
        assert_eq!(get_table(&conn, "客户").unwrap().comment, None);
    }

    #[test]
    fn 只读的库里没有元数据表也能列结构() {
        let conn = mem();
        // 库是"外面的"（没有 _db_ 表），读路径不能因为要读注释就把库写脏
        conn.execute_batch("CREATE TABLE 老表 (a TEXT, b INTEGER)")
            .unwrap();
        let list = list_tables(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "老表");
        assert_eq!(list[0].comment, None);
        let p = page_rows(&conn, "老表", None, false, None, 10).unwrap();
        assert_eq!(p.columns.len(), 3);
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE '_db_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "只读路径不该建元数据表");
        // 一旦要写（建表），元数据表才出现
        create_table(&conn, &spec("新表", vec![pk("id", ColType::Integer)])).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE '_db_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }
}
