//! 导入落库管线：「能撤销的导入才敢用」的落地部分。
//!
//! 与 `xlsx.rs` / `csv_import.rs` 的分工很清楚：那两个模块只**看**文件、绝不碰库；
//! 本模块只**写**库、绝不碰文件。中间由调用方（IPC 层）串起来。
//!
//! # 为什么必须是独立一批事务
//!
//! 几十万行塞进一个事务有三个后果，每一个都踩过：
//!   1. WAL 在整个事务期间**无法 checkpoint**（社区实测：导入 27 GB CSV 攒出 10 GB
//!      journal），磁盘被日志吃掉；
//!   2. 崩溃时整批全丢，用户看到的进度是"0 行"，等于白导；
//!   3. 事务越大，回滚代价越高。
//! 所以这里固定 batch_rows 行一个 `BEGIN IMMEDIATE ... COMMIT`，把提交切碎。
//!
//! # 已提交行数必须和数据在同一个事务里写
//!
//! 进度不是内存里的计数器，而是 `_import_job.committed_rows`，它在**每个批次的
//! 事务内部**和数据一起更新。这样"库里有 20 万行"和"作业说已提交 20 万行"永远
//! 是同一句话的两个说法，进程被强杀也不会出现"数据在、进度不在"的错位。
//!
//! # 撤销只删本次导入写的行
//!
//! 靠两层信息：
//!   · 业务行上的 `_import_job` 列 —— 标出"这行是哪次导入写的"，导入时自动补列；
//!   · `_import_row` 里的内容指纹 —— 撤销前逐行核对，**用户后来改过的行一律不删**。
//! 只有第一层的话，「用户改过的行」会被连带删掉，那才是最伤人的误删。

// ⚠️ 本模块只接线了一部分（2026-09-17）
//
// 目前接进 IPC 的只有 `recovery_notice`（`import.pending` 命令，用来在启动时
// 报告"上次没跑完的导入还剩多少行"）。真正的主体 `import` / `undo` 还没接线，
// 原因不是它不可用，而是**它要的业务表还不存在** ——
//
// `import` 要求目标表是 rowid 表、列已存在，且不走业务层的 id / 时间戳规则。
// 现在库里只有 `note` 一张表，而它的 id / created_at / updated_at 都是 NOT NULL
// 且由程序生成，直接灌会绕过这些规则。硬接上来只会做出一个"看起来能用、
// 但写进去的笔记缺 id"的功能 —— 那比没有更糟。
//
// 它的第一个真实宿主是 P9 的库存流水 / 单据明细（那两张表就是为批量导入设计的）。
// 到那时这一行必须删掉。**这是有终止条件的豁免，不是永久静音。**
//
// 不现在删掉的理由：本模块有 27 个测试，其中「跨进程强杀后重启能报出正确进度」
// 那条是真的起了子进程然后 `std::process::abort()` 的 —— 那是整个项目里
// 唯一验证过崩溃恢复的地方。删掉就等于把"不丢数据"这条立身之本退回到
// "我们相信它是这样"。
#![allow(dead_code)]

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};

/// 本模块的返回类型。错误一律是**给人看的中文**，IPC 层可以直接往界面上贴。
pub type Result<T> = std::result::Result<T, String>;

/// 默认每批行数。
///
/// 为什么是 1 万：批越大，崩溃时丢的越多、WAL 越胖；批越小，每次提交的 fsync 越频繁
/// （本项目用的是 synchronous=FULL，每次都真下盘）。1 万行是"提交开销摊薄得差不多、
/// 崩溃损失又可接受"的折中。
pub const DEFAULT_BATCH_ROWS: usize = 10_000;

/// 作业元数据表名（作业状态存**库里**，不是内存里，否则崩溃后什么都看不到）。
const META_JOB: &str = "_import_job";

/// 行级指纹表名。撤销前靠它判断"这行还是我写进去的样子吗"。
const META_ROW: &str = "_import_row";

/// 业务表上标记导入来源的列名。与元数据表**同名但不同命名空间**，
/// 这是刻意的：界面上说「_import_job」时，用户和开发者指的是同一个概念。
pub const JOB_COLUMN: &str = "_import_job";

/// 列名/表名的分隔符（ASCII Unit Separator）。用它拼 `columns_text`，
/// 避免为了存一行列名把 serde_json 拖进这个模块。
const SEP: char = '\u{1f}';

/// 指纹里代表 SQL 的 NULL 的记号。本次导入写的都是字符串，NULL 只可能是
/// 用户或触发器后来改的，所以给一个不会和真实字符串相撞的记号就够了。
const NULL_MARK: &str = "\u{1}NULL";

/// WAL 文件的大小上限（64 MiB）。超过就交给 checkpoint 截断，
/// 否则连续导入几个大文件后，光是日志就把用户磁盘占满。
const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

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
    conn: &mut Connection,
    rows: &[Vec<String>],
    opts: &ImportOptions,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<JobReport> {
    let started = std::time::Instant::now();
    let total = rows.len() as u64;

    // 空输入：连元数据表都不建，更不动业务表。
    // 用户点了取消、或者文件只剩表头时，不该在导入历史里留一条 0 行的记录，
    // 也不该让"是否加过 _import_job 列"这种副作用莫名其妙地发生。
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
    let plan = Plan::build(conn, opts, rows)?;

    // PRAGMA 必须在事务外：foreign_keys 在事务内设置是 no-op，
    // journal_mode 官方明确要求不能在事务里改。
    // 明确**不碰** journal_mode=OFF / synchronous=OFF —— 官方原话是崩溃时
    // "corruption is likely"，宁可慢。
    apply_pragmas(conn)?;

    ensure_meta_tables(conn)?;
    ensure_job_column(conn, &plan)?;
    ensure_natural_key_index(conn, &plan)?;

    // 作业先独立提交一次：这样"我开始导入了"本身就是一个已落盘的既成事实，
    // 第一批还没提交就被强杀也能在下次启动时看到它。
    let job_id = ulid::Ulid::generate().to_string();
    create_job_row(conn, &job_id, &plan, total)?;

    match run_batches(conn, &plan, &job_id, rows, total, on_progress) {
        Ok((counts, batches)) => {
            // 索引最后建：业务表上给 _import_job 建索引，撤销时才能按作业快速定位。
            // 放到数据全写完再建，是因为先建索引会让每一行插入都多维护一棵 B 树
            // （第三方实测 1M 行：带 4 个索引 60–80 s vs 先插后建 25–30 s）。
            ensure_job_index(conn, &plan)?;
            set_job_state(conn, &job_id, JobState::Done, None)?;
            // 给 WAL 一次 checkpoint 的机会。拿不到锁就拉倒（别的连接在读），
            // 反正 journal_size_limit 已经把上限卡住了。
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");

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
            // 走到这里说明批次事务已经回滚（语句作用域结束就回滚了），
            // 现在可以安全地在库上留一条"失败"记录。
            // 记不上也无所谓：作业还停在"进行中"，那同样是个诚实的说法。
            let _ = set_job_state(conn, &job_id, JobState::Failed, Some(&msg));
            Err(msg)
        }
    }
}

/// 撤销一次导入：按 job id 删除这次导入写进去的行。
///
/// 返回删除的行数。要更细的结果（保留了多少用户改过的行）用 `undo_with_report`。
///
/// 语义：
///   · 只删**这个作业写进去的**行，靠业务行上的 `_import_job` 标记定位；
///   · 用户后来改过的行**不删**（内容指纹对不上），并把它的作业标记摘掉 ——
///     从此它归用户自己；
///   · 本次导入覆盖过的老行不删也不还原（原值在导入那一刻就被覆盖了）；
///   · 整个过程一个事务：要么全撤，要么一行不动，不会留下半撤的中间态。
pub fn undo(conn: &mut Connection, job_id: &str) -> Result<u64> {
    Ok(undo_with_report(conn, job_id)?.deleted)
}

