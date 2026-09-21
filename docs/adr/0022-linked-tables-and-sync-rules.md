# ADR-0022: 多表关联与协同更新模型（可配置、可追溯）

- **状态**：已接受
- **日期**：2026-09-20
- **决策者**：核心维护者
- **相关**：[ADR-0021 · 移除 SQL 与自研存储](0021-remove-sql-own-storage-engine.md)、[ADR-0020 · 表格优先的 UI](0020-table-first-ui-and-ai-tables.md)、[ADR-0012 · 元数据存储与 ID 方案](0012-metadata-storage-and-id-scheme.md)

---

## 背景

去掉 SQL 之后，用户原本靠 `JOIN`、外键级联、触发器获得的"表与表之间是连着的"体验必须换一种方式提供。
需求原话：

> 多个分表，改掉一个表的数据，另一个表的数据也需要协同更新，但是需要**可以调整**，**知道这个数据是共通可以更新的**。

拆成四个可验收的要求：

| # | 要求 | 关键词 |
|:-:|------|--------|
| R1 | 多张表，表之间有关系 | 多分表 |
| R2 | 改一处，关联处跟着变 | 协同更新 |
| R3 | 这个联动用户能开、能关、能改方向 | 可以调整 |
| R4 | 用户看得出"这个值是从哪来的、和谁共用" | 知道是共通的 |

SQL 做不到 R3 和 R4：外键和触发器藏在 DDL 里，用户既看不见也改不动。这正是新模型的价值所在。

---

## 决策

提供**两层机制**，分别对应"共通"和"协同"：

| 机制 | 解决什么 | 对应要求 |
|------|---------|---------|
| **共通字段**（Shared Field） | 一个字段定义被多张表引用，改一处处处生效 | R1 · R4 |
| **同步规则**（Sync Rule） | 经由关联，把 A 表的值写到 B 表，方向/冲突可配 | R2 · R3 |

外加两种只读派生字段用于"看"而不是"改"：**查找字段**（Lookup）与**汇总字段**（Rollup）。

**查询一律不用语言**：筛选 / 排序 / 分组 / 隐藏列组成**命名视图**（View），保存后复用。不提供任何查询输入框。

---

## 1. 关联字段（Link）

```rust
pub struct Field {
    pub id: FieldId,          // ULID，见 ADR-0012
    pub name: String,         // 中文名，用户可见
    pub kind: FieldKind,
    pub shared: Option<SharedFieldId>,  // 若为共通字段，指向共享定义
    pub sync: Option<FieldSyncMark>,    // 由哪条同步规则写入（自动标注）
}

pub enum FieldKind {
    Text, Number, Money, Date, DateTime, Bool, Select { options: Vec<String> },
    Link   { target_table: TableId, cardinality: Cardinality, back_field: Option<FieldId> },
    Lookup { via: FieldId,  target_field: FieldId },
    Rollup { via: FieldId,  target_field: FieldId, agg: Agg },
    Formula{ expr: String },
}
```

- `Link` 建的是**双向**关系：创建时可勾选"在目标表同时创建反向字段"，之后任一侧增删都会自动维护另一侧。
- `Lookup` / `Rollup` 是**只读派生**：值随源记录实时计算，不落盘为独立副本（避免"改了源这边没变"的经典不一致）。
- 不允许自引用到自身以外的环：关联图在建字段时做一次可达性检查，成环则拒绝并给出说明。

---

## 2. 共通字段（Shared Field）——「知道这个数据是共通的」

> 场景：多张表都有"客户名""项目编号""状态"，改一个状态选项，所有表跟着变。

```rust
pub struct SharedField {
    pub id: SharedFieldId,
    pub name: String,
    pub kind: SharedKind,          // Select / Text / Number / Money / Date / Bool
    pub options: Vec<String>,      // Select 时有效
    pub used_by: Vec<FieldId>,     // 反向索引：哪些表的哪些字段引用了它
}
```

**规则**：

1. 字段创建时可选「设为共通字段」或「引用已有共通字段」；
2. 引用共通字段的本地字段，其**类型与选项**由共通定义决定，本地不可单方面改类型；
3. 改共通定义（改名 / 改选项）→ 所有引用字段同步更新，并在变更日志里列出"影响了 N 张表"；
4. **界面强制可见**：共通字段的列头带 `⇄ 共通` 徽标，悬浮显示"共通字段《状态》，被 4 张表共用，点此查看/修改"；
5. 解除引用时给出两条路：**断开**（本地保留当前值为普通字段）/ **跟随**（保持联动），不静默处理。

---

## 3. 同步规则（Sync Rule）——「改一个表，另一个表协同更新」

```rust
pub enum SyncMode {
    Mirror,   // 单向镜像：源 → 目标；目标字段在界面上只读
    TwoWay,   // 双向：任一侧改，另一侧跟着改
    Suggest,  // 不自动写：生成"建议更新"条目，用户逐条确认
}

pub enum ConflictPolicy {
    SourceWins, TargetWins, LastWriteWins, Ask,
}

pub enum ApplyScope {
    Always,          // 总是覆盖目标
    FillEmptyOnly,   // 只填目标为空的
}

pub struct SyncRule {
    pub id: SyncRuleId,
    pub name: String,
    pub enabled: bool,                 // ← 「可以调整」的总开关
    pub source: (TableId, FieldId),
    pub target: (TableId, FieldId),
    pub via: FieldId,                  // 从源表走到目标表的 Link 字段
    pub mode: SyncMode,
    pub conflict: ConflictPolicy,
    pub scope: ApplyScope,
}
```

