//! 只读解析旧版的 SQLite 数据库（`main.db`）。
//!
//! **为什么自己写解析器，而不是引 `rusqlite`**：
//! 1. ADR-0021/0022 刚把 SQL 依赖从项目里去掉（体积 + 供应链），加回来是倒退；
//! 2. 我们只需要"**读**"这一件事，而且只需要读**我们自己的**旧库（结构简单）；
//! 3. 红线 R2 要求新增依赖要在 DECISIONS_LOG 写理由 —— 这个理由写不漂亮。
//!
//! **为什么要做这件事**：旧库换引擎后读不了。原来的说法是"让用户用旧版导出成
//! Excel 再导入"，但**用户很可能已经没有旧版程序了**（卸载了、换机了），
//! 只剩一个 `main.db` —— 那句指引等于帮不上忙，而里面是他唯一的数据。
//!
//! **支持范围**（只读）：表 B-tree、记录解码、溢出页。
//! **不支持**：索引、视图、触发器、WITHOUT ROWID 表、加密库。
//! 遇到不支持的地方**明确报错**，不猜 —— 这是用户唯一的数据，猜错比不猜更糟。
//!
//! 格式依据：https://www.sqlite.org/fileformat2.html（公开且稳定）

use std::path::{Path, PathBuf};

/// SQLite 文件头固定 16 字节魔数。
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Clone, PartialEq)]
pub enum LegacyValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl LegacyValue {
    /// 给界面用的展示文本。值与类型分开给（前端要按类型决定怎么显示）。
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            LegacyValue::Null => serde_json::Value::Null,
            LegacyValue::Int(i) => serde_json::json!(i),
            LegacyValue::Real(f) => serde_json::json!(f),
            LegacyValue::Text(s) => serde_json::json!(s),
            LegacyValue::Blob(b) => {
                // 二进制不给前端传原文（可能是图片、也可能是乱码），只给长度
                serde_json::json!(format!("<二进制 {} 字节>", b.len()))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LegacyTable {
    pub name: String,
    /// 建表语句（原样保留 —— 里面有列名和类型，界面要展示给用户看）
    pub sql: String,
    pub root_page: u32,
    /// 从建表语句里解出来的列名
    pub columns: Vec<String>,
    /// 与 columns 一一对应的**声明类型**（解不出来就是空串）。
    /// 导入时按它决定新表的列类型 —— 金额列若一律按文本存，
    /// "按分存"的语义就没了，之后排序和计算都会出问题。
    pub column_types: Vec<String>,
}

pub struct LegacyDb {
    path: PathBuf,
    bytes: Vec<u8>,
    page_size: usize,
    /// 文本编码：1=UTF-8、2=UTF-16le、3=UTF-16be
    encoding: u32,
    page_count: u32,
}

type R<T> = Result<T, String>;

/// 手写 Debug：**不打印文件内容**。
/// 这个结构里揣着整个库的字节（可能几百 MB），derive 出来的 Debug 一旦被
/// 打印（测试失败、日志、unwrap 的报错）就会把日志冲爆。只暴露摘要。
impl std::fmt::Debug for LegacyDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyDb")
            .field("path", &self.path)
            .field("page_size", &self.page_size)
            .field("page_count", &self.page_count)
            .finish()
    }
}

