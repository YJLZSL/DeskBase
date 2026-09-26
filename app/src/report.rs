//! 表格打印 / 报表出口（v1.10.0 补的一个"接不住"）
//! ============================================================
//! 为什么要有这个模块：表格页原来只有两个出口 ——「导出全部数据」（全库 CSV）与
//! 「导出当前表格」（Excel）。两者的产物都是**给别的软件吃的中间格式**，
//! 而用户真正会做的另一件事是"把这张台账打出来贴墙上 / 发给不看电子表格的人"。
//! 在此之前这件事要绕六步：导出全库 → 打开目录 → 找 CSV → Excel 打开 → 排版 → 打印，
//! 而且打出来的**不是当前这张表**。
//!
//! 设计上的三条硬约束（都是有原因的，别顺手改掉）：
//!
//! 1. **自包含**：CSS 内联、不引用任何外部资源。产物要能被单独拷到别处、发给别人、
//!    在没网的机器上打开 —— 本程序不联网（ADR-0018），导出文件里出现外链就等于
//!    制造了一个"打开就悄悄请求外部地址"的文件，那是立场问题，不是性能问题。
//! 2. **只写新文件，绝不覆盖**：与 `xlsx.exportNotes` / `export.table` 同一套策略。
//!    具体做法见 [`write_new_file`] —— 用 `create_new(true)` 打开，
//!    连"先检查存在、再写入"之间的那个竞态窗口都不留。
//! 3. **一切用户数据都转义**：表名、列名、单元格里什么都可能有
//!    （`<script>`、`&`、引号）。这条与 AGENTS.md 的「不用 innerHTML 渲染用户数据」
//!    同源 —— 导出的 HTML 是要被浏览器**执行**的，而它经常被转发给别人打开；
//!    里面被注入一段脚本，是在用户完全没预期的地方执行代码。
//! ============================================================

use crate::model::{ColType, Db};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::macros::format_description;
use time::OffsetDateTime;

/// 打印视图一次最多导出多少行。
///
/// 这不是产品限制，是**防跑飞**：整表导出靠 `next_cursor` 一页页翻，
/// 万一存储层给出"不前进的游标"，没有上限就是死循环 + 内存吃光。
/// 20 万行远超"打出来贴墙"的量级，正常用户撞不到。
const MAX_REPORT_ROWS: usize = 200_000;

/// 转义用户数据里的 HTML 元字符。
///
/// `&` 必须先于它右边出现的字符被处理 —— 用**单遍逐字符**扫描天然满足
/// （不存在"先换 `<` 再换 `&`，把刚生成的 `&lt;` 又换成 `&amp;lt;`"的经典顺序坑）。
///
/// 零散几处（标题、页眉）用它；循环里的单元格用 [`escape_into`]，
/// 那一条路不产生临时 String（几万行 × 十几列时差别是实打实的）。
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    escape_into(&mut out, s);
    out
}

/// 转义并追加。渲染一个几万行的表会调用它几十万次，
/// 直接写进目标 String 可以省掉每次一个临时 String。
fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

/// 数字列要右对齐：金额、整数、小数。对账时数位不对齐，眼睛就对不出来。
fn is_numeric(ty: ColType) -> bool {
    matches!(ty, ColType::Integer | ColType::Real | ColType::Money)
}

