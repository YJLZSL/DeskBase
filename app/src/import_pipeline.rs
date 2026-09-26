//! 导入落库管线：「能撤销的导入才敢用」的落地部分。
//!
//! 与 `xlsx.rs` / `csv_import.rs` 的分工很清楚：那两个模块只**看**文件、绝不碰库；
//! 本模块只**写**库、绝不碰文件。中间由调用方（IPC 层）串起来。
//!
//! # 现在怎么落库（无 SQL，见 docs/adr/0021）
//!
//! 本模块只调用 [`crate::model::Db`]（自研单文件存储）。导入作业与行指纹不再建
//! SQL 表，而是直接写存储键：
//!
//! ```text
//! job/<job_id>            作业元数据（JSON：状态、进度、列清单、自然键…）
//! job/<job_id>/row/<n>   本次导入写入的每一行的指纹（JSON：rid + fp）
//! ```
//!
//! 业务数据走 [`crate::model`] 通用的 `rec/<表>/<20位rowid>`，撤销时只删
//! 「这次作业写进去、且指纹还对得上」的行。
//!
//! # 为什么要分批提交
//!
//! 几十万行如果一把提交，进程被强杀就整批全丢。这里固定 batch_rows 行提交一次，
//! 每次把「这批数据」和「作业进度 committed_rows」放进**同一个 store 事务**
//! （一条 commit = 一个原子日志）一起落盘 —— 这样"库里有 N 行"和"作业说已提交 N 行"
//! 永远一致，进程被强杀也不会出现"数据在、进度不在"。
//!
//! # 撤销只删本次导入写的行
//!
//! 靠两层信息：
//!   · `job/<job_id>/row/<n>` 里的 (rid, 指纹) —— 撤销前逐行核对，
//!     用户后来改过的行（指纹对不上）一律不删；
//!   · 本次覆盖过的老行（updated）只计数、不进 row 清单，所以不会被删也不会还原。
//!
//! # 本模块只接线了一部分（2026-09-17）
//!
//! 目前接进 IPC 的只有 `recovery_notice`（`import.pending` 命令，用来在启动时
//! 报告"上次没跑完的导入还剩多少行"）。真正的主体 `import` / `undo` 还没接线，
//! 原因不是它不可用，而是**它要的业务表还不存在** —— 到那时这一行必须删掉。
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::{self, ColType, Db, Table};
use crate::store::Batch;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as Json;

#[cfg(test)]
use crate::model::{ColumnDef, TableSpec};
#[cfg(test)]
use std::path::Path;

/// 本模块的返回类型。错误一律是**给人看的中文**，IPC 层可以直接往界面上贴。
pub type Result<T> = std::result::Result<T, String>;

/// 默认每批行数。
pub const DEFAULT_BATCH_ROWS: usize = 10_000;

/// 业务表上标记导入来源的列名（保留作"不能当业务列导入"的保留名）。
pub const JOB_COLUMN: &str = "_import_job";

/// 列名/表名的分隔符（ASCII Unit Separator）。用它拼 `columns_text`，
/// 避免为了存一行列名把 serde_json 拖进存储格式。
const SEP: char = '\u{1f}';

// ---------------------------------------------------------------- 作业状态

/// 一次导入作业的状态。
///
/// 注意 `Running` 同时代表"正在跑"和"上次被强杀"—— 强杀来不及改状态，
/// 所以启动时看到 `Running` 就是「上次没跑完」的意思。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// 进行中（含被强杀后遗留在库里的样子）
    Running,
    /// 已完成：所有行都提交了
    Done,
    /// 已撤销：这次导入写进去的行已经删掉
    Undone,
    /// 失败：中途出错中断。**已提交的批次仍在库里**，不是回滚
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Undone => "undone",
            JobState::Failed => "failed",
        }
    }

    /// 界面上显示的词
    pub fn label(self) -> &'static str {
        match self {
            JobState::Running => "进行中",
            JobState::Done => "已完成",
            JobState::Undone => "已撤销",
            JobState::Failed => "失败",
        }
    }

    fn from_db(s: &str) -> JobState {
        match s {
            "running" => JobState::Running,
            "done" => JobState::Done,
            "undone" => JobState::Undone,
            "failed" => JobState::Failed,
            // 认不出来的状态（比如更新版本写的）一律当失败：绝不猜成"已完成"，
            // 猜错会让用户以为数据全都导进来了，那比报错严重得多。
            _ => JobState::Failed,
        }
    }
}

/// 一次导入作业的句柄。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportJob {
    pub id: String,
    pub table: String,
    pub state: JobState,
    /// 已提交行数：数据落到库里、并且进度也一起提交了的那部分
    pub committed_rows: u64,
    pub total_rows: u64,
}

impl ImportJob {
    /// 还没导入的行数 —— 崩溃恢复的提示语全靠它。
    /// 用 `saturating_sub` 是防守：万一库里出现 committed > total 的脏数据，
    /// 也要给出一个"不像 bug"的数字而不是下溢成大数。
    pub fn remaining_rows(&self) -> u64 {
        self.total_rows.saturating_sub(self.committed_rows)
    }
}

// ---------------------------------------------------------------- 选项与报告

/// 落库选项
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub table: String,
    /// 目标列名。顺序就是 `rows` 里每一列的顺序。
    pub columns: Vec<String>,
    /// 自然键：用来做幂等去重。空表示不去重（注意：那样重跑会产生重复行）
    pub natural_key: Vec<String>,
    /// true = 冲突时更新已存在的行；false = 保留已有行不变（默认）
    pub update_existing: bool,
    /// 每批提交多少行
    pub batch_rows: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            table: String::new(),
            columns: Vec::new(),
            natural_key: Vec::new(),
            update_existing: false,
            batch_rows: DEFAULT_BATCH_ROWS,
        }
    }
}

impl ImportOptions {
    /// 最常用的形状：给表名和列名，其余走默认（不去重、不覆盖、每批 1 万行）
    pub fn new(table: &str, columns: &[&str]) -> Self {
        ImportOptions {
            table: table.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            ..Default::default()
        }
    }

    /// 声明自然键（幂等去重的依据）
    pub fn with_natural_key(mut self, key: &[&str]) -> Self {
        self.natural_key = key.iter().map(|c| (*c).to_string()).collect();
        self
    }

    /// 冲突时覆盖已存在的行
    pub fn overwrite(mut self) -> Self {
        self.update_existing = true;
        self
    }

    /// 调整批大小（测试与小文件用得上）
    pub fn with_batch_rows(mut self, n: usize) -> Self {
        self.batch_rows = n;
        self
    }
}

/// 一次导入的结果。
#[derive(Debug, Clone)]
pub struct JobReport {
    pub job: ImportJob,
    /// 真正新插进去的行
    pub inserted: u64,
    /// 命中自然键、被这次导入覆盖掉的行（update_existing = true 才可能非 0）
    pub updated: u64,
    /// 命中自然键、保持原样没动的行（幂等重跑时全是它）
    pub unchanged: u64,
    /// 自然键整列为空的行数。空串是**一个真实的键值**，所以这些行会被唯一起来看成
    /// 同一行、最后只留下一条 —— 是个静默少数据的坑，必须报给用户。
    /// （NULL 才会绕过去重，但本模块从不写 NULL，所以只有空串这一种情况。）
    pub blank_key_rows: u64,
    /// 实际提交的事务数。回调次数 = batches + 1（开头还有一次 (0, total)）
    pub batches: u64,
    pub elapsed_ms: i64,
}

impl JobReport {
    /// 一句话总结，界面可以直接显示
    pub fn summary(&self) -> String {
        let mut s = format!(
            "「{}」{}：新增 {} 行、更新 {} 行、跳过 {} 行，耗时 {}",
            self.job.table,
            self.job.state.label(),
            self.inserted,
            self.updated,
            self.unchanged,
            human_ms(self.elapsed_ms)
        );
        if self.blank_key_rows > 0 {
            s.push_str(&format!(
                "。注意：有 {} 行的编号（自然键）是空的 —— 空编号会被当成同一个键，\
                 这些行最后只留下了一条。建议先在原文件里补上编号再导入",
                self.blank_key_rows
            ));
        }
        s
    }
}

/// 撤销的结果。
///
/// 只返回一个数字是不够的：用户需要知道"有多少行因为我自己改过而没被删"，
/// 否则会以为撤销漏了。
#[derive(Debug, Clone)]
pub struct UndoReport {
    pub job_id: String,
    /// 已删除的行数
    pub deleted: u64,
    /// 本次导入写的、但**用户后来改过**因而保留的行
    pub kept_modified: u64,
    /// 本次导入覆盖过的老行（它们本来就存在，撤销不删也不还原）
    pub kept_updated: u64,
}

impl UndoReport {
    pub fn summary(&self) -> String {
        let mut s = format!("已撤销：删除 {} 行", self.deleted);
        if self.kept_modified > 0 {
            s.push_str(&format!(
                "；保留 {} 行（这些行你后来改过，撤销不替你决定）",
                self.kept_modified
            ));
        }
        if self.kept_updated > 0 {
            s.push_str(&format!(
                "；另有 {} 行是这次覆盖过的老数据，原值已被覆盖、无法恢复",
                self.kept_updated
            ));
        }
        s
    }
}

