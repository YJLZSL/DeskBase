//! 笔记（首页的「随手记」）+ 数据目录解析
//!
//! 原来这一层是整个 SQLite 的门面（`Db` + 迁移框架），现在**门面已经换成
//! `crate::model::Db`**（见 docs/adr/0021），本文件只留下两样东西：
//!
//!   1. 笔记的读写 —— 存进自研存储的 `note/<ulid>` 前缀下；
//!   2. [`default_data_dir`] —— 决定用户数据放哪儿。
//!
//! 迁移框架整体删除：新引擎里表结构就是 JSON，字段增删是模型层面的事，
//! 不需要 `ALTER TABLE`，也不需要"升级前先 VACUUM 一份"。
//! 但**「不丢数据」的承诺没变**：崩溃安全交给 store 的日志 + CRC + fsync 保证。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::Db;

pub type Result<T> = std::result::Result<T, String>;

/// 列表里的一条笔记。
#[derive(Debug, Clone, Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
    /// 正文摘要（前若干字符）。列表要能按内容搜索与筛选，又不该把整篇正文
    /// 拖进列表接口 —— 摘要在 Rust 侧截断。
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// 软删除时间戳（进回收站，见 docs/adr/0013）
    #[serde(default)]
    pub deleted_at: Option<i64>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn key(id: &str) -> String {
    format!("note/{id}")
}

fn load_all(db: &Db) -> Vec<Note> {
    db.store()
        .scan("note/")
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<Note>(&v).ok())
        .filter(|n| n.deleted_at.is_none())
        .collect()
}

/// 摘取摘要：压平换行、截到 240 字符。
///
/// 与旧实现（SQL 侧 `substr` + `replace`）行为保持一致 —— 列表不该出现换行。
fn excerpt_of(content: &str) -> String {
    let flat: String = content
        .chars()
        .take(240)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    flat.trim().to_string()
}

pub fn list_notes(db: &Db) -> Result<Vec<NoteSummary>> {
    let mut all = load_all(db);
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
    Ok(all
        .into_iter()
        .map(|n| NoteSummary {
            id: n.id,
            title: n.title,
            updated_at: n.updated_at,
            excerpt: excerpt_of(&n.content),
        })
        .collect())
}

pub fn get_note(db: &Db, id: &str) -> Result<Option<Note>> {
    match db.store().get(&key(id)) {
        None => Ok(None),
        Some(raw) => {
            let n: Note = serde_json::from_str(raw)
                .map_err(|e| format!("笔记「{id}」读不出来: {e}"))?;
            if n.deleted_at.is_some() {
                Ok(None)
            } else {
                Ok(Some(n))
            }
        }
    }
}

pub fn create_note(db: &mut Db, title: &str) -> Result<Note> {
    let id = ulid::Ulid::generate().to_string();
    let ts = now_ms();
    let n = Note {
        id: id.clone(),
        title: title.to_string(),
        content: String::new(),
        created_at: ts,
        updated_at: ts,
        deleted_at: None,
    };
    let raw = serde_json::to_string(&n).map_err(|e| format!("新建笔记失败: {e}"))?;
    db.store_mut().put(key(&id), raw)?;
    Ok(n)
}

pub fn save_note(db: &mut Db, id: &str, title: &str, content: &str) -> Result<i64> {
    let raw = db
        .store()
        .get(&key(id))
        .ok_or_else(|| format!("笔记不存在或已删除: {id}"))?
        .to_string();
    let mut n: Note = serde_json::from_str(&raw).map_err(|e| format!("笔记读不出来: {e}"))?;
    if n.deleted_at.is_some() {
        return Err(format!("笔记不存在或已删除: {id}"));
    }
    let ts = now_ms();
    n.title = title.to_string();
    n.content = content.to_string();
    n.updated_at = ts;
    let out = serde_json::to_string(&n).map_err(|e| format!("保存笔记失败: {e}"))?;
    db.store_mut().put(key(id), out)?;
    Ok(ts)
}

/// 软删除，进回收站（见 docs/adr/0013）
pub fn delete_note(db: &mut Db, id: &str) -> Result<()> {
    let raw = db
        .store()
        .get(&key(id))
        .ok_or_else(|| format!("笔记不存在: {id}"))?
        .to_string();
    let mut n: Note = serde_json::from_str(&raw).map_err(|e| format!("笔记读不出来: {e}"))?;
    if n.deleted_at.is_some() {
        return Err(format!("笔记不存在: {id}"));
    }
    let ts = now_ms();
    n.deleted_at = Some(ts);
    n.updated_at = ts;
    let out = serde_json::to_string(&n).map_err(|e| format!("删除笔记失败: {e}"))?;
    db.store_mut().put(key(id), out)?;
    Ok(())
}