/// 带明细的撤销。见 `undo` 的语义说明。
pub fn undo_with_report(conn: &mut Connection, job_id: &str) -> Result<UndoReport> {
    let job = get_job(conn, job_id)?.ok_or_else(|| format!("找不到导入作业 {job_id}"))?;
    if job.state == JobState::Undone {
        return Err(format!("导入作业 {job_id} 已经撤销过了，不会重复执行"));
    }
    let stored = job_detail(conn, job_id)?
        .ok_or_else(|| format!("找不到导入作业 {job_id} 的明细记录"))?;
    if stored.columns.is_empty() {
        return Err(format!("导入作业 {job_id} 没记下目标列，无法安全撤销"));
    }

    // 撤销也是大批量写，同样要 WAL + FULL + 卡住日志上限
    apply_pragmas(conn)?;

    let quoted_table = q(&job.table);
    // 取的是"存进去之后 SQLite 眼里的文本形式"，不是裸列值。
    // 为什么必须 CAST：列上带数字亲和性时（比如 id INTEGER PRIMARY KEY），
    // 输入串 "007" 会被存成整数 7，裸读回来是 Integer 而不是 Text —— 既读不出来，
    // 也会让指纹对不上，把没改过的行误判成"用户改过"。
    // 导入侧记指纹时用的是同一个表达式（见 insert_sql 的 RETURNING），两边永远一致。
    let cols = stored
        .columns
        .iter()
        .map(|c| format!("CAST(b.{} AS TEXT)", q(c)))
        .collect::<Vec<_>>()
        .join(", ");

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("开启撤销事务失败：{e}"))?;

    let mut doomed: Vec<i64> = Vec::new();
    let mut kept_modified = 0u64;
    {
        // 查出来一批"本作业写的行 + 它们现在的样子"，在 Rust 侧逐行核对指纹。
        // 核对必须在这里做：SQL 里没法用同一个哈希函数。
        let sql = format!(
            "SELECT r.rid, r.fp, {cols}
             FROM {row_meta} r
             JOIN {t} b ON b.rowid = r.rid
             WHERE r.job_id = ?1
             ORDER BY r.rid",
            row_meta = q(META_ROW),
            t = quoted_table,
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| format!("准备撤销查询失败：{e}"))?;
        let width = stored.columns.len();
        let mut rows = stmt.query(params![job_id]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let rid: i64 = r.get(0).map_err(|e| e.to_string())?;
            let saved: String = r.get(1).map_err(|e| e.to_string())?;
            let mut cur: Vec<String> = Vec::with_capacity(width);
            for i in 0..width {
                // NULL 不可能来自本次导入（我们写的都是字符串），
                // 出现 NULL 只可能是用户或触发器改的，给个不会和真实值相撞的记号
                let v: Option<String> = r.get(2 + i).map_err(|e| e.to_string())?;
                cur.push(v.unwrap_or_else(|| NULL_MARK.to_string()));
            }
            if fingerprint_row(&cur) == saved {
                doomed.push(rid);
            } else {
                // 用户改过了。删它就是删用户自己的劳动成果 —— 留下。
                kept_modified += 1;
            }
        }
    }

    let mut deleted = 0u64;
    {
        let mut del = tx
            .prepare_cached(&format!("DELETE FROM {} WHERE rowid = ?1", quoted_table))
            .map_err(|e| format!("准备撤销删除失败：{e}"))?;
        for rid in &doomed {
            deleted += del.execute(params![rid]).map_err(|e| e.to_string())? as u64;
        }
    }

    {
        // 还挂着本作业标记的行都是"被保留的用户数据"，把标记摘掉：
        // 作业已经撤销，这些行从此刻起不属于任何一次导入。
        let cleared = tx
            .execute(
                &format!(
                    "UPDATE {} SET {} = NULL WHERE {} = ?1",
                    quoted_table,
                    q(JOB_COLUMN),
                    q(JOB_COLUMN)
                ),
                params![job_id],
            )
            .map_err(|e| format!("摘除导入标记失败：{e}"))?;
        debug_assert!(cleared as u64 >= kept_modified);
    }

    tx.execute(&format!("DELETE FROM {} WHERE job_id = ?1", q(META_ROW)), params![job_id])
        .map_err(|e| format!("清理行级指纹失败：{e}"))?;
    tx.execute(
        "UPDATE _import_job SET state = ?2, finished_at = ?3, error = NULL WHERE id = ?1",
        params![job_id, JobState::Undone.as_str(), now_ms()],
    )
    .map_err(|e| format!("更新作业状态失败：{e}"))?;
    tx.commit().map_err(|e| format!("提交撤销失败：{e}"))?;

    let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");

    Ok(UndoReport {
        job_id: job_id.to_string(),
        deleted,
        kept_modified,
        kept_updated: stored.updated_rows,
    })
}

// ---------------------------------------------------------------- 崩溃恢复

/// 列出所有作业，最新的在前。
pub fn list_jobs(conn: &Connection) -> Result<Vec<ImportJob>> {
    if !meta_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, table_name, state, committed_rows, total_rows
             FROM _import_job ORDER BY started_at DESC, id DESC",
        )
        .map_err(|e| e.to_string())?;
    collect_jobs(&mut stmt)
}

/// 列出**上次没跑完**的作业：状态是"进行中"的那些。
///
/// 进程被强杀时来不及改状态，所以启动时看到的就是它们。
/// 这个函数是只读的 —— **绝不自动清理、绝不自动回滚**，让用户决定。
pub fn pending_jobs(conn: &Connection) -> Result<Vec<ImportJob>> {
    if !meta_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, table_name, state, committed_rows, total_rows
             FROM _import_job WHERE state = 'running'
             ORDER BY started_at ASC, id ASC",
        )
        .map_err(|e| e.to_string())?;
    collect_jobs(&mut stmt)
}

