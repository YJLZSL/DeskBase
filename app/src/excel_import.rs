//! Excel / CSV 导入建表：把「读文件 → 推断表头与类型 → 预览 → 落库」串起来。
//!
//! ## 分工（三个模块各管一段，谁都别越界）
//!
//! | 模块 | 负责 |
//! |------|------|
//! | `import_plan.rs` | **纯函数**：表头在第几行、每列是什么类型。不读文件、不碰数据库 |
//! | `xlsx.rs` / `csv_import.rs` | **只读文件**：把表读成字符串行（编码、单元格清洗都在那儿） |
//! | 本模块 | **编排**：把上面两段接起来，并把用户的调整带进落库 |
//!
//! **落库不经过 SQL**：本模块只调用 [`crate::model::Db`]（自研单文件存储，
//! 见 `docs/adr/0021`）。建表用 `Db::create_table`、写行用 `Db::insert_rows`、
//! 取消/失败清场用 `Db::drop_table`，表存在性用 `Db::get_table` 判断。
//!
//! ## 为什么计划里带路径、而不让前端传路径
//!
//! 与 `convert.rs` 同一条纪律：**路径只在 Rust 侧流转**。前端拿到的是一个
//! 一次性令牌（ULID），它既给不出路径也拿不到路径 —— 于是渲染层没有
//! 「读任意文件」的能力（docs/08 的「渲染层无文件系统直访」）。
//!
//! ## 两条字段命名约定（别混，混了两边都会静默失效）
//!
//! | 场合 | 用哪种 | 例子 |
//! |------|-------|------|
//! | **结构体**（serde 序列化给界面读） | snake_case（serde 默认） | `plan.header_row`、`col.source_index` |
//! | **请求参数键**（界面用 `json!` 传进来的） | camelCase | `planId`、`sheetIndex`、`headerRow` |
//!
//! 这是全项目既有约定（`AI_CONTEXT.md` 第 6 节）：serde 结构体走默认，
//! 手写的请求键跟 `convert.*` / `schema.*` 那批保持一致。`schema.rs` 的通用门禁
//! 会抓"结构体字段被写成驼峰"，但**抓不到请求键写错** —— 那种错误的表现是
//! "参数没生效、用了默认值"，比报错更难发现。所以这里单列一张表。
//!
//! ## 为什么"预览"和"导入"要各读一次文件
//!
//! 预览只要几十行，导入要整表。若把整表一直攥在手里等着用户点确认，
//! 一份 20 万行的表就是几十上百 MB 常驻内存 —— 而用户很可能看完预览就去
//! 干别的了。所以：预览读前 `PREVIEW_ROWS` 行，导入时**重新流式读一遍**
//! （`xlsx::for_each_row`，一次只持有一行）。
//!
//! 代价是文件被读两次（几 MB 的文件，可忽略）；换来的是内存与文件大小无关。
//! **两次读必须走同一套单元格转换**（都在 `xlsx.rs` 的 `cell_text` 与
//! `csv_import` 的解码里），否则"预览是对的、导进去不一样"。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::import_plan;
use crate::model;
use crate::model::Db;
use crate::xlsx;

pub type Result<T> = std::result::Result<T, String>;

/// 预览读多少行。要够推断类型（几百行足矣），也要够用户在界面上滚动确认表头。
const PREVIEW_ROWS: usize = 300;

/// 一次导入最多写多少行。
///
/// 比 `xlsx` 的 `MAX_ROWS`（50 万）小，是刻意的：那张表只保证"读得出来"，
/// 而这里是"要读进内存再写进去"。20 万行中文表格大约几十 MB —— 再大就该拆
/// 文件了，而不是让用户对着一个不确定会不会成功的窗口等。
pub const MAX_IMPORT_ROWS: usize = 200_000;

// ============================================================
// 计划
// ============================================================

/// 一列的导入建议。
///
/// 字段名一律 **snake_case** —— 这是全项目的约定（`AGENTS.md` 第 4 节、
/// `AI_CONTEXT.md` 第 6 节）：serde 结构体走默认的 snake_case，前端照读原样。
/// 界面里出现驼峰写法会被 `schema.rs` 的通用门禁抓出来
/// （`跨语言字段名不得在界面里写成驼峰`）。这条不是形式主义：
/// `has_more` 与 `elapsed_ms` 两次**静默失效**都是这么来的 ——
/// 值恒为 undefined，界面看起来一切正常。
#[derive(Debug, Clone, Serialize)]
pub struct PlannedColumn {
    /// 在原表里的列序号（0 基）。用户可能删掉某列，所以落库时要靠它找回原值 ——
    /// **不能用"用户选择后的顺序"当索引**，删一列就全错位了。
    pub source_index: usize,
    /// 原始表头文字（空表头会是 `空列1` 这种占位）
    pub original: String,
    /// 建议的字段名（已去重、已合法化）
    pub name: String,
    /// 建议类型（`model::ColType` 认的 snake_case 名字）
    pub ty: String,
    /// `high` / `low`。低置信度时界面要提醒用户看一眼再确认。
    pub confidence: String,
    /// 为什么这么判 —— 显示给用户，让类型建议变成"可以评判的"而不是黑箱
    pub reason: String,
    /// 几行样例值（已清洗后的样子）
    pub samples: Vec<String>,
    /// 这一列是否非空（全列都有值）。界面默认勾"必填"，用户可以改。
    pub not_null: bool,
}

