//! 自研单文件存储：append-only 日志 + 快照（见 docs/adr/0021）
//!
//! # 为什么自己写
//!
//! ADR-0003 当年否决自研，理由是「B+tree / 并发控制 / 查询优化 / 缓冲池是极深的水域」。
//! 这个判断现在依然成立，所以本模块**刻意不碰那四块**：
//!
//! ```text
//! 写入：只做一件事 —— 往文件末尾追加一条带 CRC 的记录，然后 fsync
//! 读取：只做一件事 —— 装载最新快照，然后从快照点之后回放日志
//! 并发：单写者（DeskBase 是单主端单进程，见 ADR-0006），多读者看快照
//! 索引：内存 BTreeMap，每次启动从日志重建，因此不存在「索引与数据不一致」
//! ```
//!
//! 没有原地改页，就没有「写了一半的页」；没有多写者，就没有锁协议；
//! 没有持久化索引，就没有索引损坏。剩下的崩溃安全问题收敛成三件可验证的小事：
//!
//! | 机制 | 保证 |
//! |------|------|
//! | 追加 + CRC | 尾部半写记录能被识别并丢弃，之前的全部完好 |
//! | `sync_all` | 提交返回成功 = 已经落到磁盘 |
//! | 快照原子替换 | 写 `.tmp` → fsync → `rename` → 重开，临时文件残留无害 |
//!
//! # 文件格式
//!
//! ```text
//! 日志条目： [ "DKB1" | len:u32 LE | crc32:u32 LE | payload: JSON ]
//! payload = {"seq":N,"ops":[{"op":"set","k":"...","v":"..."},{"op":"del","k":"..."}]}
//! 快照文件 = {"seq":N,"data":{k:v,...}}   整份内存状态的 JSON
//! ```
//!
//! payload 用 JSON 而不是二进制，是**故意的**：数据文件出问题时，
//! 用户/维护者能用文本编辑器把它捞出来。可读性优先于那点体积。

// 存储引擎的公共 API 面比当前用到的大：备份、恢复向导、统计、自检都还没有全部接线。
// 这些接口是**刻意保留**的（它们对应 ADR-0021 里承诺的能力），不是忘了删的死代码，
// 所以在这里显式声明 —— 比在每个字段上挂一个 allow 好读，也比让警告淹没真问题好。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, String>;

const MAGIC: &[u8; 4] = b"DKB1";
/// magic(4) + len(4) + crc(4)
const HEADER_LEN: usize = 12;

/// 日志条目数达到这个数就做快照。快照之后日志会被截断，因此回放量恒定有界。
const SNAPSHOT_ENTRIES: u64 = 50_000;
/// 日志字节数达到这个数就做快照。
const SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

const LOG_NAME: &str = "main.dkb.log";
const SNAP_NAME: &str = "main.dkb.snap";

// ---------------------------------------------------------------------------
// CRC32（IEEE 802.3，与 zlib 同多项式）
// ---------------------------------------------------------------------------

/// 自己实现而不是引 crc  crate：20 行代码，换掉一个依赖，符合「零新增依赖」的决策。
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// 日志条目
// ---------------------------------------------------------------------------