/// 查一个作业
pub fn get_job(conn: &Connection, job_id: &str) -> Result<Option<ImportJob>> {
    if !meta_exists(conn)? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT id, table_name, state, committed_rows, total_rows
         FROM _import_job WHERE id = ?1",
        params![job_id],
        map_job,
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// 启动时给用户看的那句话。没有未完成的作业就返回 `None`。
///
/// 特意说清三件事：导进去多少、还剩多少、接下来能做什么。
/// 用户在崩溃后最怕的不是"少导了"，而是"不知道现在库里是什么状态"。
pub fn recovery_notice(conn: &Connection) -> Result<Option<String>> {
    let mut detail: Vec<(ImportJob, bool)> = Vec::new();
    if meta_exists(conn)? {
        let mut stmt = conn
            .prepare(
                "SELECT id, table_name, state, committed_rows, total_rows, natural_key_text
                 FROM _import_job WHERE state = 'running'
                 ORDER BY started_at ASC, id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((map_job(r)?, !r.get::<_, String>(5)?.is_empty()))
            })
            .map_err(|e| e.to_string())?;
        for r in rows {
            detail.push(r.map_err(|e| e.to_string())?);
        }
    }
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
                job.table,
                job.committed_rows,
                left
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
pub fn dismiss(conn: &Connection, job_id: &str) -> Result<()> {
    let job = get_job(conn, job_id)?.ok_or_else(|| format!("找不到导入作业 {job_id}"))?;
    if job.state != JobState::Running {
        return Err(format!(
            "作业 {job_id} 现在是「{}」，不需要忽略",
            job.state.label()
        ));
    }
    set_job_state(
        conn,
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
    conn: &mut Connection,
    plan: &Plan,
    job_id: &str,
    rows: &[Vec<String>],
    total: u64,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(Counts, u64)> {
    let mut counts = Counts::default();
    let mut batches = 0u64;
    let mut cursor = 0usize;

    on_progress(0, total);

    while cursor < rows.len() {
        let end = (cursor + plan.batch_rows).min(rows.len());
        let batch = &rows[cursor..end];

        // BEGIN IMMEDIATE：一开始就拿写锁，避免"读到一半才发现要升级锁"的死锁场景。
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("开启第 {} 批事务失败：{e}", batches + 1))?;
        {
            let mut w = Writer::new(&tx, plan)?;
            for (i, row) in batch.iter().enumerate() {
                let row_no = cursor + i + 1; // 1 基，方便用户对着 Excel 找
                w.write(&tx, job_id, row, &mut counts)
                    .map_err(|e| format!("第 {row_no} 行导入失败：{e}"))?;
            }
        }
        // 进度与数据在同一个事务里提交：崩溃后库里的数字永远和实际行数对得上。
        tx.execute(
            "UPDATE _import_job
             SET committed_rows = ?2, inserted_rows = ?3, updated_rows = ?4,
                 unchanged_rows = ?5, blank_key_rows = ?6
             WHERE id = ?1",
            params![
                job_id,
                end as i64,
                counts.inserted as i64,
                counts.updated as i64,
                counts.unchanged as i64,
                counts.blank_key as i64
            ],
        )
        .map_err(|e| format!("记录导入进度失败：{e}"))?;
        tx.commit()
            .map_err(|e| format!("提交第 {} 批失败：{e}", batches + 1))?;

        batches += 1;
        cursor = end;
        on_progress(end as u64, total);
    }

    Ok((counts, batches))
}

/// 一批之内复用的语句持有者。语句只编译两次（插入 + 更新），
/// 循环里只做参数绑定 —— 逐行 insert 的开销大头在提交和 fsync，不在编译。
struct Writer<'a> {
    plan: &'a Plan,
    insert: rusqlite::CachedStatement<'a>,
    update: Option<rusqlite::CachedStatement<'a>>,
}

impl<'a> Writer<'a> {
    fn new(tx: &'a rusqlite::Transaction<'_>, plan: &'a Plan) -> Result<Writer<'a>> {
        let insert = tx
            .prepare_cached(&plan.insert_sql)
            .map_err(|e| format!("准备插入语句失败：{e}"))?;
        let update = match &plan.update_sql {
            Some(sql) => Some(
                tx.prepare_cached(sql)
                    .map_err(|e| format!("准备更新语句失败：{e}"))?,
            ),
            None => None,
        };
        Ok(Writer {
            plan,
            insert,
            update,
        })
    }

    fn write(
        &mut self,
        conn: &Connection,
        job_id: &str,
        row: &[String],
        counts: &mut Counts,
    ) -> Result<()> {
        let plan = self.plan;

        // 自然键整列为空的行会在唯一索引上互相视为同一行 —— 最后只留一条。
        // 这份计数是给用户看的"我可能丢了行"的警报，不是去重逻辑的一部分，
        // 所以按**输入里出现的次数**算：插进去的那条和被跳过的那些条都要算。
        let blank_key = plan.has_natural_key
            && plan.key_indexes.iter().all(|&i| row[i].trim().is_empty());
        if blank_key {
            counts.blank_key += 1;
        }

        // RETURNING 会带回 rowid 和"库里真实存的文本"，指纹按后者算。
        // 撞上 ON CONFLICT DO NOTHING 时返回 0 行，rusqlite 报 QueryReturnedNoRows ——
        // 那就是"这次没插进去，已有行保持原样"。
        let read = self.insert.query_row(
            params_from_iter(row.iter().map(|s| s.as_str()).chain(std::iter::once(job_id))),
            |r| {
                let rid: i64 = r.get(0)?;
                let mut stored: Vec<String> = Vec::with_capacity(plan.columns.len());
                for i in 0..plan.columns.len() {
                    let v: Option<String> = r.get(1 + i)?;
                    stored.push(v.unwrap_or_else(|| NULL_MARK.to_string()));
                }
                Ok((rid, stored))
            },
        );

        match read {
            Ok((rid, stored)) => {
                record_inserted_row(conn, job_id, rid, &fingerprint_row(&stored))?;
                counts.inserted += 1;
                return Ok(());
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // 只有一种可能：命中了自然键唯一索引，被 ON CONFLICT DO NOTHING 挡下。
                // 这正是**不用 `INSERT OR IGNORE`** 的原因 —— 官方原话是
                // "No error is returned for uniqueness, NOT NULL, and UNIQUE constraint errors"，
                // 它会把 NOT NULL / CHECK 违规一起吞掉，用户永远不知道少了数据。
                // `ON CONFLICT(<自然键>) DO NOTHING` 只挡这一个约束，别的照报。
            }
            Err(e) => return Err(hint_on_constraint(plan, e)),
        }

        match self.update.as_mut() {
            Some(upd) => {
                let n = upd
                    .execute(params_from_iter(
                        row.iter()
                            .map(|s| s.as_str())
                            .chain(plan.key_indexes.iter().map(|&i| row[i].as_str())),
                    ))
                    .map_err(|e| hint_on_constraint(plan, e))?;
                if n > 0 {
                    counts.updated += 1;
                } else {
                    counts.unchanged += 1;
                }
            }
            // update_existing = false：已有行一动不动，这正是幂等重跑要的行为
            None => counts.unchanged += 1,
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 表结构与索引

/// PRAGMA 一律在事务外设置。
///
/// `foreign_keys` 在事务里设置是 no-op；`journal_mode` 官方要求不能在事务里改。
/// 这里不假设连接是谁开的：可能来自 `db.rs`，也可能是测试里的裸内存库，
/// 所以每次导入前都把耐久性参数重新声明一遍。
fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA busy_timeout = 5000;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT};"
    ))
    .map_err(|e| format!("设置导入期的持久化参数失败：{e}"))
}

fn ensure_meta_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _import_job (
             id               TEXT PRIMARY KEY,
             table_name       TEXT NOT NULL,
             state            TEXT NOT NULL,
             total_rows       INTEGER NOT NULL DEFAULT 0,
             committed_rows   INTEGER NOT NULL DEFAULT 0,
             inserted_rows    INTEGER NOT NULL DEFAULT 0,
             updated_rows     INTEGER NOT NULL DEFAULT 0,
             unchanged_rows   INTEGER NOT NULL DEFAULT 0,
             blank_key_rows   INTEGER NOT NULL DEFAULT 0,
             columns_text     TEXT NOT NULL DEFAULT '',
             natural_key_text TEXT NOT NULL DEFAULT '',
             update_existing  INTEGER NOT NULL DEFAULT 0,
             batch_rows       INTEGER NOT NULL DEFAULT 0,
             started_at       INTEGER NOT NULL DEFAULT 0,
             finished_at      INTEGER,
             error            TEXT
         );

         CREATE TABLE IF NOT EXISTS _import_row (
             job_id TEXT NOT NULL,
             rid    INTEGER NOT NULL,
             fp     TEXT NOT NULL,
             PRIMARY KEY (job_id, rid)
         ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("创建导入元数据表失败：{e}"))
}