/// 一次导入的完整计划。**不含文件路径** —— 路径在 [`Source`] 里，只留在 Rust 侧。
#[derive(Debug, Clone, Serialize)]
pub struct ImportPlan {
    pub file_name: String,
    /// 工作表名（CSV 为空串）
    pub sheet_name: String,
    pub sheets: Vec<String>,
    pub sheet_index: usize,
    /// 表头所在行（0 基）。用户可以在预览里改。
    pub header_row: usize,
    /// 数据行数（不含表头）
    pub data_rows: usize,
    pub total_cols: usize,
    /// 表头行**之前**有几行（流水账常见的标题/日期行）。
    /// 大于 0 时界面要明说"上面这几行不会导入" —— 用户得知道丢了什么。
    pub skipped_above: usize,
    pub columns: Vec<PlannedColumn>,
    /// 预览：前若干行原文（含表头行之前的部分，用户就是靠它定位表头）
    pub preview: Vec<Vec<String>>,
    /// 预览只截了前 N 行
    pub preview_truncated: bool,
    /// 类型判断的说明（"表头在第 3 行：第 2 行有 5 个非空单元格"这种）
    pub header_reason: String,
    /// 读文件时发现的坑（合并单元格、编码、长数字被 Excel 截断……）
    pub warnings: Vec<WarningOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WarningOut {
    pub kind: String,
    pub count: usize,
    pub samples: Vec<String>,
    pub advice: String,
}

impl From<xlsx::Warning> for WarningOut {
    fn from(w: xlsx::Warning) -> Self {
        WarningOut {
            kind: w.kind,
            count: w.count,
            samples: w.samples,
            advice: w.advice,
        }
    }
}

/// 文件来源。**只在 Rust 侧存在**，前端拿到的是它的 ULID 令牌。
///
/// 只留 `path` 一个字段：文件名在 [`ImportPlan`] 里已经有了，而这里再存一份
/// 就得保证两处永远一致 —— 没必要。计划是给用户看的，来源是给程序用的。
pub struct Source {
    pub path: PathBuf,
}

// ============================================================
// 计划仓库（一次性令牌）
// ============================================================

static SOURCES: Mutex<Vec<(String, Source)>> = Mutex::new(Vec::new());

/// 最多留几个来源。为什么不无限留着：每个来源握着一个文件路径，
/// 用户导入十个文件之后旧的就没用了；留着只是让内存和"能读的文件"越堆越多。
/// 与 KEEP_SESSIONS 同理（2026-09-19 同一天都踩到）：8 也不是并发上限，
/// 只是防泄漏。测试并行 + 用户多开向导时，活跃令牌会被挤掉，
/// 前端只会看到"导入已过期"这种莫名其妙的话 —— 给足余量。
const KEEP_SOURCES: usize = 32;

/// 存一个来源，返回一次性令牌。
pub fn stash(src: Source) -> String {
    let id = ulid::Ulid::generate().to_string();
    if let Ok(mut v) = SOURCES.lock() {
        if v.len() >= KEEP_SOURCES {
            v.remove(0);
        }
        v.push((id.clone(), src));
    }
    id
}

/// 用令牌取回来源（**不删除** —— 用户会反复调预览，不该只有一次机会）。
pub fn source(id: &str) -> Result<PathBuf> {
    if id.is_empty() {
        return Err("缺少导入令牌（请重新选一次文件）".into());
    }
    let v = SOURCES.lock().map_err(|_| "导入状态锁失败".to_string())?;
    v.iter()
        .find(|(k, _)| k == id)
        .map(|(_, s)| s.path.clone())
        .ok_or_else(|| "这次导入已经过期（程序重启或选了新文件），请重新选一次。".to_string())
}

/// 丢掉一个来源（导入完成后调用，别再握着路径）。
pub fn drop_source(id: &str) {
    if let Ok(mut v) = SOURCES.lock() {
        v.retain(|(k, _)| k != id);
    }
}

/// 只供测试用：清空来源仓库（测试之间不能互相看见对方的路径）。
#[cfg(test)]
pub fn clear_sources() {
    if let Ok(mut v) = SOURCES.lock() {
        v.clear();
    }
}

// ============================================================
// 生成计划
// ============================================================

/// 读 `sheet_index` 张表，按 `header_row`（`None` = 让程序建议）生成计划。
///
/// 用户改表头行时会反复调这个函数（前端每改一次要新的列建议），所以它必须
/// **只读前 `PREVIEW_ROWS` 行** —— 不能因为"用户想改一下表头"就重读整张表。
pub fn build_plan(path: &Path, sheet_index: usize) -> Result<ImportPlan> {
    let sheets = xlsx::sheet_names(path).unwrap_or_default();
    let sr = xlsx::read_rows(path, sheet_index, Some(PREVIEW_ROWS))?;

    if sr.rows.is_empty() {
        return Err("这张表没有数据行。".into());
    }
    if sr.total_cols == 0 {
        return Err("这张表里没有可用的列。".into());
    }

    // 表头行与数据行数：**闸门要按"数据行数"卡，不是按整表行数**。
    //
    // 这里曾经写成 `sr.total_rows > MAX_IMPORT_ROWS + 1`，那个 `+1` 是在猜
    // "大概就一行表头"。实测暴露了它：一份「2 行标题 + 1 行表头 + 恰好 20 万行数据」
    // 的表（总行数 200003）会被拒绝，而它的数据行数正好等于上限 —— **少一行就过、
    // 恰好到上限反而不过**，是典型的边界判错。
    // 现在先求出表头行，再用 `data_rows` 判，语义与用户理解的一致。
    let (header_row, header_reason, _) = import_plan::suggest_header_row(&sr.rows);
    let data_rows = sr.total_rows.saturating_sub(header_row + 1);
    if data_rows > MAX_IMPORT_ROWS {
        return Err(format!(
            "这张表的数据有 {data_rows} 行，超过单次导入上限 {MAX_IMPORT_ROWS} 行。\
             建议在 Excel 里按年份/月份拆成几个文件，分几次导入。"
        ));
    }

    let columns = derive_columns(&sr.rows, header_row);

    let sheet_name = sheets.get(sheet_index).cloned().unwrap_or_default();
    Ok(ImportPlan {
        file_name: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        sheet_name,
        sheets,
        sheet_index,
        header_row,
        data_rows,
        total_cols: sr.total_cols,
        skipped_above: header_row,
        columns,
        // 直接把 rows 移进来 —— 它就是界面上要显示的那张预览表，不必再拷一份
        preview: sr.rows,
        preview_truncated: sr.truncated,
        header_reason,
        warnings: sr.warnings.into_iter().map(WarningOut::from).collect(),
    })
}

/// 由表头行推出一批列建议。`header_row` 由调用方给（用户可能手动改过）。
pub fn derive_columns(rows: &[Vec<String>], header_row: usize) -> Vec<PlannedColumn> {
    let Some(header) = rows.get(header_row) else {
        return Vec::new();
    };
    let width = header.len();
    let names = import_plan::suggest_names(header);

    let mut out = Vec::with_capacity(width);
    for i in 0..width {
        // 收集这一列在**表头之后**的所有值（预览范围内的）
        let values: Vec<String> = rows
            .iter()
            .skip(header_row + 1)
            .filter_map(|r| r.get(i))
            .map(|s| import_plan::trim_cell(s))
            .collect();

        let guess = import_plan::guess_type(&header[i], &values);
        let non_empty = values.iter().filter(|v| !v.is_empty()).count();
        let samples: Vec<String> = values
            .iter()
            .filter(|v| !v.is_empty())
            .take(3)
            .cloned()
            .collect();

        out.push(PlannedColumn {
            source_index: i,
            original: import_plan::trim_cell(&header[i]),
            name: names.get(i).cloned().unwrap_or_else(|| format!("列{}", i + 1)),
            ty: guess.ty,
            confidence: guess.confidence,
            reason: guess.reason,
            samples,
            // "必填"只在这一列**有数据且一个空值都没有**时才默认勾上。
            // 反过来的代价太大：把有空的列勾成必填，导入会整批失败。
            not_null: non_empty > 0 && non_empty == values.len(),
        });
    }
    out
}

// ============================================================
// 落库（分批）
// ============================================================
//
// **为什么不一次性写完**：IPC 处理器跑在主线程上，一口气写 20 万行会让界面
// 连同进度条一起冻住 —— 用户看到的是一条永远停在 0% 的进度条，那比没有
// 进度条更糟（它暗示"还在动"，实际是死了）。
//
// 所以拆成三段，由**前端驱动循环**：
//   begin  → 建表 + 把要写的数据读进来（一次读文件）
//   chunk  → 写一批（`BATCH` 行），返回进度
//   abort  → 失败或用户取消时把表删掉，不留半张
// 每批之间界面可以重绘、进度条可以真地动起来，用户也能中途取消。

/// 用户最终确认下来的一列。
///
/// 字段名同样 snake_case（与 [`PlannedColumn`] 一致）：界面拿到计划、改完再交回来，
/// 两端字段名不一样是最没必要的坑。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FinalColumn {
    /// 对应源表的第几列（来自计划里的 `sourceIndex`）
    pub source_index: usize,
    pub name: String,
    /// 目标类型（`model::ColType` 的 snake_case 名字）
    pub ty: String,
    #[serde(default)]
    pub not_null: bool,
}

