//! 崩溃恢复：判断「上次是否正常退出」，没正常退出就做一次完整性自检。
//!
//! ## 为什么要一个标记文件
//!
//! WAL + `synchronous=FULL` 保证的是「**已提交**的事务不丢」。但它不回答
//! 另一个问题：上次那个进程是正常退的还是被强杀的（断电、任务管理器、崩溃）？
//! 两种情况下数据库都能恢复成一致状态，可用户的处境完全不同 —— 强杀之后
//! 用户有权被告知，而不是被瞒着。「崩溃必进恢复向导」是三条底线里
//! 「不丢数据」的一部分。
//!
//! 标记文件是计算机里最老也最可靠的一招：
//!   · 启动时写 `boot.lock.json`（含 pid / 启动时间 / 版本）；
//!   · **正常退出时删掉它**（[`mark_clean`]）；
//!   · 下次启动时它还在 → 上次没走到"正常退出"这一步。
//!
//! 为什么不把标记记在数据库里：**数据库本身可能就是坏的那一个**。
//! 判断"该不该进恢复"的逻辑，必须不依赖它要判断的对象 —— 否则库坏了，
//! 连"库坏了"这件事都读不出来。
//!
//! ## 自检做什么、不做什么
//!
//! · `PRAGMA quick_check`：SQLite 官方的完整性快速检查（比 `integrity_check`
//!   快，代价是只报告有限数量的错误）。结果**原样**带进报告，不向用户翻译成
//!   "没问题"之外的说法。
//! · `PRAGMA wal_checkpoint(TRUNCATE)`：把上次残留的 WAL 合回主库并清空。
//!   正常情况下 SQLite 打开库时会自己处理，这里显式做一次是为了把
//!   "WAL 当时还剩多少"量出来（它大 = 上次确实死在写入中间）。
//! · **不做自动修复**。没有哪个自动修复能在不看现场的情况下被信任；向导给的是
//!   "看清现场"的能力（自检结果 / 快照 / 数据目录），不是一个魔法按钮。
//!
//! ## 快照
//!
//! [`snapshot`] 用 `VACUUM INTO` —— SQLite 官方的一致性拷贝通道，产出的是
//! 页级一致、任何 SQLite 工具都能打开的干净副本。**不是文件复制**：
//! WAL 模式下直接复制文件可能漏掉还没 checkpoint 的已提交事务。

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
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

