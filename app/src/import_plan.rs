//! 导入计划：推断「表头在第几行」与「每列是什么类型」。
//!
//! 为什么单独放一个模块、且**全是纯函数**：
//! 导入这件事里最容易出错、也最难事后发现的就是类型判断 ——
//! 把带前导零的工号存成整数、把 16 位订单号存成浮点、把 3 位小数的金额
//! 塞进只接受 2 位的金额列……这些错误**不会报错**，只会安静地改掉用户的原始数据。
//! 所以规则本身必须能被单独测，不能埋在"读文件 → 建表"的大流程里。
//!
//! 本模块不读文件、不碰数据库。读盘与写库在 `xlsx.rs` / `schema.rs`。
//!
//! 一条贯穿始终的原则（与 `xlsx.rs` 的导出侧一致）：
//! **宁可判成文本，也不要判成一个会丢信息的类型。**
//! 文本是最安全的 —— 它什么都不会改。所以所有的"拿不准"最后都落到文本上。

use serde::Serialize;

/// 列的候选类型。这些都是 `schema.rs` 认可的类型名。
const T_TEXT: &str = "text";
const T_INTEGER: &str = "integer";
const T_REAL: &str = "real";
const T_MONEY: &str = "money";
const T_DATE: &str = "date";
const T_BOOLEAN: &str = "boolean";

/// 置信度。界面要靠它决定"直接照建议建表"还是"提醒用户看一眼"。
pub const C_HIGH: &str = "高";
pub const C_MID: &str = "中";
pub const C_LOW: &str = "低";

#[derive(Debug, Clone, Serialize)]
pub struct TypeGuess {
    /// 建议的类型名（`schema.rs` 的口径）
    pub ty: String,
    pub confidence: String,
    /// **为什么这么判** —— 给用户看的。用户凭这句话判断要不要改
    pub reason: String,
}

// ============================================================
// 单元格清洗
// ============================================================

/// 去掉首尾空白与不换行空格。
///
/// 不换行空格（U+00A0）必须单独处理：中文 Excel 导出的文件里大量存在，
/// 而 `trim()` 不认它 —— 结果就是"看起来一模一样的两个值"被判成不等，
/// 去重与类型判断会跟着一起错。
pub fn trim_cell(raw: &str) -> String {
    raw.trim_matches(|c: char| c.is_whitespace() || c == '\u{00a0}' || c == '\u{3000}')
        .to_string()
}

/// 全角数字与全角符号转半角。
///
/// `１２３` 与 `123` 在用户眼里是同一个数，在字节层面不是。
/// 中文输入法下被误打成全角是常见事，不处理就等于把它判成文本。
fn to_halfwidth(s: &str) -> String {
    s.chars()
        .map(|c| {
            let u = c as u32;
            // 全角数字 ０-９
            if (0xFF10..=0xFF19).contains(&u) {
                char::from_u32(u - 0xFF10 + 0x30).unwrap_or(c)
            }
            // 全角句点（当作小数点）、全角逗号、全角负号、全角货币号
            else if c == '．' {
                '.'
            } else if c == '，' {
                ','
            } else if c == '－' {
                '-'
            } else if c == '￥' {
                '¥'
            } else {
                c
            }
        })
        .collect()
}

