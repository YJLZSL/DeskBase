//! 数据表模型：表 / 字段 / 记录 / 视图 / 关联 / 共通 / 同步（见 docs/adr/0021 · 0022）
//!
//! # 这一层为什么存在
//!
//! 去掉 SQL 之后，原来由 `JOIN`、外键级联、触发器、`SELECT` 承担的四件事，
//! 在这里各有新的、且**用户看得见**的载体：
//!
//! | 原来靠 SQL | 现在靠什么 |
//! |-----------|-----------|
//! | `JOIN` | [`LinkSpec`] 关联字段 |
//! | 外键级联 / 触发器 | [`SyncRule`] 同步规则（可开关、可改方向、有冲突策略） |
//! | `SELECT / WHERE / ORDER BY` | [`View`] 命名视图（筛选/排序/分组，无查询语言） |
//! | 到处重复维护的同一字段 | [`SharedField`] 共通字段（改一处，处处生效） |
//!
//! # 存储键的约定
//!
//! ```text
//! sys/<key>                  系统键值（AI 设置、更新器设置、工作区…）
//! tbl/<表名>                 表定义
//! rec/<表名>/<20 位 rowid>    一条记录
//! shr/<id>                   共通字段定义
//! syn/<id>                   同步规则
//! note/<id>                  笔记
//! ```
//!
//! rowid 用 20 位零填充，是为了让字典序 = 数字序，于是"取某张表的全部记录"
//! 就是一次前缀范围扫描 —— 不需要任何查询语言。

use std::collections::BTreeMap;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

use crate::store::{Batch, Store};

pub type Result<T> = std::result::Result<T, String>;
pub type Json = serde_json::Value;

/// 一页最多多少行。界面请求更多会被夹到这里 —— 一次搬太多行既卡界面也没人看。
pub const MAX_PAGE_LIMIT: usize = 500;
/// `Page` 的第 0 列恒为行标识。界面用它定位要改/删的行，不显示给用户。
pub const ROWID_COLUMN: &str = "_rowid";

/// 变更历史：每张表最多留这么多条，超了从最旧的开始丢。
///
/// 为什么要限：历史是"改错了能回退"，不是"全量审计"。全量留会让数据文件
/// 无限长，而用户真正会去回退的只有最近那几十步。500 条对"改错了"这个场景
/// 绰绰有余，同时把体积增长变成有界的。
pub const HISTORY_KEEP: usize = 500;

/// 一条变更记录。回退用它把数据改回原样。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    /// store 里的键 —— 回退时按它定位（同时也是排序依据）
    pub key: String,
    pub at_ms: u64,
    /// update / delete
    pub op: String,
    pub rowid: i64,
    /// 改了哪一列（delete 为空串）
    pub column: String,
    /// 旧值：update 时是该列的旧值，delete 时是**整行**的 JSON
    pub before: String,
    /// 新值（delete 为空串）
    pub after: String,
    /// 给人看的一句摘要，界面直接显示
    pub preview: String,
}
/// 递归同步的深度上限。防的是"两条规则互相触发"形成死循环。
pub const MAX_SYNC_DEPTH: u32 = 5;

const MAX_IDENT_CHARS: usize = 64;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ===========================================================================
// 字段类型
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColType {
    Text,
    Integer,
    Real,
    Money,
    Boolean,
    Date,
    #[serde(alias = "datetime")]
    DateTime,
    Json,
    Blob,
}

impl ColType {
    pub fn from_name(s: &str) -> Option<ColType> {
        let t: String = s
            .trim()
            .chars()
            .filter(|c| *c != '-' && *c != '_' && *c != ' ')
            .flat_map(|c| c.to_lowercase())
            .collect();
        match t.as_str() {
            "text" => Some(ColType::Text),
            "integer" => Some(ColType::Integer),
            "real" => Some(ColType::Real),
            "money" => Some(ColType::Money),
            "boolean" => Some(ColType::Boolean),
            "date" => Some(ColType::Date),
            "datetime" => Some(ColType::DateTime),
            "json" => Some(ColType::Json),
            "blob" => Some(ColType::Blob),
            _ => None,
        }
    }

    /// 界面用的中文标签。只在界面层用，不进协议。
    pub fn label(self) -> &'static str {
        match self {
            ColType::Text => "文本",
            ColType::Integer => "数字（整数）",
            ColType::Real => "数字（小数）",
            ColType::Money => "金额",
            ColType::Boolean => "是 / 否",
            ColType::Date => "日期",
            ColType::DateTime => "日期时间",
            ColType::Json => "JSON（进阶）",
            ColType::Blob => "二进制（进阶）",
        }
    }
}

/// 关联字段：本表的这个字段指向另一张表的记录。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkSpec {
    /// 目标表名
    pub target: String,
    /// true = 可以关联多条；false = 只关联一条
    pub many: bool,
    /// 目标表上的反向字段名（创建时可选自动建立）
    pub back_field: Option<String>,
}

/// 查找字段：顺着关联把目标记录的某个字段"看"过来。**只读派生，不落盘副本**。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LookupSpec {
    /// 本表里的一个 Link 字段名
    pub via: String,
    /// 目标表里的字段名
    pub target_field: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// 汇总字段：顺着关联把目标记录聚合起来（计数/求和/平均…）。同样只读。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RollupSpec {
    pub via: String,
    pub target_field: String,
    pub agg: Agg,
}

/// 字段上的来源标记：这个值是被哪条规则从哪儿同步过来的。
///
/// 存在的唯一理由：用户必须能回答"这个数哪来的、我能不能改"。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncMark {
    pub rule_id: String,
    /// 冗余存表名/字段名：源表被删之后，来源仍然说得清楚。
    pub source_table: String,
    pub source_field: String,
    pub synced_at: i64,
}

// ===========================================================================
// 字段 / 表
// ===========================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: ColType,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub comment: Option<String>,
    /// 引用的共通字段 id（ADR-0022 §2）
    #[serde(default)]
    pub shared: Option<String>,
    #[serde(default)]
    pub link: Option<LinkSpec>,
    #[serde(default)]
    pub lookup: Option<LookupSpec>,
    #[serde(default)]
    pub rollup: Option<RollupSpec>,
    #[serde(default)]
    pub formula: Option<String>,
    /// 被同步规则写入时自动标注
    #[serde(default)]
    pub sync: Option<SyncMark>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Table {
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub fields: Vec<Field>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// 下一个 rowid。只增不减，删除过的位置不复用。
    #[serde(default)]
    pub next_rowid: i64,
}

impl Table {
    pub fn field(&self, name: &str) -> Option<&Field> {
        let l = name.to_lowercase();
        self.fields.iter().find(|f| f.name.to_lowercase() == l)
    }
    pub fn field_mut(&mut self, name: &str) -> Option<&mut Field> {
        let l = name.to_lowercase();
        self.fields.iter_mut().find(|f| f.name.to_lowercase() == l)
    }
}

/// 新建表格时的列定义（界面向导的产物，与原 schema.rs 保持同形）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColType,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub shared: Option<String>,
    #[serde(default)]
    pub link: Option<LinkSpec>,
    #[serde(default)]
    pub lookup: Option<LookupSpec>,
    #[serde(default)]
    pub rollup: Option<RollupSpec>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableSpec {
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub columns: Vec<ColumnDef>,
}

/// 读回来的列信息（界面表格头用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub decl_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub pk: bool,
}

/// 列的附加元数据。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ColumnMeta {
    pub name: String,
    pub comment: Option<String>,
    pub semantic: Option<ColType>,
    /// 共通字段 id（有值 = 这是共通字段，界面要打 `⇄ 共通` 徽标）
    pub shared: Option<String>,
    /// 关联目标（有值 = 这是关联字段）
    pub link: Option<LinkSpec>,
    /// 由同步规则写入的字段：界面要显示来源、并按模式决定能否编辑
    pub sync: Option<SyncMark>,
    /// Mirror 模式下目标字段只读
    pub read_only: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TableInfo {
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ColumnInfo>,
    /// 精确行数（自研引擎能数得清，不需要"约"）
    pub row_estimate: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub rows: Vec<Vec<Json>>,
    pub columns: Vec<String>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

// ===========================================================================
// 共通字段 / 同步规则（ADR-0022 的核心）
// ===========================================================================

/// 共通字段：一次定义，多张表引用。改定义 → 所有引用它的字段一起变。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SharedField {
    pub id: String,
    pub name: String,
    pub ty: ColType,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    /// 反向索引："哪些表的哪些字段引用了我" —— 改定义时要知道影响面。
    #[serde(default)]
    pub used_by: Vec<String>,
}

/// 同步方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// 单向镜像：源 → 目标，目标字段只读
    Mirror,
    /// 双向：任一侧改，另一侧跟着改
    TwoWay,
    /// 不自动写：生成建议，用户确认后才落盘
    Suggest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    SourceWins,
    TargetWins,
    LastWriteWins,
    Ask,
}

/// 写入范围：是总覆盖，还是只填空的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyScope {
    Always,
    FillEmptyOnly,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncRule {
    pub id: String,
    pub name: String,
    /// 总开关 —— 「可以调整」的落地：关掉它，目标字段立刻不再被改写。
    pub enabled: bool,
    pub source_table: String,
    pub source_field: String,
    pub target_table: String,
    pub target_field: String,
    /// 从源表走到目标表所经由的 Link 字段名
    pub via: String,
    pub mode: SyncMode,
    pub conflict: ConflictPolicy,
    pub scope: ApplyScope,
    #[serde(default)]
    pub created_at: i64,
}

/// 一次同步传播的结果。回给界面，好让它说清楚"改了哪几条"。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SyncReport {
    pub applied: usize,
    pub skipped: usize,
    pub visited_rules: Vec<String>,
}