impl LegacyDb {
    pub fn open(path: &Path) -> R<Self> {
        let bytes = std::fs::read(path).map_err(|e| format!("读不了这个文件：{e}"))?;
        if bytes.len() < 100 {
            return Err("文件太小，不像 SQLite 数据库".to_string());
        }
        if &bytes[0..16] != MAGIC {
            // 旧版 DeskBase 的库也可能是别的格式，如实说
            return Err("这不是 SQLite 数据库文件（文件头对不上）".to_string());
        }
        // 页大小是 2 字节大端；值 1 是"65536"的特例（放不进 2 字节）
        let raw_ps = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
        let page_size = if raw_ps == 1 { 65536 } else { raw_ps };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(format!("页大小看起来不对：{page_size}"));
        }
        let encoding = u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);
        let page_count =
            u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);

        let db = LegacyDb {
            path: path.to_path_buf(),
            bytes,
            page_size,
            encoding,
            page_count,
        };
        // 页数对不上就说明文件被截断过 —— 这种必须马上说，后面读出来的是半截数据
        let actual_pages = db.bytes.len() / db.page_size;
        if page_count as usize > actual_pages {
            return Err(format!(
                "文件不完整：头部写着 {} 页，实际只有 {actual_pages} 页",
                page_count
            ));
        }
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 页大小。**只给测试用** —— 验证头部解析对了没有。
    /// 生产代码不需要它（读页的逻辑内部用字段），所以标 cfg(test)，
    /// 免得在 release 构建里变成一个从未使用的报错。
    #[cfg(test)]
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    fn page(&self, n: u32) -> R<&[u8]> {
        if n == 0 {
            return Err("页号 0 不存在（SQLite 从 1 开始编号）".to_string());
        }
        let start = (n as usize - 1) * self.page_size;
        let end = start + self.page_size;
        if end > self.bytes.len() {
            return Err(format!("页 {n} 超出文件范围"));
        }
        Ok(&self.bytes[start..end])
    }

    /// 从 sqlite_master（第 1 页）读出所有表。
    pub fn tables(&self) -> R<Vec<LegacyTable>> {
        // sqlite_master 的列序：type, name, tbl_name, rootpage, sql
        let rows = self.scan_table(1, usize::MAX)?;
        let mut out = Vec::new();
        for r in rows {
            let ty = match r.first() {
                Some(LegacyValue::Text(s)) => s.clone(),
                _ => continue,
            };
            if ty != "table" {
                continue; // 索引/视图/触发器这次不做
            }
            let name = match r.get(1) {
                Some(LegacyValue::Text(s)) => s.clone(),
                _ => continue,
            };
            // sqlite_ 开头的是内部表（sqlite_sequence 等），不是用户的表
            if name.starts_with("sqlite_") {
                continue;
            }
            let root_page = match r.get(3) {
                Some(LegacyValue::Int(i)) => *i as u32,
                _ => continue,
            };
            let sql = match r.get(4) {
                Some(LegacyValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let decls = parse_column_decls(&sql);
            let columns: Vec<String> = decls.iter().map(|(n, _)| n.clone()).collect();
            let column_types: Vec<String> = decls.iter().map(|(_, t)| t.clone()).collect();
            out.push(LegacyTable {
                name,
                sql,
                root_page,
                columns,
                column_types,
            });
        }
        Ok(out)
    }

    /// 读一张表的前 `limit` 行。传 `usize::MAX` 表示全读。
    pub fn rows(&self, table: &LegacyTable, limit: usize) -> R<Vec<Vec<LegacyValue>>> {
        self.scan_table(table.root_page, limit)
    }

    /// 遍历一张表的 B-tree。
    ///
    /// 内部页存的是"子页指针 + 分隔键"，真正的记录都在叶子页上 ——
    /// 所以这里递归下去，只在叶子页取数据。
    fn scan_table(&self, root: u32, limit: usize) -> R<Vec<Vec<LegacyValue>>> {
        let mut out = Vec::new();
        let mut queue = vec![root];
        let mut visited = std::collections::HashSet::new();
        while let Some(pn) = queue.pop() {
            // 防环：损坏的文件里可能有自指的页，转下去会挂死
            if !visited.insert(pn) {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            let page = self.page(pn)?;
            let header_off = if pn == 1 { 100 } else { 0 };
            let page_type = page[header_off];
            let cell_count =
                u16::from_be_bytes([page[header_off + 3], page[header_off + 4]]) as usize;
            match page_type {
                // 13 = 表叶子页
                13 => {
                    let ptrs = header_off + 8;
                    for i in 0..cell_count {
                        if out.len() >= limit {
                            break;
                        }
                        let off = ptrs + i * 2;
                        if off + 1 >= page.len() {
                            break;
                        }
                        let cell_off =
                            u16::from_be_bytes([page[off], page[off + 1]]) as usize;
                        if let Some(rec) = self.read_leaf_cell(page, cell_off)? {
                            out.push(rec);
                        }
                    }
                }
                // 5 = 表内部页：8 字节头之后是"最右子页指针"，末尾 4 字节
                5 => {
                    let ptrs = header_off + 12;
                    for i in 0..cell_count {
                        let off = ptrs + i * 2;
                        if off + 1 >= page.len() {
                            break;
                        }
                        let cell_off =
                            u16::from_be_bytes([page[off], page[off + 1]]) as usize;
                        if cell_off + 4 <= page.len() {
                            let child = u32::from_be_bytes([
                                page[cell_off],
                                page[cell_off + 1],
                                page[cell_off + 2],
                                page[cell_off + 3],
                            ]);
                            queue.push(child);
                        }
                    }
                    // 最右指针
                    let right_off = header_off + 8;
                    if right_off + 4 <= page.len() {
                        let right = u32::from_be_bytes([
                            page[right_off],
                            page[right_off + 1],
                            page[right_off + 2],
                            page[right_off + 3],
                        ]);
                        queue.push(right);
                    }
                }
                other => {
                    return Err(format!(
                        "页 {pn} 的类型是 {other}，只支持表页（索引页/空闲页不处理）"
                    ));
                }
            }
        }
        Ok(out)
    }

    /// 读一个表叶子页的单元 → 一行记录。
    fn read_leaf_cell(&self, page: &[u8], cell_off: usize) -> R<Option<Vec<LegacyValue>>> {
        if cell_off >= page.len() {
            return Ok(None);
        }
        let (payload_len, n1) = read_varint(page, cell_off);
        let (_rowid, n2) = read_varint(page, cell_off + n1);
        let payload_len = payload_len as usize;
        let payload_start = cell_off + n1 + n2;

        let usable = self.page_size;
        // 本地载荷的阈值（thttps://www.sqlite.org/fileformat2.html#b_tree_pages）
        // 表叶子页：X = U - 35
        let x = usable.saturating_sub(35);
        if payload_len <= x {
            if payload_start + payload_len > page.len() {
                return Ok(None);
            }
            return Ok(Some(self.decode_record(
                &page[payload_start..payload_start + payload_len],
            )?));
        }
        // 有溢出页：本地留 M 字节，其余在页链里
        let m = ((usable - 12) * 32 / 255) - 23;
        let k = m + ((payload_len - m) % (usable - 4));
        let local = if k <= x { k } else { m };
        if payload_start + local + 4 > page.len() {
            return Ok(None);
        }
        let mut buf = page[payload_start..payload_start + local].to_vec();
        let mut next = u32::from_be_bytes([
            page[payload_start + local],
            page[payload_start + local + 1],
            page[payload_start + local + 2],
            page[payload_start + local + 3],
        ]);
        let mut remaining = payload_len - local;
        let mut guard = 0;
        while next != 0 && remaining > 0 {
            guard += 1;
            if guard > 100_000 {
                return Err("溢出页链太长，文件可能损坏".to_string());
            }
            let op = self.page(next)?;
            let avail = (usable - 4).min(remaining);
            buf.extend_from_slice(&op[4..4 + avail]);
            remaining -= avail;
            next = u32::from_be_bytes([op[0], op[1], op[2], op[3]]);
        }
        Ok(Some(self.decode_record(&buf)?))
    }

    /// 解码一条记录（record format）。
    ///
    /// 结构：头部长度 varint + 每个字段的 serial type varint + 字段数据。
    fn decode_record(&self, payload: &[u8]) -> R<Vec<LegacyValue>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let (header_len, mut pos) = read_varint(payload, 0);
        let header_len = header_len as usize;
        if header_len > payload.len() {
            return Err("记录头部长度超出范围，文件可能损坏".to_string());
        }
        // 先收集 serial types
        let mut types = Vec::new();
        while pos < header_len {
            let (t, n) = read_varint(payload, pos);
            types.push(t);
            pos += n;
        }
        // 再按类型取值
        let mut data_pos = header_len;
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let (v, consumed) = self.decode_value(t, payload, data_pos)?;
            data_pos += consumed;
            out.push(v);
        }
        Ok(out)
    }

    /// serial type → 值。
    fn decode_value(&self, t: u64, buf: &[u8], pos: usize) -> R<(LegacyValue, usize)> {
        let need = |n: usize| -> R<()> {
            if pos + n > buf.len() {
                Err("字段数据不完整，文件可能损坏".to_string())
            } else {
                Ok(())
            }
        };
        match t {
            0 => Ok((LegacyValue::Null, 0)),
            1 => {
                need(1)?;
                Ok((LegacyValue::Int(buf[pos] as i8 as i64), 1))
            }
            2 => {
                need(2)?;
                Ok((
                    LegacyValue::Int(i16::from_be_bytes([buf[pos], buf[pos + 1]]) as i64),
                    2,
                ))
            }
            3 => {
                need(3)?;
                let v = ((buf[pos] as i64) << 16)
                    | ((buf[pos + 1] as i64) << 8)
                    | (buf[pos + 2] as i64);
                // 24 位是有符号的
                let v = if v & 0x800000 != 0 { v - 0x1000000 } else { v };
                Ok((LegacyValue::Int(v), 3))
            }
            4 => {
                need(4)?;
                Ok((
                    LegacyValue::Int(i32::from_be_bytes([
                        buf[pos],
                        buf[pos + 1],
                        buf[pos + 2],
                        buf[pos + 3],
                    ]) as i64),
                    4,
                ))
            }
            5 => {
                need(6)?;
                let mut b = [0u8; 8];
                b[2..].copy_from_slice(&buf[pos..pos + 6]);
                Ok((LegacyValue::Int(i64::from_be_bytes(b) >> 16), 6))
            }
            6 => {
                need(8)?;
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[pos..pos + 8]);
                Ok((LegacyValue::Int(i64::from_be_bytes(b)), 8))
            }
            7 => {
                need(8)?;
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[pos..pos + 8]);
                Ok((LegacyValue::Real(f64::from_be_bytes(b)), 8))
            }
            8 => Ok((LegacyValue::Int(0), 0)),
            9 => Ok((LegacyValue::Int(1), 0)),
            10 | 11 => Err(format!("serial type {t} 是内部保留值，不该出现在文件里")),
            n if n >= 12 && n % 2 == 0 => {
                let len = ((n - 12) / 2) as usize;
                need(len)?;
                Ok((LegacyValue::Blob(buf[pos..pos + len].to_vec()), len))
            }
            n => {
                // n >= 13 且为奇数 → TEXT
                let len = ((n - 13) / 2) as usize;
                need(len)?;
                let raw = &buf[pos..pos + len];
                let s = match self.encoding {
                    2 => decode_utf16(raw, true),
                    3 => decode_utf16(raw, false),
                    // 1 = UTF-8（默认）；未知编码按 UTF-8 试，失败就换成替换字符，
                    // 不让一个字把整行卡住
                    _ => String::from_utf8_lossy(raw).to_string(),
                };
                Ok((LegacyValue::Text(s), len))
            }
        }
    }
}