fn human_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms} 毫秒")
    } else {
        format!("{:.1} 秒", ms as f64 / 1000.0)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 存储键

fn job_key(id: &str) -> String {
    format!("job/{id}")
}

fn job_row_key(id: &str, n: u64) -> String {
    format!("job/{id}/row/{n}")
}

fn record_key(table: &str, rid: i64) -> String {
    format!("rec/{table}/{rid:020}")
}

fn record_prefix(table: &str) -> String {
    format!("rec/{table}/")
}

/// 一次导入作业的元数据（取代原来的 `_import_job` SQL 表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobMeta {
    id: String,
    table: String,
    state: String,
    total_rows: u64,
    committed_rows: u64,
    inserted_rows: u64,
    updated_rows: u64,
    unchanged_rows: u64,
    blank_key_rows: u64,
    /// 本次导入的目标列（SEP 分隔），撤销时按这个顺序重算指纹
    columns_text: String,
    /// 自然键（SEP 分隔）
    natural_key_text: String,
    update_existing: bool,
    batch_rows: usize,
    started_at: i64,
    finished_at: Option<i64>,
    error: Option<String>,
}

/// 一条被导入写进去的行的指纹（取代原来的 `_import_row` SQL 表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RowFp {
    rid: i64,
    fp: String,
}

fn load_job(db: &Db, id: &str) -> Option<JobMeta> {
    db.store()
        .get(&job_key(id))
        .and_then(|s| serde_json::from_str(s).ok())
}

fn save_job(db: &mut Db, m: &JobMeta) -> Result<()> {
    db.store_mut()
        .put(
            job_key(&m.id),
            serde_json::to_string(m).map_err(|e| e.to_string())?,
        )
        .map(|_| ())
        .map_err(|e| format!("写导入作业元数据失败：{e}"))
}

/// 读回所有作业元数据（过滤掉 `job/<id>/row/<n>` 这类子键）。
fn load_all_jobs(db: &Db) -> Vec<JobMeta> {
    let mut out = Vec::new();
    for (k, v) in db.store().scan("job/") {
        // 只取形如 `job/<id>`（后面没有 `/`）的元数据键
        let suffix = match k.strip_prefix("job/") {
            Some(s) => s,
            None => continue,
        };
        if suffix.contains('/') {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<JobMeta>(&v) {
            out.push(m);
        }
    }
    out
}

fn to_import_job(m: &JobMeta) -> ImportJob {
    ImportJob {
        id: m.id.clone(),
        table: m.table.clone(),
        state: JobState::from_db(&m.state),
        committed_rows: m.committed_rows,
        total_rows: m.total_rows,
    }
}

fn json_to_string(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
        other => other.to_string(),
    }
}

fn split_columns(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split(SEP).map(|s| s.to_string()).collect()
    }
}

// ---------------------------------------------------------------- 主入口