// ===========================================================================
// 视图（替代 SELECT）
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Contains,
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    IsEmpty,
    IsNotEmpty,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: FilterOp,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FilterGroup {
    /// true = 组内全部满足；false = 满足任一
    #[serde(default = "default_true")]
    pub all: bool,
    #[serde(default)]
    pub items: Vec<Filter>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sort {
    pub field: String,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct View {
    pub id: String,
    pub table: String,
    pub name: String,
    #[serde(default)]
    pub filter: Option<FilterGroup>,
    #[serde(default)]
    pub sorts: Vec<Sort>,
    #[serde(default)]
    pub group_by: Option<String>,
    #[serde(default)]
    pub hidden: Vec<String>,
}

// ===========================================================================
// Db
// ===========================================================================

/// 索引定义：哪张表的哪一列要建索引。
///
/// **持久化**（存在 store 的 `idx/<表名>` 下）—— 用户建了索引，重开还在。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexSpec {
    pub table: String,
    pub column: String,
}

/// 一列的**值索引**：去重后的值 → 行号集合。
///
/// 为什么这样做，而不是搞一套完整的倒排/分词：我们要解决的是"搜某一格内容"，
/// 而**值去重之后通常远少于行数** —— "客户"这一列 10 万行可能只有几千个不同的名字。
/// 扫几千个唯一值比扫 10 万行快一个数量级，而实现复杂度只有完整倒排的零头。
///
/// **这是朴素但真实的索引，不是摆设**：搜有索引的列时不再全表扫描。
/// 将来真要做到"任意子串秒搜"，再上分词与倒排，这个结构也不浪费（可以只换内层）。
#[derive(Debug, Default)]
pub struct ValueIndex {
    /// 值 → 行号。**存原始值**（不预先转小写）—— 否则把它显示出来时，
    /// 英文会全变成小写。大小写不敏感在**比较时**做即可。
    pub map: BTreeMap<String, Vec<i64>>,
    /// 建这个索引时扫了多少行 —— 界面上要**如实**显示，
    /// 因为它决定了"这个索引覆盖全表了吗"
    pub scanned_rows: usize,
    /// 唯一值的个数（界面显示用，也让人直观感受它比全表小多少）
    pub distinct: usize,
}

pub struct Db {
    store: Store,
    /// 历史键的自增后缀。时间戳定宽（12 位）保证按字典序就是时间序，
    /// 同一毫秒内的多次变更靠它区分 —— 否则两次写入会覆盖成同一条历史。
    hst_seq: u64,
    /// 索引**内容**的缓存。只放内存、不落盘 —— 数据是会变的，
    /// 落盘就得跟着每次写一起维护，代价高且容易与数据不一致。
    /// 代价是重开要重建；好处是**永远不会有"索引和数据对不上"这种错**。
    index_cache: HashMap<String, ValueIndex>,
}

impl Db {
    pub fn open(data_dir: &std::path::Path) -> Result<Self> {
        Ok(Db {
            store: Store::open(data_dir)?,
            hst_seq: 0,
            index_cache: HashMap::new(),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    // ---------- 系统键值（取代原来的 sys_meta 表） ----------

    pub fn meta_get(&self, key: &str) -> Option<String> {
        self.store.get(&format!("sys/{key}")).map(|s| s.to_string())
    }

    pub fn meta_set(&mut self, key: &str, value: &str) -> Result<u64> {
        let mut b = Batch::new();
        b.set(format!("sys/{key}"), value);
        self.store.commit(b)
    }

    /// 删除一个系统键值。目前界面上还没有"清空某项设置"的入口，
    /// 但重置设置时一定用得到 —— 保留而不是等到需要时再写。
    #[allow(dead_code)]
    pub fn meta_del(&mut self, key: &str) -> Result<u64> {
        self.store.remove(format!("sys/{key}"))
    }

    // ---------- 表 ----------

    fn load_table(&self, name: &str) -> Result<Table> {
        let raw = self
            .store
            .get(&format!("tbl/{name}"))
            .ok_or_else(|| format!("表「{name}」不存在"))?;
        serde_json::from_str(raw).map_err(|e| format!("表「{name}」的定义读不出来: {e}"))
    }

    fn save_table(&mut self, t: &Table) -> Result<u64> {
        let raw = serde_json::to_string(t).map_err(|e| format!("序列化表定义失败: {e}"))?;
        self.store.put(format!("tbl/{}", t.name), raw)
    }

    pub fn list_tables(&self) -> Result<Vec<TableInfo>> {
        let mut out = Vec::new();
        for (k, v) in self.store.scan("tbl/") {
            let t: Table = match serde_json::from_str(&v) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = k.strip_prefix("tbl/").unwrap_or(&t.name).to_string();
            out.push(TableInfo {
                row_estimate: self.store.count(&record_prefix(&name)) as i64,
                columns: t
                    .fields
                    .iter()
                    .map(|f| ColumnInfo {
                        name: f.name.clone(),
                        decl_type: format!("{:?}", f.ty),
                        not_null: f.not_null,
                        default: f.default.clone(),
                        pk: f.primary_key,
                    })
                    .collect(),
                comment: t.comment.clone(),
                name,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn get_table(&self, name: &str) -> Result<TableInfo> {
        let t = self.load_table(name)?;
        Ok(TableInfo {
            name: t.name.clone(),
            comment: t.comment.clone(),
            row_estimate: self.store.count(&record_prefix(&t.name)) as i64,
            columns: t
                .fields
                .iter()
                .map(|f| ColumnInfo {
                    name: f.name.clone(),
                    decl_type: format!("{:?}", f.ty),
                    not_null: f.not_null,
                    default: f.default.clone(),
                    pk: f.primary_key,
                })
                .collect(),
        })
    }

    pub fn column_meta(&self, name: &str) -> Result<Vec<ColumnMeta>> {
        let t = self.load_table(name)?;
        let governed: Vec<String> = self
            .list_sync_rules()?
            .iter()
            .filter(|r| r.enabled && r.mode == SyncMode::Mirror && r.target_table == t.name)
            .map(|r| r.target_field.clone())
            .collect();
        Ok(t
            .fields
            .iter()
            .map(|f| ColumnMeta {
                name: f.name.clone(),
                comment: f.comment.clone(),
                semantic: Some(f.ty),
                shared: f.shared.clone(),
                link: f.link.clone(),
                sync: f.sync.clone(),
                read_only: f.lookup.is_some()
                    || f.rollup.is_some()
                    || governed.iter().any(|g| g.eq_ignore_ascii_case(&f.name)),
            })
            .collect())
    }

    pub fn create_table(&mut self, spec: &TableSpec) -> Result<()> {
        validate_identifier(&spec.name)?;
        if spec.columns.is_empty() {
            return Err("至少要有一列".to_string());
        }
        if self.store.contains(&format!("tbl/{}", spec.name)) {
            return Err(format!("表「{}」已经存在", spec.name));
        }
        let ts = now_ms();
        let mut fields = Vec::new();
        for c in &spec.columns {
            validate_column_name(&c.name)?;
            if fields
                .iter()
                .any(|f: &Field| f.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(format!("列名「{}」重复", c.name));
            }
            self.check_link_target(c)?;
            fields.push(Field {
                name: c.name.clone(),
                ty: c.ty,
                not_null: c.not_null,
                default: c.default.clone(),
                primary_key: c.primary_key,
                comment: c.comment.clone(),
                shared: c.shared.clone(),
                link: c.link.clone(),
                lookup: c.lookup.clone(),
                rollup: c.rollup.clone(),
                formula: None,
                sync: None,
            });
        }
        let t = Table {
            name: spec.name.clone(),
            comment: spec.comment.clone(),
            fields,
            created_at: ts,
            updated_at: ts,
            next_rowid: 1,
        };
        let mut b = Batch::new();
        let raw = serde_json::to_string(&t).map_err(|e| format!("序列化表定义失败: {e}"))?;
        b.set(format!("tbl/{}", t.name), raw);
        // 反向登记共通字段的引用
        for f in &t.fields {
            if let Some(sid) = &f.shared {
                if let Some(mut sf) = self.get_shared_field(sid) {
                    let entry = format!("{}::{}", t.name, f.name);
                    if !sf.used_by.contains(&entry) {
                        sf.used_by.push(entry);
                        let sraw = serde_json::to_string(&sf).unwrap_or_default();
                        b.set(format!("shr/{sid}"), sraw);
                    }
                }
            }
        }
        self.store.commit(b)?;
        Ok(())
    }

    pub fn rename_table(&mut self, from: &str, to: &str) -> Result<()> {
        validate_identifier(from)?;
        validate_identifier(to)?;
        if from == to {
            return Ok(());
        }
        if self.store.contains(&format!("tbl/{to}")) {
            return Err(format!("表「{to}」已经存在"));
        }
        let mut t = self.load_table(from)?;
        t.name = to.to_string();
        t.updated_at = now_ms();
        let raw = serde_json::to_string(&t).map_err(|e| format!("序列化表定义失败: {e}"))?;

        // 记录要跟着搬：一次事务里改写键，中途崩了也不会出现"表改名了记录还在旧键下"
        let old_prefix = record_prefix(from);
        let rows = self.store.scan(&old_prefix);
        let mut b = Batch::new();
        for (k, v) in rows {
            let rest = k[old_prefix.len()..].to_string();
            b.del(k);
            b.set(format!("{}{}", record_prefix(to), rest), v);
        }
        b.del(format!("tbl/{from}"));
        b.set(format!("tbl/{to}"), raw);
        self.store.commit(b)?;
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str, confirm_name: &str) -> Result<()> {
        validate_identifier(name)?;
        if name != confirm_name {
            return Err(format!(
                "确认名不一致：要删的是「{name}」，填的是「{confirm_name}」"
            ));
        }
        let _ = self.load_table(name)?;
        let mut b = Batch::new();
        for (k, _) in self.store.scan(&record_prefix(name)) {
            b.del(k);
        }
        b.del(format!("tbl/{name}"));
        // 同步规则里指向这张表的一并失效，免得留下"改一个不存在的表"的僵尸规则
        for r in self.list_sync_rules()? {
            if r.source_table == name || r.target_table == name {
                b.del(format!("syn/{}", r.id));
            }
        }
        self.store.commit(b)?;
        Ok(())
    }

    pub fn set_table_comment(&mut self, table: &str, comment: &str) -> Result<()> {
        let mut t = self.load_table(table)?;
        t.comment = if comment.is_empty() {
            None
        } else {
            Some(comment.to_string())
        };
        t.updated_at = now_ms();
        self.save_table(&t)?;
        Ok(())
    }

    #[allow(dead_code)] // 与 set_table_comment 成对存在，界面下一步会接
    pub fn set_column_comment(&mut self, table: &str, column: &str, comment: &str) -> Result<()> {
        let mut t = self.load_table(table)?;
        let f = t
            .field_mut(column)
            .ok_or_else(|| format!("表「{table}」没有列「{column}」"))?;
        f.comment = if comment.is_empty() {
            None
        } else {
            Some(comment.to_string())
        };
        t.updated_at = now_ms();
        self.save_table(&t)?;
        Ok(())
    }

    /// 关联字段指向的目标表必须存在。
    ///
    /// 抽出来的原因写在调用处：create_table 有、add_column 漏了，
    /// 两条入口共用一份实现，就不会再分叉。
    fn check_link_target(&self, c: &ColumnDef) -> Result<()> {
        if let Some(link) = &c.link {
            if !self.store.contains(&format!("tbl/{}", link.target)) {
                return Err(format!("关联的目标表「{}」不存在", link.target));
            }
        }
        Ok(())
    }

    pub fn add_column(&mut self, table: &str, col: &ColumnDef) -> Result<()> {
        validate_identifier(table)?;
        validate_column_name(&col.name)?;
        // 关联的目标表必须存在。
        //
        // ⚠️ 这条校验 create_table 里一直有，**add_column 却漏了** ——
        // 于是"不能建指向不存在表的关联"这条约束，在加列时形同不存在。
        // 实测确认过：通过 schema.addColumn 能建出关联到「根本不存在的表」的列，
        // 之后读取/同步会拿到一个悬空引用。而**加列才是更常用的入口**
        // （建表时要先有目标表，加列时往往两边都已存在），漏掉的恰好是常用的那个。
        self.check_link_target(col)?;
        let mut t = self.load_table(table)?;
        if t.field(&col.name).is_some() {
            return Err(format!("列「{}」已经存在", col.name));
        }
        t.fields.push(Field {
            name: col.name.clone(),
            ty: col.ty,
            not_null: col.not_null,
            default: col.default.clone(),
            primary_key: col.primary_key,
            comment: col.comment.clone(),
            shared: col.shared.clone(),
            link: col.link.clone(),
            lookup: col.lookup.clone(),
            rollup: col.rollup.clone(),
            formula: None,
            sync: None,
        });
        t.updated_at = now_ms();
        self.save_table(&t)?;
        Ok(())
    }

    pub fn drop_column(&mut self, table: &str, column: &str) -> Result<()> {
        let mut t = self.load_table(table)?;
        if t.fields.len() <= 1 {
            return Err("至少要保留一列".to_string());
        }
        let pos = t
            .fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(column))
            .ok_or_else(|| format!("表「{table}」没有列「{column}」"))?;
        t.fields.remove(pos);
        t.updated_at = now_ms();
        let raw = serde_json::to_string(&t).map_err(|e| format!("序列化表定义失败: {e}"))?;

        // 列删了，值也要从每条记录里拿掉 —— 留着会在导出和筛选里诈尸
        let prefix = record_prefix(table);
        let mut b = Batch::new();
        for (k, v) in self.store.scan(&prefix) {
            let mut rec: BTreeMap<String, Json> = serde_json::from_str(&v).unwrap_or_default();
            rec.remove(column);
            let nv = serde_json::to_string(&rec).unwrap_or(v);
            b.set(k, nv);
        }
        b.set(format!("tbl/{table}"), raw);
        self.store.commit(b)?;
        Ok(())
    }

    pub fn rename_column(&mut self, table: &str, column: &str, to: &str) -> Result<()> {
        validate_column_name(to)?;
        let mut t = self.load_table(table)?;
        if t.field(to).is_some() && !to.eq_ignore_ascii_case(column) {
            return Err(format!("列「{to}」已经存在"));
        }
        {
            let f = t
                .field_mut(column)
                .ok_or_else(|| format!("表「{table}」没有列「{column}」"))?;
            f.name = to.to_string();
        }
        t.updated_at = now_ms();
        let raw = serde_json::to_string(&t).map_err(|e| format!("序列化表定义失败: {e}"))?;

        let prefix = record_prefix(table);
        let mut b = Batch::new();
        for (k, v) in self.store.scan(&prefix) {
            let mut rec: BTreeMap<String, Json> = serde_json::from_str(&v).unwrap_or_default();
            if let Some(val) = rec.remove(column) {
                rec.insert(to.to_string(), val);
            }
            let nv = serde_json::to_string(&rec).unwrap_or(v);
            b.set(k, nv);
        }
        b.set(format!("tbl/{table}"), raw);
        self.store.commit(b)?;
        Ok(())
    }

    /// 整理一整列的值（只改值，不改类型）。
    ///
    /// 三条纪律（与旧实现一致）：看不懂的值**原样保留**并报出来；
    /// 已经规范的值不写；**主键列不动** —— 它可能被别的表引用。
    pub fn normalize_column(
        &mut self,
        table: &str,
        column: &str,
        rule: NormalizeRule,
    ) -> Result<NormalizeReport> {
        validate_identifier(table)?;
        validate_column_name(column)?;
        let t = self.load_table(table)?;
        let f = t
            .field(column)
            .ok_or_else(|| format!("表「{table}」没有列「{column}」"))?;
        if f.primary_key {
            return Err("主键列不参与整理 —— 它可能被别的表引用，值不该被改写".into());
        }
        let prefix = record_prefix(table);
        let mut b = Batch::new();
        let mut rep = NormalizeReport {
            total: 0,
            changed: 0,
            skipped: Vec::new(),
        };
        for (k, v) in self.store.scan(&prefix) {
            let mut rec: BTreeMap<String, Json> =
                serde_json::from_str(&v).map_err(|e| format!("记录读不出来: {e}"))?;
            rep.total += 1;
            let cur = rec.get(column).cloned().unwrap_or(Json::Null);
            let raw = match &cur {
                Json::Null => None,
                Json::String(s) if !s.is_empty() => Some(s.clone()),
                other => Some(value_text(other)),
            };
            let Some(raw) = raw else { continue };
            if raw.is_empty() {
                continue;
            }
            let next = match rule {
                NormalizeRule::Date => iso_date(&raw),
                NormalizeRule::Number => clean_number(&raw),
            };
            match next {
                Some(n) if n != raw => {
                    rec.insert(column.to_string(), Json::String(n));
                    b.set(k, serde_json::to_string(&rec).map_err(|e| e.to_string())?);
                    rep.changed += 1;
                }
                Some(_) => {}
                None => rep.skipped.push(SkippedRow {
                    rowid: rec.get(ROWID_COLUMN).and_then(|x| x.as_i64()).unwrap_or(0),
                    value: raw,
                }),
            }
        }
        self.store.commit(b)?;
        Ok(rep)
    }

    // ---------- 共通字段 ----------

    pub fn list_shared_fields(&self) -> Vec<SharedField> {
        self.store
            .scan("shr/")
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_str::<SharedField>(&v).ok())
            .collect()
    }

    pub fn get_shared_field(&self, id: &str) -> Option<SharedField> {
        self.store
            .get(&format!("shr/{id}"))
            .and_then(|s| serde_json::from_str(s).ok())
    }

    /// 新建或修改共通字段。`apply = true` 时把类型/选项/默认值同步到所有引用它的字段。
    ///
    /// 返回受到影响的表数量 —— 界面要拿它做"影响 N 张表"的二次确认。
    pub fn upsert_shared_field(&mut self, sf: SharedField, apply: bool) -> Result<usize> {
        let mut b = Batch::new();
        let mut touched = 0usize;
        if apply {
            for entry in &sf.used_by {
                let Some((tname, fname)) = entry.split_once("::") else {
                    continue;
                };
                let raw = match self.store.get(&format!("tbl/{tname}")) {
                    Some(r) => r.to_string(),
                    None => continue,
                };
                let mut t: Table = match serde_json::from_str(&raw) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let mut changed = false;
                if let Some(f) = t.field_mut(fname) {
                    if f.shared.as_deref() == Some(sf.id.as_str()) {
                        if f.ty != sf.ty {
                            f.ty = sf.ty;
                            changed = true;
                        }
                        let d = sf.default.clone();
                        if f.default != d {
                            f.default = d;
                            changed = true;
                        }
                        let c = sf.comment.clone();
                        if f.comment != c {
                            f.comment = c;
                            changed = true;
                        }
                    }
                }
                if changed {
                    let nr = serde_json::to_string(&t).map_err(|e| e.to_string())?;
                    b.set(format!("tbl/{tname}"), nr);
                    touched += 1;
                }
            }
        }
        b.set(
            format!("shr/{}", sf.id),
            serde_json::to_string(&sf).map_err(|e| e.to_string())?,
        );
        self.store.commit(b)?;
        Ok(touched)
    }

    // ---------- 同步规则 ----------

    pub fn list_sync_rules(&self) -> Result<Vec<SyncRule>> {
        let mut out: Vec<SyncRule> = self
            .store
            .scan("syn/")
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_str::<SyncRule>(&v).ok())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn upsert_sync_rule(&mut self, r: SyncRule) -> Result<()> {
        if self.store.get(&format!("tbl/{}", r.source_table)).is_none() {
            return Err(format!("源表「{}」不存在", r.source_table));
        }
        if self.store.get(&format!("tbl/{}", r.target_table)).is_none() {
            return Err(format!("目标表「{}」不存在", r.target_table));
        }
        self.store.put(
            format!("syn/{}", r.id),
            serde_json::to_string(&r).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }

    pub fn delete_sync_rule(&mut self, id: &str) -> Result<()> {
        if self.store.get(&format!("syn/{id}")).is_none() {
            return Err(format!("同步规则「{id}」不存在"));
        }
        self.store.remove(format!("syn/{id}"))?;
        Ok(())
    }

    /// 「可以调整」最常用的一步：只改开关。
    pub fn set_rule_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        let mut r: SyncRule = self
            .store
            .get(&format!("syn/{id}"))
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(|| format!("同步规则「{id}」不存在"))?;
        r.enabled = enabled;
        self.store.put(
            format!("syn/{id}"),
            serde_json::to_string(&r).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }

    // ---------- 记录 ----------

    // ----------------------------------------------------------
    // 变更历史（"改错了能回退"）
    // ----------------------------------------------------------
    //
    // 为什么必须有它：项目承诺的是「不丢数据」。崩溃不丢那半边已经由
    // append-only 日志 + fsync 兑现了，但**改错了能回退**一直是空的 ——
    // 误删一行、误改一格，用户只能眼睁睁看着。日志记的是"当前值"，
    // 不记"上一个值"，所以历史必须显式写下来。
    //
    // 为什么必须和变更在同一个 Batch 里：否则会出现"数据改了、历史没记"
    // 或反过来 —— 回退就成了假的。Batch 是一个事务，要么都成要么都不成。

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn hst_key(&mut self, table: &str) -> String {
        self.hst_seq += 1;
        format!("hst/{}/{:012}_{}", table, Self::now_ms(), self.hst_seq)
    }

    fn push_history(
        &mut self,
        b: &mut Batch,
        table: &str,
        op: &str,
        rowid: i64,
        column: &str,
        before: &str,
        after: &str,
    ) {
        let key = self.hst_key(table);
        let preview = match op {
            "update" => format!("第 {} 行的「{}」：{} → {}", rowid, column,
                trim_for_preview(before), trim_for_preview(after)),
            "delete" => format!("删掉了第 {} 行", rowid),
            _ => format!("{} 第 {} 行", op, rowid),
        };
        let e = HistoryEntry {
            key: key.clone(),
            at_ms: Self::now_ms(),
            op: op.to_string(),
            rowid,
            column: column.to_string(),
            before: before.to_string(),
            after: after.to_string(),
            preview,
        };
        if let Ok(j) = serde_json::to_string(&e) {
            b.set(key, j);
        }
    }

    /// 超出上限时丢最旧的。单独提交 —— 它只是清理，不影响刚那次变更的正确性。
    fn prune_history(&mut self, table: &str) -> Result<()> {
        let prefix = format!("hst/{table}/");
        let mut keys: Vec<String> = self
            .store
            .scan(&prefix)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        if keys.len() <= HISTORY_KEEP {
            return Ok(());
        }
        keys.sort(); // 时间戳定宽，字典序 = 时间序
        let drop = keys.len() - HISTORY_KEEP;
        let mut b = Batch::new();
        for k in keys.into_iter().take(drop) {
            b.del(k);
        }
        self.store.commit(b)?;
        Ok(())
    }

    /// 列出某表的变更历史，新的在前。
    pub fn history(&self, table: &str, limit: usize) -> Vec<HistoryEntry> {
        let prefix = format!("hst/{table}/");
        let mut v: Vec<HistoryEntry> = self
            .store
            .scan(&prefix)
            .into_iter()
            .filter_map(|(_, s)| serde_json::from_str(&s).ok())
            .collect();
        v.sort_by(|a, b| b.key.cmp(&a.key));
        if limit > 0 {
            v.truncate(limit);
        }
        v
    }

    /// 回退一条变更。
    ///
    /// 只处理 update 与 delete —— 这两种是"误操作会丢数据"的高发区。
    /// insert 不记历史：新增不会丢东西，记了反而让列表变吵。
    pub fn undo(&mut self, table: &str, key: &str) -> Result<String> {
        validate_identifier(table)?;
        let raw = self
            .store
            .get(key)
            .ok_or_else(|| "找不到这条历史（可能已被清理）".to_string())?
            .to_string();
        let e: HistoryEntry =
            serde_json::from_str(&raw).map_err(|_| "这条历史读不出来".to_string())?;
        if !e.key.starts_with(&format!("hst/{table}/")) {
            return Err("这条历史不属于这张表".to_string());
        }
        let mut b = Batch::new();
        match e.op.as_str() {
            "update" => {
                let rk = record_key(table, e.rowid);
                let cur = self
                    .store
                    .get(&rk)
                    .ok_or_else(|| "那一行已经不在了，回退不了".to_string())?
                    .to_string();
                let mut rec: BTreeMap<String, Json> =
                    serde_json::from_str(&cur).map_err(|x| format!("记录读不出来: {x}"))?;
                let old: Json = serde_json::from_str(&e.before).unwrap_or(Json::Null);
                rec.insert(e.column.clone(), old);
                b.set(rk, serde_json::to_string(&rec).map_err(|x| x.to_string())?);
            }
            "delete" => {
                // before 存的是整行 JSON，直接放回去就是恢复
                b.set(record_key(table, e.rowid), e.before.clone());
            }
            _ => return Err("不认识的变更类型，回退不了".to_string()),
        }
        // 回退完这条历史就作废了 —— 留着会让人以为还能再退一次
        b.del(key);
        self.store.commit(b)?;
        Ok(format!("已回退：{}", e.preview))
    }

    pub fn insert_rows(&mut self, table: &str, columns: &[String], rows: &[Vec<Option<String>>]) -> Result<usize> {
        self.invalidate_indexes(table);
        validate_identifier(table)?;
        let mut t = self.load_table(table)?;
        let mut b = Batch::new();
        let mut n = 0usize;
        for row in rows {
            let rid = t.next_rowid;
            t.next_rowid += 1;
            let mut rec = BTreeMap::new();
            rec.insert(ROWID_COLUMN.to_string(), Json::from(rid));
            for (i, c) in columns.iter().enumerate() {
                let raw = row.get(i).and_then(|v| v.as_deref());
                let ty = t.field(c).map(|f| f.ty);
                let v = coerce_value(ty, c, raw)?;
                rec.insert(c.clone(), json_from_value(&v));
            }
            // ① 没给值的列：填字段上定义的默认值
            // ② 给了空值的列：也填默认值 —— 界面上"这一格空着"通常就是想要默认值
            for f in &t.fields {
                let given = columns.iter().any(|c| c.eq_ignore_ascii_case(&f.name));
                let empty = match rec.get(&f.name) {
                    None => true,
                    Some(Json::Null) => true,
                    Some(Json::String(x)) => x.is_empty(),
                    _ => false,
                };
                if (given && !empty) || f.default.is_none() {
                    continue;
                }
                let d = f.default.clone().unwrap_or_default();
                let d = unquote_default(&d).to_string();
                let v = coerce_value(Some(f.ty), &f.name, Some(&d))?;
                rec.insert(f.name.clone(), json_from_value(&v));
            }
            b.set(
                record_key(table, rid),
                serde_json::to_string(&rec).map_err(|e| e.to_string())?,
            );
            n += 1;
        }
        t.updated_at = now_ms();
        b.set(
            format!("tbl/{table}"),
            serde_json::to_string(&t).map_err(|e| e.to_string())?,
        );
        self.store.commit(b)?;
        Ok(n)
    }

    pub fn update_cell(&mut self, table: &str, rowid: i64, column: &str, value: Option<&str>) -> Result<SyncReport> {
        self.invalidate_indexes(table);
        validate_identifier(table)?;
        validate_column_name(column)?;
        let t = self.load_table(table)?;
        let f = t
            .field(column)
            .ok_or_else(|| format!("表「{table}」没有列「{column}」"))?;
        if f.lookup.is_some() || f.rollup.is_some() {
            return Err(format!(
                "「{column}」是自动算出来的字段（查找/汇总），请去改它来源的那张表"
            ));
        }
        let v = coerce_value(Some(f.ty), column, value)?;
        let key = record_key(table, rowid);
        // 先把整行读出来（借用结束），后面既要算旧值、也要改它
        let cur = match self.store.get(&key) {
            Some(x) => x.to_string(),
            None => {
                return Err("没有找到要修改的那一行（可能已被删除），请刷新后再试".to_string())
            }
        };
        // 回退要用：先记住这个格子**原来**是什么
        let before = {
            let old: BTreeMap<String, Json> =
                serde_json::from_str(&cur).map_err(|e| format!("记录读不出来: {e}"))?;
            old.get(column).cloned().unwrap_or(Json::Null).to_string()
        };

        let mut b = Batch::new();
        let mut rec: BTreeMap<String, Json> =
            serde_json::from_str(&cur).map_err(|e| format!("记录读不出来: {e}"))?;
        rec.insert(column.to_string(), json_from_value(&v));
        rec.insert(
            ROWID_COLUMN.to_string(),
            Json::from(rowid),
        );
        b.set(
            key,
            serde_json::to_string(&rec).map_err(|e| e.to_string())?,
        );
        let after = rec.get(column).cloned().unwrap_or(Json::Null).to_string();
        // 历史与这次改动**同批提交**：要么都记下，要么都不记
        self.push_history(&mut b, table, "update", rowid, column, &before, &after);
        self.store.commit(b)?;
        self.prune_history(table)?;

        // 传播：改完源字段，看有没有规则要跟着动别的表
        Ok(self.propagate(table, rowid, column, 0))
    }

    pub fn delete_rows(&mut self, table: &str, rowids: &[i64]) -> Result<usize> {
        self.invalidate_indexes(table);
        if rowids.is_empty() {
            return Ok(0);
        }
        validate_identifier(table)?;
        let mut b = Batch::new();
        let mut n = 0usize;
        for r in rowids {
            let k = record_key(table, *r);
            // 整行 JSON 存进历史 —— 删掉之后就没处找了，回退只能靠这一份
            let whole = self.store.get(&k).map(|x| x.to_string());
            if let Some(w) = whole {
                b.del(k);
                self.push_history(&mut b, table, "delete", *r, "", &w, "");
                n += 1;
            }
        }
        self.store.commit(b)?;
        Ok(n)
    }

    fn all_rows(&self, table: &str) -> Vec<(i64, BTreeMap<String, Json>)> {
        let prefix = record_prefix(table);
        self.store
            .scan(&prefix)
            .into_iter()
            .filter_map(|(_, v)| {
                let rec: BTreeMap<String, Json> = serde_json::from_str(&v).ok()?;
                let rid = rec
                    .get(ROWID_COLUMN)
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                Some((rid, rec))
            })
            .collect()
    }

    /// 一条记录里 via 字段是否指向 want 这个 rowid。
    fn row_links_to(rec: &BTreeMap<String, Json>, via: &str, want: i64) -> bool {
        match rec.get(via) {
            Some(Json::Number(n)) => n.as_i64() == Some(want),
            Some(Json::String(s)) => s
                .split(',')
                .filter_map(|p| p.trim().parse::<i64>().ok())
                .any(|x| x == want),
            Some(Json::Array(a)) => a.iter().any(|x| x.as_i64() == Some(want)),
            _ => false,
        }
    }

    /// 找出源记录通过 via 连着的目标 rowid。
    ///
    /// **两种方向都要支持**，因为关联字段挂在哪张表上取决于用户怎么建：
    ///   - 挂在源表上（客户.订单列表）→ 顺着值找；
    ///   - 挂在目标表上（订单.客户）→ 得在目标表里反查"谁的关联字段指向我"。
    /// 只支持第一种的话，最常见的一对多场景一条都同步不到。
    fn resolve_targets(&self, r: &SyncRule, source_rowid: i64) -> Vec<i64> {
        if let Ok(st) = self.load_table(&r.source_table) {
            if st.field(&r.via).is_some() {
                return self.linked_rowids(&r.source_table, source_rowid, &r.via);
            }
        }
        self.all_rows(&r.target_table)
            .into_iter()
            .filter(|(_, rec)| Self::row_links_to(rec, &r.via, source_rowid))
            .map(|(rid, _)| rid)
            .collect()
    }

    /// 按某个字段的关联值，找出源记录连着的那些目标 rowid。
    fn linked_rowids(&self, table: &str, rowid: i64, via: &str) -> Vec<i64> {
        let Some((_, rec)) = self.all_rows(table).into_iter().find(|(r, _)| *r == rowid) else {
            return Vec::new();
        };
        match rec.get(via) {
            Some(Json::Array(arr)) => arr.iter().filter_map(|x| x.as_i64()).collect(),
            Some(Json::String(s)) => s
                .split(',')
                .filter_map(|p| p.trim().parse::<i64>().ok())
                .collect(),
            Some(x) => x.as_i64().into_iter().collect(),
            None => Vec::new(),
        }
    }

    // ---------- 同步传播（ADR-0022 §3） ----------

    /// 一次记录变更后，把命中的同步规则应用下去。
    ///
    /// 三条防线：深度上限 [`MAX_SYNC_DEPTH`]、本轮访问集去重（防环）、
    /// 每次落盘都是一个 store 事务（不留半改状态）。
    pub fn propagate(
        &mut self,
        table: &str,
        rowid: i64,
        column: &str,
        depth: u32,
    ) -> SyncReport {
        let mut report = SyncReport::default();
        if depth >= MAX_SYNC_DEPTH {
            return report;
        }
        let rules = match self.list_sync_rules() {
            Ok(r) => r,
            Err(_) => return report,
        };
        let lower = column.to_lowercase();
        let mut visited: Vec<(String, i64)> = Vec::new();

        for r in rules {
            if !r.enabled {
                continue;
            }
            if !r.source_table.eq_ignore_ascii_case(table) {
                continue;
            }
            if r.source_field.to_lowercase() != lower {
                continue;
            }
            if visited.iter().any(|(rid, rw)| *rid == r.id && *rw == rowid) {
                report.skipped += 1;
                continue;
            }
            visited.push((r.id.clone(), rowid));
            report.visited_rules.push(r.id.clone());

            for target_rowid in self.resolve_targets(&r, rowid) {
                match self.apply_rule(&r, rowid, target_rowid) {
                    Ok(true) => {
                        report.applied += 1;
                        if r.mode == SyncMode::TwoWay {
                            let sub = self.propagate(&r.target_table, target_rowid, &r.target_field, depth + 1);
                            report.applied += sub.applied;
                            report.skipped += sub.skipped;
                        }
                    }
                    Ok(false) => report.skipped += 1,
                    Err(_) => report.skipped += 1,
                }
            }
        }
        report
    }

    /// 把一条规则应用到一对记录上。返回是否真的改了。
    fn apply_rule(&mut self, r: &SyncRule, source_rowid: i64, target_rowid: i64) -> Result<bool> {
        let src = self
            .all_rows(&r.source_table)
            .into_iter()
            .find(|(rid, _)| *rid == source_rowid)
            .map(|(_, rec)| rec)
            .ok_or_else(|| "源记录不存在".to_string())?;
        let source_val = src.get(&r.source_field).cloned().unwrap_or(Json::Null);

        let key = record_key(&r.target_table, target_rowid);
        let raw = self
            .store
            .get(&key)
            .ok_or_else(|| "目标记录不存在".to_string())?
            .to_string();
        let mut rec: BTreeMap<String, Json> =
            serde_json::from_str(&raw).map_err(|e| format!("目标记录读不出来: {e}"))?;

        let cur = rec.get(&r.target_field).cloned().unwrap_or(Json::Null);
        let cur_empty = is_empty_value(&cur);
        if r.scope == ApplyScope::FillEmptyOnly && !cur_empty {
            return Ok(false);
        }
        let next = resolve_conflict(r, &source_val, &cur);
        if values_equal(&next, &cur) {
            return Ok(false);
        }

        rec.insert(r.target_field.clone(), next);
        rec.insert(
            format!("{}", ROWID_COLUMN),
            Json::from(target_rowid),
        );
        let mut b = Batch::new();
        b.set(key, serde_json::to_string(&rec).map_err(|e| e.to_string())?);

        // 在目标表的字段定义上打来源标记 —— 「可追溯」的落点
        let mut touched_table = false;
        if let Ok(mut t) = self.load_table(&r.target_table) {
            if let Some(f) = t.field_mut(&r.target_field) {
                f.sync = Some(SyncMark {
                    rule_id: r.id.clone(),
                    source_table: r.source_table.clone(),
                    source_field: r.source_field.clone(),
                    synced_at: now_ms(),
                });
                touched_table = true;
            }
            if touched_table {
                b.set(
                    format!("tbl/{}", t.name),
                    serde_json::to_string(&t).map_err(|e| e.to_string())?,
                );
            }
        }
        self.store.commit(b)?;
        Ok(true)
    }

    // ---------- 视图 ----------

    pub fn list_views(&self, table: &str) -> Vec<View> {
        self.store
            .scan("viw/")
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_str::<View>(&v).ok())
            .filter(|v| v.table == table)
            .collect()
    }

    /// 按 id 找视图。
    ///
    /// 为什么要它而不是 `list_views(table)`：界面翻页时手里只有视图 id
    /// （从列表里点进来的），让它再回传一次表名既啰嗦又容易传错。
    pub fn find_view(&self, id: &str) -> Option<View> {
        self.store
            .scan("viw/")
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_str::<View>(&v).ok())
            .find(|v| v.id == id)
    }

    pub fn upsert_view(&mut self, v: View) -> Result<()> {
        self.store.put(
            format!("viw/{}", v.id),
            serde_json::to_string(&v).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }

    pub fn delete_view(&mut self, id: &str) -> Result<()> {
        self.store.remove(format!("viw/{id}"))?;
        Ok(())
    }

    /// 按视图取一页数据（替代 `SELECT ... WHERE ... ORDER BY`）。
    pub fn view_page(&self, v: &View, cursor: Option<&str>, limit: usize) -> Result<Page> {
        let t = self.load_table(&v.table)?;
        let mut rows: Vec<(i64, BTreeMap<String, Json>)> = self.all_rows(&v.table);
        if let Some(g) = &v.filter {
            rows.retain(|(_, rec)| match_group(g, rec, &t));
        }
        sort_rows(&mut rows, &v.sorts);
        finish_page(&t, rows, cursor, limit)
    }

    // ---------- 索引 ----------

    fn idx_key(table: &str) -> String {
        format!("idx/{table}")
    }

    /// 这张表上建了哪些索引。
    pub fn list_indexes(&self, table: &str) -> Result<Vec<IndexSpec>> {
        let k = Self::idx_key(table);
        match self.store.get(&k) {
            Some(v) => serde_json::from_str(&v).map_err(|e| format!("索引定义读不出来：{e}")),
            None => Ok(Vec::new()),
        }
    }

    /// 给一列建索引。列不存在要报错 —— 给一个不存在的列建索引没有意义，
    /// 而且会让人误以为"建了但搜不到"是坏了。
    pub fn create_index(&mut self, table: &str, column: &str) -> Result<()> {
        let t = self.load_table(table)?;
        if t.field(column).is_none() {
            return Err(format!("「{table}」里没有「{column}」这一列"));
        }
        let mut list = self.list_indexes(table)?;
        if list.iter().any(|x| x.column == column) {
            return Ok(()); // 已经有了，不重复建
        }
        list.push(IndexSpec {
            table: table.to_string(),
            column: column.to_string(),
        });
        let v = serde_json::to_string(&list).map_err(|e| format!("索引定义存不下来：{e}"))?;
        self.store.put(&Self::idx_key(table), v)?;
        // 顺手把缓存作废：下次用到会按新定义重建
        self.index_cache.remove(&format!("{table}/{column}"));
        Ok(())
    }

    pub fn drop_index(&mut self, table: &str, column: &str) -> Result<()> {
        let mut list = self.list_indexes(table)?;
        let before = list.len();
        list.retain(|x| x.column != column);
        if list.len() == before {
            return Err(format!("「{column}」上本来就没有索引"));
        }
        let v = serde_json::to_string(&list).map_err(|e| format!("索引定义存不下来：{e}"))?;
        self.store.put(&Self::idx_key(table), v)?;
        self.index_cache.remove(&format!("{table}/{column}"));
        Ok(())
    }

    /// 取（必要时构建）某列的索引。
    pub fn get_index(&mut self, table: &str, column: &str) -> Result<&ValueIndex> {
        let ck = format!("{table}/{column}");
        if !self.index_cache.contains_key(&ck) {
            let mut idx = ValueIndex::default();
            let page = self.page_rows(table, None, false, None, usize::MAX)?;
            let ci = page
                .columns
                .iter()
                .position(|c| c == column)
                .ok_or_else(|| format!("「{table}」里没有「{column}」这一列"))?;
            idx.scanned_rows = page.rows.len();
            for row in &page.rows {
                // 第 0 列是 rowid
                let rowid = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
                let val = row.get(ci);
                let txt = match val {
                    Some(serde_json::Value::String(x)) => x.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    _ => continue,
                };
                idx.map.entry(txt).or_default().push(rowid);
            }
            idx.distinct = idx.map.len();
            self.index_cache.insert(ck.clone(), idx);
        }
        self.index_cache
            .get(&ck)
            .ok_or_else(|| "索引建好了却取不到".to_string())
    }

    /// 写操作之后调用：这张表的所有索引作废。
    ///
    /// **宁可作废也不要增量维护** —— 增量维护一旦漏了一种写路径，
    /// 索引就会静默地和数据不一致，而这种错最难查（现象只是"偶尔搜不到"）。
    fn invalidate_indexes(&mut self, table: &str) {
        let prefix = format!("{table}/");
        self.index_cache.retain(|k, _| !k.starts_with(&prefix));
    }

    pub fn page_rows(
        &self,
        table: &str,
        order_by: Option<&str>,
        desc: bool,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page> {
        self.page_rows_filtered(table, order_by, desc, cursor, limit, &[])
    }

    /// 带列关键词筛选的分页（数据网格筛选行）。语义与旧实现一致：包含、忽略大小写、AND。
    pub fn page_rows_filtered(
        &self,
        table: &str,
        order_by: Option<&str>,
        desc: bool,
        cursor: Option<&str>,
        limit: usize,
        filters: &[(String, String)],
    ) -> Result<Page> {
        validate_identifier(table)?;
        if limit == 0 {
            return Err("每页行数至少为 1".to_string());
        }
        let limit = limit.min(MAX_PAGE_LIMIT);
        let t = self.load_table(table)?;
        if t.fields.is_empty() {
            return Err(format!("表「{table}」没有任何列"));
        }
        let mut rows = self.all_rows(table);
        for (col, kw) in filters {
            if kw.is_empty() {
                continue;
            }
            if t.field(col).is_none() {
                return Err(format!("表「{table}」没有列「{col}」，没法按它筛选"));
            }
            let needle = kw.to_lowercase();
            rows.retain(|(_, rec)| {
                rec.get(col)
                    .map(|v| value_text(v).to_lowercase().contains(&needle))
                    .unwrap_or(false)
            });
        }
        let sorts = match order_by {
            Some(c) if t.field(c).is_some() => vec![Sort { field: c.to_string(), desc }],
            _ => vec![Sort { field: ROWID_COLUMN.to_string(), desc }],
        };
        sort_rows(&mut rows, &sorts);
        finish_page(&t, rows, cursor, limit)
    }
}

// ===========================================================================
// 自由函数
// ===========================================================================

fn record_prefix(table: &str) -> String {
    format!("rec/{table}/")
}

/// 历史摘要里用的短值：JSON 字符串带着引号，长文本会把列表撑爆，都截一下。
fn trim_for_preview(v: &str) -> String {
    let t = v.trim_matches('"');
    if t.is_empty() {
        return "（空）".to_string();
    }
    let chars: Vec<char> = t.chars().collect();
    if chars.len() > 12 {
        let head: String = chars.into_iter().take(12).collect();
        format!("{head}…")
    } else {
        t.to_string()
    }
}

fn record_key(table: &str, rowid: i64) -> String {
    format!("rec/{table}/{:020}", rowid)
}

fn is_empty_value(v: &Json) -> bool {
    match v {
        Json::Null => true,
        Json::String(s) => s.is_empty(),
        Json::Array(a) => a.is_empty(),
        _ => false,
    }
}

fn values_equal(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Null, Json::Null) => true,
        (Json::String(x), Json::String(y)) => x == y,
        (Json::Number(x), Json::Number(y)) => x == y,
        (Json::Bool(x), Json::Bool(y)) => x == y,
        (Json::Array(x), Json::Array(y)) => x == y,
        _ => false,
    }
}

/// 冲突策略的裁决。规则写死在这里，界面只负责选 —— 不允许在别处临时发挥。
fn resolve_conflict(r: &SyncRule, source: &Json, target: &Json) -> Json {
    if is_empty_value(target) {
        return source.clone();
    }
    if values_equal(source, target) {
        return target.clone();
    }
    match r.conflict {
        ConflictPolicy::SourceWins => source.clone(),
        ConflictPolicy::TargetWins => target.clone(),
        // LastWrite 在本模型里就是"这次改的为准"：源是刚被改的那个
        ConflictPolicy::LastWriteWins | ConflictPolicy::Ask => source.clone(),
    }
}

fn match_group(g: &FilterGroup, rec: &BTreeMap<String, Json>, t: &Table) -> bool {
    if g.items.is_empty() {
        return true;
    }
    let mut results = g.items.iter().map(|f| match_filter(f, rec, t));
    if g.all {
        results.all(|x| x)
    } else {
        results.any(|x| x)
    }
}

fn match_filter(f: &Filter, rec: &BTreeMap<String, Json>, t: &Table) -> bool {
    let v = rec.get(&f.field).cloned().unwrap_or(Json::Null);
    let empty = is_empty_value(&v);
    match f.op {
        FilterOp::IsEmpty => empty,
        FilterOp::IsNotEmpty => !empty,
        FilterOp::Contains => value_text(&v).to_lowercase().contains(&f.value.to_lowercase()),
        FilterOp::Eq => cmp_value(&v, &f.value, t) == Some(std::cmp::Ordering::Equal),
        FilterOp::Ne => cmp_value(&v, &f.value, t) != Some(std::cmp::Ordering::Equal),
        FilterOp::Gt => cmp_value(&v, &f.value, t) == Some(std::cmp::Ordering::Greater),
        FilterOp::Gte => matches!(
            cmp_value(&v, &f.value, t),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        FilterOp::Lt => cmp_value(&v, &f.value, t) == Some(std::cmp::Ordering::Less),
        FilterOp::Lte => matches!(
            cmp_value(&v, &f.value, t),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}

fn cmp_value(v: &Json, raw: &str, _t: &Table) -> Option<std::cmp::Ordering> {
    match v {
        Json::Number(n) => {
            let rhs = raw.trim().parse::<f64>().ok()?;
            n.as_f64()?.partial_cmp(&rhs)
        }
        Json::Bool(b) => {
            let rhs = raw.trim().eq_ignore_ascii_case("true") || raw.trim() == "1";
            b.partial_cmp(&rhs)
        }
        Json::String(s) => s.partial_cmp(&raw.to_string()),
        _ => None,
    }
}

fn value_text(v: &Json) -> String {
    match v {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => if *b { "是" } else { "否" }.to_string(),
        other => other.to_string(),
    }
}

fn sort_rows(rows: &mut Vec<(i64, BTreeMap<String, Json>)>, sorts: &[Sort]) {
    rows.sort_by(|a, b| {
        for s in sorts {
            let av = a.1.get(&s.field).cloned().unwrap_or(Json::Null);
            let bv = b.1.get(&s.field).cloned().unwrap_or(Json::Null);
            let o = cmp_json(&av, &bv);
            let o = if s.desc { o.reverse() } else { o };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.0.cmp(&b.0)
    });
}

fn cmp_json(a: &Json, b: &Json) -> std::cmp::Ordering {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Json::Bool(x), Json::Bool(y)) => x.cmp(y),
        (Json::String(x), Json::String(y)) => x.cmp(y),
        (Json::Null, Json::Null) => std::cmp::Ordering::Equal,
        (Json::Null, _) => std::cmp::Ordering::Less,
        (_, Json::Null) => std::cmp::Ordering::Greater,
        _ => value_text(a).cmp(&value_text(b)),
    }
}

fn finish_page(
    t: &Table,
    rows: Vec<(i64, BTreeMap<String, Json>)>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page> {
    // next_cursor 是"第一条没被包含进本页的行"，所以下一页要从它本身开始 ——
    // 再 +1 就会跳过一行（这是个真 bug，被分页测试抓住过）。
    let start = match cursor {
        Some(c) => rows
            .iter()
            .position(|(rid, _)| c == format!("{rid}"))
            .unwrap_or(0),
        None => 0,
    };
    let mut columns = vec![ROWID_COLUMN.to_string()];
    columns.extend(t.fields.iter().map(|f| f.name.clone()));

    let mut out = Vec::new();
    let mut has_more = false;
    let mut next = None;
    for (rid, rec) in rows.iter().skip(start) {
        if out.len() >= limit {
            has_more = true;
            next = Some(rid.to_string());
            break;
        }
        let mut line = vec![Json::from(*rid)];
        for f in &t.fields {
            line.push(rec.get(&f.name).cloned().unwrap_or(Json::Null));
        }
        out.push(line);
    }
    Ok(Page {
        rows: out,
        columns,
        has_more,
        next_cursor: next,
    })
}

/// 剥掉默认值外层的成对引号。
///
/// 为什么需要它：default 这个字段历史上承载的是"原样的 SQL 片段"
/// （例如 '12.34' 或 CURRENT_TIMESTAMP），引号由 SQL 自己解析。去掉 SQL 之后
/// 这层解析得自己来 —— 否则用户填的默认值 12.34 到了金额列会变成
/// 「'12.34' 不是金额」这种看不懂的错。
fn unquote_default(raw: &str) -> &str {
    let t = raw.trim();
    if t.len() >= 2 {
        let first = t.chars().next().unwrap();
        let last = t.chars().last().unwrap();
        if (first == '\'' || first == '"') && first == last {
            return &t[1..t.len() - 1];
        }
    }
    t
}

// ---------- 值校验（从 schema.rs 搬过来，去掉了 SQL 部分） ----------

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    #[allow(dead_code)] // 保留二进制列类型；导入含二进制的表时会用到
    Blob(Vec<u8>),
}

fn json_from_value(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::from(*i),
        Value::Real(r) => Json::from(*r),
        Value::Text(s) => Json::from(s.clone()),
        Value::Blob(b) => Json::from(b.clone()),
    }
}

pub fn coerce_value(ty: Option<ColType>, column: &str, raw: Option<&str>) -> Result<Value> {
    let Some(raw) = raw else { return Ok(Value::Null) };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    let err = |e: String| -> String { format!("列「{column}」：{e}") };
    match ty {
        Some(ColType::Integer) => Ok(Value::Integer(
            parse_integer(raw).map_err(err)?,
        )),
        Some(ColType::Real) => Ok(Value::Real(parse_real(raw).map_err(err)?)),
        Some(ColType::Money) => Ok(Value::Integer(money_parse(raw).map_err(err)?)),
        Some(ColType::Boolean) => Ok(Value::Integer(parse_bool(raw).map_err(err)?)),
        Some(ColType::Date) => Ok(Value::Text(normalize_date(raw).map_err(err)?)),
        Some(ColType::DateTime) => Ok(Value::Text(normalize_datetime(raw).map_err(err)?)),
        Some(ColType::Json) => {
            serde_json::from_str::<Json>(raw).map_err(|e| err(format!("不是合法 JSON: {e}")))?;
            Ok(Value::Text(raw.to_string()))
        }
        Some(ColType::Blob) | Some(ColType::Text) | None => Ok(Value::Text(raw.to_string())),
    }
}

fn parse_integer(raw: &str) -> Result<i64> {
    let s = to_halfwidth(raw).replace([' ', ','], "");
    s.parse::<i64>()
        .map_err(|_| format!("「{raw}」不是整数"))
}

fn parse_real(raw: &str) -> Result<f64> {
    let s = to_halfwidth(raw).replace([' ', ','], "");
    s.parse::<f64>()
        .map_err(|_| format!("「{raw}」不是数字"))
}

fn parse_bool(raw: &str) -> Result<i64> {
    let s = raw.trim().to_lowercase();
    match s.as_str() {
        "1" | "true" | "yes" | "y" | "是" | "真" | "对" => Ok(1),
        "0" | "false" | "no" | "n" | "否" | "假" | "错" => Ok(0),
        _ => Err(format!("「{raw}」不是是/否值")),
    }
}

/// 金额一律存「分」的整数。浮点存钱迟早出事，这是不想再付一次的学费。
pub fn money_parse(raw: &str) -> Result<i64> {
    let s = to_halfwidth(raw).trim().replace([',', ' ', '¥', '￥'], "");
    if s.is_empty() {
        return Err("金额为空".to_string());
    }
    let neg = s.starts_with('-') || (s.starts_with('(') && s.ends_with(')'));
    let body = s.trim_start_matches('-').trim_matches(|c| c == '(' || c == ')');
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit()) && !int_part.is_empty() {
        return Err(format!("「{raw}」不是金额"));
    }
    if !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("「{raw}」不是金额"));
    }
    if frac_part.len() > 2 {
        // 分以下没有单位，悄悄截断会让对账差钱 —— 宁可报错让用户改
        return Err(format!(
            "「{raw}」的小数位超过 2 位 —— 金额最小到分，请改成两位小数"
        ));
    }
    let yuan: i64 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse::<i64>()
            .map_err(|_| format!("「{raw}」不是金额"))?
    };
    let mut frac = frac_part.to_string();
    while frac.len() < 2 {
        frac.push('0');
    }
    let cents: i64 = frac[..2]
        .parse::<i64>()
        .map_err(|_| format!("「{raw}」不是金额"))?;
    let v = yuan * 100 + cents;
    Ok(if neg { -v } else { v })
}

/// 分 → 元字符串。前端有同一份实现（`db.js` 的 centsToYuan），
/// 后端保留它是为了导出与报表 —— 两条规则必须永远一致，改一处要改两处。
#[allow(dead_code)]
pub fn money_display(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let a = cents.abs();
    format!("{sign}{}.{:02}", a / 100, a % 100)
}

/// 全角数字/符号转半角 —— 中文 Excel 里导出的数字经常是全角的。
fn to_halfwidth(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            '．' => '.',
            '－' | '—' | '–' => '-',
            '＋' => '+',
            '（' => '(',
            '）' => ')',
            '，' => ',',
            '　' => ' ',
            other => other,
        })
        .collect()
}