/// 变长整数（1–9 字节，大端，每字节 7 位有效）。
fn read_varint(buf: &[u8], pos: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    for i in 0..9 {
        let idx = pos + i;
        if idx >= buf.len() {
            return (v, i.max(1));
        }
        let b = buf[idx];
        if i == 8 {
            // 第 9 个字节是完整的 8 位
            v = (v << 8) | b as u64;
            return (v, 9);
        }
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    (v, 9)
}

fn decode_utf16(raw: &[u8], little: bool) -> String {
    let u: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&u)
}

/// SQLite 的声明类型 → DeskBase 的列类型。
///
/// 为什么不一律按文本存：金额列用文本会丢掉"按分存"的语义，
/// 之后排序和计算都会出问题 —— 导入的意义是"接着用"，不是"存下来看看"。
///
/// 判断方式照着 SQLite 官方的类型亲和性规则来（类型名是宽松的，
/// INT / INTEGER / VARCHAR(10) / BOOLEAN 都合法），所以按**关键字包含**判，
/// 不要求完全匹配。
pub fn map_decl_type(decl: &str) -> &'static str {
    let d = decl.trim().to_uppercase();
    if d.is_empty() {
        return "text";
    }
    if d.contains("INT") {
        return "integer";
    }
    if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        return "real";
    }
    // CHAR / CLOB / TEXT / 其他一律按文本 —— 文本能装下任何东西，不会丢数据
    "text"
}