pub fn count_notes(db: &Db) -> Result<i64> {
    Ok(load_all(db).len() as i64)
}

/// 数据目录解析（先用户目录；便携版逻辑后续接入）
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

    fn fresh(tag: &str) -> (std::path::PathBuf, Db) {
        let d = std::env::temp_dir().join(format!("dkb_note_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = Db::open(&d).unwrap();
        (d, db)
    }

    #[test]
    fn note_read_back_after_create() {
        let (_d, mut db) = fresh("basic");
        let n = create_note(&mut db, "测试").unwrap();
        assert_eq!(n.title, "测试");
        let got = get_note(&db, &n.id).unwrap().unwrap();
        assert_eq!(got.content, "");
        assert_eq!(count_notes(&db).unwrap(), 1);
    }

    #[test]
    fn save_updates_content_and_timestamp() {
        let (_d, mut db) = fresh("save");
        let n = create_note(&mut db, "").unwrap();
        let ts = save_note(&mut db, &n.id, "标题", "正文").unwrap();
        let got = get_note(&db, &n.id).unwrap().unwrap();
        assert_eq!(got.title, "标题");
        assert_eq!(got.content, "正文");
        assert!(ts >= got.created_at);
    }

    #[test]
    fn delete_is_soft_list_hides_record_remains() {
        let (_d, mut db) = fresh("del");
        let n = create_note(&mut db, "待删").unwrap();
        delete_note(&mut db, &n.id).unwrap();
        assert_eq!(count_notes(&db).unwrap(), 0);
        assert!(get_note(&db, &n.id).unwrap().is_none());
    }

    #[test]
    fn saving_missing_note_errors_not_silent() {
        let (_d, mut db) = fresh("missing");
        assert!(save_note(&mut db, "不存在", "x", "y").is_err());
    }

    #[test]
    fn chinese_title_and_content_roundtrip() {
        let (_d, mut db) = fresh("cjk");
        let n = create_note(&mut db, "会议记录").unwrap();
        save_note(&mut db, &n.id, "会议记录", "讨论了「宣纸主题」的落地。\n第二行。")
            .unwrap();
        let got = get_note(&db, &n.id).unwrap().unwrap();
        assert!(got.content.contains("宣纸主题"));
        assert!(got.content.contains('\n'));
    }

    #[test]
    fn list_excerpt_flattens_newlines_and_truncates() {
        let (_d, mut db) = fresh("excerpt");
        let n = create_note(&mut db, "带正文").unwrap();
        let long = "第一行\n第二行\r\n".to_string() + &"很长".repeat(200);
        save_note(&mut db, &n.id, "带正文", &long).unwrap();

        let list = list_notes(&db).unwrap();
        assert_eq!(list.len(), 1);
        let ex = &list[0].excerpt;
        assert!(!ex.contains('\n'), "摘要里不该有换行：{ex:?}");
        assert!(!ex.contains('\r'), "摘要里不该有回车：{ex:?}");
        assert!(ex.starts_with("第一行 第二行"), "实际：{ex:?}");
        assert!(ex.chars().count() <= 240, "摘要太长了：{}", ex.chars().count());
    }

    #[test]
    fn empty_note_excerpt_is_empty_string() {
        let (_d, mut db) = fresh("emptyex");
        create_note(&mut db, "空").unwrap();
        let list = list_notes(&db).unwrap();
        assert_eq!(list[0].excerpt, "");
    }

    #[test]
    fn note_survives_reopen() {
        let (d, mut db) = fresh("persist");
        let n = create_note(&mut db, "持久").unwrap();
        save_note(&mut db, &n.id, "持久", "内容").unwrap();
        drop(db);
        let db2 = Db::open(&d).unwrap();
        assert_eq!(count_notes(&db2).unwrap(), 1);
        assert_eq!(get_note(&db2, &n.id).unwrap().unwrap().content, "内容");
    }

    #[test]
    fn data_dir_not_inside_program_dir() {
        // 覆盖 exe / 解压新便携包更新时，数据不能被一起替换掉
        let data_dir = default_data_dir();
        let exe = std::env::current_exe().expect("拿不到当前 exe 路径");
        let exe_dir = exe.parent().expect("exe 没有父目录");
        assert!(
            !data_dir.starts_with(exe_dir),
            "数据目录 {} 落在了程序目录 {} 内",
            data_dir.display(),
            exe_dir.display()
        );
    }
}