fn normalize_date(raw: &str) -> Result<String> {
    let s = to_halfwidth(raw);
    let s = s.trim();
    let parts: Vec<&str> = s
        .split(|c: char| c == '-' || c == '/' || c == '.' || c == '年' || c == '月')
        .collect();
    if parts.len() < 3 {
        return Err(format!("「{raw}」不是日期，想要的样子是 2026-09-20"));
    }
    let y: i64 = parts[0]
        .trim_end_matches('年')
        .trim()
        .parse()
        .map_err(|_| format!("「{raw}」的年份看不懂"))?;
    let m: i64 = parts[1]
        .trim_end_matches('月')
        .trim()
        .parse()
        .map_err(|_| format!("「{raw}」的月份看不懂"))?;
    let d: i64 = parts[2]
        .trim_end_matches('日')
        .trim()
        .parse()
        .map_err(|_| format!("「{raw}」的日期看不懂"))?;
    if !(1..=12).contains(&m) {
        return Err(format!("「{raw}」的月份不在 1–12 之间"));
    }
    if d < 1 || d > days_in_month(y, m) {
        return Err(format!("「{raw}」的日期超出了那个月的天数"));
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

fn normalize_datetime(raw: &str) -> Result<String> {
    let s = to_halfwidth(raw).trim().replace('T', " ");
    let (d, t) = match s.split_once(' ') {
        Some((a, b)) => (a, b),
        None => (s.as_str(), "00:00:00"),
    };
    let date = normalize_date(d)?;
    let tp: Vec<&str> = t
        .trim_end_matches(|c| c == '秒')
        .split(|c| c == ':' || c == '时' || c == '分')
        .collect();
    let h: i64 = tp
        .first()
        .and_then(|x| x.trim().parse().ok())
        .unwrap_or(0);
    let mi: i64 = tp.get(1).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let se: i64 = tp.get(2).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    if h > 23 || mi > 59 || se > 59 {
        return Err(format!("「{raw}」的时间部分不对"));
    }
    Ok(format!("{date} {h:02}:{mi:02}:{se:02}"))
}

// ---------- 标识符 ----------

pub fn validate_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err("名字不能为空".to_string());
    }
    if name.chars().count() > MAX_IDENT_CHARS {
        return Err(format!(
            "名字最多 {MAX_IDENT_CHARS} 个字符，现在是 {} 个",
            name.chars().count()
        ));
    }
    let mut it = name.chars();
    let first = it.next().unwrap();
    if !first.is_alphabetic() && first != '_' {
        return Err(format!("名字「{name}」不能以「{first}」开头"));
    }
    if !it.all(|c| c.is_alphanumeric() || c == '_') {
        return Err(format!(
            "名字「{name}」只能包含中文、字母、数字和下划线"
        ));
    }
    Ok(())
}

