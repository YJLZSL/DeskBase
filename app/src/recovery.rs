//! ## 为什么要一个标记文件
//!
//! 自研存储（append-only 日志 + fsync）保证的是「**已提交**的事务不丢」。
//! 但它不回答另一个问题：上次那个进程是正常退的还是被强杀的
//! （断电、任务管理器、崩溃）？两种情况下数据都能恢复成一致状态，
//! 可用户的处境完全不同 —— 强杀之后用户有权被告知，而不是被瞒着。
//! 「崩溃必进恢复向导」是三条底线里「不丢数据」的一部分。
//!
//! 标记文件是计算机里最老也最可靠的一招：
//!   · 启动时写 `boot.lock.json`（含 pid / 启动时间 / 版本）；
//!   · **正常退出时删掉它**（[`mark_clean`]）；
//!   · 下次启动时它还在 → 上次没走到"正常退出"这一步。
//!
//! 为什么不把标记记在数据文件里：**数据文件本身可能就是坏的那一个**。
//! 判断"该不该进恢复"的逻辑，必须不依赖它要判断的对象 —— 否则数据坏了，
//! 连"数据坏了"这件事都读不出来。
//!
//! ## 自检做什么、不做什么
//!
//! · **日志能否被完整解析**（[`store::check_log`]）：新引擎没有 `quick_check`
//!   这种引擎内自检，但判据其实更直接 —— 日志从头能不能解析完。
//!   解析不完的尾部就是上次崩溃留下的半写事务，会被丢弃。
//!   **这不叫损坏，叫恢复**，报告里必须这么告诉用户。
//! · **不做自动修复**。没有哪个自动修复能在不看现场的情况下被信任；
//!   向导给的是"看清现场"的能力（自检结果 / 快照 / 数据目录），不是一个魔法按钮。
//!
//! ## 快照
//!
//! [`snapshot`] 先把内存状态**压实成快照**再复制那个文件。
//! **不是直接复制数据文件**：直接复制正在追加的日志可能拿到半截状态。
//!
//! > 2026-09-20 改：原来这一段靠 `VACUUM INTO` + `PRAGMA quick_check` +
//! > `wal_checkpoint(TRUNCATE)`。SQL 移除后三者都换了实现，行为不变、说法要变。
//!
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::Db;
use crate::store;
use serde::Serialize;

/// 标记文件名（放在数据目录下）。存在 = 上次没有走到正常退出。
const LOCK_NAME: &str = "boot.lock.json";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 上一次启动的自述信息（从标记文件里读回来）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LastBoot {
    pub pid: u32,
    pub started_at_ms: i64,
    pub version: String,
}

/// 启动自检结论。**这个结构会原样发给前端**（`recovery.status`），
/// 字段名的 snake_case 就是前端的字段名 —— 改字段 = 改契约。
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    /// 上次没有正常退出
    pub unclean: bool,
    /// 上次启动的自述（标记文件解析不出来时为 None，但 `unclean` 仍是 true）
    pub last_boot: Option<LastBoot>,
    /// `quick_check` 是否通过。`unclean == false` 时无意义（没检查过）。
    pub quick_check_ok: bool,
    /// `quick_check` 的原文结论：`"ok"` 或问题摘要
    pub quick_check: String,
    /// `wal_checkpoint(TRUNCATE)` 的结果：`"ok"` / `"busy（…）"` / 失败原因
    pub wal_checkpoint: String,
    /// 自检那一刻的主库大小（字节）
    pub db_bytes: u64,
    /// WAL 残留大小（checkpoint 之前量到的）
    pub wal_bytes: u64,
    /// 自检时间（Unix 毫秒，前端本地化显示）
    pub checked_at_ms: i64,
    /// 数据目录（只给"打开文件夹"按钮用；不参与任何拼接）
    pub data_dir: String,
}

impl Default for RecoveryReport {
    fn default() -> Self {
        Self {
            unclean: false,
            last_boot: None,
            quick_check_ok: true,
            quick_check: String::new(),
            wal_checkpoint: String::new(),
            db_bytes: 0,
            wal_bytes: 0,
            checked_at_ms: 0,
            data_dir: String::new(),
        }
    }
}