/// 一次正在进行的导入。**活在 [`SESSIONS`] 里，前端只拿令牌。**
pub struct Session {
    /// 要写的行（只在表头行之后、按用户保留的列裁好）。
    ///
    /// `None` = 这一格在源文件里是空的。**"空 → NULL"的决定在这里做一次**：
    /// Q-047 拍板"导入路径上，空 = 没有值"（含文本列），而"空"的判据
    /// （trim 后为空）只有读文件这一处位置知道 —— 落到写库时已经分不出
    /// "源文件空格子"与"用户真的填了个空串"了。
    rows: Vec<Vec<Option<String>>>,
    cursor: usize,
    table: String,
    col_names: Vec<String>,
    skipped_empty: usize,
    warnings: Vec<WarningOut>,
}

/// 开始导入的应答。
#[derive(Debug, Clone, Serialize)]
pub struct Begun {
    pub table: String,
    /// 待写入的总行数（进度条的分母）
    pub total: usize,
    /// 表头行之前被跳过的行数（如实告诉用户丢了什么）
    pub skipped_above: usize,
    /// **后端实际收到的表头行号**。
    ///
    /// 为什么要把它回传：`headerRow` 是界面传进来的请求参数，而请求参数写错
    /// **不会报错**，只会用默认值（0）——症状是"表头行被当成数据写进去了"，
    /// 而报错会指向某个字段解析失败，离真正的原因隔着好几层。
    /// 回传它，调用方就能一对一核对"我传的"与"你收的"是否一致。
    pub header_row: usize,
    pub warnings: Vec<WarningOut>,
}