pub fn validate_column_name(name: &str) -> Result<()> {
    validate_identifier(name)?;
    // rowid 是内部行号，被遮蔽之后"编辑"会改到别的行上
    for reserved in ["rowid", "_rowid_", "oid"] {
        if name.eq_ignore_ascii_case(reserved) {
            return Err(format!("列名不能用「{reserved}」，它是系统保留的行号"));
        }
    }
    Ok(())
}

// ===========================================================================
// 测试
// ===========================================================================

// ---------- 列值规范化（P1-5，从 schema.rs 搬来，去掉 SQL 遍历） ----------

/// 规范化规则。只做两条最痛的：日期格式、数字清洁。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NormalizeRule {
    Date,
    Number,
}

/// 一条看不懂的值（原样保留，但要让用户知道是哪一行）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkippedRow {
    pub rowid: i64,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NormalizeReport {
    pub total: usize,
    pub changed: usize,
    pub skipped: Vec<SkippedRow>,
}

/// 日期 → `YYYY-MM-DD`。带时间的（含空格或 T）只取日期部分。
fn iso_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let date_part = match s.find([' ', 'T']) {
        Some(i) => &s[..i],
        None => s,
    };
    normalize_date(date_part).ok()
}

/// 数字清洁：去掉千分位、空格与全角，留下一个能解析的数。
fn clean_number(raw: &str) -> Option<String> {
    let s = to_halfwidth(raw).trim().replace([',', ' ', '¥', '￥'], "");
    if s.is_empty() {
        return None;
    }
    let neg = s.starts_with('-');
    let body = s.trim_start_matches('-');
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    if body.matches('.').count() > 1 {
        return None;
    }
    let t = body.trim_matches('.');
    if t.is_empty() {
        return None;
    }
    Some(if neg { format!("-{t}") } else { t.to_string() })
}