/// 启动时调用。返回本次的结论，并写好新的标记文件。
///
/// **必须在打开主数据库连接之前调用**：自检要看的是磁盘上的"现场"
/// （WAL 还剩多少、能不能 checkpoint）—— 主连接一开，SQLite 可能已经
/// 自己把一部分现场收拾掉了。
pub fn begin_boot(data_dir: &Path, db_path: &Path) -> RecoveryReport {
    let lock = data_dir.join(LOCK_NAME);
    let mut report = RecoveryReport {
        checked_at_ms: now_ms(),
        data_dir: data_dir.display().to_string(),
        ..Default::default()
    };

    if lock.exists() {
        report.unclean = true;
        report.last_boot = fs::read_to_string(&lock)
            .ok()
            .and_then(|s| serde_json::from_str::<LastBoot>(&s).ok());
        assess(&mut report, db_path);
    }

    write_lock(&lock);
    report
}

/// 正常退出时调用（删标记）。**必须在唯一的收尾路径上调用**：
/// 窗口关闭 / 更新拉起 / 烟测结束都汇到那里，漏掉任何一条，
/// 下次启动都会被误报"上次没有正常退出"。
pub fn mark_clean(data_dir: &Path) {
    let _ = fs::remove_file(data_dir.join(LOCK_NAME));
}

/// 生成一致性快照，返回快照文件路径。
///
/// 新引擎没有 VACUUM INTO，等价动作是：先把内存状态**压实成快照**
/// （store::Store::force_snapshot），再把那个干净的快照文件复制走。
/// 直接复制正在追加的日志会拿到半截状态，不能那么干。
///
/// 为什么要用户主动点、而不是自动做：快照会把库完整复制一份（大库=几百 MB），
/// 自动做是有代价的；而且"什么时候需要快照"是用户/向导的判断。
pub fn snapshot(db: &mut Db, data_dir: &Path) -> Result<String, String> {
    let dir = data_dir.join("recovery");
    fs::create_dir_all(&dir).map_err(|e| format!("建快照目录失败：{e}"))?;
    let out = dir.join(format!("snap-{}.dkb", now_ms()));
    db.store_mut()
        .force_snapshot()
        .map_err(|e| format!("压实快照失败：{e}"))?;
    let snap = db.store().snap_path().to_path_buf();
    fs::copy(&snap, &out).map_err(|e| format!("生成快照失败：{e}"))?;
    Ok(out.to_string_lossy().to_string())
}

// ---------- 内部 ----------

fn write_lock(lock: &Path) {
    let info = LastBoot {
        pid: std::process::id(),
        started_at_ms: now_ms(),
        version: crate::updater::current_version().to_string(),
    };
    if let Ok(s) = serde_json::to_string(&info) {
        let _ = fs::write(lock, s);
    }
}

/// 数据文件 main.dkb 对应的日志文件 main.dkb.log。
fn log_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(".log");
    PathBuf::from(s)
}

