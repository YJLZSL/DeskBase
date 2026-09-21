# 06 · 数据表引擎设计（无 SQL）

> 本文件取代原 `06-sql-database.md`。**DeskBase 不再内置 SQL 引擎、不再提供任何查询输入框。**
> 多表关联、跨表协同更新、筛选排序，全部用结构化配置表达。
> 决策依据：[ADR-0021 · 彻底移除 SQL，改用自研单文件存储引擎](adr/0021-remove-sql-own-storage-engine.md)、
> [ADR-0022 · 多表关联与协同更新模型（可配置、可追溯）](adr/0022-linked-tables-and-sync-rules.md)、
> 调研 [18 · 存储引擎选型](local-docs/reference/18-storage-engine-research.md)。

---

## 一、设计立场

| 立场 | 具体含义 |
|------|---------|
| **单文件优先** | 默认一个库 = 一个文件（`<数据目录>/data/main.dkb`），便于携带、备份、迁移 |
| **嵌入式** | 不要求用户安装数据库服务、不开端口、不配置连接串 |
| **无 SQL** | 代码里不出现 SQL 字符串、不依赖 SQL 引擎、界面不提供查询输入框 |
| **能力不阉割** | 高级用户拿到的是完整的**共通字段 / 同步规则 / 关联 / 查找 / 汇总 / 命名视图**，而非降级体验 |
| **关系看得见** | 表与表之间如何连、如何协同更新、数据从哪来，全部显式、可见、可改、可追溯 |
| **可解释** | 每条同步规则的来源、方向、冲突策略都展示给用户；AI 生成的视图配置必须展示供确认 |
| **可撤销** | 破坏性操作（删表、删字段、批量更新、改共通定义）必须有确认、有预览、有回退 |
| **安全默认** | 不记录值日志（隐私）、不自动联网、API 默认仅本机 |

---

## 二、存储层：单文件日志 + 快照（对齐 ADR-0021）

自研存储引擎 `store`，纯 Rust 实现，无第三方数据库依赖。设计范围被严格限定：只做「日志 + 快照」，不做 B+tree 页管理、不做查询优化器、不做多写者并发、不做持久化索引。

### 2.1 写入路径

每条写入是一条追加记录：

```
┌────────┬─────────┬─────────┬─────────────────────┐
│ magic  │  len    │  crc32  │  payload (len 字节)  │
│ 4 字节 │ 4 字节  │ 4 字节  │                      │
└────────┴─────────┴─────────┴─────────────────────┘
   "DKB1"  u32 LE    u32 LE     UTF-8 JSON（msgpack 风格的自编码）
```

- 追加 `[len][crc32][payload]` 后调用 `File::sync_all()`（fsync）才算提交；
- `payload` 顶层是 UTF-8 JSON（可读性优先，便于排障与人工拯救数据）；
- 每条含自增 `seq`，快照记录「已回放到的 seq」；
- 尾部一条不完整的条目（长度不足 / CRC 不符 / 非 magic）**一律丢弃**，不报错、不猜。

### 2.2 读取路径

装载最新快照到内存 → 从快照点之后回放日志 → 内存里重建 BTreeMap 索引（索引不持久化，回放即得）。

### 2.3 快照

- 触发条件：日志条目数 ≥ 50 000 或日志字节数 ≥ 8 MB（可在设置中调整）；
- 写入 `main.dkb.snap.tmp` → `sync_all` → `rename` 覆盖 `main.dkb.snap` → 目录 `sync_all`；
- 快照成功后**原子截断日志**：新日志写 `main.dkb.log.tmp` → 落盘 → rename 覆盖。

### 2.4 崩溃一致性判据

| 场景 | 期望 |
|------|------|
| 事务未提交时被杀 | 该事务完全不存在 |
| 事务已 `sync_all` 后被杀 | 该事务完整存在 |
| 日志尾部半写 | 半写条目被丢弃，之前的数据全部完好 |
| 快照临时文件残留 | 下次启动忽略 `.tmp` 并清理 |

日志同目录 `main.dkb.log`；并发模型为**单写者 + 多读者快照**（DeskBase 是单主端单进程，见 ADR-0006）。

---

## 三、数据模型：表 / 字段 / 记录 / 视图

上层自建数据模型 `model`，不含查询语言；查询通过结构化视图配置表达。

| 概念 | 说明 |
|------|------|
| **表（Table）** | 一张二维数据表，含字段定义与记录集合；表名经标识符校验 |
| **字段（Field）** | 列定义，含名称、类型（`FieldKind`）、是否共通、是否被同步规则写入（来源标记） |
| **记录（Record）** | 一行数据，由字段值组成；记录 ID 用 ULID（见 ADR-0012） |
| **视图（View）** | 一张表的可命名呈现配置：筛选 / 排序 / 分组 / 隐藏列，存进 `store`，可复用 |

字段结构（节选自 ADR-0022）：