#[cfg(test)]
mod tests {
    // ---------- 变更历史（"改错了能回退"）----------

    #[test]
    fn 改一格能退回去() {
        let dir = tmp("hist-update");
        let mut d = Db::open(&dir).unwrap();
        d.create_table(&spec("账本", &[("名称", ColType::Text), ("金额", ColType::Money)]))
            .unwrap();
        d.insert_rows(
            "账本",
            &["名称".to_string(), "金额".to_string()],
            &[vec![Some("甲".to_string()), Some("12.34".to_string())]],
        )
        .unwrap();
        // rowid 在返回行的第 0 列（ROWID_COLUMN），界面不显示但内部要用
        let rid = d.page_rows("账本", None, false, None, 10).unwrap().rows[0][0]
            .as_i64()
            .unwrap();

        // 改一次 → 应该留下一条历史
        d.update_cell("账本", rid, "金额", Some("99.99")).unwrap();
        let h = d.history("账本", 10);
        assert_eq!(h.len(), 1, "改一格应该留一条历史");
        assert_eq!(h[0].op, "update");
        assert_eq!(h[0].column, "金额");

        // 回退 → 值应该变回 12.34 元（库里按分存，即 1234）
        let key = h[0].key.clone();
        d.undo("账本", &key).unwrap();
        let raw = d.store.get(&record_key("账本", rid)).unwrap().to_string();
        assert!(raw.contains("1234"), "回退后应该回到 1234 分，实际：{raw}");
        // 回退完这条历史就作废了
        assert!(
            d.history("账本", 10).iter().all(|x| x.key != key),
            "回退过的历史不该还在列表里"
        );
    }

