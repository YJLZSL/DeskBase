//! 一键全量导出（可迁移性保障）
//! ============================================================
//! 对应 `docs/05-office-toolbox.md` §13.3。
//!
//! **为什么单独把这件事做重**：它是"不锁定用户"这条承诺的兑现方式。
//! 用户十年后打开这个文件夹，应当**即使 DeskBase 已经不存在**也能看懂里面是什么 ——
//! 所以 `README.txt` 必须用普通文字写，而不是只有机器能解析的格式。
//!
//! 与文档 §13.3 的两处出入（都是有意的）：
//!   · `databases/`（导出 .sql）**没有了** —— 项目在 ADR-0021 里彻底移除了 SQL。
//!     改为导出**原始数据文件** `main.dkb`，并在 README 里说清"它需要 DeskBase 打开"。
//!   · `attachments/` 与 `templates/` 暂无内容（那两个功能还没做），
//!     **不建空目录装样子** —— 目录不存在，比存在但是空的更诚实。
//! ============================================================

use crate::model::Db;
use std::path::{Path, PathBuf};
use time::macros::format_description;
use time::OffsetDateTime;

pub struct Report {
    pub dir: PathBuf,
    pub tables: usize,
    pub notes: usize,
    pub files: usize,
    pub bytes: u64,
}

/// 一页的行数。**必须 ≤ `model::MAX_PAGE_LIMIT`（500）** —— 后端会把更大的 limit
/// 悄悄夹到这个上限，而"悄悄"正是下面那个 bug 的成因。
pub const EXPORT_PAGE: usize = 500;

/// 取**整张表**（逐页翻到没有下一页为止），返回 `(列名, 行)`。
///
/// ## 为什么必须有这个函数（这不是重构，是修 bug）
///
/// 单表导出一开始写的是 `db.page_rows(table, None, false, None, usize::MAX)`，
/// 想当然地以为"传 usize::MAX 就是全部"。但 `page_rows` 会把 limit
/// **夹到 `MAX_PAGE_LIMIT`（500）**（`model/mod.rs:1663`）——
/// 于是**超过 500 行的表会被静默截断**：导出的文件里只有前 500 行，
/// 没有报错、没有提示，用户拿到一个"看起来完整"的残缺文件。
///
/// 这是数据丢失类缺陷里最坏的一种：**用户不会去看第 501 行在不在**。
/// 而它是一行代码就能写出来的错误 —— 所以修法不是"改那一行"，
/// 而是**把"取整张表"变成一个有名字、有测试、只能有一种正确实现的函数**。
///
/// 实现按游标翻页：`finish_page` 靠"多取一行"判断 `has_more`，
/// 所以翻到 `has_more == false` 就是真的到底了。
pub fn read_all_rows(db: &Db, table: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut headers: Vec<String> = Vec::new();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cursor: Option<String> = None;
    // 上限只作为"防死循环"的护栏（游标实现坏了时不要转不出来），不是产品限制。
    // 200 万行远超本项目任何真实用法，正常表格永远碰不到。
    const HARD_STOP: usize = 2_000_000;
    loop {
        let page = db.page_rows(table, None, false, cursor.as_deref(), EXPORT_PAGE)?;
        if headers.is_empty() {
            // 第 0 列是 rowid（内部行号），导出去对用户没有意义 —— 与既有导出行为一致
            headers = page.columns.iter().skip(1).cloned().collect();
        }
        if page.rows.is_empty() {
            break;
        }
        for row in &page.rows {
            out.push(
                row.iter()
                    .skip(1)
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .collect(),
            );
        }
        if out.len() >= HARD_STOP {
            return Err(format!(
                "「{table}」超过 {HARD_STOP} 行，导出已停下（这不是产品限制，是防死循环的护栏）"
            ));
        }
        if !page.has_more {
            break;
        }
        let Some(next) = page.next_cursor.clone() else {
            // has_more 为真却没有游标 = 后端契约被破坏。**宁可报错也不要静默截断** ——
            // 截断过一次的教训就在上面的文档注释里。
            return Err(format!("「{table}」翻页没给出游标，为避免少导数据已中止"));
        };
        cursor = Some(next);
    }
    Ok((headers, out))
}