/// 解出 (列名, 声明类型)。列名后面没写类型时给空串。
pub fn parse_column_decls(sql: &str) -> Vec<(String, String)> {
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let Some(close) = sql.rfind(')') else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    let body = &sql[open + 1..close];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                push_decl(&mut out, &cur);
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    push_decl(&mut out, &cur);
    out
}

fn push_decl(out: &mut Vec<(String, String)>, raw: &str) {
    let s = raw.trim();
    if s.is_empty() {
        return;
    }
    let mut it = s.split_whitespace();
    let first = it.next().unwrap_or("");
    let name = first
        .trim_matches('"')
        .trim_matches('`')
        .trim_matches('[')
        .trim_matches(']')
        .to_string();
    if name.is_empty() {
        return;
    }
    let upper = name.to_uppercase();
    if matches!(
        upper.as_str(),
        "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "CONSTRAINT"
    ) {
        return;
    }
    // 类型是第二个词（可能带括号，如 VARCHAR(10)）。没有就留空。
    let ty = it.next().unwrap_or("").to_string();
    out.push((name, ty));
}

/// 只取列名（内部复用 parse_column_decls）。**只给测试用** ——
/// 生产代码走 tables()，它要的是"列名 + 类型"两样。
/// 为什么不各写一套实现：两套解析迟早分叉，而这两件事本来就是同一份解析的两种读法。
#[cfg(test)]
pub fn parse_create_table_columns(sql: &str) -> Vec<String> {
    parse_column_decls(sql)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 变长整数解得对() {
        // 0x7f = 127（单字节）
        assert_eq!(read_varint(&[0x7f], 0), (127, 1));
        // 0x81 0x00 = 128（两字节：1<<7 | 0）
        assert_eq!(read_varint(&[0x81, 0x00], 0), (128, 2));
        // 0x81 0x01 = 129
        assert_eq!(read_varint(&[0x81, 0x01], 0), (129, 2));
        // 三字节上限
        assert_eq!(read_varint(&[0xff, 0xff, 0x7f], 0), (0x1FFFFF, 3));
    }

    #[test]
    fn 建表语句能抠出列名() {
        assert_eq!(
            parse_create_table_columns("CREATE TABLE 客户 (客户名 TEXT, 余额 INTEGER)"),
            vec!["客户名", "余额"]
        );
        // 带主键与表级约束
        assert_eq!(
            parse_create_table_columns(
                "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, PRIMARY KEY (a))"
            ),
            vec!["a", "b"]
        );
        // 列类型里带括号（不要把它当成列分隔）
        assert_eq!(
            parse_create_table_columns("CREATE TABLE t (a VARCHAR(10), b DECIMAL(10,2))"),
            vec!["a", "b"]
        );
        // 解不出来就给空 —— 不猜
        assert!(parse_create_table_columns("not a create statement").is_empty());
    }

    /// 拼一个最小但**格式正确**的 SQLite 文件：第 1 页是 sqlite_master，第 2 页是一张表。
    ///
    /// 为什么自己拼字节：项目里没有 sqlite 样本文件，测试里也造不出一个
    /// （引 sqlite3 工具就又是依赖）。拼的过程本身是对格式的复核 ——
    /// 哪里理解错了，这里会先暴露出来。
    ///
    /// 布局（页大小 512）：
    /// ```text
    /// p1: [0..100 文件头][100 页类型13][103 单元数1][105 内容起点][108 指针→459][459.. 单元]
    /// p2: [0 页类型13][3 单元数1][5 内容起点][8 指针→501][501.. 单元]
    /// ```
    fn build_two_page_db() -> Vec<u8> {
        const PS: usize = 512;
        let mut f = vec![0u8; PS * 2];

        // ---- 文件头（偏移严格按 https://www.sqlite.org/fileformat2.html#the_database_header）----
        f[0..16].copy_from_slice(MAGIC);
        f[16..18].copy_from_slice(&(PS as u16).to_be_bytes());
        f[18] = 1; // 文件格式写版本
        f[19] = 1; // 文件格式读版本
        f[20] = 0; // 每页保留字节
        f[21] = 64; // 最大嵌入载荷分数
        f[22] = 32;
        f[23] = 32;
        f[28..32].copy_from_slice(&2u32.to_be_bytes()); // 数据库页数 = 2
        f[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
        f[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema 格式号
        f[56..60].copy_from_slice(&1u32.to_be_bytes()); // 文本编码 = UTF-8

        // ---- 第 1 页：sqlite_master 的一行 ----
        // 列序：type, name, tbl_name, rootpage, sql
        let sql = "CREATE TABLE t1 (a TEXT, b INTEGER)";
        let mut payload = Vec::new();
        payload.push(6u8); // 头部长度：1 + 5 个 serial type
        payload.push(23); // TEXT(5) = "table"
        payload.push(17); // TEXT(2) = "t1"
        payload.push(17); // TEXT(2) = "t1"
        payload.push(1); // INT(1 字节) = 2
        payload.push(13 + 2 * sql.len() as u8); // TEXT(sql)
        payload.extend_from_slice(b"table");
        payload.extend_from_slice(b"t1");
        payload.extend_from_slice(b"t1");
        payload.push(2); // rootpage = 2
        payload.extend_from_slice(sql.as_bytes());

        let mut cell = Vec::new();
        cell.push(payload.len() as u8); // 载荷长度 varint（<128 所以一个字节）
        cell.push(1); // rowid = 1
        cell.extend_from_slice(&payload);
        let cell_off = PS - cell.len();
        f[100] = 13; // 表叶子页
        f[103..105].copy_from_slice(&1u16.to_be_bytes()); // 单元数
        f[105..107].copy_from_slice(&(cell_off as u16).to_be_bytes()); // 内容起点
        f[108..110].copy_from_slice(&(cell_off as u16).to_be_bytes()); // 单元指针
        f[cell_off..cell_off + cell.len()].copy_from_slice(&cell);

        // ---- 第 2 页：t1 的一行 (a="hello", b=42) ----
        let mut p2 = Vec::new();
        p2.push(3u8); // 头部长度：1 + 2 个 serial type
        p2.push(23); // TEXT(5)
        p2.push(1); // INT(1 字节)
        p2.extend_from_slice(b"hello");
        p2.push(42);
        let mut cell2 = Vec::new();
        cell2.push(p2.len() as u8);
        cell2.push(1); // rowid = 1
        cell2.extend_from_slice(&p2);
        let base = PS;
        let c2 = PS - cell2.len();
        f[base] = 13;
        f[base + 3..base + 5].copy_from_slice(&1u16.to_be_bytes());
        f[base + 5..base + 7].copy_from_slice(&(c2 as u16).to_be_bytes());
        f[base + 8..base + 10].copy_from_slice(&(c2 as u16).to_be_bytes());
        f[base + c2..base + c2 + cell2.len()].copy_from_slice(&cell2);

        f
    }

    #[test]
    fn 能读出表清单和列名() {
        let d = std::env::temp_dir().join("dkb_legacy_min.db");
        std::fs::write(&d, build_two_page_db()).unwrap();
        let db = LegacyDb::open(&d).unwrap();
        assert_eq!(db.page_size(), 512);
        let ts = db.tables().unwrap();
        assert_eq!(ts.len(), 1, "应当只有一张表");
        assert_eq!(ts[0].name, "t1");
        assert_eq!(ts[0].columns, vec!["a", "b"], "列名要从建表语句里解出来");
        assert_eq!(ts[0].root_page, 2);
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn 能读出表里的行和值() {
        let d = std::env::temp_dir().join("dkb_legacy_rows.db");
        std::fs::write(&d, build_two_page_db()).unwrap();
        let db = LegacyDb::open(&d).unwrap();
        let ts = db.tables().unwrap();
        let rows = db.rows(&ts[0], 100).unwrap();
        assert_eq!(rows.len(), 1, "应当有一行");
        assert_eq!(rows[0][0], LegacyValue::Text("hello".to_string()));
        assert_eq!(rows[0][1], LegacyValue::Int(42));
        let _ = std::fs::remove_file(&d);
    }

    /// **整条导入链**：从旧库读出来 → 建新表 → 写行 → 读回确认。
    ///
    /// 为什么这个测试值得单写：上面那些只证明"解析对了"，
    /// 而用户要的是"数据真的进了新库"。中间还有类型映射、行补齐、
    /// 值转换三段胶水，任何一段错了，用户看到的都是"导进去是空的"。
    #[test]
    fn 导入链路_从旧库读出来能建新表写进去() {
        use crate::model::{ColType, ColumnDef, Db, TableSpec};

        let old = std::env::temp_dir().join("dkb_legacy_chain.db");
        std::fs::write(&old, build_two_page_db()).unwrap();
        let ldb = LegacyDb::open(&old).unwrap();
        let ts = ldb.tables().unwrap();
        let t = &ts[0];
        let rows = ldb.rows(t, usize::MAX).unwrap();
        assert_eq!(rows.len(), 1);

        // 类型映射：a 是 TEXT、b 是 INTEGER（不能让金额类的东西落到文本上）
        assert_eq!(map_decl_type(&t.column_types[0]), "text");
        assert_eq!(map_decl_type(&t.column_types[1]), "integer");

        let dir = std::env::temp_dir().join("dkb_legacy_chain_new");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir).unwrap();

        let columns: Vec<ColumnDef> = t
            .columns
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let decl = t.column_types.get(i).map(|s| s.as_str()).unwrap_or("");
                ColumnDef {
                    name: n.clone(),
                    ty: match map_decl_type(decl) {
                        "integer" => ColType::Integer,
                        "real" => ColType::Real,
                        _ => ColType::Text,
                    },
                    not_null: false,
                    default: None,
                    primary_key: false,
                    comment: None,
                    shared: None,
                    link: None,
                    lookup: None,
                    rollup: None,
                }
            })
            .collect();
        db.create_table(&TableSpec {
            name: "导入的表".to_string(),
            comment: None,
            columns,
        })
        .unwrap();

        let data: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        LegacyValue::Null => None,
                        LegacyValue::Int(i) => Some(i.to_string()),
                        LegacyValue::Real(f) => Some(f.to_string()),
                        LegacyValue::Text(s) => Some(s.clone()),
                        LegacyValue::Blob(b) => Some(format!("<二进制 {} 字节>", b.len())),
                    })
                    .collect()
            })
            .collect();
        let n = db.insert_rows("导入的表", &t.columns, &data).unwrap();
        assert_eq!(n, 1, "应当写进 1 行");

        // 读回来 —— 值真的进去了才算数
        let p = db.page_rows("导入的表", None, false, None, 10).unwrap();
        assert_eq!(p.rows.len(), 1, "新表里应当有 1 行");
        // 首列是 rowid，所以值从下标 1 开始
        assert_eq!(p.rows[0][1], serde_json::json!("hello"));
        // b 列映射成了 integer，所以写进去的字符串 "42" 会被引擎规范化成数字 42。
        // **这正是类型映射想要的效果** —— 旧库里的整数进来还是整数，
        // 排序和计算能接着用；全按文本存的话，它们会退化成字符串比较
        // （"10" < "9"）。这条断言是被测试逼着改对的，一开始我写成字符串了。
        assert_eq!(p.rows[0][2], serde_json::json!(42));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&old);
    }

    #[test]
    fn 页数对不上要报文件不完整() {
        let mut f = build_two_page_db();
        // 头部说 99 页，实际只有 2 页
        f[28..32].copy_from_slice(&99u32.to_be_bytes());
        let d = std::env::temp_dir().join("dkb_legacy_short.db");
        std::fs::write(&d, f).unwrap();
        let e = LegacyDb::open(&d).unwrap_err();
        assert!(e.contains("不完整"), "要说清文件不完整：{e}");
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn 文件头不对要明确报错() {
        let d = std::env::temp_dir().join("dkb_legacy_notsqlite.db");
        std::fs::write(&d, vec![0u8; 200]).unwrap();
        let e = LegacyDb::open(&d).unwrap_err();
        assert!(e.contains("不是 SQLite"), "要说清不是 SQLite：{e}");
        let _ = std::fs::remove_file(&d);
    }
}