/// NULL → 空、字符串 → 原文、其余 → JSON 文本。
///
/// 与 `export.table` 的取值规则一致：`serde_json::Value` 直接 `to_string()`
/// 会把字符串连引号一起打出来（`"甲"`），那是给机器看的，不是给人看的。
fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 单元格 → 给人看的文本。**与网格里显示的一致** —— 用户打印的是他屏幕上那张表。
fn cell_text(v: &Value, ty: ColType) -> String {
    // 布尔列在网格里显示「是 / 否」而不是 true / false（grid.js 的 valueView），
    // 而库里布尔是按整数 1 / 0 存的（`coerce_value` 的 Boolean 分支）。
    if ty == ColType::Boolean {
        return match v {
            Value::Null => String::new(),
            Value::Bool(b) => if *b { "是" } else { "否" }.to_string(),
            Value::Number(n) => if n.as_i64() == Some(1) { "是" } else { "否" }.to_string(),
            Value::String(s) => {
                if s.trim() == "1" || s.trim().eq_ignore_ascii_case("true") {
                    "是".to_string()
                } else {
                    "否".to_string()
                }
            }
            other => plain(other),
        };
    }
    // 金额按「分」存，显示要换算成「元」。换算**只有 `model::money_display` 一份实现**，
    // 这里不自己算 —— 金额口径多一份实现，就多一个"两个地方对不上"的未来（D-034）。
    if ty == ColType::Money {
        if let Value::Number(n) = v {
            if let Some(cents) = n.as_i64() {
                return crate::model::money_display(cents);
            }
        }
    }
    plain(v)
}