/// 分批把行写进主库。
///
/// `rows` 是**数据行，不含表头**（表头归 `xlsx.rs` / `csv_import.rs` 管）。
/// `on_progress(已提交行数, 总行数)` 在开头先回调一次 `(0, total)` 让界面立刻能画
/// 进度条，之后每提交一批回调一次。
///
/// 出错时不回滚已提交的批次：库里的行留着，作业标记为失败，
/// 用户可以选择「重跑」（靠自然键幂等）或「撤销」。
///
/// 空输入（`rows` 为空）直接返回，**不碰数据库任何一处**；
/// 这种情况下报告里的 `job.id` 是空串，别拿它去查作业。
pub fn import(
    db: &mut Db,
    rows: &[Vec<String>],
    opts: &ImportOptions,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<JobReport> {
    let started = std::time::Instant::now();
    let total = rows.len() as u64;

    // 空输入：连作业元数据都不建，更不动业务表。
    // 用户点了取消、或者文件只剩表头时，不该在导入历史里留一条 0 行的记录。
    if rows.is_empty() {
        return Ok(JobReport {
            job: ImportJob {
                id: String::new(),
                table: opts.table.clone(),
                state: JobState::Done,
                committed_rows: 0,
                total_rows: 0,
            },
            inserted: 0,
            updated: 0,
            unchanged: 0,
            blank_key_rows: 0,
            batches: 0,
            elapsed_ms: started.elapsed().as_millis() as i64,
        });
    }

    // 前置校验一次做完：宁可一行不写，也不要导一半再报"第 400001 行有问题"。
    let mut plan = Plan::build(db, opts, rows)?;

    // 作业先独立提交一次：这样"我开始导入了"本身就是一个已落盘的既成事实，
    // 第一批还没提交就被强杀也能在下次启动时看到它。
    let job_id = ulid::Ulid::generate().to_string();
    create_job(db, &job_id, &plan, total)?;

    // 结构层面"会有列被留成 NULL"的检查：表上有 NOT NULL 列、但本次导入没提供它、
    // 而且表上也没有默认值 —— 这种导入必然失败，不如在写任何行之前就说清楚。
    // （放在作业创建之后，是为了让"失败≠回滚"在作业历史里留下诚实的 Failed 记录。）
    let provided: HashSet<&str> = plan.columns.iter().map(|s| s.as_str()).collect();
    let uncovered: Vec<&String> = plan
        .not_null_cols
        .iter()
        .filter(|c| !provided.contains(c.as_str()))
        .collect();
    if !uncovered.is_empty() {
        let msg = format!(
            "表「{}」的列 {} 是 NOT NULL，但本次导入没有提供（且没有默认值），无法导入",
            plan.table,
            uncovered
                .iter()
                .map(|s| format!("「{s}」"))
                .collect::<Vec<_>>()
                .join("、")
        );
        let _ = set_job_state(db, &job_id, JobState::Failed, Some(&msg));
        return Err(msg);
    }

    match run_batches(db, &mut plan, &job_id, rows, total, on_progress) {
        Ok((counts, batches)) => {
            set_job_state(db, &job_id, JobState::Done, None)?;
            Ok(JobReport {
                job: ImportJob {
                    id: job_id,
                    table: plan.table.clone(),
                    state: JobState::Done,
                    committed_rows: total,
                    total_rows: total,
                },
                inserted: counts.inserted,
                updated: counts.updated,
                unchanged: counts.unchanged,
                blank_key_rows: counts.blank_key,
                batches,
                elapsed_ms: started.elapsed().as_millis() as i64,
            })
        }
        Err(msg) => {
            // 走到这里说明某些批次已经提交了，现在可以安全地在库上留一条"失败"记录。
            // 记不上也无所谓：作业还停在"进行中"，那同样是个诚实的说法。
            let _ = set_job_state(db, &job_id, JobState::Failed, Some(&msg));
            Err(msg)
        }
    }
}

/// 撤销一次导入：按 job id 删除这次导入写进去的行。
///
/// 返回删除的行数。要更细的结果（保留了多少用户改过的行）用 `undo_with_report`。
///
/// 语义：
///   · 只删**这个作业写进去的**行，靠 `job/<job_id>/row/<n>` 里的 (rid, 指纹) 定位；
///   · 用户后来改过的行**不删**（内容指纹对不上），并把它的作业标记摘掉 ——
///     从此它归用户自己；
///   · 本次导入覆盖过的老行不删也不还原（原值在导入那一刻就被覆盖了）；
///   · 整个过程一个事务：要么全撤，要么一行不动，不会留下半撤的中间态。
pub fn undo(db: &mut Db, job_id: &str) -> Result<u64> {
    Ok(undo_with_report(db, job_id)?.deleted)
}

/// 带明细的撤销。见 `undo` 的语义说明。
pub fn undo_with_report(db: &mut Db, job_id: &str) -> Result<UndoReport> {
    let m = load_job(db, job_id).ok_or_else(|| format!("找不到导入作业 {job_id}"))?;
    // 只有"已经撤销过"的作业不能再来一次；已完成（done）/ 进行中（running，被强杀）/
    // 失败（failed，部分提交）都能撤销——它们的共同点是"库里还有这次写进去、且指纹对得上的行"。
    if m.state == JobState::Undone.as_str() {
        return Err(format!("导入作业 {job_id} 已经撤销过了，不会重复执行"));
    }

    let cols = split_columns(&m.columns_text);
    let fps = load_job_rows(db, job_id);

    let mut doomed: Vec<i64> = Vec::new();
    let mut kept_modified = 0u64;
    for (_, rf) in &fps {
        match db.store().get(&record_key(&m.table, rf.rid)).map(|s| s.to_string()) {
            Some(raw) => {
                let rec: BTreeMap<String, Json> = match serde_json::from_str(&raw) {
                    Ok(r) => r,
                    Err(e) => return Err(format!("记录读不出来: {e}")),
                };
                let vals: Vec<String> = cols
                    .iter()
                    .map(|c| rec.get(c).map(json_to_string).unwrap_or_default())
                    .collect();
                if fingerprint_row(&vals) == rf.fp {
                    doomed.push(rf.rid);
                } else {
                    // 用户改过了。删它就是删用户自己的劳动成果 —— 留下。
                    kept_modified += 1;
                }
            }
            None => {
                // 行已经不在了（可能之前被撤销过一半）：既不删也不算保留。
            }
        }
    }

    // 一个事务里：删业务行 + 清掉本作业的行指纹 + 把作业标成已撤销。
    let mut b = Batch::new();
    for rid in &doomed {
        b.del(record_key(&m.table, *rid));
    }
    for (k, _) in &fps {
        b.del(k.clone());
    }
    let mut m2 = m;
    m2.state = JobState::Undone.as_str().to_string();
    m2.finished_at = Some(now_ms());
    b.set(
        job_key(job_id),
        serde_json::to_string(&m2).map_err(|e| e.to_string())?,
    );
    db.store_mut()
        .commit(b)
        .map_err(|e| format!("提交撤销失败：{e}"))?;

    Ok(UndoReport {
        job_id: job_id.to_string(),
        deleted: doomed.len() as u64,
        kept_modified,
        kept_updated: m2.updated_rows,
    })
}

/// 列出所有作业，最新的在前。
pub fn list_jobs(db: &Db) -> Result<Vec<ImportJob>> {
    let mut jobs = load_all_jobs(db)
        .into_iter()
        .map(|m| (m.started_at, m.id.clone(), to_import_job(&m)))
        .collect::<Vec<_>>();
    // 最新的在前：started_at 大者优先，相同则 id 大者优先。
    jobs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    Ok(jobs.into_iter().map(|(_, _, j)| j).collect())
}

/// 列出**上次没跑完**的作业：状态是"进行中"的那些。
///
/// 进程被强杀时来不及改状态，所以启动时看到的就是它们。
/// 这个函数是只读的 —— **绝不自动清理、绝不自动回滚**，让用户决定。
pub fn pending_jobs(db: &Db) -> Result<Vec<ImportJob>> {
    let mut jobs = load_all_jobs(db)
        .into_iter()
        .filter(|m| m.state == JobState::Running.as_str())
        .map(|m| (m.started_at, m.id.clone(), to_import_job(&m)))
        .collect::<Vec<_>>();
    // 最老的在前：先发生的没跑完的先报。
    jobs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(jobs.into_iter().map(|(_, _, j)| j).collect())
}

/// 查一个作业
pub fn get_job(db: &Db, job_id: &str) -> Result<Option<ImportJob>> {
    Ok(load_job(db, job_id).map(|m| to_import_job(&m)))
}

/// 启动时给用户看的那句话。没有未完成的作业就返回 `None`。
///
/// 特意说清三件事：导进去多少、还剩多少、接下来能做什么。
/// 用户在崩溃后最怕的不是"少导了"，而是"不知道现在库里是什么状态"。
pub fn recovery_notice(db: &Db) -> Result<Option<String>> {
    let detail: Vec<(ImportJob, bool)> = load_all_jobs(db)
        .into_iter()
        .filter(|m| m.state == JobState::Running.as_str())
        .map(|m| {
            let has_key = !m.natural_key_text.is_empty();
            (to_import_job(&m), has_key)
        })
        .collect();
    if detail.is_empty() {
        return Ok(None);
    }

    let mut parts = Vec::new();
    let mut any_without_key = false;
    for (job, has_key) in &detail {
        let left = job.remaining_rows();
        if left == 0 {
            parts.push(format!(
                "「{}」的 {} 行已经全部写入，只是没来得及标记完成",
                job.table, job.committed_rows
            ));
        } else {
            parts.push(format!(
                "「{}」已导入 {} 行，还剩 {} 行没导入",
                job.table, job.committed_rows, left
            ));
        }
        if !has_key {
            any_without_key = true;
        }
    }

    let mut s = format!(
        "上次有 {} 个导入没有跑完：{}。已写入的数据完好、可以正常使用。",
        detail.len(),
        parts.join("；")
    );
    if any_without_key {
        s.push_str(
            "这些导入没有设置自然键，直接重跑会产生重复行 —— 建议先撤销这次导入，再重新开始。",
        );
    } else {
        s.push_str("重新导入同一个文件不会产生重复（按自然键去重），也可以撤销这次导入。");
    }
    Ok(Some(s))
}

/// 用户看过提示、决定不再搭理某个作业时调用。**只改状态，一行数据都不碰。**
///
/// 没有这个出口的话，"未完成的作业"会在每次启动时反复弹出来，
/// 而本项目不允许自动把提示清掉 —— 清不清必须由用户说了算。
pub fn dismiss(db: &mut Db, job_id: &str) -> Result<()> {
    let job = get_job(db, job_id)?.ok_or_else(|| format!("找不到导入作业 {job_id}"))?;
    if job.state != JobState::Running {
        return Err(format!(
            "作业 {job_id} 现在是「{}」，不需要忽略",
            job.state.label()
        ));
    }
    set_job_state(
        db,
        job_id,
        JobState::Failed,
        Some("用户选择不再处理这个作业（数据保持原样，未做任何回滚）"),
    )
}

// ---------------------------------------------------------------- 批次执行

#[derive(Default, Clone, Copy)]
struct Counts {
    inserted: u64,
    updated: u64,
    unchanged: u64,
    blank_key: u64,
}

/// 分批提交。返回 (计数, 批次数)。
///
/// 任何一批出错都直接返回 Err —— 出错那一刻的事务因为作用域结束会被自动回滚，
/// 更早的批次早就提交了，所以"已提交的行留在库里"是这里天然的结果。
fn run_batches(
    db: &mut Db,
    plan: &mut Plan,
    job_id: &str,
    rows: &[Vec<String>],
    total: u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(Counts, u64)> {
    let mut counts = Counts::default();
    let mut batches = 0u64;
    let mut cursor = 0usize;
    // 跨批自增的行指纹序号：每个新插入的行都占一个 `job/<id>/row/<n>`。
    let mut row_seq: u64 = 0;

    on_progress(0, total);

    while cursor < rows.len() {
        let end = (cursor + plan.batch_rows).min(rows.len());
        let batch = &rows[cursor..end];

        // 这一批要写的所有内容（数据 + 表定义里的 next_rowid + 作业进度 + 行指纹）
        // 放进**同一个 Batch**，一次性 commit —— 这就是"数据与进度原子一致"的保证。
        let mut b = Batch::new();
        // 本批内刚写进去、还没提交的行（同批内再次命中自然键时要就地覆盖，
        // 它们此刻在 store 里还查不到，所以先在内存里留一份）。
        let mut pending: HashMap<i64, BTreeMap<String, Json>> = HashMap::new();

        // 读表定义，拿到当前的 next_rowid（每行一个自增 rowid）。
        let tbl_raw = db
            .store()
            .get(&format!("tbl/{}", plan.table))
            .map(|s| s.to_string())
            .ok_or_else(|| format!("表「{}」不见了（可能已被删除）", plan.table))?;
        let mut tbl: Table = serde_json::from_str(&tbl_raw)
            .map_err(|e| format!("表「{}」的定义读不出来: {e}", plan.table))?;

        for (i, row) in batch.iter().enumerate() {
            let row_no = cursor + i + 1; // 1 基，方便用户对着 Excel 找

            // 先判断这一行是不是"自然键整列为空"——这是"空编号"坑：空串是**合法键值**，
            // 会互相去重成同一行，必须计数、报告给用户，但**不算 NOT NULL 违规**
            // （否则会在真正的"必填列漏填"和"编号故意留空"之间说不清）。
            let blank_key = plan.has_natural_key
                && plan.key_indexes.iter().all(|&k| row[k].trim().is_empty());

            // 逐列类型校验 + NOT NULL 校验。空白自然键列豁免 NOT NULL（见上）。
            for (ci, col) in plan.columns.iter().enumerate() {
                let raw = row.get(ci).map(|s| s.as_str()).unwrap_or("");
                let exempt = blank_key && plan.has_natural_key && plan.key_indexes.contains(&ci);
                if plan.not_null_cols.contains(col) && raw.trim().is_empty() && !exempt {
                    return Err(format!(
                        "第 {row_no} 行：列「{col}」是 NOT NULL，不能为空"
                    ));
                }
                if let Some(ty) = plan.col_types.get(col) {
                    if let Err(e) = validate_value(*ty, raw) {
                        return Err(format!("第 {row_no} 行{e}"));
                    }
                }
            }

            if blank_key {
                counts.blank_key += 1;
            }

            let key = plan.key_tuple(row);

            if plan.has_natural_key {
                if let Some(&existing_rid) = plan.dedup.get(&key) {
                    if plan.update_existing {
                        update_record(db, &mut b, plan, &pending, existing_rid, row)?;
                        counts.updated += 1;
                    } else {
                        counts.unchanged += 1;
                    }
                    continue;
                }
                // 新行：分配 rowid 并落盘，记指纹。
                let rid = tbl.next_rowid;
                tbl.next_rowid += 1;
                let rec = write_record(&mut b, plan, rid, row)?;
                pending.insert(rid, rec);
                let fp = fingerprint_row(&plan.row_values(row));
                b.set(
                    job_row_key(job_id, row_seq),
                    serde_json::to_string(&RowFp { rid, fp }).map_err(|e| e.to_string())?,
                );
                row_seq += 1;
                counts.inserted += 1;
                // 本批内若再次出现同样的自然键，也按"已存在"处理（不重复插入）。
                plan.dedup.insert(key, rid);
            } else {
                let rid = tbl.next_rowid;
                tbl.next_rowid += 1;
                let rec = write_record(&mut b, plan, rid, row)?;
                pending.insert(rid, rec);
                let fp = fingerprint_row(&plan.row_values(row));
                b.set(
                    job_row_key(job_id, row_seq),
                    serde_json::to_string(&RowFp { rid, fp }).map_err(|e| e.to_string())?,
                );
                row_seq += 1;
                counts.inserted += 1;
            }
        }

        // 写回表定义（next_rowid 推进）。
        b.set(
            format!("tbl/{}", plan.table),
            serde_json::to_string(&tbl).map_err(|e| format!("序列化表定义失败: {e}"))?,
        );

        // 作业进度与数据同一笔提交：崩溃后库里的数字永远和实际行数对得上。
        let mut m = load_job(db, job_id)
            .ok_or_else(|| format!("作业 {job_id} 不见了（可能已被删除）"))?;
        m.committed_rows = end as u64;
        m.inserted_rows = counts.inserted;
        m.updated_rows = counts.updated;
        m.unchanged_rows = counts.unchanged;
        m.blank_key_rows = counts.blank_key;
        b.set(
            job_key(job_id),
            serde_json::to_string(&m).map_err(|e| e.to_string())?,
        );

        db.store_mut()
            .commit(b)
            .map_err(|e| format!("提交第 {} 批失败：{e}", batches + 1))?;

        batches += 1;
        cursor = end;
        on_progress(end as u64, total);
    }

    Ok((counts, batches))
}

/// 把一行业务字段写成 `rec/<表>/<rowid>` 记录（含 `_rowid`），返回这份记录供本批内复用。
fn write_record(b: &mut Batch, plan: &Plan, rid: i64, row: &[String]) -> Result<BTreeMap<String, Json>> {
    let mut rec: BTreeMap<String, Json> = BTreeMap::new();
    rec.insert(model::ROWID_COLUMN.to_string(), Json::from(rid));
    for (ci, col) in plan.columns.iter().enumerate() {
        rec.insert(col.clone(), Json::from(row[ci].clone()));
    }
    b.set(
        record_key(&plan.table, rid),
        serde_json::to_string(&rec).map_err(|e| format!("序列化记录失败: {e}"))?,
    );
    Ok(rec)
}

/// 覆盖一条已存在记录的指定业务列（保留 `_rowid` 与其它列）。
///
/// 同一批内刚写进去、还没提交的行在 store 里还查不到，所以优先用 `pending` 里的
/// 那份；其余情况从已提交的 store 里读原记录再覆盖导入列。
fn update_record(
    db: &Db,
    b: &mut Batch,
    plan: &Plan,
    pending: &HashMap<i64, BTreeMap<String, Json>>,
    rid: i64,
    row: &[String],
) -> Result<()> {
    let key = record_key(&plan.table, rid);
    let mut rec = if let Some(r) = pending.get(&rid) {
        r.clone()
    } else {
        let raw = db
            .store()
            .get(&key)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("要覆盖的行（rid={rid}）不见了，可能已被删除"))?;
        serde_json::from_str(&raw).map_err(|e| format!("记录读不出来: {e}"))?
    };
    rec.insert(model::ROWID_COLUMN.to_string(), Json::from(rid));
    for (ci, col) in plan.columns.iter().enumerate() {
        rec.insert(col.clone(), Json::from(row[ci].clone()));
    }
    b.set(
        key,
        serde_json::to_string(&rec).map_err(|e| format!("序列化记录失败: {e}"))?,
    );
    Ok(())
}

// ---------------------------------------------------------------- 作业元数据

fn create_job(db: &mut Db, job_id: &str, plan: &Plan, total: u64) -> Result<()> {
    let m = JobMeta {
        id: job_id.to_string(),
        table: plan.table.clone(),
        state: JobState::Running.as_str().to_string(),
        total_rows: total,
        committed_rows: 0,
        inserted_rows: 0,
        updated_rows: 0,
        unchanged_rows: 0,
        blank_key_rows: 0,
        columns_text: plan.columns.join(&SEP.to_string()),
        natural_key_text: plan.natural_key.join(&SEP.to_string()),
        update_existing: plan.update_existing,
        batch_rows: plan.batch_rows,
        started_at: now_ms(),
        finished_at: None,
        error: None,
    };
    save_job(db, &m)
}

fn set_job_state(db: &mut Db, job_id: &str, state: JobState, err: Option<&str>) -> Result<()> {
    let mut m = load_job(db, job_id).ok_or_else(|| format!("找不到导入作业 {job_id}"))?;
    m.state = state.as_str().to_string();
    m.finished_at = Some(now_ms());
    m.error = err.map(|s| s.to_string());
    save_job(db, &m)
}

fn load_job_rows(db: &Db, job_id: &str) -> Vec<(String, RowFp)> {
    let prefix = format!("job/{job_id}/row/");
    let mut out = Vec::new();
    for (k, v) in db.store().scan(&prefix) {
        if let Ok(rf) = serde_json::from_str::<RowFp>(&v) {
            out.push((k, rf));
        }
    }
    out
}

// ---------------------------------------------------------------- 校验与拼装

/// 一次导入的"执行计划"：校验通过后把去重索引与列信息也准备好，批次循环里不再做字符串活。
struct Plan {
    /// 用户给的表名（只用于提示语）
    table: String,
    columns: Vec<String>,
    /// 各列的类型（去重与校验用）
    col_types: HashMap<String, ColType>,
    /// 自然键各列在 `row` 里的下标
    key_indexes: Vec<usize>,
    natural_key: Vec<String>,
    has_natural_key: bool,
    update_existing: bool,
    batch_rows: usize,
    /// 表上 NOT NULL、但本次导入没有提供的列（会先被结构检查拦下）
    not_null_cols: HashSet<String>,
    /// 已存在行的"自然键 → rowid"映射（兼做幂等去重与"表里已有重复键"检测）
    dedup: HashMap<Vec<String>, i64>,
}

impl Plan {
    fn build(db: &Db, opts: &ImportOptions, rows: &[Vec<String>]) -> Result<Plan> {
        if opts.table.trim().is_empty() {
            return Err("没有指定要导入到哪张表".to_string());
        }
        if opts.table.contains('\0') {
            return Err("表名里有非法字符".to_string());
        }
        // 保留给「导入模块自己的元数据」的名字（新引擎里元数据在 job/ 前缀下，
        // 但这两个名字仍然不许当表名，免得和旧库/导出文件对不上）
        if opts.table == "_import_job" || opts.table == "_import_row" {
            return Err(format!(
                "{} 是数据库自己或导入模块的元数据表，不能作为导入目标",
                opts.table
            ));
        }
        if opts.columns.is_empty() {
            return Err("没有指定目标列".to_string());
        }
        if opts.batch_rows == 0 {
            return Err("每批行数不能是 0（每批至少要能提交 1 行）".to_string());
        }
        if opts.columns.iter().any(|c| c.contains('\0')) {
            return Err("列名里有非法字符".to_string());
        }
        for (i, c) in opts.columns.iter().enumerate() {
            if opts.columns[..i].iter().any(|p| p.eq_ignore_ascii_case(c)) {
                return Err(format!("目标列里有两个「{c}」，列名不能重复"));
            }
        }
        if opts.columns.iter().any(|c| c.eq_ignore_ascii_case(JOB_COLUMN)) {
            return Err(format!(
                "{JOB_COLUMN} 是导入模块用来标记数据来源的列，不能当作业务列导入"
            ));
        }

        // 表必须已经存在（建表由调用方负责）。
        let ti = db
            .get_table(&opts.table)
            .map_err(|_| {
                format!(
                    "库里没有表「{}」：导入只负责写数据，建表由调用方负责",
                    opts.table
                )
            })?;
        let actual: Vec<String> = ti.columns.iter().map(|c| c.name.clone()).collect();
        let mut col_types: HashMap<String, ColType> = HashMap::new();
        let mut not_null_cols: HashSet<String> = HashSet::new();
        for c in &ti.columns {
            if let Some(ty) = ColType::from_name(&c.decl_type.to_lowercase()) {
                col_types.insert(c.name.clone(), ty);
            }
            if c.not_null {
                not_null_cols.insert(c.name.clone());
            }
        }

        // 缺列检查
        let mut missing = Vec::new();
        for c in &opts.columns {
            if !actual.iter().any(|a| a.eq_ignore_ascii_case(c)) {
                missing.push(c.clone());
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "目标表 {} 里没有这些列：{}（表里现有：{}）",
                opts.table,
                missing.join("、"),
                actual.join("、")
            ));
        }

        let mut key_indexes = Vec::new();
        for k in &opts.natural_key {
            let pos = opts
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(k))
                .ok_or_else(|| {
                    format!(
                        "自然键里的「{k}」不在本次导入的目标列里（目标列：{}）。\
                         自然键必须是这次真的导进来的列，否则没法用它去重",
                        opts.columns.join("、")
                    )
                })?;
            key_indexes.push(pos);
        }

        // 行宽检查放在最前面：一行没写就报错，比"导到一半才炸"体面得多。
        for (i, row) in rows.iter().enumerate() {
            if row.len() != opts.columns.len() {
                return Err(format!(
                    "第 {} 行有 {} 列，但目标列有 {} 个（{}）",
                    i + 1,
                    row.len(),
                    opts.columns.len(),
                    opts.columns.join("、")
                ));
            }
        }

        // 从表里已有的记录建"自然键 → rowid"映射。
        // 同时检测：如果表里已经有重复的自然键，幂等就没法保证，必须报错让用户先去重。
        let mut dedup: HashMap<Vec<String>, i64> = HashMap::new();
        if !opts.natural_key.is_empty() {
            let prefix = record_prefix(&opts.table);
            for (_, v) in db.store().scan(&prefix) {
                let rec: BTreeMap<String, Json> = match serde_json::from_str(&v) {
                    Ok(r) => r,
                    Err(e) => return Err(format!("记录读不出来: {e}")),
                };
                let key: Vec<String> = opts
                    .natural_key
                    .iter()
                    .map(|nk| {
                        rec.get(nk)
                            .map(|x| json_to_string(x).trim().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                if dedup.contains_key(&key) {
                    return Err(format!(
                        "表「{}」里自然键（{}）已经有重复数据，无法保证导入是幂等的：\
                         请先去重再导入 —— 有重复的情况下没法保证导入是幂等的。",
                        opts.table,
                        opts.natural_key.join("、")
                    ));
                }
                let rid = rec
                    .get(model::ROWID_COLUMN)
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                dedup.insert(key, rid);
            }
        }

        Ok(Plan {
            table: opts.table.clone(),
            columns: opts.columns.clone(),
            col_types,
            key_indexes,
            natural_key: opts.natural_key.clone(),
            has_natural_key: !opts.natural_key.is_empty(),
            update_existing: opts.update_existing,
            batch_rows: opts.batch_rows,
            not_null_cols,
            dedup,
        })
    }

    /// 自然键各列的值（trim 后），顺序与 `natural_key` 一致。
    fn key_tuple(&self, row: &[String]) -> Vec<String> {
        self.key_indexes
            .iter()
            .map(|&i| row[i].trim().to_string())
            .collect()
    }

    /// 这一行所有业务列的值（按 `columns` 顺序），用于算指纹。
    fn row_values(&self, row: &[String]) -> Vec<String> {
        self.columns
            .iter()
            .map(|c| row_value(row, &self.columns, c))
            .collect()
    }
}

/// 从 row 里取出某一列的值：按 `columns` 顺序定位下标再取。
/// `columns` 不会有重复（Plan::build 已查），线性查找足够。
fn row_value(row: &[String], columns: &[String], col: &str) -> String {
    columns
        .iter()
        .position(|c| c == col)
        .and_then(|i| row.get(i).cloned())
        .unwrap_or_default()
}

/// 逐列类型校验：非空值必须符合列类型（整数 / 实数 / 金额 / 是否…）。
///
/// 旧代码靠 SQLite 的亲和性在写入时拦下脏值；新引擎不强制，这里在导入期先拦，
/// 免得脏值静默落库。空串交给 NOT NULL 检查处理，这里放行。
fn validate_value(ty: ColType, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    match ty {
        ColType::Integer => {
            if raw.replace([',', ' '], "").parse::<i64>().is_err() {
                return Err(format!("列的值「{raw}」不是整数"));
            }
        }
        ColType::Real => {
            if raw.replace([',', ' '], "").parse::<f64>().is_err() {
                return Err(format!("列的值「{raw}」不是数字"));
            }
        }
        ColType::Money => {
            if money_parse(raw).is_err() {
                return Err(format!("列的值「{raw}」不是金额"));
            }
        }
        ColType::Boolean => {
            let s = raw.to_lowercase();
            if ![
                "1", "true", "yes", "y", "是", "真", "对", "0", "false", "no", "n", "否", "假", "错",
            ]
            .contains(&s.as_str())
            {
                return Err(format!("列的值「{raw}」不是是/否值"));
            }
        }
        // Date / DateTime / Json / Blob / Text 一律放行（不强制格式）。
        _ => {}
    }
    Ok(())
}

/// 金额解析（分）：与 [`crate::model::money_parse`] 同口径，但本模块不依赖其可见性。
fn money_parse(raw: &str) -> Result<i64> {
    let s = raw.trim().replace([',', ' ', '¥', '￥'], "");
    if s.is_empty() {
        return Err("金额为空".to_string());
    }
    let neg = s.starts_with('-') || (s.starts_with('(') && s.ends_with(')'));
    let body = s.trim_start_matches('-').trim_matches(|c| c == '(' || c == ')');
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit()) && !int_part.is_empty() {
        return Err(format!("「{raw}」不是金额"));
    }
    if !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("「{raw}」不是金额"));
    }
    let yuan: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse::<i64>().map_err(|_| format!("「{raw}」不是金额"))?
    };
    let mut frac = frac_part.to_string();
    while frac.len() < 2 {
        frac.push('0');
    }
    let cents: i64 = frac[..2].parse::<i64>().map_err(|_| format!("「{raw}」不是金额"))?;
    let v = yuan * 100 + cents;
    Ok(if neg { -v } else { v })
}

