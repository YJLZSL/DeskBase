//! 手动备份（v0.3.0 · 不丢数据）
//!
//! 为什么单开一个模块：迁移前的自动备份（`db.rs::backup_before_upgrade`）只在
//! "库结构要升级"时发生；用户想**主动**留一份（换机、折腾数据前、把台账发给同事）
//! 时没有入口。三条底线第一条是"不丢数据"，主动备份是它最直接的兑现。
//!
//! 三个设计取舍：
//!   1. 放 `<数据目录>/backups/`，与迁移备份同处 —— 用户只需要记住一个地方；
//!   2. 用 `VACUUM INTO` 而不是复制文件：它产出**一致快照**，还顺带整理碎片；
//!      复制一个正在写入的 .db 可能拿到半截状态（WAL 还没落盘）；
//!   3. 建完**立刻三步校验**（非空 / 能独立打开 / quick_check 通过且表数一致）——
//!      不校验的备份等于没有备份：磁盘满、半途失败都会留下一个"看起来像备份"的文件。

use rusqlite::Connection;
use std::path::{Path, PathBuf};

use time::macros::format_description;
use time::OffsetDateTime;

/// 给界面用的备份描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Unix 毫秒（取文件的修改时间）
    pub created_ms: i64,
}

pub fn backup_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

fn stamp() -> String {
    OffsetDateTime::now_utc()
        .format(&format_description!("[year][month][day]-[hour][minute][second]"))
        .unwrap_or_else(|_| "backup".into())
}

/// 源库的用户表数 —— 第三步校验用它挡住"空壳备份"。
fn table_count(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("读取表数失败：{e}"))
}

/// 三步校验。`expect_tables` 给 `Some(n)` 时会比对表数（备份必须与源库同构）。
pub fn verify(path: &Path, expect_tables: Option<i64>) -> Result<u64, String> {
    // ① 非空
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("备份文件读不到（{}）：{e}", path.display()))?;
    if meta.len() == 0 {
        return Err("备份文件是 0 字节 —— 这次备份不可用，请重试".into());
    }
    // ② 能独立打开（新开连接，而不是复用当前连接）
    let conn = Connection::open(path).map_err(|e| format!("备份文件打不开：{e}"))?;
    // ③ 完整性 + 表数
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| format!("完整性检查失败：{e}"))?;
    if check != "ok" {
        return Err(format!("备份文件没通过完整性检查：{check}"));
    }
    if let Some(n) = expect_tables {
        let got = table_count(&conn)?;
        if got != n {
            return Err(format!(
                "备份里的表数（{got}）与当前库（{n}）对不上 —— 备份可能不完整，已保留原文件供排查"
            ));
        }
    }
    Ok(meta.len())
}

fn info_of(path: &Path, size: u64) -> BackupInfo {
    let created_ms = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    BackupInfo {
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: path.to_string_lossy().to_string(),
        size,
        created_ms,
    }
}

/// 创建一份备份（含三步校验）。
pub fn create(conn: &Connection, data_dir: &Path) -> Result<BackupInfo, String> {
    let dir = backup_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建备份目录失败（{}）：{e}", dir.display()))?;
    let name = format!("deskbase-{}.db", stamp());
    let path = dir.join(&name);
    if path.exists() {
        // 同一秒内点两次：不覆盖，直接把已有的那份校验一遍返回（幂等）
        let size = verify(&path, None)?;
        return Ok(info_of(&path, size));
    }
    // VACUUM INTO 的目标是 SQL 字面量 —— 路径里的单引号必须转义（与 db.rs 同款处理）
    let lit = path.to_string_lossy().replace('\'', "''");
    let expect = table_count(conn)?;
    conn.execute_batch(&format!("VACUUM INTO '{lit}'"))
        .map_err(|e| format!("备份失败（目标 {}）：{e}", path.display()))?;
    // 立刻校验：不校验的备份等于没有备份
    match verify(&path, Some(expect)) {
        Ok(size) => Ok(info_of(&path, size)),
        Err(e) => Err(format!("备份已生成但校验没过：{e}")),
    }
}

/// 列出备份（新 → 旧）。读不到目录时返回空表 —— "没有备份"与"读不到"对界面是同一件事。
pub fn list(data_dir: &Path) -> Vec<BackupInfo> {
    let dir = backup_dir(data_dir);
    let mut out: Vec<BackupInfo> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "db").unwrap_or(false) {
            if let Ok(m) = std::fs::metadata(&p) {
                out.push(info_of(&p, m.len()));
            }
        }
    }
    out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "deskbase-backup-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(dir: &Path) -> Connection {
        let conn = Connection::open(dir.join("main.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE a(x INTEGER); CREATE TABLE b(y TEXT); INSERT INTO a VALUES (1);",
        )
        .unwrap();
        conn
    }

    /// 正向：备份能建、能独立打开、数据在、表数一致。
    #[test]
    fn 备份_创建后可独立打开且表数一致() {
        let d = tmp_dir("ok");
        let conn = seed(&d);
        let info = create(&conn, &d).unwrap();
        assert!(info.size > 0, "备份不能是空文件");
        assert!(
            info.name.starts_with("deskbase-"),
            "命名应带 deskbase 前缀：{}",
            info.name
        );
        // 独立打开（不复用源连接）并读数据 —— 这才是"这份备份能用"的证明
        let b = Connection::open(&info.path).unwrap();
        let n: i64 = b
            .query_row("SELECT COUNT(*) FROM a", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "备份里应能读到源库的数据");
        assert_eq!(table_count(&b).unwrap(), 2);
        assert!(verify(Path::new(&info.path), Some(2)).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 列表要新的在前（界面靠它）。
    #[test]
    fn 备份_列表按时间倒序() {
        let d = tmp_dir("list");
        let conn = seed(&d);
        let first = create(&conn, &d).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // 文件名精确到秒
        let second = create(&conn, &d).unwrap();
        let items = list(&d);
        assert!(items.len() >= 2);
        assert_eq!(items[0].name, second.name, "最新的应排最前");
        assert!(items.iter().any(|i| i.name == first.name));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 负向：0 字节文件必须被拦下（"看起来像备份"是最危险的）。
    #[test]
    fn 备份_空文件校验必失败() {
        let d = tmp_dir("empty");
        let p = d.join("empty.db");
        std::fs::write(&p, b"").unwrap();
        let e = verify(&p, None).unwrap_err();
        assert!(e.contains("0 字节"), "错误要说清原因：{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 负向：表数对不上要报出来（挡住"备份中途失败留下的空壳"）。
    #[test]
    fn 备份_表数对不上要被拦下() {
        let d = tmp_dir("mismatch");
        let conn = seed(&d);
        let info = create(&conn, &d).unwrap();
        let e = verify(Path::new(&info.path), Some(9)).unwrap_err();
        assert!(e.contains("对不上"), "要明确说表数对不上：{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 幂等：同一秒内连点两次不覆盖、不报错。
    #[test]
    fn 备份_同秒重复调用不覆盖() {
        let d = tmp_dir("again");
        let conn = seed(&d);
        let a = create(&conn, &d).unwrap();
        let b = create(&conn, &d).unwrap();
        assert_eq!(a.name, b.name, "同一秒内应复用同一份，不覆盖");
        let _ = std::fs::remove_dir_all(&d);
    }
}
