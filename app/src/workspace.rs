//! 工作区状态持久化
//!
//! 把「当前视图 / 打开的表 / 侧栏状态 / 窗口尺寸」这类**界面状态**（不是用户数据）
//! 存进已有的 `sys_meta` 表，用一条 JSON 记录。这样覆盖 exe 升级、崩溃、重开之后，
//! 用户回到的是离开时的样子 —— 这就是「更新时工作区不丢失」的落点。
//!
//! 与笔记 / 库表数据走不同的 IPC、不同的表，互不影响；本模块只认 `sys_meta`。

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

const KEY: &str = "workspace";

/// 工作区状态。**所有字段都有安全默认值**（见 `Default`），缺字段或解析失败时
/// 不会让启动崩掉，只是回到一个合理的初始界面。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkspaceState {
    /// 当前视图：notes / database / workbench / settings
    pub view: String,
    /// 数据库页当前打开的表（可能为空）
    pub active_table: String,
    /// 侧栏状态：full / rail / hidden / open（空串表示交给前端自身规则）
    pub sidebar: String,
    /// 窗口内部宽高（像素）
    pub window_w: u32,
    pub window_h: u32,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        WorkspaceState {
            view: "notes".into(),
            active_table: String::new(),
            sidebar: String::new(),
            window_w: 1180,
            window_h: 780,
        }
    }
}

impl WorkspaceState {
    /// 缺失/非法字段收敛到安全默认，避免藏着的脏值让界面进异常状态。
    fn normalize(&mut self) {
        if self.view.is_empty() {
            self.view = "notes".into();
        }
        if self.window_w == 0 {
            self.window_w = 1180;
        }
        if self.window_h == 0 {
            self.window_h = 780;
        }
    }
}

/// 保存工作区状态进 `sys_meta`（UPSERT）。`conn` 只需 `&Connection`：写入走的是
/// 普通 INSERT/UPDATE，rusqlite 的 `execute` 本身接受 `&self`。
pub fn save(conn: &Connection, state: &WorkspaceState) -> Result<()> {
    let json = serde_json::to_string(state).map_err(|e| format!("序列化工作区状态失败: {e}"))?;
    conn.execute(
        "INSERT INTO sys_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![KEY, json],
    )
    .map_err(|e| format!("保存工作区状态失败: {e}"))?;
    Ok(())
}

/// 从 `sys_meta` 读回工作区状态。没有记录 → 安全默认；记录损坏 → 也回退默认
/// （不让启动崩）。
pub fn load(conn: &Connection) -> Result<WorkspaceState> {
    let json: Option<String> = conn
        .query_row(
            "SELECT value FROM sys_meta WHERE key = ?1",
            rusqlite::params![KEY],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| format!("读取工作区状态失败: {e}"))?;
    match json {
        None => Ok(WorkspaceState::default()),
        Some(s) => match serde_json::from_str::<WorkspaceState>(&s) {
            Ok(mut st) => {
                st.normalize();
                Ok(st)
            }
            Err(e) => {
                // 脏数据比「没有数据」更糟：宁可回到默认，也不把异常状态喂给界面
                log_warn(&format!("工作区状态解析失败，已回退默认：{e}"));
                Ok(WorkspaceState::default())
            }
        },
    }
}

#[cfg(not(test))]
fn log_warn(msg: &str) {
    // 本模块不依赖 main 的日志入口：工作区状态损坏很罕见，打到 stderr 即可。
    eprintln!("[deskbase] {msg}");
}

#[cfg(test)]
fn log_warn(_msg: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE sys_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn
    }

    #[test]
    fn 保存后重开读到同一份() {
        let conn = fresh_db();
        let st = WorkspaceState {
            view: "database".into(),
            active_table: "客户表".into(),
            sidebar: "rail".into(),
            window_w: 1280,
            window_h: 820,
        };
        save(&conn, &st).unwrap();
        let got = load(&conn).unwrap();
        assert_eq!(got, st);
    }

    #[test]
    fn 缺字段给安全默认值() {
        let conn = fresh_db();
        // 只写了 view，其余字段缺失
        conn.execute(
            "INSERT INTO sys_meta (key, value) VALUES ('workspace', '{\"view\":\"settings\"}')",
            [],
        )
        .unwrap();
        let got = load(&conn).unwrap();
        assert_eq!(got.view, "settings");
        assert_eq!(got.active_table, "");
        assert_eq!(got.sidebar, "");
        assert_eq!(got.window_w, 1180);
        assert_eq!(got.window_h, 780);
    }

    #[test]
    fn 损坏的记录回退默认而不是崩() {
        let conn = fresh_db();
        conn.execute(
            "INSERT INTO sys_meta (key, value) VALUES ('workspace', '这不是 json')",
            [],
        )
        .unwrap();
        let got = load(&conn).unwrap();
        assert_eq!(got, WorkspaceState::default());
    }

    #[test]
    fn 没有记录时给默认() {
        let conn = fresh_db();
        let got = load(&conn).unwrap();
        assert_eq!(got, WorkspaceState::default());
    }
}