    #[test]
    fn 删掉一行能整行恢复() {
        let dir = tmp("hist-delete");
        let mut d = Db::open(&dir).unwrap();
        d.create_table(&spec("名单", &[("名称", ColType::Text), ("备注", ColType::Text)]))
            .unwrap();
        d.insert_rows(
            "名单",
            &["名称".to_string(), "备注".to_string()],
            &[vec![Some("张三".to_string()), Some("重要客户".to_string())]],
        )
        .unwrap();
        // rowid 在返回行的第 0 列（ROWID_COLUMN），界面不显示但内部要用
        let rid = d.page_rows("名单", None, false, None, 10).unwrap().rows[0][0]
            .as_i64()
            .unwrap();
        let before_rows = d.store.count(&format!("rec/名单/"));

        d.delete_rows("名单", &[rid]).unwrap();
        assert_eq!(d.store.count(&format!("rec/名单/")), before_rows - 1);

        let h = d.history("名单", 10);
        assert_eq!(h.len(), 1, "删一行应该留一条历史");
        assert_eq!(h[0].op, "delete");

        d.undo("名单", &h[0].key).unwrap();
        assert_eq!(
            d.store.count(&format!("rec/名单/")),
            before_rows,
            "回退删除后行数应该恢复"
        );
        // 整行内容也要回来，不能只是多了一行空壳
        let raw = d.store.get(&record_key("名单", rid)).unwrap().to_string();
        assert!(raw.contains("张三"), "恢复的行应该带着原来的内容，实际：{raw}");
        assert!(raw.contains("重要客户"), "备注也该一起回来");
    }

