//! 存储层：单文件嵌入式 SQLite（见 docs/adr/0003）
//!
//! 持久性约定（见 docs/07）：
//!   - WAL 模式，不可关闭
//!   - synchronous = FULL，每次提交都 fsync —— 宁可慢，不丢数据
//!   - 外键约束开启
//!   - 忙等超时，避免瞬时锁冲突直接失败

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// 当前数据模型版本。每次结构变更都要 +1，并在 `migrate` 里加一段。
const SCHEMA_VERSION: i64 = 1;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Db {
    conn: Connection,
}

fn now_ms() -> i64 {
    // 用系统时间换算 UTC 毫秒。项目约定所有时间存 UTC 毫秒（见 local-docs/reference/04）
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Db {
    /// 打开（必要时创建）数据库文件，并执行迁移。
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据目录失败 {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
        let db = Db { conn };
        db.apply_durability_pragmas()?;
        db.migrate()?;
        Ok(db)
    }

    /// 只为测试用的内存库
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| format!("打开内存库失败: {e}"))?;
        let db = Db { conn };
        db.conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| e.to_string())?;
        db.migrate()?;
        Ok(db)
    }

    fn apply_durability_pragmas(&self) -> Result<()> {
        // WAL + FULL 同步：崩在任意时刻都不丢已提交的数据
        self.conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;
                 PRAGMA wal_autocheckpoint = 1000;",
            )
            .map_err(|e| format!("设置持久化参数失败: {e}"))?;
        Ok(())
    }

    /// 迁移框架：读当前版本 → 逐级升级 → 写回版本。
    ///
    /// v0.1.0 只有版本 1，所以这里只建表。将来加版本时**必须**：
    ///   1. 在升级前先备份（由上层调用负责）
    ///   2. 每级迁移在一个事务内完成
    fn migrate(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("读取结构版本失败: {e}"))?;

        if current == 0 {
            self.migrate_to_v1()?;
        }
        if current > SCHEMA_VERSION {
            return Err(format!(
                "数据文件的结构版本（{current}）高于本程序支持的版本（{SCHEMA_VERSION}）。\
                 请升级 DeskBase 后再打开，避免数据损坏。"
            ));
        }
        Ok(())
    }

    fn migrate_to_v1(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS sys_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS note (
                 id         TEXT PRIMARY KEY,
                 title      TEXT NOT NULL DEFAULT '',
                 content    TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 deleted_at INTEGER
             );

             CREATE INDEX IF NOT EXISTS idx_note_updated
                 ON note (deleted_at, updated_at DESC);",
        )
        .map_err(|e| format!("建表失败: {e}"))?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| format!("写入结构版本失败: {e}"))?;
        tx.commit().map_err(|e| format!("提交迁移失败: {e}"))?;
        Ok(())
    }

    // ---------- 笔记 ----------

    pub fn list_notes(&self) -> Result<Vec<NoteSummary>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, title, updated_at FROM note
                 WHERE deleted_at IS NULL
                 ORDER BY updated_at DESC, id DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(NoteSummary {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn get_note(&self, id: &str) -> Result<Option<Note>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, title, content, created_at, updated_at FROM note
                 WHERE id = ?1 AND deleted_at IS NULL",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![id]).map_err(|e| e.to_string())?;
        match rows.next().map_err(|e| e.to_string())? {
            Some(r) => Ok(Some(Note {
                id: r.get(0).map_err(|e| e.to_string())?,
                title: r.get(1).map_err(|e| e.to_string())?,
                content: r.get(2).map_err(|e| e.to_string())?,
                created_at: r.get(3).map_err(|e| e.to_string())?,
                updated_at: r.get(4).map_err(|e| e.to_string())?,
            })),
            None => Ok(None),
        }
    }

    pub fn create_note(&self, title: &str) -> Result<Note> {
        // ulid 3.x 的构造函数是 generate()，不是 new()
        let id = ulid::Ulid::generate().to_string();
        let ts = now_ms();
        self.conn
            .execute(
                "INSERT INTO note (id, title, content, created_at, updated_at)
                 VALUES (?1, ?2, '', ?3, ?3)",
                params![id, title, ts],
            )
            .map_err(|e| format!("新建笔记失败: {e}"))?;
        Ok(Note {
            id,
            title: title.to_string(),
            content: String::new(),
            created_at: ts,
            updated_at: ts,
        })
    }

    pub fn save_note(&self, id: &str, title: &str, content: &str) -> Result<i64> {
        let ts = now_ms();
        let n = self
            .conn
            .execute(
                "UPDATE note SET title = ?2, content = ?3, updated_at = ?4
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, title, content, ts],
            )
            .map_err(|e| format!("保存笔记失败: {e}"))?;
        if n == 0 {
            return Err(format!("笔记不存在或已删除: {id}"));
        }
        Ok(ts)
    }

    /// 软删除，进回收站（见 docs/adr/0013）
    pub fn delete_note(&self, id: &str) -> Result<()> {
        let ts = now_ms();
        let n = self
            .conn
            .execute(
                "UPDATE note SET deleted_at = ?2, updated_at = ?2
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, ts],
            )
            .map_err(|e| format!("删除笔记失败: {e}"))?;
        if n == 0 {
            return Err(format!("笔记不存在: {id}"));
        }
        Ok(())
    }

    pub fn count_notes(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM note WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())
    }
}

/// 数据目录解析（v0.1.0 先用用户目录，便携版逻辑后续接入）
pub fn default_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DESKBASE_DATA_DIR") {
        return PathBuf::from(p);
    }
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("DeskBaseData")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 建库后可以建笔记并读回() {
        let db = Db::open_in_memory().unwrap();
        let n = db.create_note("测试").unwrap();
        assert_eq!(n.title, "测试");
        let got = db.get_note(&n.id).unwrap().unwrap();
        assert_eq!(got.content, "");
        assert_eq!(db.count_notes().unwrap(), 1);
    }

    #[test]
    fn 保存后内容与时间戳都更新() {
        let db = Db::open_in_memory().unwrap();
        let n = db.create_note("").unwrap();
        let ts = db.save_note(&n.id, "标题", "正文").unwrap();
        let got = db.get_note(&n.id).unwrap().unwrap();
        assert_eq!(got.title, "标题");
        assert_eq!(got.content, "正文");
        assert!(ts >= got.created_at);
    }

    #[test]
    fn 删除是软删除_列表里不再出现但记录还在() {
        let db = Db::open_in_memory().unwrap();
        let n = db.create_note("待删").unwrap();
        db.delete_note(&n.id).unwrap();
        assert_eq!(db.count_notes().unwrap(), 0);
        assert!(db.get_note(&n.id).unwrap().is_none());
    }

    #[test]
    fn 保存不存在的笔记要报错而不是静默成功() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.save_note("不存在", "x", "y").is_err());
    }

    #[test]
    fn 中文标题与内容能正确往返() {
        let db = Db::open_in_memory().unwrap();
        let n = db.create_note("会议记录").unwrap();
        db.save_note(&n.id, "会议记录", "讨论了「宣纸主题」的落地。\n第二行。").unwrap();
        let got = db.get_note(&n.id).unwrap().unwrap();
        assert!(got.content.contains("宣纸主题"));
        assert!(got.content.contains('\n'));
    }
}