/// 一行内容（本次导入的目标列）的指纹。
///
/// 自己写 FNV-1a 而不是用 `DefaultHasher`：撤销可能发生在**另一次进程启动**里，
/// 标准库不保证哈希器跨版本稳定，指纹一旦对不上就变成"用户改过了"的误判，
/// 结果就是撤销删不掉本该删的行。20 行代码换一个永久成立的判断，值。
fn fingerprint_row(values: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in values {
        for b in v.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // 0xFF 在合法 UTF-8 里永远不会出现，用它当字段分隔符不会撞：
        // ["ab","c"] 和 ["a","bc"] 必须给出不同的指纹
        h ^= 0xFF;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ================================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 造一个临时目录库（新引擎的 `Db::open` 吃一个目录）。
    fn tmp_db(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deskbase_import_{tag}_{}_{}",
            std::process::id(),
            ulid::Ulid::generate()
        ))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn open_db(path: &std::path::Path) -> Db {
        Db::open(path).unwrap()
    }

    /// 业务表由调用方建 —— 这里就照着"调用方"的样子建一张
    fn orders_table(db: &mut Db) {
        let cols = vec![
            col("id", ColType::Integer),
            col("no", ColType::Text),
            col("customer", ColType::Text),
            col("amount", ColType::Text),
            col("qty", ColType::Integer),
        ];
        let spec = TableSpec {
            name: "orders".to_string(),
            comment: None,
            columns: cols,
        };
        db.create_table(&spec).unwrap();
    }

    /// 自定义表（可带不同类型列，用于类型校验 / 亲和性测试）
    fn make_table(db: &mut Db, name: &str, cols: Vec<ColumnDef>) {
        let spec = TableSpec {
            name: name.to_string(),
            comment: None,
            columns: cols,
        };
        db.create_table(&spec).unwrap();
    }

    fn col(name: &str, ty: ColType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty,
            not_null: name == "no" || name == "customer",
            default: None,
            primary_key: name == "id",
            comment: None,
            shared: None,
            link: None,
            lookup: None,
            rollup: None,
        }
    }

    fn rows(n: usize) -> Vec<Vec<String>> {
        (0..n)
            .map(|i| {
                vec![
                    format!("NO{i:05}"),
                    format!("客户{i}"),
                    format!("{}", i * 10),
                ]
            })
            .collect()
    }

    fn opts(cols: &[&str]) -> ImportOptions {
        ImportOptions::new("orders", cols).with_natural_key(&["no"])
    }

    /// 统计业务表里的记录数
    fn count_rows(db: &Db, table: &str) -> usize {
        db.store().scan(&record_prefix(table)).len()
    }

    /// 读回某张表所有记录（列名 → 值）
    fn all_records(db: &Db, table: &str) -> Vec<BTreeMap<String, Json>> {
        db.store()
            .scan(&record_prefix(table))
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_str(&v).ok())
            .collect()
    }

    fn rec_get(rec: &BTreeMap<String, Json>, col: &str) -> String {
        rec.get(col).map(json_to_string).unwrap_or_default()
    }

    fn job_key_count(db: &Db) -> usize {
        db.store().scan("job/").len()
    }

    fn freeze(_: u64, _: u64) {}

    // ---------------------------------------------------------- 基础

    #[test]
    fn small_batch_import_reads_back_unchanged() {
        let p = tmp_db("small");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let rep = import(
            &mut db,
            &rows(5),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();

        assert_eq!(rep.inserted, 5);
        assert_eq!(rep.updated, 0);
        assert_eq!(rep.unchanged, 0);
        assert_eq!(rep.job.state, JobState::Done);
        assert_eq!(rep.job.committed_rows, 5);
        assert_eq!(rep.job.total_rows, 5);
        assert_eq!(rep.batches, 1);
        assert_eq!(count_rows(&db, "orders"), 5);

        // 中文与列顺序都要能原样往返
        let rec = all_records(&db, "orders")
            .into_iter()
            .find(|r| rec_get(r, "no") == "NO00003")
            .unwrap();
        assert_eq!(rec_get(&rec, "no"), "NO00003");
        assert_eq!(rec_get(&rec, "customer"), "客户3");
        assert_eq!(rec_get(&rec, "amount"), "30");

        // 每一行都记着指纹 —— 撤销唯一的定位依据（行清单条数 = 插入条数）
        assert_eq!(load_job_rows(&db, &rep.job.id).len(), 5);

        // 作业状态是**落库**的，不是内存里的
        let job = get_job(&db, &rep.job.id).unwrap().unwrap();
        assert_eq!(job, rep.job);
        cleanup(&p);
    }

    #[test]
    fn natural_key_dedup_second_import_adds_nothing() {
        let p = tmp_db("idem");
        let mut db = open_db(&p);
        orders_table(&mut db);
        let o = opts(&["no", "customer", "amount"]);

        let first = import(&mut db, &rows(5), &o, &mut freeze).unwrap();
        assert_eq!(first.inserted, 5);

        // 重跑同一个文件：幂等靠自然键，不靠"记住上次导过"
        let second = import(&mut db, &rows(5), &o, &mut freeze).unwrap();
        assert_eq!(second.inserted, 0, "第二次不该新增");
        assert_eq!(second.unchanged, 5);
        assert_eq!(count_rows(&db, "orders"), 5);
        cleanup(&p);
    }

    #[test]
    fn mid_run_inserted_rows_only_new_ones_written() {
        let p = tmp_db("partial");
        let mut db = open_db(&p);
        orders_table(&mut db);
        let o = opts(&["no", "customer", "amount"]);

        import(&mut db, &rows(3), &o, &mut freeze).unwrap();
        let mut data = rows(3);
        data.extend(rows(5).into_iter().skip(3)); // 追加 NO00003、NO00004
        let rep = import(&mut db, &data, &o, &mut freeze).unwrap();

        assert_eq!(rep.inserted, 2);
        assert_eq!(rep.unchanged, 3);
        assert_eq!(count_rows(&db, "orders"), 5);
        cleanup(&p);
    }

    #[test]
    fn update_existing_false_keeps_true_overwrites() {
        let p = tmp_db("update");
        let mut db = open_db(&p);
        orders_table(&mut db);
        // 用户自己先录了一行
        db.insert_rows(
            "orders",
            &["no".to_string(), "customer".to_string(), "amount".to_string()],
            &[vec![
                Some("NO00001".to_string()),
                Some("老客户".to_string()),
                Some("999".to_string()),
            ]],
        )
        .unwrap();

        let keep = import(
            &mut db,
            &rows(3),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(keep.inserted, 2);
        assert_eq!(keep.unchanged, 1);
        assert_eq!(keep.updated, 0);
        let who = all_records(&db, "orders")
            .into_iter()
            .find(|r| rec_get(r, "no") == "NO00001")
            .map(|r| rec_get(&r, "customer"))
            .unwrap();
        assert_eq!(who, "老客户", "默认不该动已有行");

        let over = import(
            &mut db,
            &rows(3),
            &opts(&["no", "customer", "amount"]).overwrite(),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(over.inserted, 0);
        assert_eq!(over.updated, 3, "三行都命中了已有自然键");
        let who = all_records(&db, "orders")
            .into_iter()
            .find(|r| rec_get(r, "no") == "NO00001")
            .map(|r| rec_get(&r, "customer"))
            .unwrap();
        assert_eq!(who, "客户1");
        assert_eq!(count_rows(&db, "orders"), 3);
        cleanup(&p);
    }

    #[test]
    fn constraint_violation_errors_not_silently_swallowed() {
        let p = tmp_db("notnull");
        let mut db = open_db(&p);
        orders_table(&mut db);

        // customer 是 NOT NULL，但这次导入不包含它。
        // 如果用 INSERT OR IGNORE，这一行会被悄悄丢掉 —— 用户永远不知道少了数据。
        let o = ImportOptions::new("orders", &["no", "amount"]).with_natural_key(&["no"]);
        let data = vec![vec!["N1".to_string(), "10".to_string()]];
        let err = import(&mut db, &data, &o, &mut freeze).unwrap_err();

        assert!(
            err.to_uppercase().contains("NOT NULL"),
            "要如实报出 NOT NULL 违规，实际：{err}"
        );
        assert_eq!(count_rows(&db, "orders"), 0, "整批回滚，一行不留");
        let jobs = list_jobs(&db).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
        assert_eq!(jobs[0].committed_rows, 0, "进度要如实记录");
        cleanup(&p);
    }

    // ---------------------------------------------------------- 校验

    #[test]
    fn column_count_mismatch_reports_row() {
        let p = tmp_db("width");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let mut data = rows(2);
        data.push(vec!["NO00002".to_string(), "客户2".to_string()]); // 少一列
        let err = import(
            &mut db,
            &data,
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap_err();

        assert!(err.contains("第 3 行"), "错误里要指到具体行：{err}");
        assert!(err.contains('2') && err.contains('3'), "要说清几列对几列：{err}");
        // 校验在任何写入之前完成：一行都不该写进去
        assert_eq!(count_rows(&db, "orders"), 0);
        assert_eq!(job_key_count(&db), 0);
        cleanup(&p);
    }

    #[test]
    fn missing_target_column_reports_column() {
        let p = tmp_db("nocol");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let err = import(
            &mut db,
            &rows(1),
            &opts(&["no", "customer", "备注"]),
            &mut freeze,
        )
        .unwrap_err();
        assert!(err.contains("备注"), "要点名缺的列：{err}");
        assert!(err.contains("orders"), "要说清是哪张表：{err}");
        cleanup(&p);
    }

    #[test]
    fn natural_key_must_be_in_this_import() {
        let p = tmp_db("badkey");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let o = ImportOptions::new("orders", &["customer", "amount"]).with_natural_key(&["no"]);
        let err = import(&mut db, &rows(1), &o, &mut freeze).unwrap_err();
        assert!(err.contains("no"), "要点名是哪个键：{err}");
        cleanup(&p);
    }

    #[test]
    fn metadata_columns_not_imported_as_business() {
        let p = tmp_db("reserved");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let err = import(
            &mut db,
            &vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]],
            &opts(&["no", "_import_job", "amount"]),
            &mut freeze,
        )
        .unwrap_err();
        assert!(err.contains(JOB_COLUMN), "实际：{err}");
        cleanup(&p);
    }

    #[test]
    fn missing_target_table_errors_not_created() {
        let p = tmp_db("notable");
        let mut db = open_db(&p);
        let err = import(&mut db, &rows(1), &opts(&["no"]), &mut freeze).unwrap_err();
        assert!(err.contains("orders"), "实际：{err}");
        cleanup(&p);
    }

    #[test]
    fn empty_input_no_error_no_write() {
        let p = tmp_db("empty");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let rep = import(
            &mut db,
            &[],
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();

        assert_eq!(rep.job.total_rows, 0);
        assert_eq!(rep.inserted, 0);
        assert_eq!(rep.batches, 0);
        // 连作业元数据都不该建出来：用户取消导入不该留下任何痕迹
        assert_eq!(job_key_count(&db), 0);
        // 业务表也不该被加列
        let cols = db.get_table("orders").unwrap().columns;
        assert!(
            !cols.iter().any(|c| c.name == JOB_COLUMN),
            "空导入不该动业务表：{cols:?}"
        );
        cleanup(&p);
    }

    // ---------------------------------------------------------- 分批与进度

    #[test]
    fn multi_batch_commit_with_consistent_progress() {
        let p = tmp_db("batch");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let rep = import(&mut db, &rows(250), &o, &mut |done, total| {
            seen.push((done, total));
        })
        .unwrap();

        // 250 行 / 每批 100 → 3 批
        assert_eq!(rep.batches, 3, "要真的是三次提交，不是一个大事务");
        // 回调 = 开头的 (0,total) + 每批一次
        assert_eq!(
            seen,
            vec![(0, 250), (100, 250), (200, 250), (250, 250)],
            "每次提交都要报一次进度"
        );
        assert_eq!(count_rows(&db, "orders"), 250);
        // 落库的进度和真实行数一致
        let job = get_job(&db, &rep.job.id).unwrap().unwrap();
        assert_eq!(job.committed_rows, 250);
        cleanup(&p);
    }

    #[test]
    fn progress_monotonic_ends_at_total() {
        let p = tmp_db("progress");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let o = opts(&["no", "customer", "amount"]).with_batch_rows(3);
        let mut done_seq: Vec<u64> = Vec::new();
        let mut total_seq: Vec<u64> = Vec::new();
        let rep = import(&mut db, &rows(10), &o, &mut |done, total| {
            done_seq.push(done);
            total_seq.push(total);
        })
        .unwrap();

        assert!(done_seq.len() > 1, "要被多次调用：{done_seq:?}");
        assert!(
            done_seq.windows(2).all(|w| w[1] >= w[0]),
            "已提交行数不能回退：{done_seq:?}"
        );
        assert!(total_seq.iter().all(|&t| t == 10), "总数要一直是 10：{total_seq:?}");
        assert_eq!(*done_seq.last().unwrap(), 10);
        assert_eq!(rep.batches, 4); // 3+3+3+1
        cleanup(&p);
    }

    // ---------------------------------------------------------- 撤销

    #[test]
    fn undo_deletes_only_this_job_rows() {
        let p = tmp_db("undo");
        let mut db = open_db(&p);
        orders_table(&mut db);
        // 两行是用户自己录的：没有进任何作业的行清单，撤销绝不能碰
        db.insert_rows(
            "orders",
            &["no".to_string(), "customer".to_string(), "amount".to_string()],
            &[
                vec![
                    Some("KEEP1".to_string()),
                    Some("用户自己录的".to_string()),
                    Some("1".to_string()),
                ],
                vec![
                    Some("KEEP2".to_string()),
                    Some("也是".to_string()),
                    Some("2".to_string()),
                ],
            ],
        )
        .unwrap();

        let rep = import(
            &mut db,
            &rows(4),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(count_rows(&db, "orders"), 6);

        let deleted = undo(&mut db, &rep.job.id).unwrap();
        assert_eq!(deleted, 4);
        assert_eq!(count_rows(&db, "orders"), 2);
        // 用户自己录的两行必须原封不动
        let keepers: Vec<String> = all_records(&db, "orders")
            .iter()
            .map(|r| rec_get(r, "no"))
            .collect();
        assert!(keepers.contains(&"KEEP1".to_string()));
        assert!(keepers.contains(&"KEEP2".to_string()));
        assert_eq!(load_job_rows(&db, &rep.job.id).len(), 0, "指纹清单应被清掉");

        let job = get_job(&db, &rep.job.id).unwrap().unwrap();
        assert_eq!(job.state, JobState::Undone);
        // 重复撤销要报错，不能悄悄再删一次
        assert!(undo(&mut db, &rep.job.id).is_err());
        cleanup(&p);
    }

    #[test]
    fn undo_leaves_user_edited_rows() {
        let p = tmp_db("undo_modified");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let rep = import(
            &mut db,
            &rows(3),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        // 用户导完之后自己改了一行：找到它的 rid 再改
        let rid = all_records(&db, "orders")
            .into_iter()
            .find(|r| rec_get(r, "no") == "NO00001")
            .and_then(|r| r.get(model::ROWID_COLUMN).and_then(|x| x.as_i64()))
            .unwrap();
        db.update_cell("orders", rid, "amount", Some("改过了")).unwrap();

        let r = undo_with_report(&mut db, &rep.job.id).unwrap();
        assert_eq!(r.deleted, 2);
        assert_eq!(r.kept_modified, 1, "改过的那行要留下");
        assert_eq!(count_rows(&db, "orders"), 1);
        let keep = all_records(&db, "orders")
            .into_iter()
            .find(|r| rec_get(r, "no") == "NO00001")
            .map(|r| rec_get(&r, "amount"))
            .unwrap();
        assert_eq!(keep, "改过了", "用户的修改必须活着");
        cleanup(&p);
    }

    #[test]
    fn undo_keeps_previously_overwritten_rows() {
        let p = tmp_db("undo_updated");
        let mut db = open_db(&p);
        orders_table(&mut db);
        db.insert_rows(
            "orders",
            &["no".to_string(), "customer".to_string(), "amount".to_string()],
            &[vec![
                Some("NO00000".to_string()),
                Some("老数据".to_string()),
                Some("1".to_string()),
            ]],
        )
        .unwrap();

        let rep = import(
            &mut db,
            &rows(3),
            &opts(&["no", "customer", "amount"]).overwrite(),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(rep.inserted, 2);
        assert_eq!(rep.updated, 1);

        let r = undo_with_report(&mut db, &rep.job.id).unwrap();
        assert_eq!(r.deleted, 2, "只删本次新插进去的两行");
        assert_eq!(r.kept_updated, 1);
        assert_eq!(count_rows(&db, "orders"), 1);
        assert_eq!(
            all_records(&db, "orders")
                .iter()
                .find(|r| rec_get(r, "no") == "NO00000")
                .is_some(),
            true
        );
        cleanup(&p);
    }

    #[test]
    fn reimport_after_undo_works() {
        let p = tmp_db("redo");
        let mut db = open_db(&p);
        orders_table(&mut db);
        let o = opts(&["no", "customer", "amount"]);

        let first = import(&mut db, &rows(3), &o, &mut freeze).unwrap();
        assert_eq!(undo(&mut db, &first.job.id).unwrap(), 3);
        assert_eq!(count_rows(&db, "orders"), 0);

        // 撤销就得是"当作没导过"：再导一次必须能全量写进去，
        // 不能因为残留的唯一索引/指纹把行挡住
        let again = import(&mut db, &rows(3), &o, &mut freeze).unwrap();
        assert_eq!(again.inserted, 3);
        assert_eq!(count_rows(&db, "orders"), 3);
        cleanup(&p);
    }

    #[test]
    fn in_memory_dedup_gives_idempotence_without_unique_index() {
        let p = tmp_db("nkidx");
        let mut db = open_db(&p);
        // 不带任何唯一索引：幂等完全靠本模块的内存去重映射
        make_table(
            &mut db,
            "orders",
            vec![
                col("no", ColType::Text),
                col("customer", ColType::Text),
                col("amount", ColType::Text),
            ],
        );

        let o = opts(&["no", "customer", "amount"]);
        let first = import(&mut db, &rows(4), &o, &mut freeze).unwrap();
        assert_eq!(first.inserted, 4);

        // 重跑必须幂等：不靠数据库唯一索引，靠自然键去重
        let second = import(&mut db, &rows(4), &o, &mut freeze).unwrap();
        assert_eq!(second.inserted, 0, "有自然键去重才有幂等");
        assert_eq!(second.unchanged, 4);
        cleanup(&p);
    }

    #[test]
    fn duplicate_natural_keys_in_table_errors() {
        let p = tmp_db("dupkey");
        let mut db = open_db(&p);
        make_table(
            &mut db,
            "orders",
            vec![
                col("no", ColType::Text),
                col("customer", ColType::Text),
                col("amount", ColType::Text),
            ],
        );
        db.insert_rows(
            "orders",
            &["no".to_string(), "customer".to_string(), "amount".to_string()],
            &[
                vec![Some("DUP".to_string()), Some("a".to_string()), Some("1".to_string())],
                vec![Some("DUP".to_string()), Some("b".to_string()), Some("2".to_string())],
            ],
        )
        .unwrap();

        let err = import(
            &mut db,
            &rows(1),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap_err();
        assert!(err.contains("去重") || err.contains("唯一索引"), "要给出去重建议：{err}");
        assert_eq!(count_rows(&db, "orders"), 2, "原有数据一行不动");
        cleanup(&p);
    }

    #[test]
    fn undo_only_touches_own_job() {
        let p = tmp_db("tworun");
        let mut db = open_db(&p);
        orders_table(&mut db);
        let o = opts(&["no", "customer", "amount"]);

        let a = import(&mut db, &rows(3), &o, &mut freeze).unwrap();
        let mut more = rows(3);
        more.push(vec!["NO00009".to_string(), "另一批".to_string(), "9".to_string()]);
        let b = import(&mut db, &more, &o, &mut freeze).unwrap();
        assert_eq!(b.inserted, 1);
        assert_eq!(count_rows(&db, "orders"), 4);

        // 撤销 A 只该删掉 A 写的那 3 行，B 补的那行要留下
        assert_eq!(undo(&mut db, &a.job.id).unwrap(), 3);
        assert_eq!(count_rows(&db, "orders"), 1);
        assert_eq!(load_job_rows(&db, &b.job.id).len(), 1, "另一个作业的清单不该被碰");
        assert_eq!(
            get_job(&db, &b.job.id).unwrap().unwrap().state,
            JobState::Done
        );
        cleanup(&p);
    }

    #[test]
    fn numeric_affinity_not_mistaken_for_user_edit() {
        let p = tmp_db("affinity");
        let mut db = open_db(&p);
        // qty 是 INTEGER：输入串 "007" 原样存成字符串，指纹按原串算，绝不靠数据库亲和性。
        make_table(
            &mut db,
            "orders",
            vec![
                col("no", ColType::Text),
                col("qty", ColType::Integer),
            ],
        );

        let o = ImportOptions::new("orders", &["no", "qty"]).with_natural_key(&["no"]);
        let data = vec![
            vec!["A1".to_string(), "007".to_string()],
            vec!["A2".to_string(), " 12 ".to_string()],
        ];
        let rep = import(&mut db, &data, &o, &mut freeze).unwrap();
        assert_eq!(rep.inserted, 2);

        let r = undo_with_report(&mut db, &rep.job.id).unwrap();
        assert_eq!(r.kept_modified, 0, "没人改过，不该有保留");
        assert_eq!(r.deleted, 2, "输入带前导零/空格也要能撤干净");
        assert_eq!(count_rows(&db, "orders"), 0);
        cleanup(&p);
    }

    #[test]
    fn empty_natural_key_rows_counted_and_named() {
        let p = tmp_db("blankkey");
        let mut db = open_db(&p);
        orders_table(&mut db);

        let data = vec![
            vec!["".to_string(), "没写编号".to_string(), "1".to_string()],
            vec!["NO00001".to_string(), "有编号".to_string(), "2".to_string()],
            vec!["".to_string(), "也没写".to_string(), "3".to_string()],
        ];
        let rep = import(
            &mut db,
            &data,
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();

        assert_eq!(rep.blank_key_rows, 2, "两行没编号");
        assert_eq!(rep.inserted, 2, "空编号只会留下一条，另一条被去重挡下");
        assert_eq!(rep.unchanged, 1);
        assert!(rep.summary().contains("空"), "报告要把这件事说出来：{}", rep.summary());
        cleanup(&p);
    }

    #[test]
    fn rerun_without_natural_key_duplicates_and_reports() {
        let p = tmp_db("nokey");
        let mut db = open_db(&p);
        orders_table(&mut db);
        // 不去重：重跑会产生重复
        let o = ImportOptions::new("orders", &["no", "customer", "amount"]);

        import(&mut db, &rows(2), &o, &mut freeze).unwrap();
        let rep = import(&mut db, &rows(2), &o, &mut freeze).unwrap();

        assert_eq!(rep.inserted, 2, "没有自然键就挡不住重复，这是如实报告而不是假装安全");
        assert_eq!(count_rows(&db, "orders"), 4);
        assert!(rep.summary().contains("orders"));
        cleanup(&p);
    }

    // ---------------------------------------------------------- 崩溃恢复

    /// 手工造出"作业还在进行中、行只写了一半"的现场 —— 这正是被强杀后库里的样子。
    #[test]
    fn crash_midway_reports_imported_and_pending() {
        let p = tmp_db("halfway");
        let mut db = open_db(&p);
        orders_table(&mut db);
        // 手工写 900 行（用模型自己的 insert_rows，保证 next_rowid 正确推进）。
        let col_names = vec![
            "no".to_string(),
            "customer".to_string(),
            "amount".to_string(),
        ];
        let mut data: Vec<Vec<Option<String>>> = Vec::new();
        for n in 0..900 {
            data.push(vec![
                Some(format!("NO{n:05}")),
                Some("客户".to_string()),
                Some("0".to_string()),
            ]);
        }
        db.insert_rows("orders", &col_names, &data).unwrap();

        // 手工造一个 running 作业：已提交 900，总 1000
        let m = JobMeta {
            id: "01JOBHALFWAY00000000000000".to_string(),
            table: "orders".to_string(),
            state: JobState::Running.as_str().to_string(),
            total_rows: 1000,
            committed_rows: 900,
            inserted_rows: 900,
            updated_rows: 0,
            unchanged_rows: 0,
            blank_key_rows: 0,
            columns_text: format!("no{c}customer{c}amount", c = SEP),
            natural_key_text: "no".to_string(),
            update_existing: false,
            batch_rows: 100,
            started_at: 1,
            finished_at: None,
            error: None,
        };
        save_job(&mut db, &m).unwrap();

        let pending = pending_jobs(&db).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, JobState::Running);
        assert_eq!(pending[0].table, "orders");
        assert_eq!(pending[0].committed_rows, 900, "已导入多少行");
        assert_eq!(pending[0].remaining_rows(), 100, "还差多少行");
        assert_eq!(count_rows(&db, "orders"), 900);

        let notice = recovery_notice(&db).unwrap().unwrap();
        assert!(notice.contains("900"), "提示里要有已导入行数：{notice}");
        assert!(notice.contains("100"), "提示里要有未导入行数：{notice}");
        assert!(notice.contains("orders"), "要说清是哪张表：{notice}");

        // 列一遍**绝不能**改动任何东西：不自动清理、不自动回滚
        let after = get_job(&db, "01JOBHALFWAY00000000000000").unwrap().unwrap();
        assert_eq!(after.state, JobState::Running);
        assert_eq!(after.committed_rows, 900);
        assert_eq!(count_rows(&db, "orders"), 900);

        // 用户可以选择"忽略"，那也只是改状态，数据一行不动
        dismiss(&mut db, "01JOBHALFWAY00000000000000").unwrap();
        assert!(pending_jobs(&db).unwrap().is_empty());
        assert_eq!(count_rows(&db, "orders"), 900);
        assert_eq!(recovery_notice(&db).unwrap(), None);
        cleanup(&p);
    }

    #[test]
    fn recovery_api_readonly_without_meta_table() {
        let p = tmp_db("nometa");
        let mut db = open_db(&p);
        orders_table(&mut db);

        assert!(pending_jobs(&db).unwrap().is_empty());
        assert!(list_jobs(&db).unwrap().is_empty());
        assert!(get_job(&db, "不存在").unwrap().is_none());
        assert_eq!(recovery_notice(&db).unwrap(), None);
        // 只读就真的只读：不该顺手把元数据建出来
        assert_eq!(job_key_count(&db), 0);
        cleanup(&p);
    }

    #[test]
    fn partial_commit_undoable_after_failure() {
        let p = tmp_db("failundo");
        let mut db = open_db(&p);
        orders_table(&mut db);

        // 让第一批顺利提交、第二批才炸：这样"失败≠回滚"才有东西可验。
        // 第 3 行（0 基第 2 行）的 qty 不是合法整数，触发导入期的类型校验。
        let o = ImportOptions::new("orders", &["no", "customer", "qty"])
            .with_natural_key(&["no"])
            .with_batch_rows(2);
        let mut data = Vec::new();
        for i in 0..4 {
            data.push(vec![
                format!("NO{i:05}"),
                format!("客户{i}"),
                if i == 2 { "坏".to_string() } else { i.to_string() },
            ]);
        }
        let err = import(&mut db, &data, &o, &mut freeze).unwrap_err();
        assert!(err.contains("第 3 行"), "要说清是哪一行炸的：{err}");
        assert!(err.contains("整数"), "实际：{err}");

        let jobs = list_jobs(&db).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
        assert_eq!(jobs[0].committed_rows, 2, "第一批已经提交了，进度要如实");
        assert_eq!(count_rows(&db, "orders"), 2);

        // 失败≠回滚：已提交的两行还在，用户可以选择撤销它们
        let r = undo_with_report(&mut db, &jobs[0].id).unwrap();
        assert_eq!(r.deleted, 2);
        assert_eq!(count_rows(&db, "orders"), 0);
        cleanup(&p);
    }

    // ---------------------------------------------------------- 跨进程强杀

    const CRASH_DB_ENV: &str = "DESKBASE_IMPORT_CRASH_DB";
    // ⚠️ 这个常量**必须和下面那个测试函数名一模一样** —— 子进程靠它筛测试。
    // 2026-09-26 给测试改名时，这里漏改过一次：子进程匹配不到任何测试，
    // 于是正常退出，断言报子进程本该被强杀，却正常退出了。
    // **测试名被字符串引用时，改名就不是零风险了。**
    const CRASH_TEST: &str = "kill_and_restart_reports_progress_data_usable";

    /// 真的起一个子进程，导到第二批发完就 `abort()` —— 不走析构、不回滚、
    /// 不改作业状态，和"用户在任务管理器里结束进程"是同一回事。
    #[test]
    fn kill_and_restart_reports_progress_data_usable() {
        if let Ok(path) = std::env::var(CRASH_DB_ENV) {
            crash_child(&path);
            unreachable!("子进程本该在提交第二批之后杀掉自己");
        }

        let p = tmp_db("crash");
        let exe = std::env::current_exe().unwrap();
        // 用子串匹配而不是 --exact：测试名在 libtest 里是带上模块路径的
        // （import_pipeline::tests::…），--exact 配裸函数名会一行都不跑，
        // 子进程于是"正常退出"，这个测试就白测了。
        let status = std::process::Command::new(exe)
            .arg(CRASH_TEST)
            .arg("--nocapture")
            .env(CRASH_DB_ENV, &p)
            .status()
            .unwrap();
        assert!(!status.success(), "子进程本该被强杀，却正常退出了（测试无效）");

        // —— 重启之后 ——
        let mut db = open_db(&p);
        assert_eq!(count_rows(&db, "orders"), 200, "已提交的批次要都在");

        let pending = pending_jobs(&db).unwrap();
        assert_eq!(pending.len(), 1, "启动时要能列出上次没跑完的作业");
        assert_eq!(pending[0].state, JobState::Running);
        assert_eq!(pending[0].committed_rows, 200);
        assert_eq!(pending[0].total_rows, 250);
        assert_eq!(pending[0].remaining_rows(), 50);

        let notice = recovery_notice(&db).unwrap().unwrap();
        assert!(notice.contains("200") && notice.contains("50"), "实际：{notice}");

        // 重跑靠自然键幂等：只剩 50 行会被写进去
        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let rep = import(&mut db, &rows(250), &o, &mut freeze).unwrap();
        assert_eq!(rep.inserted, 50, "重跑应该补上缺的 50 行");
        assert_eq!(rep.unchanged, 200, "已经导进去的 200 行不该重复");
        assert_eq!(count_rows(&db, "orders"), 250);
        cleanup(&p);
    }

    fn crash_child(path: &str) {
        let mut db = open_db(Path::new(path));
        orders_table(&mut db);
        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let _ = import(&mut db, &rows(250), &o, &mut |done, total| {
            eprintln!("[子进程] 已提交 {done}/{total} 行");
            if done >= 200 {
                eprintln!("[子进程] 现在硬杀自己");
                std::process::abort();
            }
        });
        eprintln!("[子进程] 竟然正常跑完了，这个测试没测到东西");
        std::process::exit(9);
    }
}