/// 把单元格清洗成"可当数字解析"的形式，失败返回 `None`。
///
/// 处理的是真实导出文件里常见、且**含义明确**的写法：
/// · 货币符号前缀：`¥1,234.50`、`$12`、`RMB 100`
/// · 千分位逗号：`1,234.50`（**只删合法的千分位**，不做无脑替换）
/// · 会计括号负数：`(123.45)` 表示 -123.45
/// · 结尾单位：`1234.5 元`
///
/// 明确**不认**科学计数法（`1e5`）：用户的原文如果是 `1e5`，他多半就是想让
/// 这一列当文本（编号、型号），改成 100000 才是毁数据。
pub fn clean_number(raw: &str) -> Option<String> {
    let mut s = to_halfwidth(&trim_cell(raw));
    if s.is_empty() {
        return None;
    }

    // 会计括号负数
    let mut negative = false;
    if s.starts_with('(') && s.ends_with(')') && s.len() > 2 {
        negative = true;
        s = s[1..s.len() - 1].to_string();
    }

    // 前缀货币符号 / 币种代码
    for p in ["¥", "$", "€", "£", "₩"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.trim_start().to_string();
        }
    }
    for p in ["RMB", "CNY", "USD", "rmb", "cny", "usd"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.trim_start().to_string();
        }
    }

    // 结尾单位。
    // 「万元」必须先判：它是**量纲**而不是单纯的单位 —— 1万元 究竟是 10000 还是 1？
    // 不换算就等于把 1 万读成 1（差一万倍），换算又是在替用户做他没说的决定。
    // 所以遇到它直接放弃数字判断，转文本交给用户自己定。
    if s.contains("万元") {
        return None;
    }
    for su in ["元", "块"] {
        if let Some(rest) = s.strip_suffix(su) {
            s = rest.trim_end().to_string();
        }
    }

    if s.is_empty() {
        return None;
    }

    // 千分位：只接受「每三位一组」的写法，避免把 "1,23" 这种（可能是笔误或别的
    // 含义）当成 123
    let body = if s.contains(',') {
        let parts: Vec<&str> = s.split('.').collect();
        let int_part = parts[0];
        let groups: Vec<&str> = int_part.split(',').collect();
        let ok = groups.len() >= 2
            && !groups[0].is_empty()
            && groups[0].len() <= 3
            && groups[1..]
                .iter()
                .all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit()));
        if !ok {
            return None;
        }
        // 千分位只可能出现在整数部分 —— 小数部分若也有逗号则放弃
        if parts[1..].iter().any(|p| p.contains(',')) {
            return None;
        }
        let head = int_part.replace(',', "");
        match parts.get(1) {
            Some(frac) => format!("{head}.{frac}"),
            None => head,
        }
    } else {
        s.clone()
    };

    let candidate = if negative { format!("-{body}") } else { body };

    // 科学计数法一律拒绝（见函数注释）
    if candidate.chars().any(|c| c == 'e' || c == 'E') {
        return None;
    }
    if !candidate
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return None;
    }
    // 只允许一个负号且在开头、最多一个小数点
    if candidate.matches('-').count() > 1 || candidate.trim_start_matches('-').contains('-') {
        return None;
    }
    if candidate.matches('.').count() > 1 {
        return None;
    }
    let numeric = candidate.trim_start_matches('-').replace('.', "");
    if numeric.is_empty() || !numeric.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(candidate)
}

/// 这个值像不像「编号」——像的话**绝不能**按数字存。
///
/// 两类：
/// 1. **前导零**（`007`、`0123`）：商品编码、工号、行政区划码大量长这样。
///    当数字存就是 `7` / `123`，前导零没了，对账时才发现编号全对不上。
/// 2. **16 位以上的纯数字**：IEEE-754 双精度只有 15–17 位有效数字，
///    订单号/身份证/银行卡超了就**永久**变成 0 结尾，事后改格式也救不回来。
pub fn looks_like_id(cleaned: &str) -> bool {
    let digits = cleaned.trim_start_matches('-').replace('.', "");
    if digits.len() >= 16 {
        return true;
    }
    // 前导零：`0` 与 `0.5` 不算（它们是合法的数）
    if cleaned.starts_with('0') && !cleaned.starts_with("0.") && cleaned.len() > 1 {
        return true;
    }
    false
}

/// 小数位数。
fn decimals_of(cleaned: &str) -> usize {
    match cleaned.split_once('.') {
        Some((_, frac)) => frac.len(),
        None => 0,
    }
}

// ============================================================
// 日期
// ============================================================

