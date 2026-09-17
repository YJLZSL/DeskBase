# DeskBase 文档中心

这里是 DeskBase（桌库）的产品与工程方案文档。**建议按顺序阅读**，也可以只挑你关心的部分。

---

## 阅读路径建议

### 🧭 我是第一次了解这个项目

1. [项目 README](../README.md) — 这是什么、能做什么
2. [00 · 总体方案摘要](00-executive-summary.md) — 一页纸看懂整个设计
3. [01 · 愿景、边界与核心原则](01-vision-boundary-principles.md) — 为什么这样设计
4. [03 · 信息架构](03-information-architecture.md) — 功能怎么分区

### 👤 我是使用者，想确认它靠不靠谱

1. [07 · 备份、恢复与断电保护](07-backup-recovery-dr.md) — 我的数据会不会丢
2. [08 · 隐私与安全](08-privacy-security.md) — 我的数据会不会外流
3. [09 · 本地 AI](09-local-ai.md) — AI 会不会偷看我的东西
4. [12 · 安装与卸载](12-installer-and-uninstaller.md) — 装了能不能干净卸掉

### 💻 我是开发者，想参与贡献

1. [CONTRIBUTING.md](../CONTRIBUTING.md) — 怎么开始
2. [03 · 信息架构](03-information-architecture.md) — 模块怎么划分
3. [04 · 功能矩阵](04-feature-matrix.md) — 哪些要做什么阶段做
4. [17 · 技术选型](17-tech-selection.md) — 候选方案与取舍
5. [adr/](adr/) — 已定的架构决策
6. [14 · 性能指标](14-performance-budget.md) — 什么算达标

### 📢 我是社区/潜在用户，想评估项目成熟度

1. [15 · 开源治理与许可证](15-open-source-governance.md)
2. [16 · 路线图、风险与验收](16-roadmap-risks-acceptance.md)
3. [ROADMAP.md](../ROADMAP.md) / [GOVERNANCE.md](../GOVERNANCE.md)

---

## 文档清单

| 编号 | 文档 | 一句话 |
|------|------|--------|
| 00 | [总体方案摘要](00-executive-summary.md) | 一页纸说清整个产品与工程方案 |
| 01 | [愿景、边界与核心原则](01-vision-boundary-principles.md) | 我们要做什么，更重要的是不做什么 |
| 02 | [目标用户与典型场景](02-users-and-scenarios.md) | 谁会用它，以及他们的一天 |
| 03 | [信息架构](03-information-architecture.md) | 一级模块 / 二级功能 / 三级设置全表 |
| 04 | [完整功能矩阵](04-feature-matrix.md) | MVP / 进阶 / 插件化的边界 |
| 05 | [办公工具箱详细设计](05-office-toolbox.md) | 笔记到长截图，逐项设计 |
| 06 | [SQL 数据库能力设计](06-sql-database.md) | 从查询向导到执行计划 |
| 07 | [备份、恢复与断电保护](07-backup-recovery-dr.md) | 怎么保证不丢数据 |
| 08 | [隐私、安全、加密、权限与审计](08-privacy-security.md) | 默认最小权限的最后一道防线 |
| 09 | [本地 AI 架构](09-local-ai.md) | AI 不出网是怎么做到的 |
| 10 | [桌面端与手机远程端](10-desktop-mobile-remote.md) | 主端与远程端的分工 |
| 11 | [安全与资源优化专项](11-security-and-resource-optimization.md) | 零污染、低占用、无残留 |
| 12 | [安装与卸载体验](12-installer-and-uninstaller.md) | 从欢迎页到彻底清除 |
| 13 | [设置、教程与帮助系统](13-settings-tutorial-help.md) | 让用户不用查外部文档 |
| 14 | [性能优化与量化指标](14-performance-budget.md) | 每条指标的目标值与测法 |
| 15 | [开源治理与插件生态](15-open-source-governance.md) | 许可证、社区、插件权限模型 |
| 16 | [路线图、风险与验收](16-roadmap-risks-acceptance.md) | 分阶段推进与达标判据 |
| 17 | [技术选型建议](17-tech-selection.md) | 只给候选与取舍，不绑定 |
| 99 | [术语表](99-glossary.md) | 全项目名词统一解释 |
| — | [ADR 目录](adr/) | 架构决策记录 |

---

## 文档约定

### 状态标记

文档与条目可能带有状态标记：

| 标记 | 含义 |
|------|------|
| `✅ 已定` | 已由 ADR 固化，改动需走 RFC |
| `🚧 草案` | 正在讨论，欢迎提出意见 |
| `💭 设想` | 长期方向，尚无实现计划 |
| `❓ 待定` | 存在多个合理方案，尚未决策 |

### 优先级标记

| 标记 | 含义 |
|------|------|
| `MVP` | 首个可用版本必须包含 |
| `进阶` | MVP 之后的核心能力 |
| `插件` | 通过插件机制扩展，不进入核心 |
| `不做` | 明确排除，见 [01](01-vision-boundary-principles.md) 边界章节 |

### 术语一致性

所有文档中的专有名词以 [99 · 术语表](99-glossary.md) 为准。新增术语请同步补充术语表。

### 文档即契约

本目录下的文档是项目的**设计契约**：

- 代码行为与文档不符时，以文档为准，代码需修正（或走 RFC 修改文档）。
- 面向用户的功能变更必须同步更新文档，见 [CONTRIBUTING.md](../CONTRIBUTING.md) 第八节。
- 架构级决策必须产生 ADR。

---

## 文档维护

- **负责人**：各文档由对应领域的维护者负责，见 [GOVERNANCE.md](../GOVERNANCE.md)。
- **修改方式**：直接提交 PR。大范围改写请先开 Issue 讨论结构。
- **淘汰机制**：被新文档取代的旧文档移入 `docs/archive/`，保留历史，不直接删除。

---

<div align="center">

找不到你要的内容？到 Discussions 里说一声，我们会补。

</div>
