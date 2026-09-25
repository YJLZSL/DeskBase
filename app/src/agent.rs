//! 命令行 Agent 接口
//! ============================================================
//! **为什么要有它：让 AI 真的能干活。**
//!
//! 光给 AI 一份说明，它只能嘴上指导；要做到"导入导出、在应用里改数据"，
//! 就得给它一个**能执行东西的入口**。
//!
//! **为什么是命令行，不是本地 HTTP**：本项目"网络默认关闭"是红线，起一个监听
//! 端口（哪怕是回环）也和这条红线拧着，而且会引入一整套鉴权问题。命令行没有
//! 这个负担，任何 AI 工具（终端、脚本、MCP 的 command 通道）都能直接调。
//!
//! 用法见 `usage()` —— `deskbase agent` 不带参数就会打印它。
//! ============================================================

use crate::db;
use crate::model;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    db::default_data_dir()
}

/// 取一个 `--key value` 形式的参数。
fn opt(rest: &[String], key: &str) -> Option<String> {
    rest.iter()
        .position(|a| a == key)
        .and_then(|i| rest.get(i + 1))
        .cloned()
}

fn usage() -> &'static str {
    r#"用法：deskbase agent <子命令> [参数]

给 AI / 脚本用的命令行接口。所有输入输出都是 UTF-8 文本，
表格类命令输出 JSON，方便直接解析。

  context                          输出应用上下文（能力清单 + 当前状态），给 AI 读
  list-tables                      列出所有表（JSON）
  get-table  --name X [--rows N]   表的列名与数据（JSON）
  export     --table X --out F     导出成 CSV
  import     --file F --table X    从 CSV 导入（表不存在就建）
  add-row    --table X --data '{...}'        加一行（JSON 对象）
  update-cell --table X --rowid N --column C --value V   改一个格
  delete-row --table X --rowid N   删一行

说明：
  · 数据目录默认与界面版相同，可用环境变量 DESKBASE_DATA_DIR 覆盖。
  · **界面版正在运行时不要写**（存储引擎是单写者）。只读命令不受影响。
  · 所有写命令都会进变更历史，界面里能回退。
"#
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        eprint!("{}", usage());
        return 2;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return 0;
    }
    let cmd = args[0].clone();
    let rest: Vec<String> = args[1..].to_vec();
    let r = match cmd.as_str() {
        "context" => cmd_context(),
        "list-tables" => cmd_list_tables(),
        "get-table" => cmd_get_table(&rest),
        "export" => cmd_export(&rest),
        "import" => cmd_import(&rest),
        "add-row" => cmd_add_row(&rest),
        "update-cell" => cmd_update_cell(&rest),
        "delete-row" => cmd_delete_row(&rest),
        other => {
            eprintln!("不认识的子命令：{other}\n\n{}", usage());
            return 2;
        }
    };
    match r {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            0
        }
        Err(e) => {
            eprintln!("失败：{e}");
            1
        }
    }
}

fn open() -> Result<model::Db, String> {
    model::Db::open(&data_dir()).map_err(|e| format!("打不开数据库（{:?}）：{e}", data_dir()))
}

