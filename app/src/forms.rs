//! 单据模板：数据模型 + 打印渲染
//! ============================================================
//! 为什么做这个（证据不是"想当然"）：
//!
//! `local-docs/reference/31-user-pain-points.md` §0.3 的结论是 ——
//! **DeskBase 现在最该补的三处：行内公式列 ＞ 打印/报表出口 ＞ 单表导出**。
//! 而三条具体场景都点名同一个缺口：
//!   · S1 小工厂仓管：❌ 打印出库单
//!   · S3 个体店主："不得已只能使用操作比较简单的 Excel 来打印" ❌ 打印小票/出库单
//!   · S4 会计：❌ 对账单的格式与打印
//! `30-competitive-landscape.md` §0.3 说到了根上：
//! **"Access 留下的空位不是'更简单的表格'，而是'能给同事用的小应用'"** ——
//! 只有表、没有出口，就接不住这个位置。
//!
//! 同类产品（送货单打印软件）的共性做法，本模块借鉴了这四条：
//!   ① 内置模板库（送货单/订货单/收据/通用）
//!   ② 可视化拖拽编辑（文字、线条、图片、字段）
//!   ③ **纸张规格可选**（A4 / 连页纸 / 小票）
//!   ④ 打印预览（生成 HTML → 浏览器 Ctrl+P，复用 v1.11.0 的通道）
//!
//! **借鉴了但没做的**：条码/二维码（要引库，体积代价与当前 2.9 MB 的定位不符）、
//! 多币种、云同步。这三条记在文档里，不是忘了。
//!
//! 单位一律 **毫米**。打印这件事上，像素是错的单位 —— 它随 DPI 变，
//! 而毫米不会。前端渲染时再按 mm 换算，两边共用同一套坐标。
//! ============================================================

use serde::{Deserialize, Serialize};

/// 画布上的一个元素。
///
/// 用「一个结构体 + kind 字段」而不是 enum，是为了让前端的序列化/反序列化
/// 与属性面板都简单：改一个属性不用管它是什么类型。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct El {
    /// `text` 文字 · `line` 直线 · `rect` 矩形 · `field` 待填字段 · `table` 表格区
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(default)]
    pub text: String,
    /// 字号（pt）。打印上 pt 是有意义的单位，不该用 px。
    #[serde(default = "d_size")]
    pub size: f64,
    #[serde(default)]
    pub bold: bool,
    /// `left` / `center` / `right`
    #[serde(default = "d_align")]
    pub align: String,
    /// `field` 用：这一栏是给谁填的（如"客户名称"）
    #[serde(default)]
    pub label: String,
    /// `table` 用：表头列名
    #[serde(default)]
    pub columns: Vec<String>,
    /// `table` 用：留几行空行给手写/打印数据
    #[serde(default = "d_rows")]
    pub rows: usize,
}

fn d_size() -> f64 {
    10.0
}
fn d_align() -> String {
    "left".to_string()
}
fn d_rows() -> usize {
    5
}

/// 纸张尺寸（毫米）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Paper {
    pub w: f64,
    pub h: f64,
    /// 给人看的名字（"A4 纵向" / "小票 80mm"），打印页脚与模板列表都用它
    #[serde(default)]
    pub label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub paper: Paper,
    pub elements: Vec<El>,
}

/// 可选的纸张规格。
///
/// **为什么把"小票"也列进来**：同类产品里连页纸/小票是标配，
/// 而目标用户（小店主）很多用的是小票机。只给 A4 是不合格的。
pub fn papers() -> Vec<Paper> {
    vec![
        Paper { w: 210.0, h: 297.0, label: "A4 纵向".into() },
        Paper { w: 297.0, h: 210.0, label: "A4 横向".into() },
        Paper { w: 241.0, h: 140.0, label: "连页纸 241×140".into() },
        Paper { w: 80.0, h: 200.0, label: "小票 80mm".into() },
        Paper { w: 58.0, h: 200.0, label: "小票 58mm".into() },
    ]
}