/// 一批的应答。
#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    /// 累计已写入（进度条的分子）
    pub written: usize,
    pub total: usize,
    pub done: bool,
}

static SESSIONS: Mutex<Vec<(String, Session)>> = Mutex::new(Vec::new());
/// 同时最多几场导入。1 就够 —— 界面同时只可能有一个导入向导。
/// 一次会话的内存占用很小（表头 + 预览），32 个也是 KB 级 ——
/// 之前是 2：用户开到第三个导入向导时，第一个就被悄悄挤掉，前端只会看到
/// "导入已结束或过期"；测试并行跑时同样互相挤（2026-09-19 实测）。
/// 这个上限的目的只是防泄漏，不是限制并发 —— 给足余量。
const KEEP_SESSIONS: usize = 32;

fn with_session<T>(id: &str, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
    let mut v = SESSIONS.lock().map_err(|_| "导入状态锁失败".to_string())?;
    let s = v
        .iter_mut()
        .find(|(k, _)| k == id)
        .map(|(_, s)| s)
        .ok_or_else(|| "这次导入已经结束或过期，请重新选一次文件。".to_string())?;
    f(s)
}

/// 丢掉一场导入（结束后调用，别再握着数据）。
pub fn close_session(id: &str) {
    if let Ok(mut v) = SESSIONS.lock() {
        v.retain(|(k, _)| k != id);
    }
}