// ---------- context：给 AI 的说明书 + 当前状态 ----------
fn cmd_context() -> Result<String, String> {
    let mut d = open()?;
    let ts = d.list_tables().unwrap_or_default();
    let rows: i64 = ts.iter().map(|t| t.row_estimate).sum();
    let notes = db::count_notes(&mut d).unwrap_or(0);

    let mut table_lines = String::new();
    for t in &ts {
        let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
        table_lines.push_str(&format!("  · {}（{} 行）列：{}\n", t.name, t.row_estimate, cols.join("、")));
    }
    if table_lines.is_empty() {
        table_lines = "  （还没有表）\n".to_string();
    }

    Ok(format!(
        r#"# DeskBase 应用上下文（给 AI 读）

版本：v{ver}
数据目录：{dir}

## 这是什么
本地优先、隐私优先的桌面办公工具箱：自建表格（类似轻量数据库）+ 笔记 + 文档导出。
单文件存储、无第三方数据库依赖、网络默认关闭。

## 现在里面有什么
- 表格 {nt} 张、记录 {rows} 行、笔记 {notes} 篇
- 表清单：
{tables}
## 你能用这些命令操作它
deskbase agent list-tables            # 看有哪些表
deskbase agent get-table --name X     # 看某张表的列名和数据
deskbase agent export --table X --out F.csv   # 导出
deskbase agent import --file F.csv --table X  # 导入
deskbase agent add-row --table X --data '{{"列名":"值"}}'
deskbase agent update-cell --table X --rowid 1 --column 列名 --value 新值
deskbase agent delete-row --table X --rowid 1

## 数据模型要点（改之前先看）
- 一张表由若干**列**组成，列有类型（text / integer / real / money / date 等）。
- **关联字段**：某列可以指向另一张表，里面存的是目标行的行号。
- **共通字段**：多张表共用的字段，改一处会按同步规则联动。
- 每一行有 rowid（整数，来自 get-table 的第 0 列），改/删都用它定位。
- 写命令都会进变更历史，界面里可以回退。

## 约束
- 界面版正在运行时不要执行写命令（存储引擎是单写者）。
- 数据目录可用环境变量 DESKBASE_DATA_DIR 覆盖。
"#,
        ver = env!("CARGO_PKG_VERSION"),
        dir = data_dir().display(),
        nt = ts.len(),
        rows = rows,
        notes = notes,
        tables = table_lines,
    ))
}

// ---------- list-tables ----------
fn cmd_list_tables() -> Result<String, String> {
    let d = open()?;
    let ts = d.list_tables().unwrap_or_default();
    let arr: Vec<serde_json::Value> = ts
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "comment": t.comment,
                "rows": t.row_estimate,
                "columns": t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&serde_json::json!({ "tables": arr }))
        .unwrap_or_else(|_| "[]".to_string()))
}

// ---------- get-table ----------
fn cmd_get_table(rest: &[String]) -> Result<String, String> {
    let name = opt(rest, "--name").ok_or("缺少 --name")?;
    let n: usize = opt(rest, "--rows")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let d = open()?;
    let page = d.page_rows(&name, None, false, None, n)?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "table": name,
        "columns": page.columns,
        "rows": page.rows,
        "has_more": page.has_more,
    }))
    .unwrap_or_else(|_| "{}".to_string()))
}

// ---------- export ----------
fn cmd_export(rest: &[String]) -> Result<String, String> {
    let table = opt(rest, "--table").ok_or("缺少 --table")?;
    let out = opt(rest, "--out").ok_or("缺少 --out")?;
    let d = open()?;
    let page = d.page_rows(&table, None, false, None, usize::MAX)?;
    let mut w = String::new();
    // 表头：跳过第 0 列（rowid）
    w.push_str(&csv_line(&page.columns.iter().skip(1).cloned().collect::<Vec<_>>()));
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
        w.push_str(&csv_line(&cells));
    }
    let p = PathBuf::from(&out);
    std::fs::write(&p, w.as_bytes()).map_err(|e| format!("写文件失败：{e}"))?;
    Ok(format!("已导出 {} 行 → {}", page.rows.len(), p.display()))
}