#[inline]
fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 把一个日期写法规范成 `YYYY-MM-DD`，失败返回 `None`。
///
/// 只认**年在前**的写法（`2026-09-19` / `2026/9/7` / `2026.9.7` / `2026年9月7日`），
/// 因为中文与日文环境的导出绝大多数是这个形态。
///
/// 明确**不认** `D/M/YYYY` 与 `M/D/YYYY`：这两者在 12 号之前完全无法区分，
/// 猜错的代价是整列日期错位。这类文件留给用户自己在预览里改成文本。
///
/// 也**必须带分隔符**才认：只写 `2026` 要判成整数（年份列），
/// 判成日期就荒唐了。
///
/// 末尾的时间部分（Excel 导出常见 `2026/9/7 0:00`）直接丢掉 ——
/// 列类型是"日期"，不带时刻。
pub fn parse_date(raw: &str) -> Option<String> {
    let s = to_halfwidth(&trim_cell(raw));
    if s.is_empty() {
        return None;
    }
    // 去掉时间部分
    let date_part = s
        .split_once(' ')
        .map(|(d, _)| d)
        .unwrap_or(s.as_str())
        .trim();

    let sep = if date_part.contains('-') {
        '-'
    } else if date_part.contains('/') {
        '/'
    } else if date_part.contains('.') {
        '.'
    } else if date_part.contains('年') {
        '年'
    } else {
        return None;
    };

    let nums: Vec<&str> = if sep == '年' {
        // 2026年9月7日 → ["2026", "9", "7"]
        let head = date_part.strip_suffix('日').unwrap_or(date_part);
        let mut out = Vec::new();
        for seg in head.split('年') {
            for sub in seg.split('月') {
                if !sub.is_empty() {
                    out.push(sub);
                }
            }
        }
        out
    } else {
        date_part.split(sep).collect()
    };

    if nums.len() != 3 {
        return None;
    }
    let y: i32 = nums[0].trim().parse().ok()?;
    let m: u32 = nums[1].trim().parse().ok()?;
    let d: u32 = nums[2].trim().parse().ok()?;
    if !(1000..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    if d > days_in_month(y, m) {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

// ============================================================
// 布尔
// ============================================================

/// 列名是否像布尔语义（`是否结清` / `启用` / `已发货`）。
fn header_says_boolean(header: &str) -> bool {
    const KEYS: &[&str] = &[
        "是否", "结清", "启用", "标志", "标记", "完成", "已发", "已收", "有效", "checked", "enabled",
        "flag", "done", "active",
    ];
    let h = header.to_lowercase();
    KEYS.iter().any(|k| h.contains(k))
}

/// 列名是否像金额语义。
///
/// 只有"像金额"才敢往 `money` 上判 —— 金额列按「分」存整数，
/// 判错的后果是数值被悄悄乘以 100 或者干脆建表失败。
///
/// ⚠️ 这里的关键词表刻意**不用单字**：`价` 会命中「评价」，`额` 会命中「额度」，
/// 而"评价"是打分行不是金额行。判错的代价不对称，所以宁可少认几个。
fn header_says_money(header: &str) -> bool {
    const KEYS: &[&str] = &[
        "金额", "单价", "价格", "售价", "进价", "出价", "总价", "价税", "费用", "合计", "总计",
        "小计", "税额", "成本", "收入", "支出", "工资", "薪酬", "付款", "收款", "余额", "amount",
        "price", "cost", "total", "fee", "salary",
    ];
    let h = header.to_lowercase();
    KEYS.iter().any(|k| h.contains(k))
}

/// 列名是否像编号语义（订单号 / 卡号 / 身份证 / 工号）—— 一律文本。
///
/// ⚠️ 同样**不用单字 `id`**：它会在 `width`（宽度）里命中。
fn header_says_identifier(header: &str) -> bool {
    const KEYS: &[&str] = &[
        "编号", "单号", "订单号", "卡号", "身份证", "工号", "账号", "编码", "货号", "手机", "电话",
        "code", "serial",
    ];
    let h = header.to_lowercase();
    KEYS.iter().any(|k| h.contains(k))
}

// ============================================================
// 主判断
// ============================================================

/// 依据列名与一列的非空取值，判断该列该用什么类型。
///
/// `values` 是**已经清洗过空白**的原文（可以为空 —— 空列默认文本）。
/// 只采样前若干个值即可，不必给全量；但给的样本越多判断越准。
pub fn guess_type(header: &str, values: &[String]) -> TypeGuess {
    let mk = |ty: &str, conf: &str, reason: String| TypeGuess {
        ty: ty.to_string(),
        confidence: conf.to_string(),
        reason,
    };

    let vals: Vec<String> = values
        .iter()
        .map(|v| trim_cell(v))
        .filter(|v| !v.is_empty())
        .collect();

    if vals.is_empty() {
        return mk(T_TEXT, C_LOW, "这一列没有数据，默认文本（不去猜）".into());
    }
    let n = vals.len();

    // ---------- ① 列名说是编号，或值本身像编号 → 文本 ----------
    // 放在最前面：这是唯一一类"判成数字会永久改坏数据"的情况
    if header_says_identifier(header) {
        return mk(
            T_TEXT,
            C_HIGH,
            format!("列名像编号（{header}）—— 按文本存，数字类型会丢掉前导零"),
        );
    }

    // ---------- ② 日期 ----------
    if vals.iter().all(|v| parse_date(v).is_some()) {
        return mk(
            T_DATE,
            C_HIGH,
            format!("{n} 个值都能识别成日期（年在前）"),
        );
    }

    // ---------- ③ 布尔 ----------
    let lower: Vec<String> = vals.iter().map(|v| v.to_lowercase()).collect();
    let all_text_bool = lower.iter().all(|v| {
        matches!(
            v.as_str(),
            "是" | "否" | "true" | "false" | "yes" | "no" | "y" | "n" | "对" | "错" | "✓" | "✗"
                | "√" | "×"
        )
    });
    let only_01 = lower.iter().all(|v| v == "1" || v == "0");

    if all_text_bool {
        return mk(T_BOOLEAN, C_HIGH, format!("{n} 个值都是是/否这类写法"));
    }
    if only_01 && header_says_boolean(header) {
        return mk(
            T_BOOLEAN,
            C_MID,
            format!("列名像布尔且取值只有 0/1（{header}）"),
        );
    }
    if only_01 {
        // 没有列名线索时，0/1 更可能是计数或档位 —— 判成布尔会把它变成是/否
        return mk(
            T_INTEGER,
            C_MID,
            format!("{n} 个值都是 0 或 1，按整数存（若它本是「是否」请改）"),
        );
    }

    // ---------- ④ 数字 ----------
    let cleaned: Option<Vec<String>> = vals.iter().map(|v| clean_number(v)).collect();
    if let Some(nums) = cleaned {
        // 只要有一个像编号，整列按文本 —— 不能"大部分是数字就存数字"，
        // 那一行的编号就被毁掉了
        if let Some(bad) = nums.iter().find(|c| looks_like_id(c)) {
            let sample = vals[nums.iter().position(|c| *c == *bad).unwrap_or(0)].clone();
            return mk(
                T_TEXT,
                C_HIGH,
                format!("有值像编号（如「{sample}」）—— 按文本存，否则前导零或末几位会被改掉"),
            );
        }
        let max_dec = nums.iter().map(|c| decimals_of(c)).max().unwrap_or(0);
        let money_like = header_says_money(header);

        if money_like && max_dec <= 2 {
            return mk(
                T_MONEY,
                C_MID,
                format!("列名像金额（{header}），且最多 {max_dec} 位小数"),
            );
        }
        if max_dec == 0 {
            return mk(T_INTEGER, C_HIGH, format!("{n} 个值都是整数"));
        }
        if money_like {
            return mk(
                T_REAL,
                C_MID,
                format!(
                    "列名像金额，但出现了 {max_dec} 位小数 —— 金额列只接受 2 位，\
                     先按小数存，导入后可以再改"
                ),
            );
        }
        return mk(T_REAL, C_MID, format!("{n} 个值都是数字，最多 {max_dec} 位小数"));
    }

    // ---------- ⑤ 兜底：文本 ----------
    // 指出"是谁让这一列当不成数字"，用户才知道要不要去改源文件
    let offender = vals
        .iter()
        .find(|v| clean_number(v).is_none())
        .cloned()
        .unwrap_or_default();
    mk(
        T_TEXT,
        C_HIGH,
        format!("有值不是数字也不是日期（如「{}」）—— 按文本存", shave(&offender, 12)),
    )
}

/// 截断一个样例，避免把长内容整段塞进提示语。
fn shave(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("{}…", chars[..max].iter().collect::<String>())
    }
}

// ============================================================
// 表头行
// ============================================================

/// 一行的"像表头"程度打分。
fn header_score(rows: &[Vec<String>], i: usize) -> i32 {
    let row = &rows[i];
    let cells: Vec<String> = row.iter().map(|c| trim_cell(c)).collect();
    let non_empty = cells.iter().filter(|c| !c.is_empty()).count();
    if non_empty == 0 {
        return -1000;
    }
    // 表头几乎都是文字
    let text_cells = cells
        .iter()
        .filter(|c| !c.is_empty() && clean_number(c).is_none() && parse_date(c).is_none())
        .count();
    // 值互不相同（表头不会重复；而数据行经常重复）
    let uniq = {
        let mut s = cells.clone();
        s.sort();
        s.dedup();
        s.len()
    };

    let mut score = 0;
    score += non_empty as i32 * 2;
    score += text_cells as i32 * 3;
    if uniq == non_empty {
        score += 2;
    }
    // 下面几行如果是数字，就更加说明这一行是表头
    if i + 1 < rows.len() {
        let below_numeric = rows[i + 1]
            .iter()
            .filter(|c| {
                let t = trim_cell(c);
                !t.is_empty() && clean_number(&t).is_some()
            })
            .count();
        score += below_numeric as i32 * 3;
    }
    // 数据行里的空单元格很多；表头一般比较整齐
    let blanks = cells.len() - non_empty;
    score -= blanks as i32;
    score
}

/// 建议表头在第几行。返回 `(0 基行号, 置信度, 依据)`。
///
/// **这是"建议"，不是"决定"。** 界面必须让用户能看到前几行原文并自己改 ——
/// 用户的表里表头在第 3 行、前面还有标题和空行是常态，"猜"这件事本身就不该做死。
pub fn suggest_header_row(rows: &[Vec<String>]) -> (usize, String, String) {
    if rows.is_empty() {
        return (0, C_LOW.to_string(), "没有数据行，默认第 1 行".into());
    }
    let limit = rows.len().min(8);
    let mut best = 0usize;
    let mut best_score = i32::MIN;
    for i in 0..limit {
        let s = header_score(rows, i);
        if s > best_score {
            best_score = s;
            best = i;
        }
    }
    let cells: Vec<String> = rows[best].iter().map(|c| trim_cell(c)).collect();
    let non_empty = cells.iter().filter(|c| !c.is_empty()).count();
    let text_cells = cells
        .iter()
        .filter(|c| !c.is_empty() && clean_number(c).is_none() && parse_date(c).is_none())
        .count();
    let uniq = {
        let mut s: Vec<String> = cells.iter().filter(|c| !c.is_empty()).cloned().collect();
        s.sort();
        s.dedup();
        s.len()
    };
    let below = if best + 1 < rows.len() {
        rows[best + 1]
            .iter()
            .filter(|c| {
                let t = trim_cell(c);
                !t.is_empty() && clean_number(&t).is_some()
            })
            .count()
    } else {
        0
    };

    // 置信度只看"这一行本身像不像表头"：全非空、全是文字、互不相同。
    // 原来还要求"下面至少有 2 列是数字"，那条太严 —— 真实台账里日期列、
    // 编号列都不是数字，一列数字就够说明问题了。
    let conf = if non_empty >= 2 && text_cells == non_empty && uniq == non_empty {
        C_HIGH
    } else if non_empty >= 1 {
        C_MID
    } else {
        C_LOW
    };
    let reason = if below >= 2 {
        format!(
            "第 {} 行是文字且互不相同，从第 {} 行起就是数字了",
            best + 1,
            best + 2
        )
    } else if below == 1 {
        format!("第 {} 行是文字且互不相同，下一行开始出现数字", best + 1)
    } else {
        format!("第 {} 行的内容最像表头（非空 {} 列）", best + 1, non_empty)
    };
    (best, conf.to_string(), reason)
}

/// 为一批表头生成**唯一且非空**的列名。
///
/// 空表头给 `列N`（N 是它在原表里的序号，1 基）—— 用序号而不是"未命名"，
/// 用户一眼能对上原表的第几列。重名加后缀，因为 `schema.rs` 要求列名唯一。
pub fn suggest_names(header: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(header.len());
    let mut seen: Vec<String> = Vec::new();
    for (i, raw) in header.iter().enumerate() {
        let base = {
            let t = trim_cell(raw);
            if t.is_empty() {
                format!("列{}", i + 1)
            } else {
                t
            }
        };
        let mut name = base.clone();
        let mut k = 2;
        while seen.iter().any(|s| s == &name) {
            name = format!("{base}{k}");
            k += 1;
        }
        seen.push(name.clone());
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---------- 编号类：判成数字就会永久改坏数据 ----------

    #[test]
    fn 前导零的工号必须判成文本() {
        let g = guess_type("工号", &v(&["007", "0123", "0042"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
        assert_eq!(g.confidence, C_HIGH);
    }

    #[test]
    fn 十六位以上的订单号必须判成文本() {
        let g = guess_type("订单号", &v(&["202609190001234567", "202609190001234568"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
    }

    #[test]
    fn 没有线索的长数字也按文本_因为双精度存不下() {
        // 列名不带"编号"字样，但值本身有 18 位
        let g = guess_type("参考", &v(&["123456789012345678"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
        assert!(g.reason.contains("编号"), "理由要说清是哪一类问题：{}", g.reason);
    }

    #[test]
    fn 科学计数法不认_转文本() {
        let g = guess_type("型号", &v(&["1e5", "2e3"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
    }

    #[test]
    fn 单个零与零点五不算前导零() {
        assert!(!looks_like_id("0"));
        assert!(!looks_like_id("0.5"));
        assert!(looks_like_id("007"));
        assert!(looks_like_id("0123"));
    }

    // ---------- 金额 ----------

    #[test]
    fn 列名像金额且两位小数判成金额() {
        let g = guess_type("金额", &v(&["12.34", "0.50", "1000.00"]));
        assert_eq!(g.ty, T_MONEY, "{}", g.reason);
    }

    #[test]
    fn 列名像金额但有三位小数只判成小数_不判金额() {
        // 金额列只接受 2 位小数，判成 money 会让导入直接失败
        let g = guess_type("单价", &v(&["12.345", "1.234"]));
        assert_eq!(g.ty, T_REAL, "{}", g.reason);
        assert!(g.reason.contains("2 位"), "要说明为什么没判金额：{}", g.reason);
    }

    #[test]
    fn 整数金额也判金额() {
        let g = guess_type("费用合计", &v(&["100", "2000"]));
        assert_eq!(g.ty, T_MONEY, "{}", g.reason);
    }

    #[test]
    fn 没有金额线索的小数判成小数() {
        let g = guess_type("温度", &v(&["36.5", "37.2"]));
        assert_eq!(g.ty, T_REAL, "{}", g.reason);
    }

    // ---------- 整数与布尔 ----------

    #[test]
    fn 纯整数判整数() {
        let g = guess_type("数量", &v(&["3", "12", "0"]));
        assert_eq!(g.ty, T_INTEGER, "{}", g.reason);
        assert_eq!(g.confidence, C_HIGH);
    }

    #[test]
    fn 只有零和一且列名像布尔才判布尔() {
        let yes = guess_type("是否结清", &v(&["1", "0", "1"]));
        assert_eq!(yes.ty, T_BOOLEAN, "{}", yes.reason);

        // 同样的取值，列名没有布尔线索 —— 判成整数（可能是计数或档位）
        let no = guess_type("档位", &v(&["1", "0", "1"]));
        assert_eq!(no.ty, T_INTEGER, "{}", no.reason);
    }

    #[test]
    fn 是否这类写法直接判布尔() {
        let g = guess_type("状态", &v(&["是", "否", "是"]));
        assert_eq!(g.ty, T_BOOLEAN, "{}", g.reason);
    }

    // ---------- 日期 ----------

    #[test]
    fn 多种年在前写法都认成日期() {
        for col in [
            v(&["2026-09-19", "2026-01-02"]),
            v(&["2026/9/7", "2026/12/31"]),
            v(&["2026.9.7", "2026.1.1"]),
            v(&["2026年9月7日", "2026年1月1日"]),
            v(&["2026/9/7 0:00", "2026/1/1 12:30"]),
        ] {
            let g = guess_type("日期", &col);
            assert_eq!(g.ty, T_DATE, "漏判了：{col:?} → {}", g.reason);
        }
    }

    #[test]
    fn 日期要带分隔符_单独的年份是整数() {
        let g = guess_type("年份", &v(&["2026", "2025"]));
        assert_eq!(g.ty, T_INTEGER, "{}", g.reason);
    }

    #[test]
    fn 不认日月在前的写法_绝不猜() {
        // 03/04/2026 到底是 3 月 4 日还是 4 月 3 日？猜错的代价是整列日期错位
        let g = guess_type("日期", &v(&["03/04/2026", "05/06/2026"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
    }

    #[test]
    fn 非法日期不算日期() {
        assert_eq!(parse_date("2026-02-30"), None, "2 月没有 30 号");
        assert_eq!(parse_date("2026-13-01"), None, "没有 13 月");
        assert_eq!(parse_date("2026-09-19"), Some("2026-09-19".into()));
        assert_eq!(parse_date("2024-02-29"), Some("2024-02-29".into()), "闰年有 2 月 29");
        assert_eq!(parse_date("2026-02-29"), None, "平年没有 2 月 29");
    }

    // ---------- 清洗 ----------

    #[test]
    fn 千分位与货币符号能当数字() {
        assert_eq!(clean_number("¥1,234.50").as_deref(), Some("1234.50"));
        assert_eq!(clean_number("1,234"), Some("1234".into()));
        assert_eq!(clean_number("$12"), Some("12".into()));
        assert_eq!(clean_number("RMB 100"), Some("100".into()));
        assert_eq!(clean_number("1234.5 元"), Some("1234.5".into()));
        assert_eq!(clean_number("(123.45)"), Some("-123.45".into()), "会计括号负数");
        assert_eq!(clean_number("１２３"), Some("123".into()), "全角数字");
    }

    #[test]
    fn 不合法的千分位不当数字() {
        // 1,23 不是合法的千分位写法，可能是笔误也可能是别的含义 —— 不猜
        assert_eq!(clean_number("1,23"), None);
        assert_eq!(clean_number("12,3456"), None);
    }

    #[test]
    fn 万元不换算_转文本() {
        // 1万元 到底是 10000 还是 1？这里不猜 —— 猜错就是差一万倍
        assert_eq!(clean_number("3万元"), None);
        let g = guess_type("预算", &v(&["3万元", "5万元"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
    }

    #[test]
    fn 不换行空格要去掉() {
        assert_eq!(trim_cell("\u{00a0}甲\u{00a0}"), "甲");
        assert_eq!(trim_cell("  乙  "), "乙");
    }

    // ---------- 兜底 ----------

    #[test]
    fn 空列默认文本且标注低置信() {
        let g = guess_type("备注", &v(&[]));
        assert_eq!(g.ty, T_TEXT);
        assert_eq!(g.confidence, C_LOW);
    }

    #[test]
    fn 混进非数字时按文本并指出是谁() {
        let g = guess_type("数量", &v(&["3", "待定", "5"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
        assert!(g.reason.contains("待定"), "要指出让这一列当不成数字的值：{}", g.reason);
    }

    #[test]
    fn 只要有一个像编号整列就按文本() {
        // 不能"大部分是数字就存数字" —— 那一行的编号会被毁掉
        let g = guess_type("单据", &v(&["123", "456", "007"]));
        assert_eq!(g.ty, T_TEXT, "{}", g.reason);
    }

    // ---------- 表头行 ----------

    #[test]
    fn 表头在第三行时能认出来() {
        let rows = vec![
            v(&["2026 年度报销台账"]),                       // 标题行
            v(&["制表：财务部"]),                             // 说明行
            v(&["事项", "金额", "日期"]),                      // ← 真正的表头
            v(&["打车", "12.34", "2026-09-19"]),
            v(&["餐费", "50.00", "2026-09-20"]),
        ];
        let (idx, conf, reason) = suggest_header_row(&rows);
        assert_eq!(idx, 2, "应认出第 3 行（0 基为 2）：{reason}");
        assert_eq!(conf, C_HIGH, "{reason}");
        assert!(reason.contains("第 3 行"), "{reason}");
    }

    #[test]
    fn 表头就在第一行时也认得出() {
        let rows = vec![
            v(&["事项", "金额"]),
            v(&["打车", "12.34"]),
            v(&["餐费", "50.00"]),
        ];
        let (idx, _, _) = suggest_header_row(&rows);
        assert_eq!(idx, 0);
    }

    #[test]
    fn 空表返回第零行且低置信() {
        let (idx, conf, _) = suggest_header_row(&[]);
        assert_eq!(idx, 0);
        assert_eq!(conf, C_LOW);
    }

    // ---------- 列名 ----------

    #[test]
    fn 空表头按原表序号补名() {
        let names = suggest_names(&v(&["事项", "", "金额", ""]));
        assert_eq!(names, v(&["事项", "列2", "金额", "列4"]));
    }

    #[test]
    fn 重名列名加后缀() {
        let names = suggest_names(&v(&["金额", "金额", "金额"]));
        assert_eq!(names, v(&["金额", "金额2", "金额3"]));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn 名字里的空白会被去掉() {
        let names = suggest_names(&v(&["  事项  ", "金额\u{00a0}"]));
        assert_eq!(names, v(&["事项", "金额"]));
    }
}