/// 生成一致性快照（`VACUUM INTO`），返回快照文件路径。
///
/// 为什么要用户主动点、而不是自动做：快照会把库完整复制一份（大库=几百 MB），
/// 自动做是有代价的；而且"什么时候需要快照"是用户/向导的判断。
pub fn snapshot(conn: &Connection, data_dir: &Path) -> Result<String, String> {
    let dir = data_dir.join("recovery");
    fs::create_dir_all(&dir).map_err(|e| format!("建快照目录失败：{e}"))?;
    let out = dir.join(format!("snap-{}.db", now_ms()));
    let out_s = out.to_string_lossy().to_string();
    conn.execute("VACUUM INTO ?1", rusqlite::params![out_s])
        .map_err(|e| format!("生成快照失败：{e}"))?;
    Ok(out_s)
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

fn wal_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

fn assess(report: &mut RecoveryReport, db_path: &Path) {
    if !db_path.exists() {
        report.quick_check_ok = false;
        report.quick_check = "未执行：数据文件不存在".to_string();
        return;
    }
    report.db_bytes = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal = wal_path(db_path);
    report.wal_bytes = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);

    match Connection::open(db_path) {
        Ok(conn) => {
            match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
                Ok(v) if v.eq_ignore_ascii_case("ok") => {
                    report.quick_check_ok = true;
                    report.quick_check = "ok".to_string();
                }
                Ok(v) => {
                    report.quick_check_ok = false;
                    report.quick_check =
                        format!("{v}（可能还有更多 —— quick_check 只报告有限数量的错误）");
                }
                Err(e) => {
                    report.quick_check_ok = false;
                    report.quick_check = format!("检查失败：{e}");
                }
            }
            match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            }) {
                Ok(0) => report.wal_checkpoint = "ok".to_string(),
                Ok(b) => {
                    report.wal_checkpoint =
                        format!("busy（{b}）：有别的连接占着 WAL，没能做完清理");
                }
                Err(e) => report.wal_checkpoint = format!("失败：{e}"),
            }
            // checkpoint 之后重新量：给用户看"WAL 被清掉了多少"
            report.db_bytes = fs::metadata(db_path).map(|m| m.len()).unwrap_or(report.db_bytes);
            report.wal_bytes = fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        }
        Err(e) => {
            report.quick_check_ok = false;
            report.quick_check = format!("打不开数据文件：{e}");
        }
    }
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

    fn make_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t(a INTEGER); INSERT INTO t(a) VALUES (1),(2);")
            .unwrap();
    }

    #[test]
    fn clean_boot_then_unclean_detected() {
        let d = tmp_dir("boot");
        let db = d.join("main.db");

        let r1 = begin_boot(&d, &db);
        assert!(!r1.unclean, "全新目录第一次启动不该报未清理");
        assert!(d.join(LOCK_NAME).exists(), "启动后必须有标记文件");

        // 正常退出
        mark_clean(&d);
        assert!(!d.join(LOCK_NAME).exists());

        let r2 = begin_boot(&d, &db);
        assert!(!r2.unclean, "正常退出之后再启动必须是干净的");

        // 模拟崩溃：不调 mark_clean，直接"再启动一次"
        let r3 = begin_boot(&d, &db);
        assert!(r3.unclean, "上次没删标记，必须被认出来");
        assert!(r3.last_boot.is_some(), "上次启动的信息要能读回来");
        assert_eq!(r3.last_boot.as_ref().unwrap().pid, std::process::id());

        mark_clean(&d);
    }

    #[test]
    fn unclean_with_healthy_db_passes_check_and_keeps_data() {
        let d = tmp_dir("healthy");
        let db = d.join("main.db");

        begin_boot(&d, &db); // 写标记
        {
            make_db(&db);
            let c = Connection::open(&db).unwrap();
            c.execute_batch("INSERT INTO t(a) VALUES (3)").unwrap();
        } // 连接在这里关闭（模拟"写完之后进程没了"）

        let r = begin_boot(&d, &db); // 不调 mark_clean = 崩溃后重启
        assert!(r.unclean);
        assert!(r.quick_check_ok, "健康库必须通过自检：{}", r.quick_check);
        assert_eq!(r.wal_checkpoint, "ok");

        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM t", [], |x| x.get(0)).unwrap();
        assert_eq!(n, 3, "崩溃重启不能丢已提交的数据");
        mark_clean(&d);
    }

    #[test]
    fn corrupt_db_is_reported_not_swallowed() {
        let d = tmp_dir("corrupt");
        let db = d.join("main.db");

        begin_boot(&d, &db);
        make_db(&db);

        // 往文件开头刷垃圾：模拟磁盘损坏 / 被别的程序写坏
        {
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().write(true).open(&db).unwrap();
            f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03])
                .unwrap();
            f.sync_all().unwrap();
        }

        let r = begin_boot(&d, &db);
        assert!(r.unclean);
        assert!(
            !r.quick_check_ok,
            "坏库必须被报出来而不是被吞掉：{}",
            r.quick_check
        );
        assert!(!r.quick_check.is_empty());
        mark_clean(&d);
    }

    #[test]
    fn snapshot_is_a_working_copy() {
        let d = tmp_dir("snap");
        let db = d.join("main.db");
        make_db(&db);

        let c = Connection::open(&db).unwrap();
        let out = snapshot(&c, &d).unwrap();
        assert!(Path::new(&out).exists());
        assert!(out.contains("recovery"));

        let sc = Connection::open(&out).unwrap();
        let n: i64 = sc.query_row("SELECT count(*) FROM t", [], |x| x.get(0)).unwrap();
        assert_eq!(n, 2, "快照必须是一份可用的完整副本");
        drop(sc);
        mark_clean(&d);
    }
}
