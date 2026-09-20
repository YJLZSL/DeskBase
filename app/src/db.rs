//! 存储层：单文件嵌入式 SQLite（见 docs/adr/0003）
//!
//! 持久性约定（见 docs/07）：
//!   - WAL 模式，不可关闭
//!   - synchronous = FULL，每次提交都 fsync —— 宁可慢，不丢数据
//!   - 外键约束开启
//!   - 忙等超时，避免瞬时锁冲突直接失败

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

use time::macros::format_description;
use time::OffsetDateTime;

/// 当前数据模型版本。每次结构变更都要 +1，并在 `migrate` 里加一段。
///
/// 框架保证「逐级递进」：从库里读到的旧版本一路升到这个数，
/// 每升一级先 `VACUUM INTO` 备份、再在一个事务里改结构、最后写回 `user_version`。
const SCHEMA_VERSION: i64 = 2;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
    /// 正文摘要（前若干字符）。列表要能按内容搜索与筛选，又不该把整篇正文
    /// 拖进列表接口 —— 摘要在 SQL 侧截断，避免把大文本搬过 IPC。
    pub excerpt: String,
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
    // 用系统时间换算 UTC 毫秒。项目约定所有时间存 UTC 毫秒（内部调研资料，未随仓库分发）
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Db {
    /// 打开（必要时创建）数据库文件，并执行迁移。
    ///
    /// `data_dir` 是用户数据目录（`<数据目录>/data/main.db` 才是库文件），
    /// 升级前的自动备份会写到 `<数据目录>/backups/`。
    pub fn open(data_dir: &Path, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据目录失败 {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
        let db = Db { conn };
        db.apply_durability_pragmas()?;
        db.migrate(Some(data_dir))?;
        Ok(db)
    }

    /// 只为测试用的内存库
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| format!("打开内存库失败: {e}"))?;
        let db = Db { conn };
        db.conn
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| e.to_string())?;
        db.migrate(None)?;
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
    /// 设计要点（见交接清单「内容也能够自动升级」）：
    ///   - 建库（user_version == 0）先落 v1 基线，再从 1 一路升到 `SCHEMA_VERSION`；
    ///   - 每升一级，**先**用 `VACUUM INTO` 把当前库整份备份到
    ///     `<数据目录>/backups/before-v<N>-<时间戳>.db`，备份失败直接中止，绝不带着
    ///     “可能成功”的侥幸往下走；
    ///   - 每级迁移都在一个事务里完成，失败自动回滚，原库结构版本不变；
    ///   - 版本高于本程序支持的，明确报错，绝不静默继续。
    ///
    /// `data_dir` 为 `None` 时（仅测试用的内存库）跳过备份——内存库没有「原库」可保。
    fn migrate(&self, data_dir: Option<&Path>) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("读取结构版本失败: {e}"))?;

        if current > SCHEMA_VERSION {
            return Err(format!(
                "数据文件的结构版本（{current}）高于本程序支持的版本（{SCHEMA_VERSION}）。\
                 请升级 DeskBase 后再打开，避免数据损坏。"
            ));
        }

        // 初始建库：先落 v1 基线（把 user_version 设成 1），再从 1 逐级升到最新。
        let mut v = if current == 0 {
            self.migrate_to_v1()?;
            1
        } else {
            current
        };

        while v < SCHEMA_VERSION {
            let next = v + 1;
            if let Some(dd) = data_dir {
                if let Err(e) = self.backup_before_upgrade(dd, next) {
                    return Err(format!(
                        "升级到 v{next} 前的自动备份失败，已中止迁移以保护原库：{e}"
                    ));
                }
            }
            if let Err(e) = self.upgrade_to(next) {
                let hint = data_dir
                    .map(|dd| dd.join("backups").to_string_lossy().to_string())
                    .unwrap_or_else(|| "<数据目录>/backups".into());
                return Err(format!(
                    "数据库结构升级失败（v{v}→v{next}）：{e}。原库未被改动，备份在 {hint}，\
                     请用上一个版本的 DeskBase 打开或恢复备份。"
                ));
            }
            v = next;
        }
        Ok(())
    }

    /// 升级前自动备份：把当前库整份复制一份到 `<数据目录>/backups/`。
    ///
    /// 用 `VACUUM INTO`（SQLite 3.27+，rusqlite 直接 `execute` 即可）得到一份
    /// 干净的独立副本，而不是文件系统级的文件拷贝——后者在 WAL 模式下会漏掉 -wal。
    fn backup_before_upgrade(&self, data_dir: &Path, target: i64) -> Result<()> {
        let backup_dir = data_dir.join("backups");
        std::fs::create_dir_all(&backup_dir)
            .map_err(|e| format!("创建备份目录失败: {e}"))?;
        let stamp = OffsetDateTime::now_utc()
            .format(&format_description!(
                "[year][month][day]-[hour][minute][second]"
            ))
            .unwrap_or_else(|_| "backup".into());
        let backup_path = backup_dir.join(format!("before-v{target}-{stamp}.db"));
        // VACUUM INTO 的目标文件名是 SQL 字符串字面量，路径里的单引号必须转义
        let lit = backup_path.to_string_lossy().replace('\'', "''");
        self.conn
            .execute_batch(&format!("VACUUM INTO '{lit}'"))
            .map_err(|e| format!("升级前备份失败（目标 {}）：{e}", backup_path.display()))?;
        Ok(())
    }

    /// 把库升级到指定版本（仅 2..=SCHEMA_VERSION）。每级一个事务，结束写回 user_version。
    fn upgrade_to(&self, target: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        match target {
            // v2：笔记增加「置顶」标记。列带默认值，对已有行零侵入。
            2 => tx
                .execute_batch("ALTER TABLE note ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;")
                .map_err(|e| format!("v2 迁移失败（新增 pinned 列）: {e}"))?,
            other => return Err(format!("未知的目标结构版本 {other}")),
        }
        tx.pragma_update(None, "user_version", target)
            .map_err(|e| format!("写入结构版本失败: {e}"))?;
        tx.commit().map_err(|e| format!("提交迁移失败: {e}"))?;
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
        .map_err(|e| format!("新建表格失败: {e}"))?;
        tx.pragma_update(None, "user_version", 1)
            .map_err(|e| format!("写入结构版本失败: {e}"))?;
        tx.commit().map_err(|e| format!("提交迁移失败: {e}"))?;
        Ok(())
    }

    // ---------- 笔记 ----------

    /// 只读借用底层连接。
    ///
    /// 为什么需要它：`import_pipeline` 的恢复查询（`pending_jobs` /
    /// `recovery_notice`）只读元数据表，不该为了它们在本模块里再包一层。
    /// **刻意只给 `&Connection`** —— 想要 `&mut` 就得让 `Db` 自己开口子，
    /// 而那意味着任何人都能绕过 `note` 表的 id / 时间戳规则。
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// 可变借用底层连接。
    ///
    /// 为什么必须开这个口子：`schema::insert_rows` 出于事务安全的理由要求
    /// `&mut Connection`（rusqlite 的事务 API 需要）。这是目前唯一一个
    /// 合法的 `&mut` 使用方 —— 不要用这个口子绕开 `note` 表的
    /// id / 时间戳规则，那类写入必须走 `create_note` / `save_note`。
    pub fn conn_mut(&mut self) -> &mut rusqlite::Connection {
        &mut self.conn
    }

    pub fn list_notes(&self) -> Result<Vec<NoteSummary>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, title, updated_at,
                        trim(replace(replace(substr(content, 1, 240), char(10), ' '), char(13), ' '))
                 FROM note
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
                    excerpt: r.get(3)?,
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

    #[test]
    fn 列表摘要会把换行压成空格并截断() {
        let db = Db::open_in_memory().unwrap();
        let n = db.create_note("带正文").unwrap();
        let long = "第一行\n第二行\r\n".to_string() + &"很长".repeat(200);
        db.save_note(&n.id, "带正文", &long).unwrap();

        let list = db.list_notes().unwrap();
        assert_eq!(list.len(), 1);
        let ex = &list[0].excerpt;
        assert!(!ex.contains('\n'), "摘要里不该有换行：{ex:?}");
        assert!(!ex.contains('\r'), "摘要里不该有回车：{ex:?}");
        assert!(ex.starts_with("第一行 第二行"), "实际：{ex:?}");
        // SQL 侧按字符截到 240，加上压平后的空格也不会失控
        assert!(ex.chars().count() <= 260, "摘要太长了：{}", ex.chars().count());
    }

    #[test]
    fn 空笔记的摘要为空串而不是缺失() {
        let db = Db::open_in_memory().unwrap();
        db.create_note("空").unwrap();
        let list = db.list_notes().unwrap();
        assert_eq!(list[0].excerpt, "");
    }

    // ---------- 数据自动升级框架（交接清单「内容也能够自动升级」）----------

    /// 造一个临时数据目录 + 库文件路径，返回 (data_dir, db_path)。
    fn tmp_db_dir(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let data_dir = std::env::temp_dir()
            .join(format!("deskbase_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(data_dir.join("data")).unwrap();
        let db_path = data_dir.join("data").join("main.db");
        (data_dir, db_path)
    }

    #[test]
    fn v1升到v2_数据不丢且生成备份() {
        let (data_dir, db_path) = tmp_db_dir("upgrade_v1v2");
        // 手工造一个 v1 库
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sys_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE note (id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', content TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER);
                 INSERT INTO note (id, title, content, created_at, updated_at) VALUES ('n1','标题','正文',1,1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        // 用新代码打开 → 应自动升级到 v2
        let db = Db::open(&data_dir, &db_path).unwrap();
        let got = db.get_note("n1").unwrap().unwrap();
        assert_eq!(got.title, "标题");
        assert_eq!(got.content, "正文");

        // 结构版本已到 2
        let ver: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 2);

        // pinned 列已存在且默认 0
        let pinned: i64 = db
            .conn()
            .query_row("SELECT pinned FROM note WHERE id='n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pinned, 0);

        // 升级前备份已生成
        let backups = std::fs::read_dir(data_dir.join("backups")).unwrap();
        let names: Vec<String> = backups
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("before-v2-")),
            "没找到升级备份，实际：{:?}",
            names
        );
    }

    #[test]
    fn 升级失败_原库未被破坏() {
        let (data_dir, db_path) = tmp_db_dir("upgrade_fail");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            // 预先造一个 v1 库，但已经带 pinned 列 —— 让 v2 的 ALTER 失败
            conn.execute_batch(
                "CREATE TABLE sys_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE note (id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', content TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER, pinned INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO note (id, title, content, created_at, updated_at) VALUES ('n1','标题','正文',1,1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        // 打开应失败（v2 迁移 ALTER 重复列）
        let res = Db::open(&data_dir, &db_path);
        assert!(res.is_err(), "升级失败却没报错");

        // 原库仍可用、数据还在、版本仍是 1
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 1, "升级失败不应改写 user_version");
        let title: String = conn
            .query_row("SELECT title FROM note WHERE id='n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "标题");
        let content: String = conn
            .query_row("SELECT content FROM note WHERE id='n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(content, "正文");
    }

    #[test]
    fn 版本高于程序支持_报错且不破坏() {
        let (data_dir, db_path) = tmp_db_dir("too_new");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sys_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 PRAGMA user_version = 999;",
            )
            .unwrap();
        }
        let res = Db::open(&data_dir, &db_path);
        assert!(res.is_err(), "版本过高却没报错");

        // 原库仍能打开，user_version 不变
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 999);
    }

    #[test]
    fn 数据目录不在程序目录内_覆盖更新不带走数据() {
        // 根本前提：用户数据目录不落在可执行文件所在目录内，
        // 这样「覆盖 exe / 解压新便携包」更新时，数据不会被一起替换掉。
        let data_dir = default_data_dir();
        let exe = std::env::current_exe().expect("拿不到当前 exe 路径");
        let exe_dir = exe.parent().expect("exe 没有父目录");
        assert!(
            !data_dir.starts_with(exe_dir),
            "数据目录 {} 落在了程序目录 {} 内——覆盖式更新会连同用户数据一起被替换",
            data_dir.display(),
            exe_dir.display()
        );
    }
}