fn t(kind: &str, x: f64, y: f64, w: f64, h: f64, text: &str, size: f64) -> El {
    El {
        kind: kind.into(),
        x,
        y,
        w,
        h,
        text: text.into(),
        size,
        bold: false,
        align: "left".into(),
        label: String::new(),
        columns: Vec::new(),
        rows: 5,
    }
}

fn field(x: f64, y: f64, w: f64, label: &str) -> El {
    let mut e = t("field", x, y, w, 8.0, "________", 10.0);
    e.label = label.into();
    e
}

fn table_el(x: f64, y: f64, w: f64, cols: &[&str], rows: usize) -> El {
    let mut e = t("table", x, y, w, 8.0 + rows as f64 * 8.0, "", 9.5);
    e.columns = cols.iter().map(|s| s.to_string()).collect();
    e.rows = rows;
    e
}

/// 内置模板。**这就是"海量模板"的最小可信版本** ——
/// 先给四种最常用的，用户能改能存；不假装有"上百套行业模板"。
pub fn builtin() -> Vec<Template> {
    let a4 = papers().remove(0);

    // ---- 送货单 ----
    let mut d = t("text", 0.0, 12.0, 210.0, 12.0, "送 货 单", 22.0);
    d.bold = true;
    d.align = "center".into();
    let mut dl = t("line", 18.0, 26.0, 174.0, 0.0, "", 10.0);
    dl.size = 1.0;
    let mut dr = t("rect", 18.0, 40.0, 174.0, 92.0, "", 10.0);
    dr.size = 0.4;
    let mut note = t("text", 18.0, 140.0, 174.0, 8.0, "备注：", 9.5);
    note.align = "left".into();

    let delivery = Template {
        id: "builtin-delivery".into(),
        name: "送货单".into(),
        paper: a4.clone(),
        elements: vec![
            d,
            dl,
            t("text", 18.0, 30.0, 100.0, 8.0, "收货单位：", 10.0),
            t("text", 140.0, 30.0, 52.0, 8.0, "单号：", 10.0),
            field(42.0, 30.0, 90.0, "收货单位"),
            field(160.0, 30.0, 32.0, "单号"),
            table_el(
                18.0,
                40.0,
                174.0,
                &["品名", "规格", "单位", "数量", "单价", "金额"],
                6,
            ),
            t("text", 18.0, 134.0, 40.0, 8.0, "合计金额：", 10.0),
            field(52.0, 134.0, 40.0, "合计"),
            t("text", 18.0, 150.0, 60.0, 8.0, "发货人：", 10.0),
            field(42.0, 150.0, 40.0, "发货人"),
            t("text", 100.0, 150.0, 60.0, 8.0, "收货人签字：", 10.0),
            field(134.0, 150.0, 58.0, "签字"),
            t("text", 18.0, 165.0, 174.0, 8.0, "第一联：存根    第二联：客户    第三联：财务", 8.5),
        ],
    };

    // ---- 订货单 ----
    let mut p = t("text", 0.0, 12.0, 210.0, 12.0, "订 货 单", 22.0);
    p.bold = true;
    p.align = "center".into();
    let order = Template {
        id: "builtin-order".into(),
        name: "订货单".into(),
        paper: a4.clone(),
        elements: vec![
            p,
            t("line", 18.0, 26.0, 174.0, 0.0, "", 1.0),
            t("text", 18.0, 30.0, 80.0, 8.0, "供货单位：", 10.0),
            field(46.0, 30.0, 86.0, "供货单位"),
            t("text", 140.0, 30.0, 52.0, 8.0, "交货日期：", 10.0),
            field(168.0, 30.0, 24.0, "日期"),
            table_el(
                18.0,
                42.0,
                174.0,
                &["品名", "规格", "数量", "单价", "金额", "交期"],
                8,
            ),
            t("text", 18.0, 152.0, 80.0, 8.0, "订货人：", 10.0),
            field(42.0, 152.0, 50.0, "订货人"),
            t("text", 120.0, 152.0, 72.0, 8.0, "供货方确认：", 10.0),
            field(160.0, 152.0, 32.0, "确认"),
        ],
    };

    // ---- 收据 ----
    let mut rt = t("text", 0.0, 10.0, 210.0, 12.0, "收    据", 22.0);
    rt.bold = true;
    rt.align = "center".into();
    let receipt = Template {
        id: "builtin-receipt".into(),
        name: "收据".into(),
        paper: a4.clone(),
        elements: vec![
            rt,
            t("line", 18.0, 24.0, 174.0, 0.0, "", 1.0),
            t("text", 18.0, 32.0, 40.0, 8.0, "今收到：", 10.5),
            field(46.0, 32.0, 146.0, "今收到"),
            t("text", 18.0, 46.0, 46.0, 8.0, "金额（大写）：", 10.5),
            field(60.0, 46.0, 100.0, "大写"),
            t("text", 164.0, 46.0, 28.0, 8.0, "￥", 10.5),
            field(172.0, 46.0, 20.0, "小写"),
            t("text", 18.0, 62.0, 20.0, 8.0, "事由：", 10.5),
            field(36.0, 62.0, 156.0, "事由"),
            t("line", 18.0, 96.0, 174.0, 0.0, "", 1.0),
            t("text", 18.0, 100.0, 80.0, 8.0, "收款人：", 10.5),
            field(42.0, 100.0, 50.0, "收款人"),
            t("text", 120.0, 100.0, 72.0, 8.0, "日期：", 10.5),
            field(138.0, 100.0, 54.0, "日期"),
        ],
    };

    // ---- 通用空白（给"我就想自己画一张"的人）----
    let blank = Template {
        id: "builtin-blank".into(),
        name: "空白 A4".into(),
        paper: a4,
        elements: vec![
            t("text", 20.0, 20.0, 170.0, 10.0, "标题", 16.0),
            t("line", 20.0, 32.0, 170.0, 0.0, "", 1.0),
        ],
    };

    vec![delivery, order, receipt, blank]
}