/// 准备导入：建表，并把要写的数据读进会话。
///
/// **顺序刻意是「先建表、再读数据」**：读文件失败时表还没建出来，什么都不用收拾；
/// 表建好之后如果读失败，`abort` 一次就干净了。
pub fn begin_import(
    db: &mut Db,
    path: &Path,
    sheet_index: usize,
    header_row: usize,
    table: &str,
    columns: &[FinalColumn],
) -> Result<(String, Begun)> {
    if columns.is_empty() {
        return Err("至少要保留一列".into());
    }
    let mut seen = std::collections::HashSet::new();
    for c in columns {
        let n = c.name.trim();
        if n.is_empty() {
            return Err("字段名不能是空的".into());
        }
        if !seen.insert(n.to_lowercase()) {
            return Err(format!("字段名「{n}」重复了"));
        }
    }

    // 目标表不能已经存在 —— 覆盖别人的表比"导入失败"严重得多
    if db.get_table(table).is_ok() {
        return Err(format!(
            "已经有一张叫「{table}」的表了。请换个名字，或先把旧表改名 —— \
             导入不会覆盖已有数据。"
        ));
    }

    let spec = model::TableSpec {
        name: table.to_string(),
        comment: Some(format!(
            "从 {} 导入",
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        )),
        columns: columns
            .iter()
            .map(|c| -> Result<model::ColumnDef> {
                let ty = model::ColType::from_name(&c.ty)
                    .ok_or_else(|| format!("不认识的字段类型：{}", c.ty))?;
                Ok(model::ColumnDef {
                    name: c.name.trim().to_string(),
                    ty,
                    not_null: c.not_null,
                    default: None,
                    primary_key: false,
                    comment: None,
                    shared: None,
                    link: None,
                    lookup: None,
                    rollup: None,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    db.create_table(&spec)?;

    // ---- 把要写的数据读进来 ----
    let source_indexes: Vec<usize> = columns.iter().map(|c| c.source_index).collect();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut skipped_empty = 0usize;
    let walk = xlsx::for_each_row(path, sheet_index, |row_no, row| {
        if row_no <= header_row {
            return Ok(());
        }
        if row.iter().all(|c| import_plan::trim_cell(c).is_empty()) {
            skipped_empty += 1;
            return Ok(());
        }
        rows.push(
            source_indexes
                .iter()
                .map(|i| {
                    row.get(*i)
                        .map(|s| import_plan::trim_cell(s))
                        // 源文件里的空格子 = "没填" → None → 落库 NULL（Q-047）。
                        // 别再退回 unwrap_or_default()：那会把空格子变成空串，
                        // 于是同一份数据里"空"又有两种表示（文本列空串 / 数字列 NULL）。
                        .filter(|s| !s.is_empty())
                })
                .collect(),
        );
        Ok(())
    });
    if let Err(e) = walk {
        // 表已经建出来了，读失败就得把它删掉 —— 否则用户下次会撞上"表已存在"
        let _ = db.drop_table(table, table);
        return Err(e);
    }

    let total = rows.len();
    let warnings = if skipped_empty > 0 {
        vec![WarningOut {
            kind: "empty_rows".into(),
            count: skipped_empty,
            samples: Vec::new(),
            advice: format!("有 {skipped_empty} 行整行都是空的，已跳过（没有建出空记录）。"),
        }]
    } else {
        Vec::new()
    };
    let begun = Begun {
        table: table.to_string(),
        total,
        skipped_above: header_row,
        header_row,
        warnings: warnings.clone(),
    };

    let session = Session {
        rows,
        cursor: 0,
        table: table.to_string(),
        col_names: spec.columns.iter().map(|c| c.name.clone()).collect(),
        skipped_empty,
        warnings,
    };
    let id = ulid::Ulid::generate().to_string();
    {
        let mut v = SESSIONS.lock().map_err(|_| "导入状态锁失败".to_string())?;
        if v.len() >= KEEP_SESSIONS {
            v.remove(0);
        }
        v.push((id.clone(), session));
    }
    Ok((id, begun))
}

/// 写一批。返回累计进度。
pub fn write_chunk(
    db: &mut Db,
    id: &str,
    batch: usize,
) -> Result<Chunk> {
    let batch = batch.clamp(1, 5000);
    with_session(id, |s| {
        let end = (s.cursor + batch).min(s.rows.len());
        if s.cursor < end {
            let slice = &s.rows[s.cursor..end];
            // 导入路径：空单元格在读文件时已标成 None → 统一落 NULL（含文本列），
            // 由 model::Db::insert_rows 的 NULL 语义保证（见 Q-047）。
            db.insert_rows(&s.table, &s.col_names, slice)?;
            s.cursor = end;
        }
        Ok(Chunk {
            written: s.cursor,
            total: s.rows.len(),
            done: s.cursor >= s.rows.len(),
        })
    })
}

/// 失败或取消：把表删掉，**不留半张**。
///
/// 这不是数据库意义上的"回滚"（已提交的批次确实进过库），而是"把现场收拾干净"：
/// 留着一张写了一半的表，用户下次导入会撞上"表已存在"，而他又看不出那张表哪来的。
pub fn abort_import(db: &mut Db, id: &str) -> Result<String> {
    let table = with_session(id, |s| Ok(s.table.clone()))?;
    let _ = db.drop_table(&table, &table);
    close_session(id);
    Ok(table)
}

/// 收尾：会话结束，返回最终结果。
pub fn finish_import(id: &str, skipped_above: usize) -> Result<ImportOutcome> {
    let (table, inserted, skipped_empty, warnings) = with_session(id, |s| {
        Ok((s.table.clone(), s.cursor, s.skipped_empty, s.warnings.clone()))
    })?;
    close_session(id);
    Ok(ImportOutcome {
        table,
        inserted,
        skipped_above,
        skipped_empty,
        warnings,
    })
}

/// 导入结果。
#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub table: String,
    pub inserted: usize,
    /// 表头行之前被跳过的行数（如实告诉用户丢了什么）
    pub skipped_above: usize,
    /// 因为整行全空而没写的行数
    pub skipped_empty: usize,
    pub warnings: Vec<WarningOut>,
}

/// 一次跑完的便捷入口。**测试专用** —— 界面走 begin / chunk / finish 三段，
/// 那样进度才是真的、界面才不冻。留这个包装是为了让测试把"循环"这件事
/// 从断言里摘出去（循环本身由上面那条 `可以分批写完且进度是累计的` 覆盖）。
#[cfg(test)]
pub fn run_import(
    db: &mut Db,
    path: &Path,
    sheet_index: usize,
    header_row: usize,
    table: &str,
    columns: &[FinalColumn],
) -> Result<ImportOutcome> {
    let (id, begun) = begin_import(db, path, sheet_index, header_row, table, columns)?;
    loop {
        let c = match write_chunk(db, &id, 500) {
            Ok(c) => c,
            Err(e) => {
                let _ = abort_import(db, &id);
                return Err(e);
            }
        };
        if c.done {
            break;
        }
    }
    let mut out = finish_import(&id, begun.skipped_above)?;
    // `begin` 阶段的告警（空行）与 `finish` 的可能是同一条，去重
    for w in begun.warnings {
        if !out.warnings.iter().any(|x| x.kind == w.kind) {
            out.warnings.push(w);
        }
    }
    Ok(out)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    static IMP_TMP: AtomicU64 = AtomicU64::new(0);

    fn mem() -> Db {
        let n = IMP_TMP.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("dkb_imp_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Db::open(&d).unwrap()
    }

    /// 造一个 CSV 并返回路径。CSV 是最容易在测试里生成的表格格式，
    /// 而它走的解析路径（`csv_import::read_rows`）与 xlsx 是并列的两条 ——
    /// 两条都要有覆盖，所以这里用 CSV 打底、xlsx 那侧靠已有的往返测试。
    fn tmp_csv(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deskbase_imp_{}_{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.csv");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn plan_recognizes_header_and_types() {
        let p = tmp_csv(
            "plan",
            "客户台账\n2026-09-01 导出\n客户名称,联系电话,金额,是否结清\n甲,13800000000,1234.50,是\n乙,13900000000,88,否\n",
        );
        let plan = build_plan(&p, 0).unwrap();
        assert_eq!(plan.header_row, 2, "表头应在第 3 行（0 基是 2）：{}", plan.header_reason);
        assert_eq!(plan.skipped_above, 2);
        assert_eq!(plan.data_rows, 2);
        assert_eq!(plan.total_cols, 4);

        let names: Vec<&str> = plan.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["客户名称", "联系电话", "金额", "是否结清"]);

        // 电话要判成文本 —— 11 位数字不丢，但绝不能变成数字（会被 Excel 科学计数）
        assert_eq!(plan.columns[1].ty, "text", "电话列必须是文本");
        // 金额列：列名是「金额」且值是两位小数
        assert_eq!(plan.columns[2].ty, "money");
        assert_eq!(plan.columns[3].ty, "boolean");
    }

    #[test]
    fn data_actually_lands_in_db() {
        let p = tmp_csv(
            "run",
            "客户名称,金额,是否结清\n甲,1234.50,是\n乙,88,否\n",
        );
        let mut conn = mem();
        let plan = build_plan(&p, 0).unwrap();
        let cols: Vec<FinalColumn> = plan
            .columns
            .iter()
            .map(|c| FinalColumn {
                source_index: c.source_index,
                name: c.name.clone(),
                ty: c.ty.clone(),
                not_null: false,
            })
            .collect();
        let out = run_import(&mut conn, &p, 0, plan.header_row, "客户台账", &cols).unwrap();
        assert_eq!(out.inserted, 2);
        assert_eq!(out.skipped_above, 0);

        // 金额按「分」存的核对：1234.50 → 123450
        let page = conn.page_rows("客户台账", None, false, None, 10).unwrap();
        assert_eq!(page.rows.len(), 2);
        // columns[0] 是 _rowid，所以金额在第 3 列
        assert_eq!(page.rows[0][2].as_i64(), Some(123450));
        // 布尔存成 1/0
        assert_eq!(page.rows[0][3].as_i64(), Some(1));
        assert_eq!(page.rows[1][3].as_i64(), Some(0));
    }

    #[test]
    fn rows_before_header_not_imported() {
        let p = tmp_csv(
            "skip",
            "标题行\n2026-09-01\n姓名,数量\n甲,1\n乙,2\n",
        );
        let mut conn = mem();
        let plan = build_plan(&p, 0).unwrap();
        assert_eq!(plan.header_row, 2);
        let cols: Vec<FinalColumn> = plan
            .columns
            .iter()
            .map(|c| FinalColumn {
                source_index: c.source_index,
                name: c.name.clone(),
                ty: c.ty.clone(),
                not_null: false,
            })
            .collect();
        let out = run_import(&mut conn, &p, 0, plan.header_row, "t", &cols).unwrap();
        assert_eq!(out.skipped_above, 2, "上面两行必须如实报告被跳过");
        assert_eq!(out.inserted, 2);
        // 库里绝不能出现「标题行」
        let page = conn.page_rows("t", None, false, None, 10).unwrap();
        for r in &page.rows {
            assert_ne!(r[1].as_str().unwrap_or(""), "标题行");
            assert_ne!(r[1].as_str().unwrap_or(""), "2026-09-01");
        }
    }

    #[test]
    fn existing_table_rejected_not_overwritten() {
        let p = tmp_csv("dup", "姓名\n甲\n");
        let mut conn = mem();
        let plan = build_plan(&p, 0).unwrap();
        let cols: Vec<FinalColumn> = plan
            .columns
            .iter()
            .map(|c| FinalColumn {
                source_index: c.source_index,
                name: c.name.clone(),
                ty: c.ty.clone(),
                not_null: false,
            })
            .collect();
        run_import(&mut conn, &p, 0, plan.header_row, "已有表", &cols).unwrap();
        let again = run_import(&mut conn, &p, 0, plan.header_row, "已有表", &cols);
        assert!(again.is_err(), "重名必须报错");
        assert!(again.unwrap_err().contains("已经有一张叫"));
    }

    #[test]
    fn failed_import_leaves_no_partial_table() {
        let p = tmp_csv("fail", "编号,数量\n甲,不是数字\n");
        let mut conn = mem();
        let plan = build_plan(&p, 0).unwrap();
        // 故意把「数量」当整数列 —— 值"不是数字"会在写入时失败
        let cols = vec![
            FinalColumn {
                source_index: 0,
                name: "编号".into(),
                ty: "text".into(),
                not_null: false,
            },
            FinalColumn {
                source_index: 1,
                name: "数量".into(),
                ty: "integer".into(),
                not_null: false,
            },
        ];
        let r = run_import(&mut conn, &p, 0, plan.header_row, "坏表", &cols);
        assert!(r.is_err(), "写不进去必须报错");
        // 关键：表不能留在库里
        let exists = conn.get_table("坏表").is_ok();
        assert!(!exists, "失败后不该留下半张表");
    }

    #[test]
    fn fully_empty_rows_skipped_and_counted() {
        let p = tmp_csv("empty", "姓名,数量\n甲,1\n,\n乙,2\n");
        let mut conn = mem();
        let plan = build_plan(&p, 0).unwrap();
        let cols: Vec<FinalColumn> = plan
            .columns
            .iter()
            .map(|c| FinalColumn {
                source_index: c.source_index,
                name: c.name.clone(),
                ty: c.ty.clone(),
                not_null: false,
            })
            .collect();
        let out = run_import(&mut conn, &p, 0, plan.header_row, "e", &cols).unwrap();
        assert_eq!(out.inserted, 2);
        assert_eq!(out.skipped_empty, 1);
    }

    #[test]
    fn subset_of_columns_kept_in_shuffled_order() {
        let p = tmp_csv("subset", "a,b,c\n1,2,3\n");
        let mut conn = mem();
        let cols = vec![
            FinalColumn {
                source_index: 2,
                name: "第三列".into(),
                ty: "text".into(),
                not_null: false,
            },
            FinalColumn {
                source_index: 0,
                name: "第一列".into(),
                ty: "text".into(),
                not_null: false,
            },
        ];
        let out = run_import(&mut conn, &p, 0, 0, "子集", &cols).unwrap();
        assert_eq!(out.inserted, 1);
        let page = conn.page_rows("子集", None, false, None, 10).unwrap();
        // columns = [_rowid, 第三列, 第一列]
        assert_eq!(page.rows[0][1].as_str(), Some("3"));
        assert_eq!(page.rows[0][2].as_str(), Some("1"));
    }

    #[test]
    fn actionable_message_when_token_unavailable() {
        clear_sources();
        let e = source("不存在的令牌").unwrap_err();
        assert!(e.contains("重新选一次"), "报错要告诉用户下一步做什么：{e}");
        let e2 = source("").unwrap_err();
        assert!(e2.contains("重新选"));
    }

    #[test]
    fn stored_source_reads_back_without_interference() {
        clear_sources();
        let a = stash(Source {
            path: PathBuf::from("C:/a.csv"),
        });
        let b = stash(Source {
            path: PathBuf::from("C:/b.csv"),
        });
        assert_eq!(source(&a).unwrap(), PathBuf::from("C:/a.csv"));
        assert_eq!(source(&b).unwrap(), PathBuf::from("C:/b.csv"));
        drop_source(&a);
        assert!(source(&a).is_err());
        assert!(source(&b).is_ok(), "删一个不该影响另一个");
        clear_sources();
    }

    /// 类型名的两个来源必须逐字一致：协议用 `ColType::from_name`，序列化用 serde。
    /// 这两者一旦分叉，导入时用户选的类型就会解析失败 —— 而报错会指向"不认识的
    /// 字段类型"，跟真正的原因（两处名字不一样）隔着好几层。
    #[test]
    fn type_names_match_serialized_form_exactly() {
        use model::ColType::*;
        let all = [
            Text, Integer, Real, Money, Boolean, Date, DateTime, Json, Blob,
        ];
        for t in all {
            let ser = serde_json::to_value(t).unwrap();
            let name = ser.as_str().unwrap();
            assert_eq!(
                model::ColType::from_name(name),
                Some(t),
                "from_name 与 serde 的 snake_case 形式对不上：{name}"
            );
            // 大小写与空白不敏感（界面可能传 "Text" 或带空格）
            assert_eq!(model::ColType::from_name(&format!(" {} ", name.to_uppercase())), Some(t));
        }
        assert_eq!(model::ColType::from_name("不存在的类型"), None);
    }
    /// 分批写入：进度要能累计，且**最后一批不足一批也要写完**。
    ///
    /// 这条测的是界面真正走的路径（begin → chunk → finish）。批大小故意取 3，
    /// 让 7 行数据跨 3 批 —— 只测"一次写完"是测不出分批的边界问题的
    /// （比如最后一批不足时被漏掉，那正是最容易写错的地方）。
    #[test]
    fn batched_write_with_cumulative_progress() {
        let mut body = String::from("名称,数量\n");
        for i in 1..=7 {
            body.push_str(&format!("第{i}行,{i}\n"));
        }
        let p = tmp_csv("chunk", &body);
        let mut conn = mem();
        let cols = vec![
            FinalColumn { source_index: 0, name: "名称".into(), ty: "text".into(), not_null: false },
            FinalColumn { source_index: 1, name: "数量".into(), ty: "integer".into(), not_null: false },
        ];
        let (id, begun) = begin_import(&mut conn, &p, 0, 0, "分批表", &cols).unwrap();
        assert_eq!(begun.total, 7, "总数是进度条的分母，必须先给出来");

        let mut steps = Vec::new();
        loop {
            let c = write_chunk(&mut conn, &id, 3).unwrap();
            steps.push((c.written, c.done));
            if c.done {
                break;
            }
        }
        assert_eq!(
            steps,
            vec![(3, false), (6, false), (7, true)],
            "进度应当累计，最后一批把剩下的写完"
        );

        let out = finish_import(&id, begun.skipped_above).unwrap();
        assert_eq!(out.inserted, 7);

        let page = conn.page_rows("分批表", None, false, None, 20).unwrap();
        assert_eq!(page.rows.len(), 7, "7 行一行都不能少");
        // 会话用完就没了
        assert!(write_chunk(&mut conn, &id, 3).is_err(), "收尾之后会话该失效");
    }

    /// 取消：表必须被删掉，不能留半张。
    #[test]
    fn cancel_import_removes_partial_table() {
        let p = tmp_csv("abort", "名称
甲
乙
丙
");
        let mut conn = mem();
        let cols = vec![FinalColumn {
            source_index: 0,
            name: "名称".into(),
            ty: "text".into(),
            not_null: false,
        }];
        let (id, _) = begin_import(&mut conn, &p, 0, 0, "半张表", &cols).unwrap();
        write_chunk(&mut conn, &id, 1).unwrap(); // 先写一行（表里已经有数据了）
        let gone = abort_import(&mut conn, &id).unwrap();
        assert_eq!(gone, "半张表");
        let exists = conn.get_table("半张表").is_ok();
        assert!(!exists, "取消之后不能留下半张表");
    }

}