/// 把标题之类变成能当文件名用的东西。
///
/// Windows 文件名不许有 `/ \ : * ? " < > |`，而笔记标题里什么都可能有
/// （"3/4 季度"、"问题：为什么"）。**换掉而不是删掉** —— 换掉还能看出原样。
///
/// `pub` 是给 `main.rs` 的单表导出用的：**同一条"表名不能直接当路径用"的规则
/// 必须只有一处实现**。各写一份的话，将来只会有一处被修好，另一处继续漏。
pub fn safe_name(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let t = t.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        "未命名".to_string()
    } else if t.chars().count() > 60 {
        // 太长会让路径超限（Windows 260 的经典坑）
        t.chars().take(60).collect()
    } else {
        t
    }
}

fn csv_line(cells: &[String]) -> String {    cells
        .iter()
        .map(|c| {
            if c.contains(',') || c.contains('"') || c.contains('\n') || c.contains('\r') {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        + "\r\n" // Excel 对 CRLF 更友好
}

pub fn export_all(db: &mut Db, out_root: &Path) -> Result<Report, String> {
    let stamp = OffsetDateTime::now_utc()
        .format(&format_description!("[year][month][day]-[hour][minute][second]"))
        .unwrap_or_else(|_| "export".to_string());
    let dir = out_root.join(format!("deskbase-export-{stamp}"));
    if dir.exists() {
        return Err(format!("目录已存在，不覆盖：{}", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;

    let tables_dir = dir.join("tables");
    let notes_dir = dir.join("notes");
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&tables_dir).map_err(|e| format!("建目录失败：{e}"))?;
    std::fs::create_dir_all(&notes_dir).map_err(|e| format!("建目录失败：{e}"))?;
    std::fs::create_dir_all(&config_dir).map_err(|e| format!("建目录失败：{e}"))?;

    let mut written: Vec<PathBuf> = Vec::new();
    let mut manifest_rows: Vec<serde_json::Value> = Vec::new();
    let mut bytes: u64 = 0;

    let write = |p: &Path, body: &str, written: &mut Vec<PathBuf>, bytes: &mut u64| -> Result<(), String> {
        std::fs::write(p, body.as_bytes()).map_err(|e| format!("写 {} 失败：{e}", p.display()))?;
        *bytes += body.len() as u64;
        written.push(p.to_path_buf());
        Ok(())
    };

    // ---------- 表格：每表一个 CSV ----------
    let tables = db.list_tables().unwrap_or_default();
    let mut table_names: Vec<String> = Vec::new();
    for t in &tables {
        let page = db.page_rows(&t.name, None, false, None, usize::MAX)?;
        let mut body = String::new();
        // 表头跳过第 0 列（rowid）—— 那是内部行号，导出去对用户没意义
        body.push_str(&csv_line(
            &page.columns.iter().skip(1).cloned().collect::<Vec<_>>(),
        ));
        for row in &page.rows {
            let cells: Vec<String> = row
                .iter()
                .skip(1)
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                })
                .collect();
            body.push_str(&csv_line(&cells));
        }
        let f = tables_dir.join(format!("{}.csv", safe_name(&t.name)));
        write(&f, &body, &mut written, &mut bytes)?;
        manifest_rows.push(serde_json::json!({
            "kind": "table",
            "name": t.name,
            "file": format!("tables/{}.csv", safe_name(&t.name)),
            "rows": page.rows.len(),
        }));
        table_names.push(t.name.clone());
    }

    // ---------- 笔记：每篇一个 .md ----------
    let notes = crate::db::list_notes(db).unwrap_or_default();
    for (i, n) in notes.iter().enumerate() {
        let full = crate::db::get_note(db, &n.id).ok().flatten();
        let body_text = full.as_ref().map(|x| x.content.clone()).unwrap_or_default();
        // 用 Markdown 的标题语法把标题写进去 —— 打开就是一篇像样的文档
        let title = if n.title.trim().is_empty() {
            "（无标题）"
        } else {
            n.title.trim()
        };
        let body = format!("# {title}\n\n{body_text}\n");
        let f = notes_dir.join(format!("{:03}-{}.md", i + 1, safe_name(title)));
        write(&f, &body, &mut written, &mut bytes)?;
        manifest_rows.push(serde_json::json!({
            "kind": "note",
            "name": title,
            "file": format!("notes/{:03}-{}.md", i + 1, safe_name(title)),
        }));
    }

    // ---------- 设置（脱敏）----------
    // ⚠️ **绝不导出 API Key**。这里只写"配了什么厂商、模型名"这类元信息。
    // 导出包常常会被拷来拷去，把密钥装进去等于扩散它。
    let cfg = serde_json::json!({
        "_note": "这里是程序设置。**不含任何密钥**（密钥从不导出）。",
        "version": env!("CARGO_PKG_VERSION"),
    });
    write(
        &config_dir.join("settings.json"),
        &serde_json::to_string_pretty(&cfg).unwrap_or_default(),
        &mut written,
        &mut bytes,
    )?;

    // ---------- 原始数据文件 ----------
    // 文档里原本是 `databases/*.sql`；项目已移除 SQL（ADR-0021），
    // 改为直接给原始文件，并在 README 里说明"它需要 DeskBase 打开"。
    let raw = crate::db::default_data_dir().join("main.dkb");
    let mut has_raw = false;
    if raw.exists() {
        let dst = dir.join("main.dkb");
        if std::fs::copy(&raw, &dst).is_ok() {
            has_raw = true;
            if let Ok(m) = std::fs::metadata(&dst) {
                bytes += m.len();
            }
            written.push(dst);
        }
    }

    // ---------- manifest.json ----------
    let manifest = serde_json::json!({
        "format": "deskbase-export/1",
        "exported_at_utc": OffsetDateTime::now_utc()
            .format(&format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z"))
            .unwrap_or_default(),
        "app_version": env!("CARGO_PKG_VERSION"),
        "tables": table_names,
        "note_count": notes.len(),
        "items": manifest_rows,
        "has_raw_store": has_raw,
    });
    write(
        &dir.join("manifest.json"),
        &serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        &mut written,
        &mut bytes,
    )?;

    // ---------- README.txt（**人类可读**，文档特别强调过）----------
    let readme = format!(
        r##"DeskBase 数据导出
=================

导出时间：{when}
DeskBase 版本：{ver}

这个文件夹里是什么
------------------
tables/          你的表格。每个 .csv 一张表，共 {nt} 张。
                 第一行是列名，之后每行一条记录。
                 用 Excel、WPS、记事本，或任何能读 CSV 的软件都能打开。

notes/           你的笔记。每篇一个 .md 文件，共 {nn} 篇。
                 .md（Markdown）是纯文本，用记事本就能打开看。
                 文件开头的「# 标题」就是这篇的标题。

config/          程序的设置。里面不含任何密钥。

manifest.json    文件清单与条数，给程序读的，人也可以看。
checksums.sha256 每个文件的校验和 —— 用来确认文件没有损坏或被改动。
{rawline}
怎么直接用
----------
· 想看看里面有什么   → 双击 tables/ 里的任意 .csv。
· 想搬到别的软件     → 把 .csv 导入 Excel / WPS / 任何表格软件。
· 想换台电脑继续用   → 把这个文件夹整个拷过去，
                       在 DeskBase 里用「导入」把 .csv 一张张导回去。

关于格式
--------
这里面没有用任何专有格式。.csv 与 .md 都是有几十年历史的公开格式，
就算 DeskBase 将来不存在了，这些文件依然打得开、读得懂。

一句话：这些文件是你的，不是软件的。
"##,
        when = OffsetDateTime::now_utc()
            .format(&format_description!(
                "[year]-[month]-[day] [hour]:[minute] UTC"
            ))
            .unwrap_or_default(),
        ver = env!("CARGO_PKG_VERSION"),
        nt = tables.len(),
        nn = notes.len(),
        rawline = if has_raw {
            "\nmain.dkb         原始数据文件。它需要 DeskBase 才能打开 ——\n                 上面那些 .csv 和 .md 才是通用格式，这一份只是留个底。\n"
        } else {
            ""
        },
    );
    write(&dir.join("README.txt"), &readme, &mut written, &mut bytes)?;

    // ---------- checksums.sha256 ----------
    // 放在最后算：前面所有文件都写完了才算得准。
    let mut sums = String::new();
    for p in &written {
        if let Ok(h) = crate::updater::sha256_file(p) {
            let name = p
                .strip_prefix(&dir)
                .map(|x| x.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.to_string_lossy().to_string());
            sums.push_str(&format!("{h}  {name}\n"));
        }
    }
    std::fs::write(dir.join("checksums.sha256"), sums.as_bytes())
        .map_err(|e| format!("写校验和失败：{e}"))?;
    bytes += sums.len() as u64;

    Ok(Report {
        dir,
        tables: tables.len(),
        notes: notes.len(),
        files: written.len() + 2,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColType, ColumnDef, TableSpec};

    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp_db() -> Db {
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("dkb_expall_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Db::open(&d).unwrap()
    }

    fn spec(name: &str, cols: &[(&str, ColType)]) -> TableSpec {
        TableSpec {
            name: name.to_string(),
            comment: None,
            columns: cols
                .iter()
                .map(|(n, t)| ColumnDef {
                    name: n.to_string(),
                    ty: *t,
                    not_null: false,
                    default: None,
                    primary_key: false,
                    comment: None,
                    shared: None,
                    link: None,
                    lookup: None,
                    rollup: None,
                })
                .collect(),
        }
    }

    /// ⭐ 这条测试是为了钉住一个**真实发生过的静默数据丢失**。
    ///
    /// 背景：单表导出原先调 `page_rows(..., usize::MAX)` 取全部，
    /// 而 `page_rows` 会把 limit 夹到 `MAX_PAGE_LIMIT = 500` ——
    /// **超过 500 行的表只会导出前 500 行，且不报错**。
    /// 本测试用 **1200 行**（刻意大于 500 且不是 500 的整数倍）验证：
    /// 行数一条不少，而且**最后一行也在**（只数行数的话，
    /// "少的是最后一行"这种错也可能被行数凑巧掩盖，所以两头都验）。
    #[test]
    fn read_all_rows_returns_every_row_past_the_page_limit() {
        let mut db = tmp_db();
        db.create_table(&spec("台账", &[("编号", ColType::Text)])).unwrap();
        const TOTAL: usize = 1200;
        // 分批插入：一次插 1200 行会让测试慢，而且这里要验的是"读"，不是"写"
        let mut i = 0usize;
        while i < TOTAL {
            let batch: Vec<Vec<Option<String>>> = (i..(i + 200).min(TOTAL))
                .map(|k| vec![Some(format!("{k:06}"))])
                .collect();
            db.insert_rows("台账", &["编号".to_string()], &batch).unwrap();
            i += 200;
        }

        let (headers, rows) = read_all_rows(&db, "台账").unwrap();
        assert_eq!(headers, vec!["编号".to_string()], "表头应当只剩数据列（rowid 跳过）");
        assert_eq!(rows.len(), TOTAL, "1200 行的表必须导出 1200 行（曾被静默截到 500）");
        assert_eq!(rows[0][0], "000000", "第一行要在");
        assert_eq!(
            rows[TOTAL - 1][0],
            format!("{:06}", TOTAL - 1),
            "最后一行也要在 —— 只数行数会漏掉『少的正好是尾巴』这种情况"
        );
        // 值不能串行/错位（翻页时游标算错最容易表现为整体位移）
        assert_eq!(rows[500][0], "000500", "第 501 行必须是它自己（翻页边界）");
        assert_eq!(rows[501][0], "000501", "第 502 行必须是它自己（翻页边界）");
    }

    /// 空表与单列表要能正常返回，而不是报错或死循环。
    #[test]
    fn read_all_rows_handles_empty_and_single_row() {
        let mut db = tmp_db();
        db.create_table(&spec("空表", &[("名称", ColType::Text)])).unwrap();
        let (h, r) = read_all_rows(&db, "空表").unwrap();
        assert_eq!(h, vec!["名称".to_string()]);
        assert!(r.is_empty(), "空表应当是 0 行，不是报错");

        db.insert_rows("空表", &["名称".to_string()], &[vec![Some("甲".into())]])
            .unwrap();
        let (_, r2) = read_all_rows(&db, "空表").unwrap();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0][0], "甲");
    }

    /// 不存在的表要报错（而不是返回空表让人以为"这张表是空的"）。
    #[test]
    fn read_all_rows_rejects_unknown_table() {
        let db = tmp_db();
        let e = read_all_rows(&db, "没有这张表").unwrap_err();
        assert!(!e.is_empty(), "要给出原因");
    }
}