**传播算法**（一次记录更新触发）：

```
on_record_updated(table, record, changed_fields):
    rules ← rule_index.lookup(table, changed_fields)   // 只查命中的规则
    for rule in rules where rule.enabled:
        for each linked record via rule.via:
            if 已达深度上限(5) 或 (rule, record) 本轮已访问 → 跳过（环检测）
            value ← resolve(rule, source_value, target_value)
            if value 有变化:
                写目标记录，并在目标字段上打 sync 标记
                递归 on_record_updated(目标表, 目标记录, [目标字段])
```

**约束**：
- 全部在一个 `store` 事务里完成：中途出错整体回滚，绝不留下"改了一半"的两张表；
- 深度上限 5、每轮访问集去重 —— 防止规则互相触发形成死循环；
- `Suggest` 模式不写业务字段，只写"建议"列表，用户确认后才落盘；
- 每条规则都可**单独停用**，停用后目标字段自动从只读变回可编辑（R3 的落地）。

---

## 4. 可追溯（R4 的落地）

任何被同步规则写入的字段，都带一个**来源标记**：

```rust
pub struct FieldSyncMark {
    pub rule_id: SyncRuleId,
    pub source_table: String,   // 冗余存表名，删表后仍能显示来源
    pub source_field: String,
    pub synced_at: i64,         // UTC 毫秒
}
```

界面三处必现：

| 位置 | 显示 |
|------|------|
| 列头 | `⇄ 同步自《客户表·客户名》` 徽标 |
| 单元格 | 值旁一个小箭头，点击跳到源记录 |
| 记录侧栏 | 「共通与同步」面板：列出本记录所有同步进来的字段与各自来源 |

**被同步字段的编辑行为**由 `mode` 决定：`Mirror` 下灰掉并提示"改源表那边（跳转）"，`TwoWay` 下允许改并反向传播，`Suggest` 下允许改并标记"与源不一致"。

---

## 5. 视图（View）——替代 SELECT

```rust
pub struct View {
    pub id: ViewId,
    pub table: TableId,
    pub name: String,           // 如「未收款的订单」
    pub filter: Option<FilterGroup>,  // and/or 嵌套，字段 + 运算符 + 值
    pub sorts: Vec<Sort>,
    pub group_by: Option<FieldId>,
    pub hidden: Vec<FieldId>,
    pub row_height: RowHeight,
}
```

- 无查询语言、无输入框；
- 视图配置本身存进 `store`，可重命名、复制、删除；
- 大表分页用 **keyset**（排序键 + 记录 id），不用 `OFFSET` —— 沿用原 `schema.rs` 里已经验证过的结论。

---

## 后果

### 正面

- 用户第一次能**看见并改**表与表之间的关系，而不是翻 DDL；
- 「共通字段」把重复维护变成一处维护，直接降低出错率；
- 同步规则可停用，出问题能一键止血；
- 全程可追溯，误同步能定位到是哪条规则改的。

### 负面

- 概念比 SQL 多（关联 / 查找 / 汇总 / 共通 / 同步规则），新手引导必须跟上，否则会变成"看不懂的高级功能"；
- 传播算法有递归，调试难度高于单表操作 —— 靠事务 + 访问集 + 深度上限把风险封住；
- 共通字段改类型会影响所有引用表，需要二次确认与影响面预览。

### 中性

- 原 `schema.rs` 的 SQL 词法判断（危险语句识别、`WHERE` 解析等约 800 行）整体作废 —— 新模型下"危险操作"变成结构化的删除确认，不需要解析文本。

---

## 验证方式

| # | 验证项 | 判据 |
|:-:|--------|------|
| 1 | 关联双向 | 在 A 表加关联，B 表出现反向字段；一侧删除，另一侧同步移除 |
| 2 | 查找/汇总实时 | 改源记录，Lookup/Rollup 立即反映，无重启、无刷新缓存 |
| 3 | 同步 Mirror | 改源 → 目标跟随；目标字段只读；停用规则 → 目标恢复可编辑且不再跟随 |
| 4 | 同步 TwoWay | 改任一侧都传播；两侧同时改按冲突策略收敛 |
| 5 | 环检测 | 构造 A→B→A 的互同步规则，不死锁、不栈溢出，达到深度上限后停止并记日志 |
| 6 | 事务性 | 传播中途注入失败 → 两张表都没有半改状态 |
| 7 | 共通字段 | 改共通选项 → 4 张引用表全部生效；界面徽标与来源面板正确 |
| 8 | 可追溯 | 每个被同步字段都能通过 UI 跳到源记录 |
| 9 | 视图 | 无 SQL 输入；筛选/排序/分组结果与原 SQL 版本一致（用旧版用例回归） |

---

## 参考

- [ADR-0021](0021-remove-sql-own-storage-engine.md) — 底层存储
- [06 · 数据表引擎设计](../06-data-table-engine.md)（原 `06-sql-database.md`，本 ADR 后重写）
- 调研记录：`local-docs/reference/18-storage-engine-research.md` 第 6 节