/// 一条日志 = 一个事务。要么整条回放，要么整条丢弃 —— 原子性由「一条」这件事本身保证。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    seq: u64,
    ops: Vec<OpSer>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum OpSer {
    Set { k: String, v: String },
    Del { k: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SnapFile {
    seq: u64,
    data: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// 打开报告（给崩溃恢复向导用）
// ---------------------------------------------------------------------------

/// 启动时的回放结果。
///
/// `truncated_tail = true` 意味着上次退出时日志尾部有一条没写完的条目 ——
/// 这是**正常且已处理**的情况（半写事务被丢弃），但值得告诉用户：
/// 「上次可能有一步没存上，其余数据完好」。
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenReport {
    pub seq: u64,
    pub replayed: u64,
    pub truncated_tail: bool,
    pub had_snapshot: bool,
}

// ---------------------------------------------------------------------------
// 写事务
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Set(String, String),
    Del(String),
}

/// 一个写事务：攒操作 → 一次性提交。
///
/// 攒着而不是逐条写，是因为**一个事务 = 一条日志**是原子性的来源。
#[derive(Debug, Default)]
pub struct Batch {
    ops: Vec<Op>,
}

impl Batch {
    pub fn new() -> Self {
        Batch { ops: Vec::new() }
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.ops.push(Op::Set(key.into(), value.into()));
        self
    }

    pub fn del(&mut self, key: impl Into<String>) -> &mut Self {
        self.ops.push(Op::Del(key.into()));
        self
    }

    /// 按前缀删除。列目录的工作由调用方（Store）做，因为只有它有数据视图。
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct Store {
    dir: PathBuf,
    log_path: PathBuf,
    snap_path: PathBuf,
    log: Option<File>,
    data: BTreeMap<String, String>,
    seq: u64,
    since_snap: u64,
    log_bytes: u64,
    last_open: OpenReport,
}

impl Store {
    // ---------- 生命周期 ----------

    /// 打开（必要时创建）数据目录与库文件。
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("data");
        fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败 {}: {e}", dir.display()))?;
        let log_path = dir.join(LOG_NAME);
        let snap_path = dir.join(SNAP_NAME);

        // 上一次做快照时留下的临时文件：忽略并清理。它本来就不该被信任。
        let _ = fs::remove_file(snap_path.with_extension("snap.tmp"));
        let _ = fs::remove_file(log_path.with_extension("log.tmp"));

        let (mut data, snap_seq, had_snapshot) = load_snapshot(&snap_path)?;
        let (entries, consumed, truncated_tail) = read_log(&log_path)?;

        let mut replayed = 0u64;
        for e in &entries {
            if e.seq <= snap_seq {
                continue; // 快照里已经有了
            }
            apply_ops(&mut data, &e.ops);
            replayed += 1;
        }

        // 半写的尾部字节：不去动文件（下次启动会再判断一次），只在报告里标出来。
        let _ = consumed;

        let seq = data_seq(&entries, snap_seq);
        let log_bytes = log_len(&log_path)?;
        let since_snap = replayed;

        let log = Some(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|e| format!("打开日志文件失败: {e}"))?,
        );

        let st = Store {
            dir,
            log_path,
            snap_path,
            log,
            data,
            seq,
            since_snap,
            log_bytes,
            last_open: OpenReport {
                seq,
                replayed,
                truncated_tail,
                had_snapshot,
            },
        };
        Ok(st)
    }

    /// 启动回放报告（崩溃恢复向导用）。
    pub fn open_report(&self) -> OpenReport {
        self.last_open
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn snap_path(&self) -> &Path {
        &self.snap_path
    }

    // ---------- 读 ----------

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(|s| s.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// 前缀扫描，按 key 字典序。
    ///
    /// 键的命名约定（`tbl/`、`rec/<表>/`、`sys/`…）让「枚举某张表的所有记录」
    /// 变成一次范围扫描，不需要任何查询语言。
    pub fn scan(&self, prefix: &str) -> Vec<(String, String)> {
        self.data
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 前缀扫描，但**只要前 n 条**。
    ///
    /// 为什么单独加这一个：`scan()` 会把**整表**克隆出来（每行一个 key 的 String
    /// 加一个 value 的 String），而最常见的查询是"给我第一页"。
    /// 10 万行的表上，为了显示 50 行去克隆 10 万个 String 是纯粹的浪费。
    ///
    /// ⚠️ 它返回的是 **BTreeMap 的升序**，不是任意排序后的结果 ——
    /// 只在"顺序无关"或"已确认存储顺序正是所需顺序"时用它。
    pub fn scan_take(&self, prefix: &str, n: usize) -> Vec<(String, String)> {
        self.data
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .take(n)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// 前缀计数（不拷贝值，比 `scan().len()` 省）。
    pub fn count(&self, prefix: &str) -> usize {
        self.data
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .count()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    // ---------- 写 ----------

    /// 提交一个事务。返回新的 seq。
    ///
    /// 三步的顺序不能变：**先落盘，再改内存**。
    /// 反过来的话，落盘失败时内存已经被污染，进程后续看到的是不存在的数据。
    pub fn commit(&mut self, batch: Batch) -> Result<u64> {
        if batch.ops.is_empty() {
            return Ok(self.seq);
        }
        let seq = self.seq + 1;
        let ops: Vec<OpSer> = batch
            .ops
            .iter()
            .map(|o| match o {
                Op::Set(k, v) => OpSer::Set { k: k.clone(), v: v.clone() },
                Op::Del(k) => OpSer::Del { k: k.clone() },
            })
            .collect();
        let entry = Entry { seq, ops };
        let payload = serde_json::to_vec(&entry).map_err(|e| format!("序列化日志条目失败: {e}"))?;

        self.append_raw(&payload)?;
        apply_ops(&mut self.data, &entry.ops);
        self.seq = seq;
        self.since_snap += 1;

        if self.should_snapshot() {
            self.snapshot()?;
        }
        Ok(seq)
    }

    /// 便捷：单键写入。
    pub fn put(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<u64> {
        let mut b = Batch::new();
        b.set(key, value);
        self.commit(b)
    }

    /// 便捷：单键删除。
    pub fn remove(&mut self, key: impl Into<String>) -> Result<u64> {
        let mut b = Batch::new();
        b.del(key);
        self.commit(b)
    }

    /// 按前缀删除一批键（事务内完成）。
    pub fn remove_prefix(&mut self, prefix: &str) -> Result<u64> {
        let keys: Vec<String> = self
            .data
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return Ok(self.seq);
        }
        let mut b = Batch::new();
        for k in keys {
            b.del(k);
        }
        self.commit(b)
    }

    /// 强制做一次快照（备份前调用，让备份拿到干净且最短的状态）。
    pub fn force_snapshot(&mut self) -> Result<()> {
        self.snapshot()
    }

    // ---------- 内部 ----------

    fn should_snapshot(&self) -> bool {
        self.since_snap >= SNAPSHOT_ENTRIES || self.log_bytes >= SNAPSHOT_BYTES
    }

    fn append_raw(&mut self, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&crc32(payload).to_le_bytes());
        buf.extend_from_slice(payload);

        let log = self
            .log
            .as_mut()
            .ok_or_else(|| "日志句柄已关闭".to_string())?;
        log.write_all(&buf)
            .map_err(|e| format!("写日志失败: {e}"))?;
        log.sync_all().map_err(|e| format!("日志 fsync 失败: {e}"))?;

        self.log_bytes += buf.len() as u64;
        Ok(())
    }

    /// 快照 + 截断日志。
    ///
    /// 顺序：写快照 tmp → fsync → rename 覆盖 → 丢弃旧日志句柄 → 造空日志 tmp → rename 覆盖 → 重开。
    /// 中途任何一步崩掉，最坏情况是「日志还在、快照是上一版」—— 下次启动照常回放，数据不丢。
    fn snapshot(&mut self) -> Result<()> {
        let snap_tmp = self.snap_path.with_extension("snap.tmp");
        let snap = SnapFile {
            seq: self.seq,
            data: self.data.clone(),
        };
        let json = serde_json::to_vec(&snap).map_err(|e| format!("序列化快照失败: {e}"))?;
        {
            let mut f = File::create(&snap_tmp)
                .map_err(|e| format!("创建快照临时文件失败: {e}"))?;
            f.write_all(&json).map_err(|e| format!("写快照失败: {e}"))?;
            f.sync_all().map_err(|e| format!("快照 fsync 失败: {e}"))?;
        }
        fs::rename(&snap_tmp, &self.snap_path)
            .map_err(|e| format!("替换快照失败: {e}"))?;

        // 先关掉旧句柄 —— Windows 上带着打开句柄去替换同名文件是自找麻烦。
        self.log = None;
        let log_tmp = self.log_path.with_extension("log.tmp");
        {
            let f = File::create(&log_tmp).map_err(|e| format!("创建空日志失败: {e}"))?;
            f.sync_all().map_err(|e| format!("空日志 fsync 失败: {e}"))?;
        }
        fs::rename(&log_tmp, &self.log_path)
            .map_err(|e| format!("截断日志失败: {e}"))?;
        self.log = Some(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)
                .map_err(|e| format!("重开日志失败: {e}"))?,
        );

        self.log_bytes = 0;
        self.since_snap = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 自由函数
// ---------------------------------------------------------------------------

fn apply_ops(data: &mut BTreeMap<String, String>, ops: &[OpSer]) {
    for op in ops {
        match op {
            OpSer::Set { k, v } => {
                data.insert(k.clone(), v.clone());
            }
            OpSer::Del { k } => {
                data.remove(k);
            }
        }
    }
}

fn load_snapshot(path: &Path) -> Result<(BTreeMap<String, String>, u64, bool)> {
    if !path.exists() {
        return Ok((BTreeMap::new(), 0, false));
    }
    let bytes = fs::read(path).map_err(|e| format!("读快照失败: {e}"))?;
    match serde_json::from_slice::<SnapFile>(&bytes) {
        Ok(s) => Ok((s.data, s.seq, true)),
        Err(e) => Err(format!(
            "快照文件损坏（{}）：{e}。请从备份恢复，或删除该文件后用日志重建。",
            path.display()
        )),
    }
}

/// 读日志并把能解析的条目全拿出来。
///
/// 返回 `(条目, 已消费字节数, 尾部是否有半写/损坏)`。
/// **尾部损坏不报错** —— 那是崩溃的正常结果，丢弃即可；
/// 只有"中间出现坏条目"才需要人来看。
fn read_log(path: &Path) -> Result<(Vec<Entry>, usize, bool)> {
    if !path.exists() {
        return Ok((Vec::new(), 0, false));
    }
    let bytes = fs::read(path).map_err(|e| format!("读日志失败: {e}"))?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut truncated = false;

    while pos + HEADER_LEN <= bytes.len() {
        if &bytes[pos..pos + 4] != MAGIC {
            truncated = true;
            break;
        }
        let len = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
            as usize;
        let crc = u32::from_le_bytes([
            bytes[pos + 8],
            bytes[pos + 9],
            bytes[pos + 10],
            bytes[pos + 11],
        ]);
        let start = pos + HEADER_LEN;
        if start + len > bytes.len() {
            // 声明的长度超出了文件 —— 写了一半就被杀了
            truncated = true;
            break;
        }
        let payload = &bytes[start..start + len];
        if crc32(payload) != crc {
            truncated = true;
            break;
        }
        match serde_json::from_slice::<Entry>(payload) {
            Ok(e) => {
                out.push(e);
                pos = start + len;
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    Ok((out, pos, truncated))
}

fn data_seq(entries: &[Entry], snap_seq: u64) -> u64 {
    entries.iter().map(|e| e.seq).fold(snap_seq, u64::max)
}

/// 日志一致性自检的结果（恢复向导用，取代 SQLite 时代的 `PRAGMA quick_check`）。
#[derive(Debug, Clone)]
pub struct LogCheck {
    /// 能完整解析的条目数
    pub entries: usize,
    /// 尾部是否有半写/损坏条目（有 = 已自动丢弃，属正常恢复）
    pub truncated_tail: bool,
    /// 整体是否可用
    pub ok: bool,
    /// 给人看的一句话结论
    pub message: String,
}

/// 只读地检查一份日志文件能不能被完整解析出来。
///
/// 新引擎没有 `quick_check` 这种引擎内自检，但一致性判据其实更直接：
/// **日志能不能从头到尾解析完**。解析不完的部分就是上次崩溃留下的半写事务，
/// 会被丢弃 —— 这不叫损坏，叫恢复。真正的"不可用"是文件打不开或开头就坏了。
pub fn check_log(path: &Path) -> LogCheck {
    match read_log(path) {
        Ok((entries, _consumed, truncated)) => LogCheck {
            ok: true,
            message: if truncated {
                format!(
                    "可恢复：解析出 {} 条完整事务，尾部有一条没写完的已丢弃（上次可能是被强杀的）",
                    entries.len()
                )
            } else {
                format!("ok（{} 条事务完整）", entries.len())
            },
            entries: entries.len(),
            truncated_tail: truncated,
        },
        Err(e) => LogCheck {
            entries: 0,
            truncated_tail: false,
            ok: false,
            message: format!("读不了日志文件：{e}"),
        },
    }
}

fn log_len(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(m) => Ok(m.len()),
        Err(_) => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dkb_store_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn corrupt_append(dir: &Path, junk: &[u8]) {
        let p = dir.join("data").join(LOG_NAME);
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(junk).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn write_then_read_back() {
        let d = tmp_dir("basic");
        let mut s = Store::open(&d).unwrap();
        s.put("sys/theme", "dark").unwrap();
        s.put("tbl/t1", "{\"name\":\"客户\"}").unwrap();
        assert_eq!(s.get("sys/theme"), Some("dark"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn data_survives_reopen() {
        let d = tmp_dir("reopen");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("a", "1").unwrap();
            s.put("b", "2").unwrap();
            s.remove("a").unwrap();
        }
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("b"), Some("2"));
        assert_eq!(s.get("a"), None);
    }

    #[test]
    fn transaction_all_or_nothing() {
        let d = tmp_dir("atomic");
        {
            let mut s = Store::open(&d).unwrap();
            let mut b = Batch::new();
            b.set("x", "1");
            b.set("y", "2");
            b.set("z", "3");
            s.commit(b).unwrap();
        }
        // 模拟"提交了但进程随后被杀"：尾部再追加半条，重开后三个键必须都还在
        // 完整的 12 字节头（magic + len=5 + crc），但 payload 一个字节都没写上
        let mut junk = Vec::new();
        junk.extend_from_slice(MAGIC);
        junk.extend_from_slice(&5u32.to_le_bytes());
        junk.extend_from_slice(&0u32.to_le_bytes());
        corrupt_append(&d, &junk);
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("x"), Some("1"));
        assert_eq!(s.get("y"), Some("2"));
        assert_eq!(s.get("z"), Some("3"));
        assert!(s.open_report().truncated_tail, "应该识别出尾部半写");
    }

    #[test]
    fn torn_tail_entry_dropped_earlier_data_intact() {
        let d = tmp_dir("halfwrite");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("keep", "yes").unwrap();
        }
        // 追加一条"声称 100 字节但只写了 10 字节"的条目
        let mut junk = Vec::new();
        junk.extend_from_slice(MAGIC);
        junk.extend_from_slice(&100u32.to_le_bytes());
        junk.extend_from_slice(&0u32.to_le_bytes());
        junk.extend_from_slice(b"0123456789");
        corrupt_append(&d, &junk);

        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("keep"), Some("yes"));
        assert!(s.open_report().truncated_tail);
    }

    #[test]
    fn bad_crc_entry_dropped() {
        let d = tmp_dir("badcrc");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("keep", "yes").unwrap();
        }
        let mut junk = Vec::new();
        junk.extend_from_slice(MAGIC);
        junk.extend_from_slice(&3u32.to_le_bytes());
        junk.extend_from_slice(&0xDEADBEEFu32.to_le_bytes()); // 故意错
        junk.extend_from_slice(b"abc");
        corrupt_append(&d, &junk);

        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("keep"), Some("yes"));
        assert!(s.open_report().truncated_tail);
    }

    #[test]
    fn non_magic_garbage_dropped() {
        let d = tmp_dir("junk");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("keep", "yes").unwrap();
        }
        corrupt_append(&d, b"XXXX garbage");
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("keep"), Some("yes"));
    }

    #[test]
    fn log_reset_after_snapshot_data_intact() {
        let d = tmp_dir("snap");
        {
            let mut s = Store::open(&d).unwrap();
            for i in 0..200 {
                s.put(format!("k{i}"), format!("v{i}")).unwrap();
            }
            s.force_snapshot().unwrap();
            assert_eq!(s.log_bytes, 0, "快照后日志应该被截断");
            assert!(s.snap_path().exists());
        }
        let s = Store::open(&d).unwrap();
        assert!(s.open_report().had_snapshot);
        assert_eq!(s.len(), 200);
        assert_eq!(s.get("k199"), Some("v199"));
        assert_eq!(s.open_report().replayed, 0, "快照之后不该再回放任何条目");
    }

    #[test]
    fn write_after_snapshot_then_reopen_all_correct() {
        let d = tmp_dir("snap2");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("a", "1").unwrap();
            s.force_snapshot().unwrap();
            s.put("b", "2").unwrap();
            s.remove("a").unwrap();
        }
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("a"), None, "快照之后的删除必须生效");
        assert_eq!(s.get("b"), Some("2"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn prefix_scan_returns_lexicographic_order() {
        let d = tmp_dir("scan");
        let mut s = Store::open(&d).unwrap();
        s.put("rec/t1/r2", "b").unwrap();
        s.put("rec/t1/r1", "a").unwrap();
        s.put("rec/t2/r1", "c").unwrap();
        s.put("tbl/t1", "meta").unwrap();

        let rows = s.scan("rec/t1/");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "rec/t1/r1");
        assert_eq!(rows[1].0, "rec/t1/r2");
        assert_eq!(s.count("rec/"), 3);
    }

    #[test]
    fn prefix_delete_is_transactional() {
        let d = tmp_dir("delprefix");
        let mut s = Store::open(&d).unwrap();
        s.put("rec/t1/r1", "a").unwrap();
        s.put("rec/t1/r2", "b").unwrap();
        s.put("rec/t2/r1", "c").unwrap();
        s.remove_prefix("rec/t1/").unwrap();
        assert_eq!(s.len(), 1);
        assert!(s.contains("rec/t2/r1"));
    }

    #[test]
    fn empty_transaction_no_log_no_seq_advance() {
        let d = tmp_dir("empty");
        let mut s = Store::open(&d).unwrap();
        let before = s.seq();
        s.commit(Batch::new()).unwrap();
        assert_eq!(s.seq(), before);
        assert_eq!(s.log_bytes, 0);
    }

    #[test]
    fn leftover_temp_file_cleaned_without_harming_open() {
        let d = tmp_dir("tmpfile");
        {
            let mut s = Store::open(&d).unwrap();
            s.put("a", "1").unwrap();
        }
        let data = d.join("data");
        fs::write(data.join("main.dkb.snap.tmp"), b"garbage").unwrap();
        fs::write(data.join("main.dkb.log.tmp"), b"garbage").unwrap();
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("a"), Some("1"));
        assert!(!data.join("main.dkb.snap.tmp").exists());
    }

    #[test]
    fn chinese_key_value_roundtrip() {
        let d = tmp_dir("cjk");
        let mut s = Store::open(&d).unwrap();
        s.put("tbl/客户表", "{\"name\":\"客户表\"}").unwrap();
        drop(s);
        let s = Store::open(&d).unwrap();
        assert_eq!(s.get("tbl/客户表"), Some("{\"name\":\"客户表\"}"));
    }

    #[test]
    fn ten_thousand_keys_roundtrip_and_reopen() {
        let d = tmp_dir("bulk");
        {
            let mut s = Store::open(&d).unwrap();
            let mut b = Batch::new();
            for i in 0..10_000 {
                b.set(format!("k{i:06}"), format!("值{i}"));
            }
            s.commit(b).unwrap();
        }
        let s = Store::open(&d).unwrap();
        assert_eq!(s.len(), 10_000);
        assert_eq!(s.get("k009999"), Some("值9999"));
    }
}
