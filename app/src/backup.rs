//! 手动备份（不丢数据）
//!
//! 为什么单开一个模块：用户想**主动**留一份（换机、折腾数据前、把台账发给同事）
//! 时得有入口。三条底线第一条是"不丢数据"，主动备份是它最直接的兑现。
//!
//! 三个设计取舍：
//!   1. 放 `<数据目录>/backups/`，用户只需要记住一个地方；
//!   2. **先压实快照，再复制快照文件**。原来这一层靠 SQLite 的 `VACUUM INTO`
//!      产出一致快照；新引擎（docs/adr/0021）的等价动作是
//!      [`Store::force_snapshot`] —— 把内存状态整份写成一个干净的快照文件，
//!      然后把那个文件复制走。直接复制正在追加的日志会拿到半截状态，不能那么干；
//!   3. 建完**立刻三步校验**（非空 / 能解析回状态 / 表数一致）——
//!      不校验的备份等于没有备份：磁盘满、半途失败都会留下一个"看起来像备份"的文件。

use std::path::{Path, PathBuf};

use time::macros::format_description;
use time::OffsetDateTime;

use crate::model::Db;

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

/// 当前库里的表数 —— 第三步校验用它挡住"空壳备份"。
pub fn table_count(db: &Db) -> i64 {
    db.store().count("tbl/") as i64
}

/// 三步校验。`expect_tables` 给 `Some(n)` 时会比对表数（备份必须与源库同构）。
///
/// 第二步原来是"用 SQLite 独立打开"，现在是"把快照 JSON 解析回来"——
/// 判据其实更严格：它要求这份文件**真的能被还原成一份状态**，而不只是"文件头没坏"。
pub fn verify(path: &Path, expect_tables: Option<i64>) -> Result<u64, String> {
    // ① 非空
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("备份文件读不到（{}）：{e}", path.display()))?;
    if meta.len() == 0 {
        return Err("备份文件是 0 字节 —— 这次备份不可用，请重试".into());
    }
    // ② 能被解析回一份完整状态（等价原来的"能独立打开"）
    let raw = std::fs::read(path).map_err(|e| format!("备份文件读不出来：{e}"))?;
    let v: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("备份文件不是一份完整的快照：{e}"))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_object())
        .ok_or_else(|| "备份文件里没有数据段 —— 可能不是 DeskBase 的备份".to_string())?;
    // ③ 表数一致
    if let Some(n) = expect_tables {
        let got = data.keys().filter(|k| k.starts_with("tbl/")).count() as i64;
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
pub fn create(db: &mut Db, data_dir: &Path) -> Result<BackupInfo, String> {
    let dir = backup_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建备份目录失败（{}）：{e}", dir.display()))?;
    let name = format!("deskbase-{}.dkb", stamp());
    let path = dir.join(&name);
    if path.exists() {
        // 同一秒内点两次：不覆盖，直接把已有的那份校验一遍返回（幂等）
        let size = verify(&path, None)?;
        return Ok(info_of(&path, size));
    }
    let expect = table_count(db);
    // 先把状态压实成快照，再复制它 —— 复制正在追加的日志会拿到半截状态
    db.store_mut()
        .force_snapshot()
        .map_err(|e| format!("备份前压实快照失败：{e}"))?;
    let snap = db.store().snap_path().to_path_buf();
    std::fs::copy(&snap, &path).map_err(|e| format!("备份失败（目标 {}）：{e}", path.display()))?;
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
        if p.extension().map(|x| x == "dkb").unwrap_or(false) {
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

    fn seed(dir: &Path) -> Db {
        let db = Db::open(dir).unwrap();
        db
    }

    fn two_tables(db: &mut Db) {
        let mk = |name: &str| crate::model::TableSpec {
            name: name.to_string(),
            comment: None,
            columns: vec![crate::model::ColumnDef {
                name: "x".to_string(),
                ty: crate::model::ColType::Text,
                not_null: false,
                default: None,
                primary_key: false,
                comment: None,
                shared: None,
                link: None,
                lookup: None,
                rollup: None,
            }],
        };
        db.create_table(&mk("a")).unwrap();
        db.create_table(&mk("b")).unwrap();
        db.insert_rows("a", &["x".to_string()], &[vec![Some("1".to_string())]])
            .unwrap();
    }

    /// 正向：备份能建、能独立读回、数据在、表数一致。
    #[test]
    fn 备份_创建后可独立读回且表数一致() {
        let d = tmp_dir("ok");
        let mut db = seed(&d);
        two_tables(&mut db);
        let info = create(&mut db, &d).unwrap();
        assert!(info.size > 0, "备份不能是空文件");
        assert!(
            info.name.starts_with("deskbase-"),
            "命名应带 deskbase 前缀：{}",
            info.name
        );
        // 独立读回（不复用源 db）并取到数据 —— 这才是"这份备份能用"的证明
        let raw = std::fs::read(&info.path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let data = v.get("data").unwrap().as_object().unwrap();
        assert!(data.contains_key("tbl/a"), "备份里应有表 a");
        assert!(data.contains_key("rec/a/00000000000000000001"), "备份里应有记录");
        assert_eq!(table_count(&db), 2);
        assert!(verify(Path::new(&info.path), Some(2)).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 列表要新的在前（界面靠它）。
    #[test]
    fn 备份_列表按时间倒序() {
        let d = tmp_dir("list");
        let mut db = seed(&d);
        two_tables(&mut db);
        let first = create(&mut db, &d).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // 文件名精确到秒
        let second = create(&mut db, &d).unwrap();
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
        let p = d.join("empty.dkb");
        std::fs::write(&p, b"").unwrap();
        let e = verify(&p, None).unwrap_err();
        assert!(e.contains("0 字节"), "错误要说清原因：{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 负向：不是快照的文件要被拦下。
    #[test]
    fn 备份_非快照文件校验必失败() {
        let d = tmp_dir("notjson");
        let p = d.join("broken.dkb");
        std::fs::write(&p, b"not a snapshot at all").unwrap();
        let e = verify(&p, None).unwrap_err();
        assert!(e.contains("完整的快照"), "错误要说清原因：{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 负向：表数对不上要报出来（挡住"备份中途失败留下的空壳"）。
    #[test]
    fn 备份_表数对不上要被拦下() {
        let d = tmp_dir("mismatch");
        let mut db = seed(&d);
        two_tables(&mut db);
        let info = create(&mut db, &d).unwrap();
        let e = verify(Path::new(&info.path), Some(9)).unwrap_err();
        assert!(e.contains("对不上"), "要明确说表数对不上：{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 幂等：同一秒内连点两次不覆盖、不报错。
    #[test]
    fn 备份_同秒重复调用不覆盖() {
        let d = tmp_dir("again");
        let mut db = seed(&d);
        two_tables(&mut db);
        let a = create(&mut db, &d).unwrap();
        let b = create(&mut db, &d).unwrap();
        assert_eq!(a.name, b.name, "同一秒内应复用同一份，不覆盖");
        let _ = std::fs::remove_dir_all(&d);
    }
}