fn ensure_job_column(conn: &Connection, plan: &Plan) -> Result<()> {
    let cols = table_columns(conn, &plan.table)?;
    if cols.iter().any(|c| c.eq_ignore_ascii_case(JOB_COLUMN)) {
        return Ok(());
    }
    // 给业务表加一列。ADD COLUMN 在 SQLite 里只改表头、不重写数据，
    // 几十万行的表也是毫秒级 —— 这是撤销唯一的定位依据，值得这一列。
    conn.execute_batch(&format!(
        "ALTER TABLE {} ADD COLUMN {} TEXT",
        plan.quoted_table,
        q(JOB_COLUMN)
    ))
    .map_err(|e| format!("给表 {} 添加 {JOB_COLUMN} 列失败：{e}", plan.table))
}

/// 建自然键唯一索引。
///
/// 这一条**必须**在插入之前建好，没法"最后再建"：`ON CONFLICT(<列>) DO NOTHING`
/// 要求目标列上存在非部分（non-partial）唯一索引，没有它整条 SQL 就直接报错。
/// "先插数据后建索引"那条经验针对的是我们自己额外加的索引（见 `ensure_job_index`）。
fn ensure_natural_key_index(conn: &Connection, plan: &Plan) -> Result<()> {
    if !plan.has_natural_key {
        return Ok(());
    }
    if has_unique_index_on(conn, &plan.table, &plan.natural_key)? {
        return Ok(());
    }
    let cols = plan
        .natural_key
        .iter()
        .map(|c| q(c))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {} ({cols})",
        q(&plan.nk_index_name),
        plan.quoted_table
    ))
    .map_err(|e| {
        format!(
            "给「{}」的自然键（{}）建唯一索引失败：{e}。\
             如果提示 UNIQUE constraint failed，说明表里已经有重复数据，\
             请先去重再导入 —— 有重复的情况下没法保证导入是幂等的。",
            plan.table,
            plan.natural_key.join("、")
        )
    })
}

/// 给业务表上的 `_import_job` 建索引 —— 放在数据全部写完之后。
fn ensure_job_index(conn: &Connection, plan: &Plan) -> Result<()> {
    // 部分索引：只索引"来自导入的行"，用户自己建的行（NULL）不进索引，索引更小。
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS {} ON {} ({}) WHERE {} IS NOT NULL",
        q(&plan.job_index_name),
        plan.quoted_table,
        q(JOB_COLUMN),
        q(JOB_COLUMN)
    ))
    .map_err(|e| format!("给 {} 建导入索引失败：{e}", plan.table))
}

fn create_job_row(conn: &Connection, job_id: &str, plan: &Plan, total: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO _import_job
             (id, table_name, state, total_rows, committed_rows,
              columns_text, natural_key_text, update_existing, batch_rows, started_at)
         VALUES (?1, ?2, 'running', ?3, 0, ?4, ?5, ?6, ?7, ?8)",
        params![
            job_id,
            plan.table,
            total as i64,
            plan.columns.join(&SEP.to_string()),
            plan.natural_key.join(&SEP.to_string()),
            plan.update_existing as i64,
            plan.batch_rows as i64,
            now_ms()
        ],
    )
    .map_err(|e| format!("登记导入作业失败：{e}"))?;
    Ok(())
}

fn record_inserted_row(conn: &Connection, job_id: &str, rid: i64, fp: &str) -> Result<()> {
    // 元数据表也用 ON CONFLICT 而不是 INSERT OR REPLACE/OR IGNORE：
    // 同一行被同一作业写两次（输入里出现重复的自然键）时，指纹要以最后一次为准。
    // 走缓存：这是逐行执行的热路径，每行现编译一次语句纯属浪费。
    let mut stmt = conn
        .prepare_cached(
            "INSERT INTO _import_row (job_id, rid, fp) VALUES (?1, ?2, ?3)
             ON CONFLICT(job_id, rid) DO UPDATE SET fp = excluded.fp",
        )
        .map_err(|e| format!("准备记录导入行指纹失败：{e}"))?;
    stmt.execute(params![job_id, rid, fp])
        .map_err(|e| format!("记录导入行指纹失败：{e}"))?;
    Ok(())
}

fn set_job_state(conn: &Connection, job_id: &str, state: JobState, err: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE _import_job SET state = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
        params![job_id, state.as_str(), now_ms(), err],
    )
    .map_err(|e| format!("更新作业状态失败：{e}"))?;
    Ok(())
}

fn meta_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![META_JOB],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

fn map_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<ImportJob> {
    Ok(ImportJob {
        id: r.get(0)?,
        table: r.get(1)?,
        state: JobState::from_db(&r.get::<_, String>(2)?),
        committed_rows: r.get::<_, i64>(3)?.max(0) as u64,
        total_rows: r.get::<_, i64>(4)?.max(0) as u64,
    })
}