    #[test]
    fn 历史有上限不会无限涨() {
        let dir = tmp("hist-prune");
        let mut d = Db::open(&dir).unwrap();
        d.create_table(&spec("流水", &[("值", ColType::Text)]))
            .unwrap();
        d.insert_rows("流水", &["值".to_string()], &[vec![Some("起".to_string())]])
            .unwrap();
        // rowid 在返回行的第 0 列（ROWID_COLUMN），界面不显示但内部要用
        let rid = d.page_rows("流水", None, false, None, 10).unwrap().rows[0][0]
            .as_i64()
            .unwrap();
        // 改的次数超过上限，历史条数应该被压住而不是无限增长
        for i in 0..(HISTORY_KEEP + 20) {
            d.update_cell("流水", rid, "值", Some(&format!("v{i}"))).unwrap();
        }
        let h = d.history("流水", 100000);
        assert!(
            h.len() <= HISTORY_KEEP,
            "历史条数应被压在上限内，实际 {} 条",
            h.len()
        );
    }

    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("dkb_model_{}_{}", tag, std::process::id()));
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

    #[test]
    fn 建表列表改名删除() {
        let d = tmp("crud");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("客户", &[("客户名", ColType::Text), ("余额", ColType::Money)]))
            .unwrap();
        assert_eq!(db.list_tables().unwrap().len(), 1);

        db.rename_table("客户", "顾客").unwrap();
        assert!(db.get_table("顾客").is_ok());
        assert!(db.get_table("客户").is_err());

        db.drop_table("顾客", "顾客").unwrap();
        assert_eq!(db.list_tables().unwrap().len(), 0);
    }

    #[test]
    fn 删表要确认名一致() {
        let d = tmp("confirm");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("a", ColType::Text)])).unwrap();
        assert!(db.drop_table("t", "x").is_err());
        assert_eq!(db.list_tables().unwrap().len(), 1);
    }

    #[test]
    fn 插入与分页() {
        let d = tmp("page");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("名字", ColType::Text), ("序号", ColType::Integer)]))
            .unwrap();
        let cols = vec!["名字".to_string(), "序号".to_string()];
        let rows: Vec<Vec<Option<String>>> = (0..10)
            .map(|i| vec![Some(format!("第{i}个")), Some(i.to_string())])
            .collect();
        assert_eq!(db.insert_rows("t", &cols, &rows).unwrap(), 10);

        let p = db.page_rows("t", Some("序号"), false, None, 4).unwrap();
        assert_eq!(p.rows.len(), 4);
        assert!(p.has_more);
        assert_eq!(p.columns[0], ROWID_COLUMN);
        // 第 0 列是 rowid，第 2 列是序号
        assert_eq!(p.rows[0][2], Json::from(0));

        let p2 = db
            .page_rows("t", Some("序号"), false, p.next_cursor.as_deref(), 4)
            .unwrap();
        assert_eq!(p2.rows[0][2], Json::from(4));

        let p3 = db.page_rows("t", Some("序号"), true, None, 3).unwrap();
        assert_eq!(p3.rows[0][2], Json::from(9));
    }

    #[test]
    fn 关键词筛选能过滤行() {
        let d = tmp("filter");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("名字", ColType::Text)])).unwrap();
        let cols = vec!["名字".to_string()];
        let rows: Vec<Vec<Option<String>>> = vec![
            vec![Some("张三".to_string())],
            vec![Some("李四".to_string())],
            vec![Some("张小三".to_string())],
        ];
        db.insert_rows("t", &cols, &rows).unwrap();
        let p = db
            .page_rows_filtered("t", None, false, None, 10, &[("名字".to_string(), "张".to_string())])
            .unwrap();
        assert_eq!(p.rows.len(), 2);
    }

    #[test]
    fn 改单元格与删行() {
        let d = tmp("cell");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("a", ColType::Text)])).unwrap();
        db.insert_rows("t", &["a".to_string()], &[vec![Some("旧".to_string())]])
            .unwrap();
        db.update_cell("t", 1, "a", Some("新")).unwrap();
        let p = db.page_rows("t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][1], Json::from("新"));
        assert_eq!(db.delete_rows("t", &[1]).unwrap(), 1);
        assert_eq!(db.delete_rows("t", &[999]).unwrap(), 0);
    }

    #[test]
    fn 类型校验拦住脏值() {
        let d = tmp("coerce");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("n", ColType::Integer), ("m", ColType::Money)]))
            .unwrap();
        let cols = vec!["n".to_string(), "m".to_string()];
        db.insert_rows("t", &cols, &[vec![Some("12".to_string()), Some("3.5".to_string())]])
            .unwrap();
        let p = db.page_rows("t", None, false, None, 10).unwrap();
        assert_eq!(p.rows[0][1], Json::from(12));
        assert_eq!(p.rows[0][2], Json::from(350), "金额按分存");
        assert!(db
            .insert_rows("t", &cols, &[vec![Some("中文".to_string()), None]])
            .is_err());
    }

    #[test]
    fn 金额解析与显示() {
        assert_eq!(money_parse("1,234.56").unwrap(), 123456);
        assert_eq!(money_parse("¥12").unwrap(), 1200);
        assert_eq!(money_parse("(5.00)").unwrap(), -500);
        assert_eq!(money_display(-500), "-5.00");
        assert_eq!(money_display(123456), "1234.56");
    }

    #[test]
    fn 日期规范化() {
        assert_eq!(normalize_date("2026/9/20").unwrap(), "2026-09-20");
        assert_eq!(normalize_date("2026年9月20日").unwrap(), "2026-09-20");
        assert!(normalize_date("2026-02-30").is_err(), "2 月没有 30 号");
        assert!(normalize_date("2026-13-01").is_err());
    }

    #[test]
    fn 标识符校验() {
        assert!(validate_identifier("客户表").is_ok());
        assert!(validate_identifier("_a1").is_ok());
        assert!(validate_identifier("1表").is_err());
        assert!(validate_identifier("a-b").is_err());
        assert!(validate_column_name("rowid").is_err());
    }

    #[test]
    fn 加列删列改列名_记录里的值跟着走() {
        let d = tmp("cols");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("t", &[("a", ColType::Text), ("b", ColType::Text)]))
            .unwrap();
        db.insert_rows(
            "t",
            &["a".to_string(), "b".to_string()],
            &[vec![Some("1".to_string()), Some("2".to_string())]],
        )
        .unwrap();

        db.rename_column("t", "b", "c").unwrap();
        let p = db.page_rows("t", None, false, None, 10).unwrap();
        assert!(p.columns.contains(&"c".to_string()));
        assert!(!p.columns.contains(&"b".to_string()));
        assert_eq!(p.rows[0][2], Json::from("2"));

        db.drop_column("t", "c").unwrap();
        let p = db.page_rows("t", None, false, None, 10).unwrap();
        assert_eq!(p.columns.len(), 2, "只剩 _rowid 和 a");
    }

    #[test]
    fn 系统键值取代sys_meta表() {
        let d = tmp("meta");
        let mut db = Db::open(&d).unwrap();
        db.meta_set("ai/enabled", "1").unwrap();
        assert_eq!(db.meta_get("ai/enabled").as_deref(), Some("1"));
        db.meta_del("ai/enabled").unwrap();
        assert_eq!(db.meta_get("ai/enabled"), None);
    }

    // ---------- 共通字段 ----------

    #[test]
    fn 共通字段改定义会推到所有引用表() {
        let d = tmp("shared");
        let mut db = Db::open(&d).unwrap();
        let sf = SharedField {
            id: "sf1".to_string(),
            name: "状态".to_string(),
            ty: ColType::Text,
            options: vec!["待办".to_string()],
            default: None,
            comment: None,
            used_by: vec!["表A::状态".to_string(), "表B::状态".to_string()],
        };
        db.upsert_shared_field(sf.clone(), false).unwrap();

        for t in ["表A", "表B"] {
            let mut s = spec(t, &[("名称", ColType::Text)]);
            s.columns.push(ColumnDef {
                name: "状态".to_string(),
                ty: ColType::Text,
                not_null: false,
                default: None,
                primary_key: false,
                comment: None,
                shared: Some("sf1".to_string()),
                link: None,
                lookup: None,
                rollup: None,
            });
            db.create_table(&s).unwrap();
        }

        let mut sf2 = sf.clone();
        sf2.ty = ColType::Integer;
        let touched = db.upsert_shared_field(sf2, true).unwrap();
        assert_eq!(touched, 2, "两张引用表都要被更新");

        let meta = db.column_meta("表A").unwrap();
        let st = meta.iter().find(|m| m.name == "状态").unwrap();
        assert_eq!(st.semantic, Some(ColType::Integer));
        assert_eq!(st.shared.as_deref(), Some("sf1"), "界面要靠它打共通徽标");
    }

    // ---------- 同步规则 ----------

    fn rule(id: &str, enabled: bool, mode: SyncMode) -> SyncRule {
        SyncRule {
            id: id.to_string(),
            name: "同步客户名".to_string(),
            enabled,
            source_table: "客户".to_string(),
            source_field: "客户名".to_string(),
            target_table: "订单".to_string(),
            target_field: "客户名".to_string(),
            via: "客户".to_string(),
            mode,
            conflict: ConflictPolicy::SourceWins,
            scope: ApplyScope::Always,
            created_at: 0,
        }
    }

    #[test]
    fn 改源表_目标表跟着改_并留下来源标记() {
        let d = tmp("sync1");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec(
            "客户",
            &[("客户名", ColType::Text), ("电话", ColType::Text)],
        ))
        .unwrap();
        let mut s = spec("订单", &[("金额", ColType::Money), ("客户名", ColType::Text)]);
        s.columns.push(ColumnDef {
            name: "客户".to_string(),
            ty: ColType::Integer,
            not_null: false,
            default: None,
            primary_key: false,
            comment: None,
            shared: None,
            link: Some(LinkSpec {
                target: "客户".to_string(),
                many: false,
                back_field: None,
            }),
            lookup: None,
            rollup: None,
        });
        db.create_table(&s).unwrap();

        db.insert_rows(
            "客户",
            &["客户名".to_string(), "电话".to_string()],
            &[vec![Some("张三".to_string()), Some("130".to_string())]],
        )
        .unwrap();
        db.insert_rows(
            "订单",
            &["金额".to_string(), "客户名".to_string(), "客户".to_string()],
            &[vec![Some("10".to_string()), None, Some("1".to_string())]],
        )
        .unwrap();

        db.upsert_sync_rule(rule("r1", true, SyncMode::Mirror))
            .unwrap();
        let rep = db.update_cell("客户", 1, "客户名", Some("李四")).unwrap();
        assert_eq!(rep.applied, 1, "应该同步了一条");

        let p = db.page_rows("订单", None, false, None, 10).unwrap();
        let ci = p.columns.iter().position(|c| c == "客户名").unwrap();
        assert_eq!(p.rows[0][ci], Json::from("李四"), "订单表的客户名要跟着变");

        let meta = db.column_meta("订单").unwrap();
        let m = meta.iter().find(|x| x.name == "客户名").unwrap();
        let mark = m.sync.as_ref().expect("要留下来源标记");
        assert_eq!(mark.source_table, "客户");
        assert_eq!(mark.source_field, "客户名");
        assert!(m.read_only, "Mirror 模式下目标字段只读");
    }

    /// 加列时也要校验关联目标。
    ///
    /// 这条校验以前**只在 create_table 里有，add_column 漏了** —— 于是
    /// "不能建指向不存在表的关联"这条约束，在加列时形同不存在。
    /// 实测能建出关联到「根本不存在的表」的列，之后读到的是悬空引用。
    /// 而加列是更常用的入口（建表时要先有目标表，加列时两边往往都已存在）。
    /// 索引要真的把"行数"压成"唯一值数" —— 否则它就是个摆设。
    #[test]
    fn 索引_建了能列出来_且值是去重的() {
        let d = tmp("idx1");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("客户", &[("客户名", ColType::Text)])).unwrap();
        // 3 行，其中两行同值 —— 去重后应当是 2
        db.insert_rows(
            "客户",
            &["客户名".to_string()],
            &vec![
                vec![Some("张三".to_string())],
                vec![Some("李四".to_string())],
                vec![Some("张三".to_string())],
            ],
        )
        .unwrap();

        assert!(db.list_indexes("客户").unwrap().is_empty(), "一开始没有索引");
        db.create_index("客户", "客户名").unwrap();
        let list = db.list_indexes("客户").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].column, "客户名");

        let idx = db.get_index("客户", "客户名").unwrap();
        assert_eq!(idx.scanned_rows, 3, "扫了 3 行");
        assert_eq!(idx.distinct, 2, "**去重后只有 2 个不同的值** —— 这就是索引的意义");
        assert_eq!(idx.map.get("张三").unwrap().len(), 2, "张三对应 2 行");

        // 给不存在的列建索引要报错（建了但搜不到会让人以为是坏了）
        let e = db.create_index("客户", "没有这列").unwrap_err();
        assert!(e.contains("没有这列"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 数据变了之后索引必须跟着变。**这是索引最容易出错的地方** ——
    /// 增量维护只要漏掉一种写路径，索引就会静默地和资料不一致，
    /// 而现象只是"偶尔搜不到"，极难查。所以这里选了"写即作废"。
    #[test]
    fn 索引_写操作后跟着数据变() {
        let d = tmp("idx2");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("客户", &[("客户名", ColType::Text)])).unwrap();
        db.insert_rows(
            "客户",
            &["客户名".to_string()],
            &vec![vec![Some("张三".to_string())]],
        )
        .unwrap();
        db.create_index("客户", "客户名").unwrap();
        assert_eq!(db.get_index("客户", "客户名").unwrap().distinct, 1);

        // 再插一行新的值 → 索引必须看得到
        db.insert_rows(
            "客户",
            &["客户名".to_string()],
            &vec![vec![Some("王五".to_string())]],
        )
        .unwrap();
        assert_eq!(
            db.get_index("客户", "客户名").unwrap().distinct,
            2,
            "插了新值之后索引必须重建，否则搜不到新数据"
        );

        // 改一个格 → 也要重建
        let rowid = db.get_index("客户", "客户名").unwrap().map.get("张三").unwrap()[0];
        db.update_cell("客户", rowid, "客户名", Some("赵六")).unwrap();
        let idx = db.get_index("客户", "客户名").unwrap();
        assert!(idx.map.contains_key("赵六"), "改过的值要能在索引里找到");
        assert!(!idx.map.contains_key("张三"), "旧值不该还在索引里");

        // 删索引
        db.drop_index("客户", "客户名").unwrap();
        assert!(db.list_indexes("客户").unwrap().is_empty());
        let e = db.drop_index("客户", "客户名").unwrap_err();
        assert!(e.contains("本来就没有"), "重复删要给个说法");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn 加列关联到不存在的表要被拦下() {
        let d = tmp("linkadd");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("订单", &[("单号", ColType::Text)])).unwrap();

        let mk = |target: &str, many: bool| ColumnDef {
            name: "客户".to_string(),
            ty: ColType::Text,
            not_null: false,
            default: None,
            primary_key: false,
            comment: None,
            shared: None,
            link: Some(LinkSpec {
                target: target.to_string(),
                many,
                back_field: None,
            }),
            lookup: None,
            rollup: None,
        };

        let e = db.add_column("订单", &mk("客户", false)).unwrap_err();
        assert!(e.contains("不存在"), "要说清是目标表不存在：{e}");

        // 目标表建出来之后就该放行
        db.create_table(&spec("客户", &[("客户名", ColType::Text)]))
            .unwrap();
        db.add_column("订单", &mk("客户", true)).unwrap();
        let meta = db.column_meta("订单").unwrap();
        let m = meta.iter().find(|x| x.name == "客户").expect("列应当在");
        let lk = m.link.as_ref().expect("关联属性要存下来");
        assert_eq!(lk.target, "客户");
        assert!(lk.many, "「可关联多条」这一档要存住");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn 关掉规则就不再同步_而且目标恢复可编辑() {
        let d = tmp("sync2");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("客户", &[("客户名", ColType::Text)])).unwrap();
        let mut s = spec("订单", &[("客户名", ColType::Text)]);
        s.columns.push(ColumnDef {
            name: "客户".to_string(),
            ty: ColType::Integer,
            not_null: false,
            default: None,
            primary_key: false,
            comment: None,
            shared: None,
            link: Some(LinkSpec { target: "客户".to_string(), many: false, back_field: None }),
            lookup: None,
            rollup: None,
        });
        db.create_table(&s).unwrap();
        db.insert_rows("客户", &["客户名".to_string()], &[vec![Some("张三".to_string())]])
            .unwrap();
        db.insert_rows(
            "订单",
            &["客户名".to_string(), "客户".to_string()],
            &[vec![None, Some("1".to_string())]],
        )
        .unwrap();
        db.upsert_sync_rule(rule("r1", true, SyncMode::Mirror)).unwrap();

        db.set_rule_enabled("r1", false).unwrap();
        let rep = db.update_cell("客户", 1, "客户名", Some("李四")).unwrap();
        assert_eq!(rep.applied, 0, "规则关了就不该再同步");

        let meta = db.column_meta("订单").unwrap();
        let m = meta.iter().find(|x| x.name == "客户名").unwrap();
        assert!(!m.read_only, "规则关掉后目标字段恢复可编辑");
    }

    #[test]
    fn 只填空模式不会覆盖已有值() {
        let d = tmp("sync3");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec("客户", &[("客户名", ColType::Text)])).unwrap();
        let mut s = spec("订单", &[("客户名", ColType::Text)]);
        s.columns.push(ColumnDef {
            name: "客户".to_string(),
            ty: ColType::Integer,
            not_null: false,
            default: None,
            primary_key: false,
            comment: None,
            shared: None,
            link: Some(LinkSpec { target: "客户".to_string(), many: false, back_field: None }),
            lookup: None,
            rollup: None,
        });
        db.create_table(&s).unwrap();
        db.insert_rows("客户", &["客户名".to_string()], &[vec![Some("张三".to_string())]])
            .unwrap();
        db.insert_rows(
            "订单",
            &["客户名".to_string(), "客户".to_string()],
            &[vec![Some("已有值".to_string()), Some("1".to_string())]],
        )
        .unwrap();

        let mut r = rule("r1", true, SyncMode::Mirror);
        r.scope = ApplyScope::FillEmptyOnly;
        db.upsert_sync_rule(r).unwrap();

        let rep = db.update_cell("客户", 1, "客户名", Some("李四")).unwrap();
        assert_eq!(rep.applied, 0);
        let p = db.page_rows("订单", None, false, None, 10).unwrap();
        let ci = p.columns.iter().position(|c| c == "客户名").unwrap();
        assert_eq!(p.rows[0][ci], Json::from("已有值"));
    }

    #[test]
    fn 互相同步不会死循环() {
        let d = tmp("cycle");
        let mut db = Db::open(&d).unwrap();
        // 先建两张普通表 —— 建表时校验关联目标必须已存在，不能互相指着还没建的表
        for t in ["A", "B"] {
            db.create_table(&spec(t, &[("v", ColType::Text)])).unwrap();
        }
        // 再各自加一条指向对方的关联列
        for t in ["A", "B"] {
            db.add_column(
                t,
                &ColumnDef {
                    name: "peer".to_string(),
                    ty: ColType::Integer,
                    not_null: false,
                    default: None,
                    primary_key: false,
                    comment: None,
                    shared: None,
                    link: Some(LinkSpec {
                        target: if t == "A" { "B" } else { "A" }.to_string(),
                        many: false,
                        back_field: None,
                    }),
                    lookup: None,
                    rollup: None,
                },
            )
            .unwrap();
        }
        db.insert_rows("A", &["v".to_string(), "peer".to_string()], &[vec![None, Some("1".to_string())]])
            .unwrap();
        db.insert_rows("B", &["v".to_string(), "peer".to_string()], &[vec![None, Some("1".to_string())]])
            .unwrap();

        // A→B 与 B→A 两条双向规则，互相触发
        let ra = SyncRule {
            id: "ra".to_string(),
            name: "A到B".to_string(),
            enabled: true,
            source_table: "A".to_string(),
            source_field: "v".to_string(),
            target_table: "B".to_string(),
            target_field: "v".to_string(),
            via: "peer".to_string(),
            mode: SyncMode::TwoWay,
            conflict: ConflictPolicy::SourceWins,
            scope: ApplyScope::Always,
            created_at: 0,
        };
        let rb = SyncRule {
            id: "rb".to_string(),
            name: "B到A".to_string(),
            enabled: true,
            source_table: "B".to_string(),
            source_field: "v".to_string(),
            target_table: "A".to_string(),
            target_field: "v".to_string(),
            via: "peer".to_string(),
            mode: SyncMode::TwoWay,
            conflict: ConflictPolicy::SourceWins,
            scope: ApplyScope::Always,
            created_at: 0,
        };
        db.upsert_sync_rule(ra).unwrap();
        db.upsert_sync_rule(rb).unwrap();

        // 能跑完就是胜利：不死循环、不栈溢出，且最终两侧一致
        let rep = db.update_cell("A", 1, "v", Some("x")).unwrap();
        assert!(rep.applied >= 1);
        let pa = db.page_rows("A", None, false, None, 10).unwrap();
        let pb = db.page_rows("B", None, false, None, 10).unwrap();
        let ia = pa.columns.iter().position(|c| c == "v").unwrap();
        let ib = pb.columns.iter().position(|c| c == "v").unwrap();
        assert_eq!(pa.rows[0][ia], pb.rows[0][ib], "环收敛后两侧应当一致");
    }

    // ---------- 视图 ----------

    #[test]
    fn 命名视图能筛选排序_不需要任何查询语言() {
        let d = tmp("view");
        let mut db = Db::open(&d).unwrap();
        db.create_table(&spec(
            "订单",
            &[("客户", ColType::Text), ("金额", ColType::Money)],
        ))
        .unwrap();
        let cols = vec!["客户".to_string(), "金额".to_string()];
        let rows: Vec<Vec<Option<String>>> = vec![
            vec![Some("张三".to_string()), Some("10".to_string())],
            vec![Some("李四".to_string()), Some("200".to_string())],
            vec![Some("王五".to_string()), Some("3000".to_string())],
        ];
        db.insert_rows("订单", &cols, &rows).unwrap();

        let v = View {
            id: "v1".to_string(),
            table: "订单".to_string(),
            name: "大额订单".to_string(),
            filter: Some(FilterGroup {
                all: true,
                items: vec![Filter {
                    field: "金额".to_string(),
                    op: FilterOp::Gte,
                    value: "20000".to_string(), // 200.00 元 = 20000 分
                }],
            }),
            sorts: vec![Sort { field: "金额".to_string(), desc: true }],
            group_by: None,
            hidden: vec![],
        };
        db.upsert_view(v.clone()).unwrap();
        let p = db.view_page(&v, None, 10).unwrap();
        assert_eq!(p.rows.len(), 2, "只有 200 和 3000 两笔");
        let mi = p.columns.iter().position(|c| c == "金额").unwrap();
        assert_eq!(p.rows[0][mi], Json::from(300000), "按金额倒序，最大的在前");

        assert_eq!(db.list_views("订单").len(), 1);
        db.delete_view("v1").unwrap();
        assert_eq!(db.list_views("订单").len(), 0);
    }

    #[test]
    fn 表中数据可持久化重开后仍在() {
        let d = tmp("persist");
        {
            let mut db = Db::open(&d).unwrap();
            db.create_table(&spec("t", &[("a", ColType::Text)])).unwrap();
            db.insert_rows("t", &["a".to_string()], &[vec![Some("值".to_string())]])
                .unwrap();
        }
        let db = Db::open(&d).unwrap();
        let p = db.page_rows("t", None, false, None, 10).unwrap();
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0][1], Json::from("值"));
    }
}