/// 转义。打印 HTML 里所有用户输入都要过这一道 ——
/// **模板文本是用户自己写的，但那不代表它可以当 HTML 跑。**
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// 生成打印用的自包含 HTML。
///
/// 几条硬要求（沿用 `report.rs` 里已经立过的规矩）：
///   · **CSS 全内联** —— 这个文件要能被单独拷走、发给别人打开，
///     引用任何外部资源都会让它换个环境就变样，也与"不联网"的立场冲突（ADR-0018）。
///   · **坐标用毫米**，与编辑器共用同一套单位 —— 两边各算一套迟早会对不上。
///   · `@page` 按所选纸张设 —— 用户选"小票 80mm"，打出来就该是 80mm。
pub fn render_print_html(t: &Template) -> String {
    let mut h = String::with_capacity(4096);
    h.push_str("<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<title>");
    h.push_str(&esc(&t.name));
    h.push_str(" · 打印</title>\n<style>\n");
    h.push_str("*{box-sizing:border-box}\n");
    h.push_str("body{margin:0;background:#f2f2f2;color:#000;");
    h.push_str("font-family:system-ui,-apple-system,\"Segoe UI\",\"Microsoft YaHei\",\"Noto Sans CJK SC\",sans-serif}\n");
    // 画布：固定毫米尺寸，元素绝对定位
    h.push_str(&format!(
        ".sheet{{position:relative;width:{w}mm;height:{h}mm;background:#fff;margin:12px auto;",
        w = t.paper.w,
        h = t.paper.h
    ));
    h.push_str("box-shadow:0 1px 6px rgba(0,0,0,.18)}\n");
    h.push_str(".el{position:absolute;overflow:hidden}\n");
    h.push_str(".el.text{display:flex;align-items:center;white-space:pre-wrap}\n");
    h.push_str(".el.line{border-top:1px solid #000}\n");
    h.push_str(".el.rect{border:1px solid #000}\n");
    h.push_str(".el.field{display:flex;align-items:flex-end;color:#000}\n");
    h.push_str("table{border-collapse:collapse;width:100%;font-size:9.5pt}\n");
    h.push_str("th,td{border:1px solid #000;padding:1.2mm 1.6mm;height:8mm}\n");
    h.push_str("th{font-weight:600;background:#f4f4f4}\n");
    h.push_str(".tip{margin:10px auto 24px;max-width:90%;color:#666;font-size:9pt;text-align:center}\n");
    // 打印样式：去背景、去阴影、按纸张设 @page
    h.push_str("@media print{body{background:#fff}.sheet{margin:0;box-shadow:none}.tip{display:none}}\n");
    h.push_str(&format!(
        "@page{{size:{}mm {}mm;margin:0}}\n",
        t.paper.w, t.paper.h
    ));
    h.push_str("</style>\n</head>\n<body>\n");
    h.push_str("<div class=\"sheet\">\n");

    for e in &t.elements {
        let mut style = format!(
            "left:{:.2}mm;top:{:.2}mm;width:{:.2}mm;",
            e.x, e.y, e.w.max(0.0)
        );
        if e.kind != "line" {
            style.push_str(&format!("height:{:.2}mm;", e.h.max(0.0)));
        }
        match e.kind.as_str() {
            "text" => {
                style.push_str(&format!(
                    "font-size:{:.1}pt;justify-content:{};",
                    e.size,
                    match e.align.as_str() {
                        "center" => "center",
                        "right" => "flex-end",
                        _ => "flex-start",
                    }
                ));
                if e.bold {
                    style.push_str("font-weight:600;");
                }
                h.push_str(&format!(
                    "<div class=\"el text\" style=\"{}\">{}</div>\n",
                    style,
                    esc(&e.text).replace('\n', "<br>")
                ));
            }
            "line" => {
                // 线宽用 border-top 的粗细表达，不是高度
                style.push_str(&format!("border-top-width:{:.2}mm;", (e.size / 3.0).max(0.2)));
                h.push_str(&format!("<div class=\"el line\" style=\"{}\"></div>\n", style));
            }
            "rect" => {
                style.push_str(&format!("border-width:{:.2}mm;", (e.size / 3.0).max(0.2)));
                h.push_str(&format!("<div class=\"el rect\" style=\"{}\"></div>\n", style));
            }
            "field" => {
                // 待填字段：标签 + 下划线。打印出来是能让手写的空栏。
                style.push_str(&format!(
                    "font-size:{:.1}pt;border-bottom:1px solid #000;padding-left:1mm;",
                    e.size
                ));
                let shown = if e.text.trim().is_empty() { "&nbsp;".to_string() } else { esc(&e.text) };
                h.push_str(&format!(
                    "<div class=\"el field\" style=\"{}\" title=\"{}\">{}</div>\n",
                    style,
                    esc(&e.label),
                    shown
                ));
            }
            "table" => {
                let cols = if e.columns.is_empty() {
                    vec!["".to_string()]
                } else {
                    e.columns.clone()
                };
                h.push_str(&format!(
                    "<div class=\"el\" style=\"{}top:{:.2}mm;position:absolute\">\n",
                    style, e.y
                ));
                h.push_str("<table>\n<thead><tr>");
                for c in &cols {
                    h.push_str(&format!("<th>{}</th>", esc(c)));
                }
                h.push_str("</tr></thead>\n<tbody>\n");
                for _ in 0..e.rows.max(1) {
                    h.push_str("<tr>");
                    for _ in &cols {
                        h.push_str("<td></td>");
                    }
                    h.push_str("</tr>\n");
                }
                h.push_str("</tbody>\n</table>\n</div>\n");
            }
            _ => {}
        }
    }

    h.push_str("</div>\n");
    h.push_str(
        "<p class=\"tip\">这个文件是给打印用的：按 <b>Ctrl+P</b> 打印，或「另存为 PDF」发给别人。<br>\
         如果没有看到打印对话框，说明你的浏览器把它当成了下载 —— 打开下载到的文件即可。</p>\n",
    );
    h.push_str("</body>\n</html>\n");
    h
}