fn collect_jobs(stmt: &mut rusqlite::Statement<'_>) -> Result<Vec<ImportJob>> {
    let rows = stmt.query_map([], map_job).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

struct JobDetail {
    columns: Vec<String>,
    updated_rows: u64,
}

fn job_detail(conn: &Connection, job_id: &str) -> Result<Option<JobDetail>> {
    conn.query_row(
        "SELECT columns_text, updated_rows FROM _import_job WHERE id = ?1",
        params![job_id],
        |r| {
            let text: String = r.get(0)?;
            Ok(JobDetail {
                columns: split_columns(&text),
                updated_rows: r.get::<_, i64>(1)?.max(0) as u64,
            })
        },
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn split_columns(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split(SEP).map(|s| s.to_string()).collect()
    }
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![table], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// 表上有没有一个"正好建在 natural_key 这几列上"的唯一索引。
///
/// 为什么不能直接 `CREATE UNIQUE INDEX IF NOT EXISTS`：名字不一样就挡不住，
/// 用户（或调用方）可能早就建了 `UNIQUE(no)`，再建一个同列索引白占空间、
/// 还让每次插入多维护一棵树。
fn has_unique_index_on(conn: &Connection, table: &str, key: &[String]) -> Result<bool> {
    let mut stmt = conn
        .prepare("SELECT name, partial FROM pragma_index_list(?1) WHERE \"unique\" = 1")
        .map_err(|e| e.to_string())?;
    let list = stmt
        .query_map(params![table], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut candidates: Vec<String> = Vec::new();
    for it in list {
        let (name, partial) = it.map_err(|e| e.to_string())?;
        // 部分唯一索引不能当 ON CONFLICT 的冲突目标
        if partial == 0 {
            candidates.push(name);
        }
    }
    for idx in candidates {
        let mut s = conn
            .prepare("SELECT name FROM pragma_index_info(?1)")
            .map_err(|e| e.to_string())?;
        let cols = s
            .query_map(params![idx], |r| r.get::<_, Option<String>>(0))
            .map_err(|e| e.to_string())?;
        let mut got: Vec<String> = Vec::new();
        let mut ok = true;
        for c in cols {
            match c.map_err(|e| e.to_string())? {
                // 表达式索引没有列名，认不出，跳过
                None => {
                    ok = false;
                    break;
                }
                Some(name) => got.push(name),
            }
        }
        if ok
            && got.len() == key.len()
            && got
                .iter()
                .zip(key.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------- 校验与拼装

/// 一次导入的"执行计划"：校验通过后把 SQL 也拼好，批次循环里不再做字符串活。
struct Plan {
    /// 用户给的表名（只用于提示语）
    table: String,
    quoted_table: String,
    columns: Vec<String>,
    /// 自然键各列在 `row` 里的下标
    key_indexes: Vec<usize>,
    natural_key: Vec<String>,
    has_natural_key: bool,
    update_existing: bool,
    batch_rows: usize,
    insert_sql: String,
    update_sql: Option<String>,
    nk_index_name: String,
    job_index_name: String,
}

impl Plan {
    fn build(conn: &Connection, opts: &ImportOptions, rows: &[Vec<String>]) -> Result<Plan> {
        if opts.table.trim().is_empty() {
            return Err("没有指定要导入到哪张表".to_string());
        }
        if opts.table.contains('\0') {
            return Err("表名里有非法字符".to_string());
        }
        if opts.table.starts_with("sqlite_") || opts.table == META_JOB || opts.table == META_ROW {
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

        let kind: Option<String> = conn
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = ?1",
                params![opts.table],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match kind.as_deref() {
            Some("table") => {}
            Some(other) => {
                return Err(format!(
                    "{0} 在库里是「{1}」而不是普通表，导入只支持普通表",
                    opts.table, other
                ))
            }
            None => {
                return Err(format!(
                    "库里没有表「{}」：导入只负责写数据，建表由调用方负责",
                    opts.table
                ))
            }
        }

        // 撤销要靠 rowid 定位行；WITHOUT ROWID 表没有 rowid。
        // 查不出来（SQLite 太老没有 pragma_table_list）就当没这个限制，
        // 别让一个安全检查把正常导入给挡死。
        let wr: Option<i64> = conn
            .query_row(
                "SELECT wr FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
                params![opts.table],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        if wr == Some(1) {
            return Err(format!(
                "{} 是 WITHOUT ROWID 表，导入需要 rowid 来记录「撤销时删哪些行」，\
                 请改成普通表（去掉 WITHOUT ROWID）",
                opts.table
            ));
        }

        let actual = table_columns(conn, &opts.table)?;
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

        let col_list = opts.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        let placeholders = (1..=opts.columns.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let job_ph = opts.columns.len() + 1;
        let quoted_table = q(&opts.table);

        let has_natural_key = !opts.natural_key.is_empty();
        // 插入语句顺手把"存进去之后的样子"取回来（RETURNING）：
        //   · rowid  —— 撤销时要靠它定位行；
        //   · CAST(列 AS TEXT) —— 指纹必须哈希"库里真实存的样子"，
        //     否则带数字亲和性的列（id INTEGER PRIMARY KEY 之类）会把
        //     输入串 "007" 存成 7，撤销时指纹永远对不上。
        // 顺带好处：撞上 ON CONFLICT DO NOTHING 时 RETURNING 不返回任何行，
        // "到底插进去没有"就不再依赖 changes()，判断更直接。
        let returning = std::iter::once("rowid".to_string())
            .chain(opts.columns.iter().map(|c| format!("CAST({} AS TEXT)", q(c))))
            .collect::<Vec<_>>()
            .join(", ");
        let insert_sql = if has_natural_key {
            let keys = opts.natural_key.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            format!(
                "INSERT INTO {quoted_table} ({col_list}, {job}) VALUES ({placeholders}, ?{job_ph}) \
                 ON CONFLICT ({keys}) DO NOTHING RETURNING {returning}",
                job = q(JOB_COLUMN)
            )
        } else {
            format!(
                "INSERT INTO {quoted_table} ({col_list}, {job}) VALUES ({placeholders}, ?{job_ph}) \
                 RETURNING {returning}",
                job = q(JOB_COLUMN)
            )
        };

        let update_sql = if has_natural_key && opts.update_existing {
            let sets = opts
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} = ?{}", q(c), i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let wheres = key_indexes
                .iter()
                .enumerate()
                .map(|(j, &i)| {
                    format!(
                        "{} = ?{}",
                        q(&opts.columns[i]),
                        opts.columns.len() + j + 1
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            Some(format!("UPDATE {quoted_table} SET {sets} WHERE {wheres}"))
        } else {
            None
        };

        let safe = sanitize(&opts.table);
        Ok(Plan {
            table: opts.table.clone(),
            quoted_table,
            columns: opts.columns.clone(),
            key_indexes,
            natural_key: opts.natural_key.clone(),
            has_natural_key,
            update_existing: opts.update_existing,
            batch_rows: opts.batch_rows,
            insert_sql,
            update_sql,
            nk_index_name: format!("uidx_{safe}_nk"),
            job_index_name: format!("idx_{safe}_import_job"),
        })
    }
}

// ---------------------------------------------------------------- 小工具

/// 给标识符加双引号。调用方可能把 Excel 表头直接当列名传进来，
/// 里面有引号、空格、分号都是常事 —— 引号内的东西一律当字面量，
/// 顺带把双引号按 SQL 规矩翻倍，避免拼出能执行别的东西的语句。
fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 拿表名派生索引名时用的安全字符集（索引名是我们自己拼的，必须自己保证干净）
fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if s.is_empty() {
        s.push('t');
    }
    // 不同表名可能被压成同一个安全名，加个短哈希防止索引名相撞
    // （撞了的话 IF NOT EXISTS 会以为已经建好，结果另一张表根本没有索引）
    format!("{s}_{:08x}", fnv1a32(name.as_bytes()))
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// 把裸的 SQLite 报错翻成"用户看得懂、并且知道下一步怎么办"的话。
///
/// 只加提示、不改事实：真正的报错原文一个字不动地留在前面，
/// 因为排障时那句话才是权威。
fn hint_on_constraint(plan: &Plan, e: rusqlite::Error) -> String {
    let msg = e.to_string();
    if !plan.has_natural_key && msg.contains("UNIQUE constraint failed") {
        return format!(
            "{msg}（「{}」上有唯一约束，但这次导入没有声明自然键，\
             所以遇到重复只能整批失败。要么先声明自然键来决定重复怎么办\
             （跳过 or 覆盖），要么先把重复的行清掉。）",
            plan.table
        );
    }
    msg
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
    use std::path::{Path, PathBuf};

    /// 造一个临时文件库。
    ///
    /// 用文件而不是 `:memory:`：WAL、崩溃恢复、跨进程这几种行为在内存库上
    /// 根本不成立（内存库连 journal_mode 都设不成 WAL），拿内存库测等于没测。
    fn tmp_db(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "deskbase_import_{tag}_{}_{}.db",
            std::process::id(),
            ulid::Ulid::generate()
        ))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn open_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn
    }

    /// 业务表由调用方建 —— 这里就照着"调用方"的样子建一张
    fn orders_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE orders (
                 id       INTEGER PRIMARY KEY,
                 no       TEXT NOT NULL,
                 customer TEXT NOT NULL,
                 amount   TEXT
             );
             CREATE UNIQUE INDEX uq_orders_no ON orders (no);",
        )
        .unwrap();
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

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn freeze(_: u64, _: u64) {}

    // ---------------------------------------------------------- 基础

    #[test]
    fn 小批量导入能原样读回来() {
        let p = tmp_db("small");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let rep = import(&mut conn, &rows(5), &opts(&["no", "customer", "amount"]), &mut freeze)
            .unwrap();

        assert_eq!(rep.inserted, 5);
        assert_eq!(rep.updated, 0);
        assert_eq!(rep.unchanged, 0);
        assert_eq!(rep.job.state, JobState::Done);
        assert_eq!(rep.job.committed_rows, 5);
        assert_eq!(rep.job.total_rows, 5);
        assert_eq!(rep.batches, 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 5);

        // 中文与列顺序都要能原样往返
        let (no, customer, amount): (String, String, String) = conn
            .query_row(
                "SELECT no, customer, amount FROM orders WHERE no = 'NO00003'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((no.as_str(), customer.as_str(), amount.as_str()), ("NO00003", "客户3", "30"));

        // 每一行都带着作业标记 —— 撤销唯一的定位依据
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM orders WHERE _import_job = '{}'", rep.job.id)
            ),
            5
        );

        // 作业状态是**落库**的，不是内存里的
        let job = get_job(&conn, &rep.job.id).unwrap().unwrap();
        assert_eq!(job, rep.job);
        cleanup(&p);
    }

    #[test]
    fn 自然键去重_同样的数据导两次第二次零新增() {
        let p = tmp_db("idem");
        let mut conn = open_db(&p);
        orders_table(&conn);
        let o = opts(&["no", "customer", "amount"]);

        let first = import(&mut conn, &rows(5), &o, &mut freeze).unwrap();
        assert_eq!(first.inserted, 5);

        // 重跑同一个文件：幂等靠自然键，不靠"记住上次导过"
        let second = import(&mut conn, &rows(5), &o, &mut freeze).unwrap();
        assert_eq!(second.inserted, 0, "第二次不该新增");
        assert_eq!(second.unchanged, 5);
        assert_eq!(second.updated, 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 5);
        cleanup(&p);
    }

    #[test]
    fn 中途插入的新行只有新增的那些被写进去() {
        let p = tmp_db("partial");
        let mut conn = open_db(&p);
        orders_table(&conn);
        let o = opts(&["no", "customer", "amount"]);

        import(&mut conn, &rows(3), &o, &mut freeze).unwrap();
        let mut data = rows(3);
        data.extend(rows(5).into_iter().skip(3)); // 追加 NO00003、NO00004
        let rep = import(&mut conn, &data, &o, &mut freeze).unwrap();

        assert_eq!(rep.inserted, 2);
        assert_eq!(rep.unchanged, 3);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 5);
        cleanup(&p);
    }

    #[test]
    fn update_existing为假时保留已有行_为真时才覆盖() {
        let p = tmp_db("update");
        let mut conn = open_db(&p);
        orders_table(&conn);
        // 用户自己先录了一行
        conn.execute(
            "INSERT INTO orders (no, customer, amount) VALUES ('NO00001', '老客户', '999')",
            [],
        )
        .unwrap();

        let keep = import(
            &mut conn,
            &rows(3),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(keep.inserted, 2);
        assert_eq!(keep.unchanged, 1);
        assert_eq!(keep.updated, 0);
        let who: String = conn
            .query_row("SELECT customer FROM orders WHERE no = 'NO00001'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(who, "老客户", "默认不该动已有行");

        let over = import(
            &mut conn,
            &rows(3),
            &opts(&["no", "customer", "amount"]).overwrite(),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(over.inserted, 0);
        assert_eq!(over.updated, 3, "三行都命中了已有自然键");
        let who: String = conn
            .query_row("SELECT customer FROM orders WHERE no = 'NO00001'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(who, "客户1");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 3);
        cleanup(&p);
    }

    #[test]
    fn 约束违规必须报错而不是被静默吞掉() {
        let p = tmp_db("notnull");
        let mut conn = open_db(&p);
        orders_table(&conn);

        // customer 是 NOT NULL，但这次导入不包含它。
        // 如果用 INSERT OR IGNORE，这一行会被悄悄丢掉 —— 用户永远不知道少了数据。
        let o = ImportOptions::new("orders", &["no", "amount"]).with_natural_key(&["no"]);
        let data = vec![vec!["N1".to_string(), "10".to_string()]];
        let err = import(&mut conn, &data, &o, &mut freeze).unwrap_err();

        assert!(
            err.to_uppercase().contains("NOT NULL"),
            "要如实报出 NOT NULL 违规，实际：{err}"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 0, "整批回滚，一行不留");
        let jobs = list_jobs(&conn).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
        assert_eq!(jobs[0].committed_rows, 0, "进度要如实记录");
        cleanup(&p);
    }

    // ---------------------------------------------------------- 校验

    #[test]
    fn 列数不匹配时报错并指出是哪一行() {
        let p = tmp_db("width");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let mut data = rows(2);
        data.push(vec!["NO00002".to_string(), "客户2".to_string()]); // 少一列
        let err = import(&mut conn, &data, &opts(&["no", "customer", "amount"]), &mut freeze)
            .unwrap_err();

        assert!(err.contains("第 3 行"), "错误里要指到具体行：{err}");
        assert!(err.contains('2') && err.contains('3'), "要说清几列对几列：{err}");
        // 校验在任何写入之前完成：一行都不该写进去
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '_import%'"), 0);
        cleanup(&p);
    }

    #[test]
    fn 目标列不存在时报错并点名是哪一列() {
        let p = tmp_db("nocol");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let err = import(
            &mut conn,
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
    fn 自然键必须是本次导入的列() {
        let p = tmp_db("badkey");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let o = ImportOptions::new("orders", &["customer", "amount"]).with_natural_key(&["no"]);
        let err = import(&mut conn, &rows(1), &o, &mut freeze).unwrap_err();
        assert!(err.contains("no"), "要点名是哪个键：{err}");
        cleanup(&p);
    }

    #[test]
    fn 元数据列不能被当成业务列导入() {
        let p = tmp_db("reserved");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let err = import(
            &mut conn,
            &vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]],
            &opts(&["no", "_import_job", "amount"]),
            &mut freeze,
        )
        .unwrap_err();
        assert!(err.contains(JOB_COLUMN), "实际：{err}");
        cleanup(&p);
    }

    #[test]
    fn 目标表不存在时报错而不是去建表() {
        let p = tmp_db("notable");
        let mut conn = open_db(&p);
        let err = import(&mut conn, &rows(1), &opts(&["no"]), &mut freeze).unwrap_err();
        assert!(err.contains("orders"), "实际：{err}");
        cleanup(&p);
    }

    #[test]
    fn 空输入不报错也不写任何东西() {
        let p = tmp_db("empty");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let rep = import(&mut conn, &[], &opts(&["no", "customer", "amount"]), &mut freeze).unwrap();

        assert_eq!(rep.job.total_rows, 0);
        assert_eq!(rep.inserted, 0);
        assert_eq!(rep.batches, 0);
        // 连元数据表都不该建出来：用户取消导入不该留下任何痕迹
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '_import%'"),
            0
        );
        // 业务表也不该被加列
        let cols = table_columns(&conn, "orders").unwrap();
        assert!(!cols.iter().any(|c| c == JOB_COLUMN), "空导入不该动业务表：{cols:?}");
        cleanup(&p);
    }

    // ---------------------------------------------------------- 分批与进度

    #[test]
    fn 超过一批的行数时会分多次提交且进度与批号一致() {
        let p = tmp_db("batch");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let rep = import(&mut conn, &rows(250), &o, &mut |done, total| {
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
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 250);
        // 落库的进度和真实行数一致
        let job = get_job(&conn, &rep.job.id).unwrap().unwrap();
        assert_eq!(job.committed_rows, 250);
        cleanup(&p);
    }

    #[test]
    fn 进度回调单调不减且最后停在总数() {
        let p = tmp_db("progress");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let o = opts(&["no", "customer", "amount"]).with_batch_rows(3);
        let mut done_seq: Vec<u64> = Vec::new();
        let mut total_seq: Vec<u64> = Vec::new();
        let rep = import(&mut conn, &rows(10), &o, &mut |done, total| {
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
    fn 撤销删掉本次导入的行_且只删这一作业的行() {
        let p = tmp_db("undo");
        let mut conn = open_db(&p);
        orders_table(&conn);
        // 两行是用户自己录的：没有 _import_job 标记，撤销绝不能碰
        conn.execute_batch(
            "INSERT INTO orders (no, customer, amount) VALUES
                 ('KEEP1', '用户自己录的', '1'), ('KEEP2', '也是', '2');",
        )
        .unwrap();

        let rep = import(
            &mut conn,
            &rows(4),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 6);

        let deleted = undo(&mut conn, &rep.job.id).unwrap();
        assert_eq!(deleted, 4);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 2);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM orders WHERE no IN ('KEEP1','KEEP2')"),
            2,
            "用户自己录的行必须原封不动"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM orders WHERE _import_job IS NOT NULL"),
            0
        );

        let job = get_job(&conn, &rep.job.id).unwrap().unwrap();
        assert_eq!(job.state, JobState::Undone);
        // 重复撤销要报错，不能悄悄再删一次
        assert!(undo(&mut conn, &rep.job.id).is_err());
        cleanup(&p);
    }

    #[test]
    fn 撤销不动用户后来改过的行() {
        let p = tmp_db("undo_modified");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let rep = import(
            &mut conn,
            &rows(3),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap();
        // 用户导完之后自己改了一行
        conn.execute("UPDATE orders SET amount = '改过了' WHERE no = 'NO00001'", [])
            .unwrap();

        let r = undo_with_report(&mut conn, &rep.job.id).unwrap();
        assert_eq!(r.deleted, 2);
        assert_eq!(r.kept_modified, 1, "改过的那行要留下");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 1);
        let keep: String = conn
            .query_row("SELECT amount FROM orders WHERE no = 'NO00001'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(keep, "改过了", "用户的修改必须活着");
        // 保留的行不再挂着已撤销作业的标记
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM orders WHERE _import_job IS NOT NULL"),
            0
        );
        cleanup(&p);
    }

    #[test]
    fn 撤销不会删掉本次覆盖过的老行() {
        let p = tmp_db("undo_updated");
        let mut conn = open_db(&p);
        orders_table(&conn);
        conn.execute(
            "INSERT INTO orders (no, customer, amount) VALUES ('NO00000', '老数据', '1')",
            [],
        )
        .unwrap();

        let rep = import(
            &mut conn,
            &rows(3),
            &opts(&["no", "customer", "amount"]).overwrite(),
            &mut freeze,
        )
        .unwrap();
        assert_eq!(rep.inserted, 2);
        assert_eq!(rep.updated, 1);

        let r = undo_with_report(&mut conn, &rep.job.id).unwrap();
        assert_eq!(r.deleted, 2, "只删本次新插进去的两行");
        assert_eq!(r.kept_updated, 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders WHERE no = 'NO00000'"), 1);
        cleanup(&p);
    }

    #[test]
    fn 撤销之后能重新导入同样的数据() {
        let p = tmp_db("redo");
        let mut conn = open_db(&p);
        orders_table(&conn);
        let o = opts(&["no", "customer", "amount"]);

        let first = import(&mut conn, &rows(3), &o, &mut freeze).unwrap();
        assert_eq!(undo(&mut conn, &first.job.id).unwrap(), 3);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 0);

        // 撤销就得是"当作没导过"：再导一次必须能全量写进去，
        // 不能因为残留的唯一索引/指纹把行挡住
        let again = import(&mut conn, &rows(3), &o, &mut freeze).unwrap();
        assert_eq!(again.inserted, 3);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 3);
        cleanup(&p);
    }

    #[test]
    fn 自然键上还没有唯一索引时会自己建一个() {
        let p = tmp_db("nkidx");
        let mut conn = open_db(&p);
        // 故意不建唯一索引：ON CONFLICT(<列>) 要求目标列上必须有非部分唯一索引，
        // 没有它整条插入语句会直接报错，所以这一步是本模块自己必须做的事。
        conn.execute_batch(
            "CREATE TABLE orders (
                 id       INTEGER PRIMARY KEY,
                 no       TEXT NOT NULL,
                 customer TEXT NOT NULL,
                 amount   TEXT
             );",
        )
        .unwrap();

        let o = opts(&["no", "customer", "amount"]);
        let first = import(&mut conn, &rows(4), &o, &mut freeze).unwrap();
        assert_eq!(first.inserted, 4);

        // 索引建出来了才算成立，否则第二次导入会撞着唯一索引报错
        let n: i64 = count(
            &conn,
            "SELECT COUNT(*) FROM pragma_index_list('orders') WHERE \"unique\" = 1",
        );
        assert!(n >= 1, "自然键的唯一索引必须被建出来");

        let second = import(&mut conn, &rows(4), &o, &mut freeze).unwrap();
        assert_eq!(second.inserted, 0, "有唯一索引才有幂等");
        assert_eq!(second.unchanged, 4);
        cleanup(&p);
    }

    #[test]
    fn 表里已有重复的自然键时明确报错而不是硬来() {
        let p = tmp_db("dupkey");
        let mut conn = open_db(&p);
        conn.execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, no TEXT NOT NULL, customer TEXT, amount TEXT);
             INSERT INTO orders (no, customer, amount) VALUES ('DUP', 'a', '1'), ('DUP', 'b', '2');",
        )
        .unwrap();

        let err = import(
            &mut conn,
            &rows(1),
            &opts(&["no", "customer", "amount"]),
            &mut freeze,
        )
        .unwrap_err();
        assert!(err.contains("去重") || err.contains("唯一索引"), "要给出去重建议：{err}");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 2, "原有数据一行不动");
        cleanup(&p);
    }

    #[test]
    fn 撤销只认自己的作业_别的作业写的行不动() {
        let p = tmp_db("tworun");
        let mut conn = open_db(&p);
        orders_table(&conn);
        let o = opts(&["no", "customer", "amount"]);

        let a = import(&mut conn, &rows(3), &o, &mut freeze).unwrap();
        let mut more = rows(3);
        more.push(vec!["NO00009".to_string(), "另一批".to_string(), "9".to_string()]);
        let b = import(&mut conn, &more, &o, &mut freeze).unwrap();
        assert_eq!(b.inserted, 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 4);

        // 撤销 A 只该删掉 A 写的那 3 行，B 补的那行要留下
        assert_eq!(undo(&mut conn, &a.job.id).unwrap(), 3);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 1);
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM orders WHERE _import_job = '{}'", b.job.id)
            ),
            1,
            "另一个作业的标记不该被碰"
        );
        assert_eq!(get_job(&conn, &b.job.id).unwrap().unwrap().state, JobState::Done);
        cleanup(&p);
    }

    #[test]
    fn 数字亲和性的列不会被误判成用户改过() {
        let p = tmp_db("affinity");
        let mut conn = open_db(&p);
        // qty 是 INTEGER：输入串 "007" 会被 SQLite 存成整数 7。
        // 指纹如果按输入串算，撤销时读回来是 7，两边对不上 ——
        // 结果就是"没改过的行被当成改过的"，撤不掉。这里把它钉死。
        conn.execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, no TEXT NOT NULL, qty INTEGER);",
        )
        .unwrap();

        let o = ImportOptions::new("orders", &["no", "qty"]).with_natural_key(&["no"]);
        let data = vec![
            vec!["A1".to_string(), "007".to_string()],
            vec!["A2".to_string(), " 12 ".to_string()],
        ];
        let rep = import(&mut conn, &data, &o, &mut freeze).unwrap();
        assert_eq!(rep.inserted, 2);

        let r = undo_with_report(&mut conn, &rep.job.id).unwrap();
        assert_eq!(r.kept_modified, 0, "没人改过，不该有保留");
        assert_eq!(r.deleted, 2, "输入带前导零/空格也要能撤干净");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 0);
        cleanup(&p);
    }

    #[test]
    fn 空自然键的行会被计数并点名() {
        let p = tmp_db("blankkey");
        let mut conn = open_db(&p);
        orders_table(&conn);

        let data = vec![
            vec!["".to_string(), "没写编号".to_string(), "1".to_string()],
            vec!["NO00001".to_string(), "有编号".to_string(), "2".to_string()],
            vec!["".to_string(), "也没写".to_string(), "3".to_string()],
        ];
        let rep = import(&mut conn, &data, &opts(&["no", "customer", "amount"]), &mut freeze)
            .unwrap();

        assert_eq!(rep.blank_key_rows, 2, "两行没编号");
        assert_eq!(rep.inserted, 2, "空编号只会留下一条，另一条被唯一索引挡下");
        assert_eq!(rep.unchanged, 1);
        assert!(rep.summary().contains("空"), "报告要把这件事说出来：{}", rep.summary());
        cleanup(&p);
    }

    #[test]
    fn 没有自然键时重跑会产生重复_报告里要说清楚() {
        let p = tmp_db("nokey");
        let mut conn = open_db(&p);
        orders_table(&conn);
        // 把唯一索引摘掉，模拟"这张表本来就不拦重复"的常见情况
        conn.execute_batch("DROP INDEX uq_orders_no;").unwrap();
        let o = ImportOptions::new("orders", &["no", "customer", "amount"]);

        import(&mut conn, &rows(2), &o, &mut freeze).unwrap();
        let rep = import(&mut conn, &rows(2), &o, &mut freeze).unwrap();

        assert_eq!(rep.inserted, 2, "没有自然键就挡不住重复，这是如实报告而不是假装安全");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 4);
        assert!(rep.summary().contains("orders"));
        cleanup(&p);
    }

    // ---------------------------------------------------------- 崩溃恢复

    /// 手工造出"作业还在进行中、行只写了一半"的现场 —— 这正是被强杀后库里的样子。
    #[test]
    fn 崩在中间_启动时能报出已导入和未导入的行数() {
        let p = tmp_db("halfway");
        let conn = open_db(&p);
        orders_table(&conn);
        ensure_meta_tables(&conn).unwrap();
        conn.execute_batch(&format!(
            "ALTER TABLE orders ADD COLUMN {JOB_COLUMN} TEXT;

             INSERT INTO _import_job
                 (id, table_name, state, total_rows, committed_rows, columns_text,
                  natural_key_text, update_existing, batch_rows, started_at)
             VALUES ('01JOBHALFWAY00000000000000', 'orders', 'running', 1000, 900,
                     'no{c}customer{c}amount', 'no', 0, 100, 1);

             WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 900)
             INSERT INTO orders (no, customer, amount, {JOB_COLUMN})
             SELECT printf('NO%05d', n - 1), '客户', '0', '01JOBHALFWAY00000000000000'
             FROM seq;",
            c = SEP
        ))
        .unwrap();

        let pending = pending_jobs(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, JobState::Running);
        assert_eq!(pending[0].table, "orders");
        assert_eq!(pending[0].committed_rows, 900, "已导入多少行");
        assert_eq!(pending[0].remaining_rows(), 100, "还差多少行");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 900);

        let notice = recovery_notice(&conn).unwrap().unwrap();
        assert!(notice.contains("900"), "提示里要有已导入行数：{notice}");
        assert!(notice.contains("100"), "提示里要有未导入行数：{notice}");
        assert!(notice.contains("orders"), "要说清是哪张表：{notice}");

        // 列一遍**绝不能**改动任何东西：不自动清理、不自动回滚
        let after = get_job(&conn, "01JOBHALFWAY00000000000000").unwrap().unwrap();
        assert_eq!(after.state, JobState::Running);
        assert_eq!(after.committed_rows, 900);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 900);

        // 用户可以选择"忽略"，那也只是改状态，数据一行不动
        dismiss(&conn, "01JOBHALFWAY00000000000000").unwrap();
        assert!(pending_jobs(&conn).unwrap().is_empty());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 900);
        assert_eq!(recovery_notice(&conn).unwrap(), None);
        cleanup(&p);
    }

    #[test]
    fn 没有元数据表时_恢复接口是安静的只读() {
        let p = tmp_db("nometa");
        let conn = open_db(&p);
        orders_table(&conn);

        assert!(pending_jobs(&conn).unwrap().is_empty());
        assert!(list_jobs(&conn).unwrap().is_empty());
        assert!(get_job(&conn, "不存在").unwrap().is_none());
        assert_eq!(recovery_notice(&conn).unwrap(), None);
        // 只读就真的只读：不该顺手把元数据表建出来
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '_import%'"),
            0
        );
        cleanup(&p);
    }

    #[test]
    fn 失败后可以撤销已提交的部分() {
        let p = tmp_db("failundo");
        let mut conn = open_db(&p);
        orders_table(&conn);

        // 让第一批顺利提交、第二批才炸：这样"失败≠回滚"才有东西可验。
        // 第 3 行的 id 撞回第 1 行，触发 SQLite 自己的主键约束。
        let o = ImportOptions::new("orders", &["id", "no", "customer", "amount"])
            .with_natural_key(&["no"])
            .with_batch_rows(2);
        let mut data = Vec::new();
        for i in 0..4 {
            let id = if i == 2 { 0 } else { i };
            data.push(vec![
                id.to_string(),
                format!("NO{i:05}"),
                format!("客户{i}"),
                "0".to_string(),
            ]);
        }
        let err = import(&mut conn, &data, &o, &mut freeze).unwrap_err();
        assert!(err.contains("第 3 行"), "要说清是哪一行炸的：{err}");
        assert!(err.contains("PRIMARY KEY") || err.contains("UNIQUE"), "实际：{err}");

        let jobs = list_jobs(&conn).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
        assert_eq!(jobs[0].committed_rows, 2, "第一批已经提交了，进度要如实");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 2);

        // 失败≠回滚：已提交的两行还在，用户可以选择撤销它们
        let r = undo_with_report(&mut conn, &jobs[0].id).unwrap();
        assert_eq!(r.deleted, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 0);
        cleanup(&p);
    }

    // ---------------------------------------------------------- 跨进程强杀

    const CRASH_DB_ENV: &str = "DESKBASE_IMPORT_CRASH_DB";
    const CRASH_TEST: &str = "跨进程强杀后重启能报出正确进度且数据可以继续用";

    /// 真的起一个子进程，导到第二批发完就 `abort()` —— 不走析构、不回滚、
    /// 不改作业状态，和"用户在任务管理器里结束进程"是同一回事。
    #[test]
    fn 跨进程强杀后重启能报出正确进度且数据可以继续用() {
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
        let conn = open_db(&p);
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok", "被强杀后库本身不能坏");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM orders"), 200, "已提交的批次要都在");

        let pending = pending_jobs(&conn).unwrap();
        assert_eq!(pending.len(), 1, "启动时要能列出上次没跑完的作业");
        assert_eq!(pending[0].state, JobState::Running);
        assert_eq!(pending[0].committed_rows, 200);
        assert_eq!(pending[0].total_rows, 250);
        assert_eq!(pending[0].remaining_rows(), 50);

        let notice = recovery_notice(&conn).unwrap().unwrap();
        assert!(notice.contains("200") && notice.contains("50"), "实际：{notice}");
        drop(conn);

        // 重跑靠自然键幂等：只剩 50 行会被写进去
        let mut conn2 = open_db(&p);
        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let rep = import(&mut conn2, &rows(250), &o, &mut freeze).unwrap();
        assert_eq!(rep.inserted, 50, "重跑应该补上缺的 50 行");
        assert_eq!(rep.unchanged, 200, "已经导进去的 200 行不该重复");
        assert_eq!(count(&conn2, "SELECT COUNT(*) FROM orders"), 250);
        drop(conn2);
        cleanup(&p);
    }

    fn crash_child(path: &str) {
        let mut conn = open_db(Path::new(path));
        orders_table(&conn);
        let o = opts(&["no", "customer", "amount"]).with_batch_rows(100);
        let _ = import(&mut conn, &rows(250), &o, &mut |done, total| {
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
