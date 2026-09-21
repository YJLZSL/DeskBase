//! 工作区状态持久化
//!
//! 把「当前视图 / 打开的表 / 侧栏状态 / 窗口尺寸」这类**界面状态**（不是用户数据）
//! 存进自研存储的一个键值项（`sys/workspace`）。这样覆盖 exe 升级、崩溃、重开之后，
//! 用户回到的是离开时的样子 —— 这就是「更新时工作区不丢失」的落点。
//!
//! 与笔记 / 表数据走不同的键前缀，互不影响；本模块只认 `sys/workspace` 这一项。

use serde::{Deserialize, Serialize};

use crate::model::Db;

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

/// 写入工作区状态。走 store 的键值接口，一次提交即落盘（含 fsync）。
pub fn save(db: &mut Db, state: &WorkspaceState) -> Result<()> {
    let json = serde_json::to_string(state).map_err(|e| format!("序列化工作区状态失败: {e}"))?;
    db.meta_set(KEY, &json)
        .map_err(|e| format!("保存工作区状态失败: {e}"))?;
    Ok(())
}

/// 读回工作区状态。没有记录 → 安全默认；记录损坏 → 也回退默认（不让启动崩）。
pub fn load(db: &Db) -> Result<WorkspaceState> {
    match db.meta_get(KEY) {
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

    fn fresh(tag: &str) -> (std::path::PathBuf, Db) {
        let d = std::env::temp_dir()
            .join(format!("dkb_ws_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = Db::open(&d).unwrap();
        (d, db)
    }

    #[test]
    fn 保存后重开读到同一份() {
        let (d, mut db) = fresh("roundtrip");
        let st = WorkspaceState {
            view: "database".into(),
            active_table: "客户表".into(),
            sidebar: "rail".into(),
            window_w: 1280,
            window_h: 820,
        };
        save(&mut db, &st).unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got, st);
        // 换一个实例打开同一目录 —— 证明它真的落盘了
        drop(db);
        let db2 = Db::open(&d).unwrap();
        assert_eq!(load(&db2).unwrap(), st);
    }

    #[test]
    fn 缺字段给安全默认值() {
        let (_d, mut db) = fresh("partial");
        db.meta_set(KEY, "{\"view\":\"settings\"}").unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got.view, "settings");
        assert_eq!(got.active_table, "");
        assert_eq!(got.sidebar, "");
        assert_eq!(got.window_w, 1180);
        assert_eq!(got.window_h, 780);
    }

    #[test]
    fn 损坏的记录回退默认而不是崩() {
        let (_d, mut db) = fresh("corrupt");
        db.meta_set(KEY, "这不是 json").unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got, WorkspaceState::default());
    }

    #[test]
    fn 没有记录时给默认() {
        let (_d, db) = fresh("empty");
        let got = load(&db).unwrap();
        assert_eq!(got, WorkspaceState::default());
    }
}