fn assess(report: &mut RecoveryReport, db_path: &Path) {
    // 新引擎的数据是「快照 + 日志」两件套：快照是状态，日志是增量。
    // 一致性判据不再是引擎内部的 quick_check，而是**日志能不能被完整解析**。
    let log = log_path(db_path);
    if !log.exists() {
        report.quick_check_ok = false;
        report.quick_check = "未执行：还没有日志文件（大概是全新数据目录）".to_string();
        return;
    }
    report.db_bytes = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    report.wal_bytes = fs::metadata(&log).map(|m| m.len()).unwrap_or(0);

    let chk = store::check_log(&log);
    report.quick_check_ok = chk.ok;
    report.quick_check = chk.message;
    // 原来这一步是「把 WAL 收敛回主库」；新引擎里对应的是"尾部半写是否已被丢弃"
    report.wal_checkpoint = if chk.truncated_tail {
        "已丢弃尾部半写事务（这是恢复，不是损坏）".to_string()
    } else {
        "ok".to_string()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "deskbase-recovery-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 数据文件与日志文件的位置（与 Store 的约定一致）
    fn paths(d: &Path) -> (PathBuf, PathBuf) {
        (d.join("data").join("main.dkb"), d.join("data").join("main.dkb.log"))
    }

    /// 造一份有两行数据的库
    fn make_db(d: &Path) {
        let mut db = Db::open(d).unwrap();
        db.create_table(&crate::model::TableSpec {
            name: "t".to_string(),
            comment: None,
            columns: vec![crate::model::ColumnDef {
                name: "a".to_string(),
                ty: crate::model::ColType::Integer,
                not_null: false,
                default: None,
                primary_key: false,
                comment: None,
                shared: None,
                link: None,
                lookup: None,
                rollup: None,
            }],
        })
        .unwrap();
        db.insert_rows(
            "t",
            &["a".to_string()],
            &[vec![Some("1".to_string())], vec![Some("2".to_string())]],
        )
        .unwrap();
    }

    #[test]
    fn clean_boot_then_unclean_detected() {
        let d = tmp_dir("boot");
        let (db, _log) = paths(&d);

        let r1 = begin_boot(&d, &db);
        assert!(!r1.unclean, "全新目录第一次启动不该报未清理");
        assert!(d.join(LOCK_NAME).exists(), "启动后必须有标记文件");

        mark_clean(&d);
        assert!(!d.join(LOCK_NAME).exists());

        let r2 = begin_boot(&d, &db);
        assert!(!r2.unclean, "正常退出之后再启动必须是干净的");

        let r3 = begin_boot(&d, &db);
        assert!(r3.unclean, "上次没删标记，必须被认出来");
        assert!(r3.last_boot.is_some(), "上次启动的信息要能读回来");
        assert_eq!(r3.last_boot.as_ref().unwrap().pid, std::process::id());

        mark_clean(&d);
    }

    #[test]
    fn unclean_with_healthy_log_passes_check_and_keeps_data() {
        let d = tmp_dir("healthy");
        let (db, _log) = paths(&d);

        begin_boot(&d, &db);
        make_db(&d);

        let r = begin_boot(&d, &db); // 不调 mark_clean = 崩溃后重启
        assert!(r.unclean);
        assert!(r.quick_check_ok, "健康的日志必须通过自检：{}", r.quick_check);
        assert_eq!(r.wal_checkpoint, "ok");

        // 崩溃重启不能丢已提交的数据
        let db2 = Db::open(&d).unwrap();
        assert_eq!(db2.store().count("rec/t/"), 2, "崩溃重启不能丢已提交的数据");
        mark_clean(&d);
    }

    #[test]
    fn torn_tail_is_recoverable_not_corrupt() {
        let d = tmp_dir("half");
        let (db, log) = paths(&d);
        begin_boot(&d, &db);
        make_db(&d);

        // 往日志尾部追加一条"声称有内容但没写完"的条目 —— 模拟被强杀
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"DKB1\x63\x00\x00\x00\x00\x00\x00\x00").unwrap();
        f.sync_all().unwrap();

        let r = begin_boot(&d, &db);
        assert!(r.unclean);
        assert!(r.quick_check_ok, "半写是可恢复的，不该报损坏：{}", r.quick_check);
        assert!(r.quick_check.contains("已丢弃"), "要说清已丢弃：{}", r.quick_check);
        mark_clean(&d);
    }

    #[test]
    fn destroyed_log_header_reported_honestly() {
        let d = tmp_dir("corrupt");
        let (db, log) = paths(&d);
        begin_boot(&d, &db);
        make_db(&d);

        {
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().write(true).open(&log).unwrap();
            f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03])
                .unwrap();
            f.sync_all().unwrap();
        }

        let r = begin_boot(&d, &db);
        assert!(r.unclean);
        assert!(!r.quick_check.is_empty());
        assert!(
            !r.quick_check.starts_with("ok"),
            "坏日志必须被报出来而不是被吞掉：{}",
            r.quick_check
        );
        mark_clean(&d);
    }

    #[test]
    fn snapshot_is_a_working_copy() {
        let d = tmp_dir("snap");
        make_db(&d);
        let mut db = Db::open(&d).unwrap();

        let out = snapshot(&mut db, &d).unwrap();
        assert!(Path::new(&out).exists());
        assert!(out.contains("recovery"));

        // 快照必须能被解析回一份完整状态，且数据在
        let raw = fs::read(&out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let data = v.get("data").unwrap().as_object().unwrap();
        assert!(data.contains_key("tbl/t"), "快照里要有表定义");
        assert_eq!(
            data.keys().filter(|k| k.starts_with("rec/t/")).count(),
            2,
            "快照必须是一份可用的完整副本"
        );
        mark_clean(&d);
    }
}