```rust
pub struct Field {
    pub id: FieldId,          // ULID
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

---

## 四、字段类型清单

| 分类 | 类型 | 说明 |
|------|------|------|
| **基础类型** | 文本 / 长文本 | 任意文字 |
| | 数字（整数 / 小数） | 计数、编号、金额、比例 |
| | 金额（按「分」精确存储） | 避免浮点误差 |
| | 日期 / 日期时间 | 只有日期 / 日期 + 时间 |
| | 是/否 | 复选框（布尔） |
| | 单选项 / 多选项 | 固定候选中选一个 / 多个 |
| | 附件 | 指向附件存储的路径引用 |
| | 自动编号 | 自增主键 |
| | JSON | 结构化数据（进阶） |
| | 二进制 | 图片、文件（进阶） |
| **关联** | 关联（Link） | 一条记录指向另一表的一条或多条记录，双向维护 |
| **只读派生** | 查找（Lookup） | 通过关联取目标字段值，随源实时计算，不落盘副本 |
| | 汇总（Rollup） | 通过关联对目标集合做聚合（计数 / 求和 / 最大 / 最小 / 平均） |
| **计算** | 公式（Formula） | 基于本记录其他字段的表达式计算 |

> 原则：**新手看到意图，老手看到类型**。切换模式时映射关系不变，不会因为切换而改变数据。

---

## 五、共通字段（Shared Field）——「知道这个数据是共通的」

> 场景：多张表都有「客户名」「项目编号」「状态」，改一个状态选项，所有表跟着变。

共通字段是一次定义、多处引用的字段。引用它的本地字段，其**类型与选项由共通定义决定**，本地不可单方面改类型。

规则：

1. 字段创建时可选「设为共通字段」或「引用已有共通字段」；
2. 引用共通字段的本地字段，类型与选项由共通定义决定，本地不可单方面改类型；
3. 改共通定义（改名 / 改选项）→ 所有引用字段同步更新，并在变更日志里列出「影响了 N 张表」；
4. **界面强制可见**：共通字段的列头带 `⇄ 共通` 徽标，悬浮显示「共通字段《状态》，被 4 张表共用，点此查看/修改」；
5. 解除引用时给出两条路：**断开**（本地保留当前值为普通字段）/ **跟随**（保持联动），不静默处理。

```rust
pub struct SharedField {
    pub id: SharedFieldId,
    pub name: String,
    pub kind: SharedKind,          // Select / Text / Number / Money / Date / Bool
    pub options: Vec<String>,      // Select 时有效
    pub used_by: Vec<FieldId>,     // 反向索引：哪些表的哪些字段引用了它
}
```

---

## 六、同步规则（Sync Rule）——「改一个表，另一个表协同更新」

同步规则把「源字段 → 目标字段」的联动显式配置出来，方向、冲突策略、作用域、总开关全部用户可改。

```rust
pub enum SyncMode {
    Mirror,   // 单向镜像：源 → 目标；目标字段在界面上只读
    TwoWay,   // 双向：任一侧改，另一侧跟着改
    Suggest,  // 不自动写：生成「建议更新」条目，用户逐条确认
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
    pub enabled: bool,                 // 「可以调整」的总开关
    pub source: (TableId, FieldId),
    pub target: (TableId, FieldId),
    pub via: FieldId,                  // 从源表走到目标表的 Link 字段
    pub mode: SyncMode,
    pub conflict: ConflictPolicy,
    pub scope: ApplyScope,
}
```

### 6.1 传播算法（一次记录更新触发）

1. 由 `rule_index` 只查命中本次变更字段的规则；
2. 逐条遍历命中且 `enabled` 的规则；
3. 沿规则的 `via` 关联找到目标记录；
4. 若已达**深度上限 5**，或本轮 `(rule, record)` 已访问 → 跳过（环检测）；
5. 按 `mode` / `conflict` / `scope` 解析目标值；
6. 若值有变化：写目标记录并在目标字段打上同步标记，然后递归触发目标记录的更新。

### 6.2 约束

- 全部在一个 `store` 事务里完成：中途出错整体回滚，绝不留下「改了一半」的两张表；
- 深度上限 5、每轮访问集去重 —— 防止规则互相触发形成死循环；
- `Suggest` 模式不写业务字段，只写「建议」列表，用户确认后才落盘；
- 每条规则都可**单独停用**，停用后目标字段自动从只读变回可编辑（即「可以调整」的落地）。

---

## 七、可追溯（R4 的落地）

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

被同步字段的编辑行为由 `mode` 决定：`Mirror` 下灰掉并提示「改源表那边（跳转）」，`TwoWay` 下允许改并反向传播，`Suggest` 下允许改并标记「与源不一致」。

---

## 八、命名视图（View）—— 替代 SELECT

视图是筛选 / 排序 / 分组 / 隐藏列的可视化配置，保存后复用。**无查询语言、无输入框。**

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

- 视图配置本身存进 `store`，可重命名、复制、删除；
- 大表分页用 **keyset**（排序键 + 记录 id），不用 `OFFSET` —— 沿用原 `schema.rs` 已验证的结论；
- 自然语言（AI 阶段）把用户的中文提问转换为视图的筛选 / 排序配置，**不生成、不展示 SQL**。

---

## 九、红线（数据表引擎相关）

| 红线 | 要求 |
|------|------|
| **值一律走类型校验** | 所有写入值按字段类型校验（范围 / 正则 / 枚举 / 格式）；**绝不把用户输入拼进字符串去构造查询** |
| **标识符校验** | 表名、字段名过标识符校验（禁 `rowid`、禁空格与标点），由 Rust 侧处理，前端不传裸名 |
| **删除需二次确认** | 删表 / 删字段 / 批量删除必须二次确认，并要求输入对象名（或显式勾选）；删表需逐字输入表名 |
| **改结构先快照** | 任何结构变更（加列 / 改共通定义 / 改类型）前自动创建快照，失败可回退 |
| **改共通需影响面预览** | 改共通字段的类型 / 选项前，先展示「将影响 N 张表」，二次确认 |
| **API 默认仅本机** | 生成的 HTTP 接口默认只绑定 `127.0.0.1`，对外暴露需显式开启并提示风险 |
| **导出留痕** | 导出操作记入审计日志 |
| **不自动上传** | 数据文件、导出结果、表结构默认不发送到任何外部服务 |

> 注：原「危险 SQL 识别」（DROP / 无 WHERE 的 UPDATE/DELETE 双重确认）已被「结构化的删除确认 + 影响预览」取代 —— 新模型下不再有可拼装的文本语句需要解析。

---

## 十、验证表（对齐 ADR-0021 / ADR-0022）

| # | 验证项 | 判据 | 来源 |
|:-:|--------|------|------|
| 1 | 无 SQL 残留 | 全仓库（`app/src`、`app/ui`）搜索 `rusqlite` / `libsqlite3` / `SELECT ` / `CREATE TABLE` 命中 0（导出模块与测试夹具除外） | ADR-0021 |
| 2 | 崩溃一致性 | 强杀 100 次，重启后校验通过，最多丢最后一个未提交事务 | ADR-0021 |
| 3 | 半写丢弃 | 人工截断/污染日志尾部，能正确丢弃并保住之前的数据 | ADR-0021 |
| 4 | 快照截断 | 达到阈值后日志归零，数据完整 | ADR-0021 |
| 5 | 关联双向 | 在 A 表加关联，B 表出现反向字段；一侧删除，另一侧同步移除 | ADR-0022 |
| 6 | 查找/汇总实时 | 改源记录，Lookup/Rollup 立即反映，无重启、无刷新缓存 | ADR-0022 |
| 7 | 同步 Mirror | 改源 → 目标跟随；目标字段只读；停用规则 → 目标恢复可编辑且不再跟随 | ADR-0022 |
| 8 | 同步 TwoWay | 改任一侧都传播；两侧同时改按冲突策略收敛 | ADR-0022 |
| 9 | 环检测 | 构造 A→B→A 的互同步规则，不死锁、不栈溢出，达到深度上限后停止并记日志 | ADR-0022 |
| 10 | 事务性 | 传播中途注入失败 → 两张表都没有半改状态 | ADR-0022 |
| 11 | 共通字段 | 改共通选项 → 引用表全部生效；界面徽标与来源面板正确 | ADR-0022 |
| 12 | 可追溯 | 每个被同步字段都能通过 UI 跳到源记录 | ADR-0022 |
| 13 | 视图 | 无 SQL 输入；筛选/排序/分组结果与原 SQL 版本一致（用旧版用例回归） | ADR-0022 |
| 14 | 迁移 | 旧库导出 JSON/CSV → 新格式导入，记录条数一致 | ADR-0021 |

---

## 十一、相关文档

- [00 · 总体方案摘要](00-executive-summary.md)
- [07 · 备份、恢复与断电保护](07-backup-recovery-dr.md)
- [ADR-0021 · 彻底移除 SQL，改用自研单文件存储引擎](adr/0021-remove-sql-own-storage-engine.md)
- [ADR-0022 · 多表关联与协同更新模型（可配置、可追溯）](adr/0022-linked-tables-and-sync-rules.md)
- [ADR-0003 · 单文件嵌入式数据库](adr/0003-single-file-embedded-database.md)（「不自研存储引擎」一条被 ADR-0021 取代，其余继续有效）
- [ADR-0012 · 元数据存储与 ID 方案](adr/0012-metadata-storage-and-id-scheme.md)
- [ADR-0020 · 表格优先的 UI](adr/0020-table-first-ui-and-ai-tables.md)
- 调研：[18 · 存储引擎选型](../../local-docs/reference/18-storage-engine-research.md)