/// 一行 CSV。含逗号/引号/换行的字段用引号包起来，内部引号翻倍。
fn csv_line(cells: &[String]) -> String {
    cells
        .iter()
        .map(|c| {
            if c.contains(',') || c.contains('"') || c.contains('\n') {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        + "\n"
}

// ---------- import ----------
fn cmd_import(rest: &[String]) -> Result<String, String> {
    let file = opt(rest, "--file").ok_or("缺少 --file")?;
    let table = opt(rest, "--table").ok_or("缺少 --table")?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("读不了文件：{e}"))?;
    let (header, rows) = parse_csv(&text)?;
    let mut d = open()?;

    // 表不存在就建（全部按 text —— 文本能装下任何东西，不会丢数据）
    if d.list_tables().unwrap_or_default().iter().all(|t| t.name != table) {
        let cols: Vec<model::ColumnDef> = header
            .iter()
            .map(|n| model::ColumnDef {
                name: n.clone(),
                ty: model::ColType::Text,
                not_null: false,
                default: None,
                primary_key: false,
                comment: None,
                shared: None,
                link: None,
                lookup: None,
                rollup: None,
            })
            .collect();
        d.create_table(&model::TableSpec {
            name: table.clone(),
            comment: Some(format!("由 agent 从 {} 导入", file)),
            columns: cols,
        })?;
    }
    let data: Vec<Vec<Option<String>>> = rows
        .iter()
        .map(|r| header.iter().enumerate().map(|(i, _)| r.get(i).cloned()).collect())
        .collect();
    let n = d.insert_rows(&table, &header, &data)?;
    Ok(format!("已导入 {} 行 → 表「{}」", n, table))
}

/// 极简 CSV 解析：支持引号包裹与内部换行。够用就好 ——
/// 完整的 RFC 4180 有更多边角，但对"把表搬进来"这件事不必要。
fn parse_csv(text: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_q = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' if in_q && it.peek() == Some(&'"') => {
                it.next();
                field.push('"');
            }
            '"' => in_q = !in_q,
            ',' if !in_q => {
                cur.push(std::mem::take(&mut field));
            }
            '\n' if !in_q => {
                cur.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut cur));
            }
            '\r' => {}
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !cur.is_empty() {
        cur.push(std::mem::take(&mut field));
        rows.push(std::mem::take(&mut cur));
    }
    rows.retain(|r| !r.is_empty() && !(r.len() == 1 && r[0].trim().is_empty()));
    let mut it2 = rows.into_iter();
    let header = it2.next().ok_or("文件是空的")?;
    Ok((header, it2.collect()))
}

// ---------- 改数据 ----------
fn cmd_add_row(rest: &[String]) -> Result<String, String> {
    let table = opt(rest, "--table").ok_or("缺少 --table")?;
    let data = opt(rest, "--data").ok_or("缺少 --data")?;
    let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&data)
        .map_err(|e| format!("--data 不是合法的 JSON 对象：{e}"))?;
    let cols: Vec<String> = obj.keys().cloned().collect();
    let vals: Vec<Option<String>> = obj
        .values()
        .map(|v| match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
        .collect();
    let mut d = open()?;
    let n = d.insert_rows(&table, &cols, &vec![vals])?;
    Ok(format!("已加 {} 行 → 表「{}」", n, table))
}

fn cmd_update_cell(rest: &[String]) -> Result<String, String> {
    let table = opt(rest, "--table").ok_or("缺少 --table")?;
    let rowid: i64 = opt(rest, "--rowid")
        .and_then(|s| s.parse().ok())
        .ok_or("缺少或非法的 --rowid")?;
    let column = opt(rest, "--column").ok_or("缺少 --column")?;
    let value = opt(rest, "--value");
    let mut d = open()?;
    d.update_cell(&table, rowid, &column, value.as_deref())?;
    Ok(format!("已改：{} #{} 的「{}」", table, rowid, column))
}

fn cmd_delete_row(rest: &[String]) -> Result<String, String> {
    let table = opt(rest, "--table").ok_or("缺少 --table")?;
    let rowid: i64 = opt(rest, "--rowid")
        .and_then(|s| s.parse().ok())
        .ok_or("缺少或非法的 --rowid")?;
    let mut d = open()?;
    let n = d.delete_rows(&table, &[rowid])?;
    Ok(format!("已删 {} 行", n))
}