/// 渲染一张表的打印友好 HTML。**纯函数**：不碰文件系统、不读时钟，
/// 所以"转义对不对""打印规则在不在"都能直接单测（不必先造一张真表）。
pub fn render_html(
    table: &str,
    cols: &[(String, ColType)],
    rows: &[Vec<String>],
    exported_at: &str,
) -> String {
    let mut h = String::with_capacity(2048 + rows.len() * (cols.len() * 24 + 32));
    h.push_str("<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>");
    h.push_str(&escape_html(table));
    h.push_str(" · 打印视图</title>\n<style>\n");
    // ---------- 屏幕样式 ----------
    // 内联 CSS 是硬要求：这个文件要能被单独拷到别处、发给别人打开，
    // 引用任何外部资源（字体、样式表、图标）都会让它换个环境就变样，
    // 而且与"程序不联网"的立场冲突（ADR-0018）。
    h.push_str("body{margin:0;padding:18px 22px;color:#111;background:#fff;");
    h.push_str("font-family:system-ui,-apple-system,\"Segoe UI\",\"Microsoft YaHei\",\"Noto Sans CJK SC\",sans-serif}\n");
    h.push_str("h1{font-size:18pt;margin:0 0 6px}\n");
    h.push_str(".meta{color:#555;font-size:10pt;margin:0 0 14px}\n");
    h.push_str("table{border-collapse:collapse;width:100%;font-size:10pt}\n");
    h.push_str("th,td{border:1px solid #bbb;padding:4px 6px;text-align:left;vertical-align:top;");
    h.push_str("word-break:break-word;white-space:pre-wrap}\n");
    h.push_str("thead th{background:#f0f0f0;font-weight:600}\n");
    // 斑马纹只在屏幕上要（宽表横向追行，没有它很容易串行）；打印时去掉，见下面 @media print
    h.push_str("tbody tr:nth-child(even) td{background:#fafafa}\n");
    h.push_str("td.num{text-align:right;font-variant-numeric:tabular-nums}\n");
    h.push_str("footer{margin-top:14px;color:#666;font-size:9pt}\n");
    // ---------- 打印样式 ----------
    // A4 横向：台账通常列多行少，纵向排会被挤成一条条竖线
    h.push_str("@page{size:A4 landscape;margin:12mm}\n");
    h.push_str("@media print{\n");
    h.push_str("body{padding:0;font-size:9pt}\n");
    h.push_str("h1{font-size:14pt}\n");
    h.push_str("table{font-size:9pt}\n");
    // 表头每页重复：台账打两三页时，第二页起没有表头就没人看得懂
    h.push_str("thead{display:table-header-group}\n");
    // 打印去掉斑马纹：省墨；黑白打印机上那层浅灰会变成脏底
    h.push_str("tbody tr:nth-child(even) td{background:transparent}\n");
    h.push_str("th,td{border-color:#000}\n");
    // 一行别被撕在两页上 —— 撕开的一行账，两边都读不成
    h.push_str("tr{page-break-inside:avoid}\n");
    h.push_str("}\n</style>\n</head>\n<body>\n");

    h.push_str("<h1>");
    h.push_str(&escape_html(table));
    h.push_str("</h1>\n<p class=\"meta\">");
    if rows.is_empty() {
        // 空表也导出：用户要的是"确认表头长什么样"，报错反而挡住了他
        h.push_str("共 0 行（表头已就绪，表里还没有数据）");
    } else {
        h.push_str(&format!("共 {} 行", rows.len()));
    }
    h.push_str(" · 导出时间 ");
    h.push_str(&escape_html(exported_at));
    // 时间戳是 UTC（项目存储约定，`now_local` 需要额外开 time 的 local-offset 特性）。
    // 标出来，别让人对着一个差 8 小时的时间猜。
    h.push_str("（UTC） · DeskBase 打印视图</p>\n");

    h.push_str("<table>\n<thead><tr>");
    for (name, ty) in cols {
        h.push_str(if is_numeric(*ty) { "<th class=\"num\">" } else { "<th>" });
        escape_into(&mut h, name);
        h.push_str("</th>");
    }
    h.push_str("</tr></thead>\n<tbody>\n");
    for row in rows {
        h.push_str("<tr>");
        for (i, (_, ty)) in cols.iter().enumerate() {
            h.push_str(if is_numeric(*ty) { "<td class=\"num\">" } else { "<td>" });
            // 按**列数**取而不是按行长取：坏行短一格时补空，保证每行的格子数与表头一致，
            // 否则后面所有列都会错位（打印出来是一张读不懂的表）。
            escape_into(&mut h, row.get(i).map(String::as_str).unwrap_or(""));
            h.push_str("</td>");
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody>\n</table>\n");
    h.push_str("<footer>由 DeskBase 导出 · 本文件自包含（不引用任何外部资源，可单独拷贝或转发）·");
    h.push_str(" 在浏览器里 Ctrl+P 即可打印，或「另存为 PDF」</footer>\n");
    h.push_str("</body>\n</html>\n");
    h
}

/// 端到端：读整张表 → 渲染 → 写一个**新文件**。返回（文件路径, 行数）。
pub fn export_table_html(db: &Db, table: &str, out_dir: &Path) -> Result<(PathBuf, usize), String> {
    let name = table.trim();
    if name.is_empty() {
        return Err("没指定要打印哪张表".to_string());
    }

    let now = OffsetDateTime::now_utc();
    let file_stamp = now
        .format(&format_description!("[year][month][day]-[hour][minute][second]"))
        .unwrap_or_else(|_| "export".to_string());
    let shown = now
        .format(&format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"))
        .unwrap_or_else(|_| String::new());

    // 列类型：金额要按分→元换算、数字列要右对齐，都得知道列是什么类型。
    // 用 `column_meta`（界面读的是同一份来源），不自己从值上猜。
    let metas = db.column_meta(name)?;
    let ty_of = |col: &str| {
        metas
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(col))
            .and_then(|m| m.semantic)
            .unwrap_or(ColType::Text)
    };

    let mut cols: Vec<(String, ColType)> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        // 一页 `MAX_PAGE_LIMIT`（500）行。**必须靠游标翻页**：分页接口会把更大的
        // limit 夹到这个常量上，直接传 `usize::MAX` 只能拿到第一页 ——
        // 「导出当前表格」现在就是这么写的，5 万行的表实际只导出 500 行。
        let page = db.page_rows(
            name,
            None,
            false,
            cursor.as_deref(),
            crate::model::MAX_PAGE_LIMIT,
        )?;
        if cols.is_empty() {
            // 第 0 列是内部 rowid（`_rowid`），打印出去对用户没有意义
            // —— 两个既有导出出口也是这么跳过的。
            if page.columns.len() <= 1 {
                return Err(format!("「{name}」没有任何列，导不出东西"));
            }
            cols = page
                .columns
                .iter()
                .skip(1)
                .map(|c| (c.clone(), ty_of(c)))
                .collect();
        }
        for row in &page.rows {
            let mut cells = Vec::with_capacity(cols.len());
            for (i, (_, ty)) in cols.iter().enumerate() {
                cells.push(row.get(i + 1).map(|v| cell_text(v, *ty)).unwrap_or_default());
            }
            rows.push(cells);
        }
        if !page.has_more {
            break;
        }
        match page.next_cursor {
            // 游标必须**真的前进**才继续；不前进（脏游标 / 行刚被删）就停。
            // 宁可少打几行，也不能让界面卡在一个死循环里。
            Some(c) if cursor.as_deref() != Some(c.as_str()) => cursor = Some(c),
            _ => break,
        }
        if rows.len() >= MAX_REPORT_ROWS {
            return Err(format!(
                "「{name}」超过 {MAX_REPORT_ROWS} 行，打印视图装不下这么多 —— \
                 先在网格里筛掉一部分，或改用「导出当前表格」。"
            ));
        }
    }

    let html = render_html(name, &cols, &rows, &shown);
    // 目录在这里建一次：本模块单独被调用（单测、将来的别的出口）时也要成立。
    // `create_dir_all` 幂等，调用方先建过也没关系。
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建导出目录失败：{e}"))?;
    // 表名进文件名前过 `safe_name`：与两个既有导出口同一条规则，只有一处实现。
    let stem = format!("{}-{file_stamp}", crate::export_all::safe_name(name));
    let path = write_new_file(out_dir, &stem, "html", html.as_bytes())?;
    Ok((path, rows.len()))
}

/// 在 `dir` 下写一个**新文件**；重名就换编号（`x-2.html`），**绝不覆盖**。
///
/// 为什么用 `create_new(true)` 而不是"先 `exists()` 再写"：
/// 检查与写入之间存在竞态窗口（连点两次按钮、或用户自己放了个同名文件），
/// 而 `create_new` 把"不覆盖"交给文件系统来保证 —— 没有窗口可言。
fn write_new_file(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    for n in 1..=50 {
        let file = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let path = dir.join(file);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes) {
                    // 半截文件比没有文件更坏：用户会以为导出成功了。
                    // 落盘失败就把它删掉，别留一个看起来正常、其实残缺的 HTML。
                    drop(f);
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("写文件失败：{e}"));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("写文件失败：{e}")),
        }
    }
    Err("导出目录里同名文件太多（已有 50 个），请先清理".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, TableSpec};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dkb_report_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
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

    fn seed(
        dir: &Path,
        table: &str,
        cols: &[(&str, ColType)],
        rows: &[Vec<Option<String>>],
    ) -> Db {
        let mut d = Db::open(dir).unwrap();
        d.create_table(&spec(table, cols)).unwrap();
        let names: Vec<String> = cols.iter().map(|(n, _)| n.to_string()).collect();
        d.insert_rows(table, &names, rows).unwrap();
        d
    }

    // ---------- 转义（硬红线） ----------

    #[test]
    fn escape_html_covers_the_dangerous_five() {
        assert_eq!(
            escape_html("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("双引号\"与单引号'"), "双引号&quot;与单引号&#39;");
        assert_eq!(escape_html("中文与 emoji 🙂 原样"), "中文与 emoji 🙂 原样");
        assert_eq!(escape_html(""), "");
    }

    #[test]
    fn escape_html_does_not_double_escape_entities() {
        // 用户自己写的 `&lt;` 必须变成 `&amp;lt;`（浏览器显示成字面的 "&lt;"），
        // 而不是被当成"已经是实体"原样放过去 —— 那正是 HTML 注入的经典入口。
        assert_eq!(escape_html("&lt;"), "&amp;lt;");
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
    }

    // ---------- 渲染 ----------

    #[test]
    fn render_html_escapes_name_headers_and_cells() {
        let cols = vec![("</th><script>".to_string(), ColType::Text)];
        let rows = vec![
            vec!["<script>alert('x')</script>".to_string()],
            vec!["a & b \"c\" it's".to_string()],
        ];
        let html = render_html("台账<b>&", &cols, &rows, "2026-09-20 03:12:45");
        // 这一条是硬红线：生成的 HTML 里**不许出现**用户数据带进来的原始标签
        assert!(!html.contains("<script>"), "用户数据里的 <script> 不许原样进 HTML");
        assert!(!html.contains("<b>"), "表名里的标签不许原样进 HTML");
        assert!(!html.contains("</th><script>"), "列名不许破坏表格结构");
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
        assert!(html.contains("台账&lt;b&gt;&amp;"));
        assert!(html.contains("a &amp; b &quot;c&quot; it&#39;s"));
    }

    #[test]
    fn render_html_print_block_is_wall_friendly() {
        let cols = vec![("名称".to_string(), ColType::Text)];
        let rows = vec![vec!["甲".to_string()]];
        let html = render_html("台账", &cols, &rows, "2026-09-20 03:12:45");
        assert!(
            html.contains("@page{size:A4 landscape"),
            "要按 A4 横向排版：{html}"
        );
        let screen = html.split("@media print").next().unwrap_or("").to_string();
        let print = html
            .split("@media print")
            .nth(1)
            .unwrap_or_else(|| panic!("必须有 @media print 段：{html}"))
            .to_string();
        // 表头每页重复 —— 台账打两三页时，第二页起没有表头就没人看得懂
        assert!(
            print.contains("display:table-header-group"),
            "表头要在每页重复：{print}"
        );
        // 打印去掉斑马纹：省墨，且黑白打印机上浅灰底会变脏
        assert!(
            print.contains("background:transparent"),
            "打印要去掉斑马纹：{print}"
        );
        assert!(!print.contains("#fafafa"), "打印段里不该再有斑马纹底色：{print}");
        assert!(
            screen.contains("nth-child(even)"),
            "屏幕上要保留斑马纹（追行用）：{screen}"
        );
        assert!(
            print.contains("font-size:9pt"),
            "打印字号不能小于 9pt：{print}"
        );
    }

    #[test]
    fn render_html_lists_every_row_under_one_header_row() {
        let cols = vec![
            ("名称".to_string(), ColType::Text),
            ("金额".to_string(), ColType::Money),
        ];
        let rows: Vec<Vec<String>> = (0..7)
            .map(|i| vec![format!("行{i}"), format!("{i}.00")])
            .collect();
        let html = render_html("台账", &cols, &rows, "2026-09-20 03:12:45");
        assert_eq!(html.matches("<tr>").count(), 1 + rows.len(), "一行表头 + 每行一个 <tr>");
        assert_eq!(html.matches("<th>").count(), 1, "文本列一个表头格");
        assert_eq!(html.matches("<th class=\"num\">").count(), 1, "金额列表头格");
        assert!(html.contains("共 7 行"), "行数要写在页眉上");
        assert!(html.contains("2026-09-20 03:12:45"), "导出时间要写在页眉上");
        assert!(
            html.contains("<td class=\"num\">"),
            "金额列要右对齐（对账时数位要对齐）"
        );
    }

    // ---------- 单元格文本（与网格一致） ----------

    #[test]
    fn cell_text_matches_the_grid() {
        assert_eq!(cell_text(&json!(1234), ColType::Money), "12.34");
        assert_eq!(cell_text(&json!(-500), ColType::Money), "-5.00");
        assert_eq!(cell_text(&json!(42), ColType::Integer), "42");
        assert_eq!(cell_text(&json!(1), ColType::Boolean), "是");
        assert_eq!(cell_text(&json!(0), ColType::Boolean), "否");
        assert_eq!(cell_text(&Value::Null, ColType::Text), "");
        assert_eq!(cell_text(&Value::Null, ColType::Money), "", "NULL 不该显示成 0.00");
        assert_eq!(cell_text(&json!("甲"), ColType::Text), "甲");
        assert_eq!(cell_text(&json!({"a": 1}), ColType::Json), "{\"a\":1}");
    }

    // ---------- 端到端：写文件 ----------

    #[test]
    fn export_writes_a_new_file_and_never_overwrites() {
        let dir = tmp("newfile");
        let d = seed(
            &dir,
            "台账",
            &[("名称", ColType::Text), ("金额", ColType::Money)],
            &[
                vec![Some("甲".to_string()), Some("12.34".to_string())],
                vec![Some("乙".to_string()), Some("0.5".to_string())],
            ],
        );
        let out = dir.join("exports");
        let (p1, n1) = export_table_html(&d, "台账", &out).unwrap();
        let (p2, n2) = export_table_html(&d, "台账", &out).unwrap();
        assert_eq!(n1, 2);
        assert_eq!(n2, 2);
        assert_ne!(p1, p2, "两次导出必须是两个新文件，绝不覆盖");
        assert!(p1.exists() && p2.exists());
        assert_eq!(p1.parent().unwrap(), out.as_path(), "导出文件必须落在导出目录里");
        assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("html"));
        assert!(
            p1.file_name().unwrap().to_string_lossy().starts_with("台账-"),
            "文件名要带表名（过 safe_name）：{:?}",
            p1.file_name()
        );
        let html = std::fs::read_to_string(&p1).unwrap();
        assert!(html.contains("12.34"), "金额要按元显示（与网格一致）");
        assert!(html.contains("0.50"));
        assert!(html.contains("甲"));
    }

    #[test]
    fn export_paginates_past_the_page_limit() {
        // 分页接口会把 limit 夹到 `model::MAX_PAGE_LIMIT`（500），所以整表导出
        // **必须靠游标翻页**：拿 usize::MAX 当 limit 只会拿到第一页。
        // 这里用一个刚好超过一页的表把它钉住（501 行必须一行不少）。
        let dir = tmp("page");
        let n = crate::model::MAX_PAGE_LIMIT + 1;
        let rows: Vec<Vec<Option<String>>> = (0..n)
            .map(|i| vec![Some(format!("行{i}"))])
            .collect();
        let d = seed(&dir, "长表", &[("名称", ColType::Text)], &rows);
        let (path, count) = export_table_html(&d, "长表", &dir.join("exports")).unwrap();
        assert_eq!(count, n);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains(&format!("行{}", n - 1)), "最后一页的行也要在");
        assert!(html.contains(&format!("共 {n} 行")));
    }

    #[test]
    fn a_single_page_call_can_never_return_a_whole_big_table() {
        // 这条钉的是**本模块必须翻页**这件事，同时给「导出当前表格」留一份证据：
        // `page_rows` 会把 limit 夹到 `MAX_PAGE_LIMIT`，所以拿 `usize::MAX` 当 limit
        // 也只能拿到第一页。`main.rs` 的 `export.table` 正是那么调的（main.rs:1069）——
        // 也就是说 5 万行的表现在只会导出 500 行，而且不报错。
        // 那一处不在本模块的修改范围内（lead 划的界），先把事实与判据留在这里。
        let dir = tmp("clamp");
        let n = crate::model::MAX_PAGE_LIMIT + 1;
        let rows: Vec<Vec<Option<String>>> = (0..n)
            .map(|i| vec![Some(format!("行{i}"))])
            .collect();
        let d = seed(&dir, "大表", &[("名称", ColType::Text)], &rows);
        let page = d.page_rows("大表", None, false, None, usize::MAX).unwrap();
        assert_eq!(
            page.rows.len(),
            crate::model::MAX_PAGE_LIMIT,
            "单次调用最多一个 MAX_PAGE_LIMIT，拿不到整表"
        );
        assert!(page.has_more, "后面还有没取到的行");
        // 本模块靠游标翻页，所以能一行不少（见 export_paginates_past_the_page_limit）
    }

    #[test]
    fn export_reports_unknown_table_in_plain_chinese() {
        let dir = tmp("missing");
        let d = Db::open(&dir).unwrap();
        let e = export_table_html(&d, "没这张表", &dir.join("exports")).unwrap_err();
        assert!(e.contains("不存在"), "错误要说人话：{e}");
    }

    // ---------- IPC 契约（走真正的 dispatch 入口） ----------
    //
    // `main.rs` 里 `report.exportTable` 那一支只有二十来行"解析参数 → 调本模块 →
    // 拼应答"，但**恰恰是这一层最容易和界面静默对不上**（历史上 `has_more` /
    // `elapsed_ms` 两次静默失效都出在这条边界上）。`dispatch` 与 `AppState` 是
    // crate 根模块的私有项，子模块可见 —— 于是这里能直接把它钉住：
    // args 叫 `name`、返回是 `{path, count, name}`（snake_case）、出错是中文人话。
    //
    // 夹具与 `main.rs` 的 `acceptance::fixture` **同形**：`AppState` 加字段时
    // 两处要一起补（缺字段的编译错误会直接指到这里，不至于漏掉）。
    fn ipc_fixture(tag: &str) -> (Arc<crate::AppState>, PathBuf) {
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("dkb_report_ipc_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let db = Db::open(&dir).unwrap();
        let state = Arc::new(crate::AppState {
            db: Arc::new(Mutex::new(db)),
            data_dir: dir.clone(),
            webview: Mutex::new(None),
            started_at: std::time::Instant::now(),
            last_workspace: Mutex::new(None),
            proxy: Mutex::new(None),
            smoke_script: None,
            smoke_fired: AtomicBool::new(false),
            e2e_source: Mutex::new(None),
            recovery_status: crate::recovery::RecoveryReport::default(),
        });
        (state, dir)
    }

    /// 走真正的分发入口（与界面同一条路），把 IPC 应答解成 JSON。
    fn call(state: &crate::AppState, cmd: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let raw = crate::dispatch(
            state,
            crate::Request {
                id: 1,
                cmd: cmd.to_string(),
                args,
            },
        )
        .expect("这条命令应当是同步的");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("应答必须是合法 JSON");
        if v["ok"].as_bool() == Some(true) {
            Ok(v["data"].clone())
        } else {
            Err(v["error"].as_str().unwrap_or("未知错误").to_string())
        }
    }

    #[test]
    fn ipc_report_export_table_returns_path_count_name() {
        let (state, dir) = ipc_fixture("contract");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "契约台账", "comment": null, "columns": [
                { "name": "名称", "ty": "text", "not_null": false, "default": null,
                  "primary_key": false, "comment": null },
                { "name": "金额", "ty": "money", "not_null": false, "default": null,
                  "primary_key": false, "comment": null },
            ]}}),
        )
        .unwrap();
        call(
            &state,
            "schema.insertRows",
            json!({
                "table": "契约台账",
                "columns": ["名称", "金额"],
                "rows": [["甲", "12.34"], ["乙", "0.50"]],
            }),
        )
        .unwrap();

        let r = call(&state, "report.exportTable", json!({"name": "契约台账"})).unwrap();
        assert_eq!(r["count"], 2);
        assert_eq!(r["name"], "契约台账");
        let p = r["path"].as_str().expect("返回里必须有 path");
        assert!(std::path::Path::new(p).exists(), "返回的路径必须真的存在：{p}");
        assert!(p.ends_with(".html"), "产物应当是 .html：{p}");
        assert!(
            std::path::Path::new(p).starts_with(dir.join("exports")),
            "必须落在 <数据目录>/exports/ 里：{p}"
        );
        let html = std::fs::read_to_string(p).unwrap();
        assert!(html.contains("12.34"), "金额要按元显示");
        assert!(html.contains("@media print"), "要带打印样式");

        // 参数不对 / 表不存在：都得是中文人话，而不是 panic 或英文
        let e = call(&state, "report.exportTable", json!({"name": ""})).unwrap_err();
        assert!(e.contains("哪张表"), "空表名要给中文提示：{e}");
        let e2 = call(&state, "report.exportTable", json!({"name": "没这张表"})).unwrap_err();
        assert!(e2.contains("不存在"), "不认识表要给中文提示：{e2}");

        let _ = std::fs::remove_dir_all(dir);
    }
}
