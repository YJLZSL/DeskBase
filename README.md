<div align="center">

# DeskBase · 桌库

[![CI](https://github.com/YJLZSL/DeskBase/actions/workflows/ci.yml/badge.svg)](https://github.com/YJLZSL/DeskBase/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-alpha-orange.svg)](#-项目状态早期-alpha)

**本地优先 · 隐私优先的办公工具箱，内置人人可上手的 SQL 数据库能力**

把笔记、表格、看板、截图、待办和一个小而快的数据库，装进同一张桌面。

`离线可用` · `默认无账号` · `默认无遥测` · `默认数据不出本机` · `解压即用`

</div>

---

## ⚠️ 项目状态：早期 alpha

**现在能用的和还不能用的，一目了然。** 下面这些是**已经跑起来**的：

| 已经在用 | 说明 |
|---|---|
| 桌面外壳 | Rust + 系统 WebView2，单文件 exe，解压即用 |
| 笔记 | 新建 / 编辑 / 自动保存 / 搜索 / 软删除 |
| 11 个主题 + 四档动效 | 主题改的是**结构**不只是色相；内嵌得意黑标题字体 |
| 命令面板 | `Ctrl+K`，支持**拼音首字母**（输入 `xjbj` 得到「新建笔记」） |
| Excel 双向 | 导出长编号不丢精度；导入前逐个点名七类「静默毁数据」的坑 |
| CSV 导入 | 自动识别 UTF-8 / GBK / UTF-16 —— 专治中文 Excel 另存为 CSV 的乱码 |
| 格式转换 | 表格 / 图片 / 文本，**纯离线**；两步式，先告诉你「会丢什么」再执行 |
| 截屏 | DPI 感知，125% / 150% 缩放下不模糊 |

**还没做的**（别去找，会找不到）：

| 还没做 | 卡在哪 |
|---|---|
| **安装版 exe** | 自建安装器，验收标准（零注册表写入、卸载残留=0）没达标不发 |
| **代码签名** | 没有证书。SmartScreen 会拦，也是将来更新器被 Defender 误报的唯一真正解法 |
| **自动更新** | 目前是手动到 Releases 下载 |
| **长截图（滚动拼接）** | 算法与测试都在，缺「界面滚一屏 → Rust 抓一帧」的往返协议 |
| **数据库模块** | 只落地了笔记。物流 / 财务 / 工厂收发那几张表设计完成但尚未建表 |
| **AI 功能** | 一个都没接。选型与隐私方案已定稿（全本地优先，云端排最后） |

完整清单在 [CHANGELOG](CHANGELOG.md#010-alpha1---2026-09-17) 的「已知未完成」一节。
工程状态：`cargo test` 103 个用例全过、编译零警告、四道 CI 门禁全绿。

**`0.x` 期间不承诺向后兼容，请勿用于唯一的生产数据。**

---

## 这是什么

DeskBase（桌库）是一个**本地优先（local-first）**的桌面办公数据工作台。它想解决的问题很朴素：

> 普通人每天在电脑上产生大量「半结构化」信息——待办、清单、截图、通讯录、记账、库存、客户名单、读书笔记——把它们记在备忘录里会乱，放进 Excel 会卡，学真正的数据库管理工具又太陡。

DeskBase 的做法是：**把数据库能力包装成办公工具的形态**。

打开就能看到表格、表单、待办、笔记和查询向导；等你哪天想认真一点，SQL 编辑器、关系设计、索引、视图、触发器、自动化就在旁边，一步之遥，不逼你学。

它的另一条主线是**克制**：不装驱动、不写注册表、不装服务、不建计划任务、不开机自启、不静默联网。**卸载要像从没装过一样干净。**

---

## 设计原则（十条）

| # | 原则 | 一句话解释 |
|---|------|-----------|
| 1 | **本地优先** | 数据默认只存在你的设备上，断网功能不缩水 |
| 2 | **渐进式复杂度** | 新手看到 5 个入口，高级用户看到 50 个，中间是一条滑杆而不是一道墙 |
| 3 | **零污染** | 便携版不写注册表、不装服务、不改文件关联 |
| 4 | **默认最克制** | 所有开关的默认值都指向「更少权限、更少联网、更少驻留」 |
| 5 | **不丢数据** | 崩溃/断电后必进恢复向导，编辑内容有草稿与版本 |
| 6 | **备份必须可验证** | 没做过恢复演练的备份不算备份 |
| 7 | **AI 不出网** | 默认本地模型推理，绝不做数据外流通道 |
| 8 | **可审计** | 依赖锁定、SBOM、可复现构建、权限清单全部公开 |
| 9 | **可迁移** | 数据可整体导出为开放格式，不制造供应商锁定 |
| 10 | **安静** | 空闲不烧 CPU、不轮询、不常驻多余进程 |

---

## 信息架构总览

```
DeskBase
├── 1. 工作台           首页 / 最近项目 / 待办聚合 / 日程 / 全局搜索 / 快捷操作
├── 2. 办公工具         笔记 · 文档 · 表格 · 看板 · 日历 · 待办
│                       文件 · 剪贴板历史 · 截图与长截图 · OCR
│                       标签 · 模板 · 导入导出
├── 3. 数据库           连接 · 表 · 视图 · 查询 · 关系 · 索引
│                       导入导出 · 备份 · 版本历史 · 权限 · 审计
├── 4. 自动化           规则 · 触发器 · 定时任务 · 批量处理 · 工作流
├── 5. AI 助手          本地模型 · 自然语言查询 · SQL 生成 · 摘要
│                       分类 · 翻译 · OCR · 语音转写 · 报表解读
├── 6. 远程访问         设备配对 · 局域网直连 · 自托管中继 · 权限 · 审计
├── 7. 备份与恢复       自动备份 · 手动备份 · 快照 · 版本历史
│                       断电恢复 · 恢复向导 · 健康检查 · 恢复演练
├── 8. 设置             基础 · 外观 · 编辑器 · 快捷键 · 数据库 · 备份
│                       隐私与安全 · AI · 远程访问 · 插件 · 高级 · 关于
├── 9. 帮助与教程       新手引导 · 场景教程 · 快捷键速查 · SQL 教程
│                       备份恢复教程 · 隐私说明
└── 10. 插件            插件市场 / 本地插件 · 权限 · 更新 · 卸载
```

> 每个模块都遵循 **一级模块 → 二级功能 → 三级设置** 三层结构，配合命令面板（`Ctrl/Cmd + K`）、面包屑、最近使用、收藏与快捷入口。
> 高级选项默认折叠，并标注「可能影响稳定性或隐私」。

---

## 核心能力速览

### 办公工具箱
笔记 / 文档 / 表格 / 看板 / 日历 / 待办 / 标签 / 模板 / 文件管理 / 剪贴板历史 / 全局搜索。

**截图能力是完整的一套**，不是随手截个屏：区域截图、窗口截图、全屏截图、延时截图、**滚动长截图与长图拼接**、标注（箭头 / 文字 / 马赛克 / 高亮 / 裁剪）、贴图置顶、截图历史。截图后可直接本地 OCR 提取文字，支持复制、搜索、翻译，**图片默认不出本机**。

### SQL 数据库
可视化建表、字段编辑、关系设计、索引、视图、触发器；SQL 编辑器带语法高亮、自动补全、查询历史、收藏查询、参数化查询、执行计划提示。支持 CSV / JSON / Excel / SQL 导入导出，数据校验、权限控制、审计日志、版本历史、API 生成。

对新手：**查询向导 + 自然语言转 SQL**；对高级用户：**完整的 SQL 能力**，不阉割。

### 备份、恢复与断电保护
默认开启自动备份，位置 / 频率 / 保留数量 / 加密方式全部可配；支持全量、增量、差异、快照与时间点恢复（PITR）；备份到本地目录、移动硬盘、NAS 或自托管服务，**默认不上传第三方云**。

数据层面对断电做真功夫：事务、WAL（或等价机制）、原子写入、`fsync`、定期 checkpoint、崩溃一致性检测。异常退出后下次启动**自动进入恢复向导**，尽可能回到最近可用状态。

并提供 **备份健康检查** 与 **恢复演练**——让用户知道备份到底能不能用，而不是等出事才发现。

### 隐私与安全
默认本地存储、默认离线可用、默认无遥测、默认无强制账号、默认无云端上传。网络**默认关闭**，只有用户显式开启远程、同步、外部 AI 或更新检查时才允许联网。本地加密存储、数据库加密、敏感字段加密、备份加密。手机远程走局域网直连或自托管中继，端到端加密 + 设备配对 + 二维码授权 + 细粒度权限 + 审计日志。

### 本地 AI
自然语言查询、SQL 生成、数据摘要、分类、翻译、OCR、语音转写、智能搜索、报表解读。**模型、向量索引、对话记录、临时数据全部留在本地**，禁止默认调用云端 API。若用户主动选择外部 AI 服务，需显式授权、逐次确认、可脱敏、可审计、可完全关闭。

### 原生桌面端 + 手机远程
桌面端是**数据主端**，手机端是**安全远程端**：查看、录入、审批、查询、接收通知。不依赖第三方云，优先局域网发现与直连、次选自托管中继。移动端按需连接，不后台常驻高耗电。

---

### 外观与动效

默认主题是**宣纸**——素底、墨字、极轻的纸纹。这不是装饰，是阅读舒适度的工程决策：低饱和、低对比噪声，长时间看不累。

在此之上提供 **11 个内置主题**，分成四个风格家族（不只是换色：圆角、描边、阴影语言、字体、行距与密度会一起变）：

| 家族 | 主题 |
|------|------|
| 内容优先 | 宣纸（默认）、书页（宋体正文，行距舒展）、夜墨（宣纸的暗色） |
| 中性 / 专业 | 纸白（白底细线，层次靠边框）、靛蓝（直角密排，数据优先）、石墨 |
| 技术 / 极值 | 控制台、终端（全等宽，直角无阴影）、高对比亮、高对比暗 |
| 表现型 | 粗野（粗描边硬阴影） |

另有「跟随系统」——随操作系统的亮暗设置自动切换（事件驱动，不轮询）。主题切换用整屏交叉淡入完成，不闪白。

**应用图标**是暖米底上两张错位的纸（靛青在后、纸白在前），前面一张压一道朱砂。它的大小两套几何是分开画的：小尺寸下铺得更满、靛青改实色、去掉会变成脏点的细灰条——所以 16px 在任务栏里也站得住。

**字体三槽位可分别配置**（标题 / 正文 / 代码），内置 5 个预设，并支持导入 `.ttf` `.otf` `.woff2` `.woff` 自定义字体。标题槽位内嵌 **得意黑 Smiley Sans**（SIL OFL 1.1，WOFF2 仅 **1.10 MB**，随程序分发、不联网下载）；正文与代码走系统字体栈——正文用字量最大，内嵌中文字库得不偿失。不想用内置字体可在设置里一键切回系统宋体系。

**动效只有四个档位**——`关` / `精简` / `标准` / `丰富`，随时可切。原则是**服务于理解，而不是吸引注意力**：拖拽 1:1 跟手、操作 100 ms 内有反馈、**只动 `transform` 与 `opacity`**、不做任何循环播放的装饰动画。档位调到「关」是把时长压到 1 毫秒，而不是把动画藏起来——元素仍然停在正确的位置，不会有东西卡在半路。系统开启「减少动效」时档位**封顶为「精简」**，且不覆盖你自己的选择（系统设置改回去就恢复）。

> 「只动 transform 与 opacity」不是口头约定：`node scripts/check-motion.cjs` 会扫描样式表，发现动画布局属性（width / height / grid-template-* 等）或硬编码时长就**让构建失败**。

完整规范见 [docs/18](docs/18-visual-and-motion-design.md)。

---

## 可量化目标（v1.0 验收线）

| 指标 | 目标 |
|------|------|
| 安装包体积（Windows 桌面版） | ≤ 80 MB（含基础本地模型则为可选增量包） |
| 便携版解压后体积 | ≤ 220 MB |
| 冷启动到可交互 | ≤ 1.2 s（SSD / 中等配置） |
| 空闲内存占用 | ≤ 400 MB（无数据库窗口；依据 PoC 实测修订，原 180 MB 是架构上达不到的拍脑袋数字） |
| 空闲 CPU 占用 | 平均 ≤ 0.3%，无周期性轮询尖峰 |
| 万行表格滚动 | ≥ 55 FPS |
| 常用查询延迟（万行表索引命中） | ≤ 50 ms |
| 全文搜索首屏结果 | ≤ 300 ms（10 万条记录） |
| 自动备份增量耗时（1 GB 库） | ≤ 20 s |
| 局域网同步延迟（同网段） | ≤ 800 ms |
| 卸载残留项 | **0**（彻底清除模式下） |

完整指标、测试方法与达标判据见 [docs/14-performance-budget.md](docs/14-performance-budget.md)。

---

## 下载、安装与卸载

### 现在能下载到什么

**只有便携版**，见 [Releases](https://github.com/YJLZSL/DeskBase/releases)（最新为 `v0.1.0-alpha.1`，**预发布**）。

| | 状态 |
|---|---|
| 便携版（解压即用） | ✅ 已发布 |
| Windows 安装版 exe | ❌ **还没做**，见下 |
| Linux / macOS | ❌ 暂不支持（架构本身跨平台，只做 Windows 是集中资源的选择） |
| 代码签名 | ❌ 没有证书，SmartScreen 会拦。便携版受影响较小 |

> **为什么没有安装版**：安装器要自己写（现成框架都做不到自绘界面，见 [ADR-0015](docs/adr/0015-distribution-and-installer-strategy.md)），
> 而它的验收标准里有「便携版零注册表写入」「卸载残留 = 0」这类**必须在真机上逐条验证**的项。
> 没验证过就不发 —— 拿一个没达标的安装器去写注册表，比让用户自己解压一个 zip 危险得多。
>
> 这一版是 **alpha**：`0.x` 期间不承诺向后兼容，**请勿用于唯一的生产数据**。
> 已知未完成的部分逐条列在 [CHANGELOG](CHANGELOG.md#010-alpha1---2026-09-17) 里。

### 便携版

解压即用。不写注册表、不写系统目录、不装服务、不建计划任务、不改文件关联、不装驱动、不开机自启。

数据默认放在 `%USERPROFILE%\DeskBaseData\`；想让它跟着 U 盘走，设一个环境变量即可：

```bat
set DESKBASE_DATA_DIR=<解压目录>\data
```

备份就是把这个目录整个拷走。卸载就是删掉程序目录 —— 数据目录要单独删，**我们不会替你删数据**。

需要 **WebView2 运行时**（Windows 10 1803+ / Windows 11 已内置；老系统装一下
[Evergreen 运行时](https://developer.microsoft.com/microsoft-edge/webview2/)）。

### 自己构建

```bat
rustup toolchain install stable-x86_64-pc-windows-msvc
git clone https://github.com/YJLZSL/DeskBase.git
cd DeskBase
node scripts/build.cjs --release     REM 产物在 app\target\release\deskbase.exe
node scripts/build.cjs --test        REM 跑测试
```

需要 Visual Studio 2022+（或 Build Tools）的「使用 C++ 的桌面开发」工作负载，
以及 Node.js ≥ 18。`build.cjs` 会自动探测 MSVC 与 Windows SDK 的位置；
探测不到时用 `DESKBASE_VCVARS` / `DESKBASE_SDK_ROOT` / `DESKBASE_SDK_VER` 指定。

打便携包（含 SHA-256 与 SBOM）：

```bat
node scripts/package.cjs
```

### 安装版（计划中）

走**用户级安装**：装到用户目录，不需要管理员权限，不写 `HKLM`，只写卸载所必需的最小注册表项。
安装向导会让你逐项选择安装路径、数据目录、快捷方式、文件关联、开机自启等，
**所有选项默认选最克制、最隐私、最干净的方案**。不捆绑、不静默安装第三方组件、不强制重启、不强制登录、不强制联网。

卸载分两档：`保留数据卸载` 与 `彻底清除卸载`。彻底清除前二次确认，并逐条列出将删除的内容。

---

## 文档地图

| 文档 | 内容 |
|------|------|
| [docs/00-executive-summary.md](docs/00-executive-summary.md) | 总体方案摘要（建议先读这一篇） |
| [docs/01-vision-boundary-principles.md](docs/01-vision-boundary-principles.md) | 愿景、边界与核心原则 |
| [docs/02-users-and-scenarios.md](docs/02-users-and-scenarios.md) | 目标用户与典型使用场景 |
| [docs/03-information-architecture.md](docs/03-information-architecture.md) | 信息架构：一级 / 二级 / 三级 |
| [docs/04-feature-matrix.md](docs/04-feature-matrix.md) | 完整功能矩阵：MVP / 进阶 / 插件化 |
| [docs/05-office-toolbox.md](docs/05-office-toolbox.md) | 办公工具箱详细设计 |
| [docs/06-sql-database.md](docs/06-sql-database.md) | SQL 数据库能力设计 |
| [docs/07-backup-recovery-dr.md](docs/07-backup-recovery-dr.md) | 备份、恢复、断电保护与容灾 |
| [docs/08-privacy-security.md](docs/08-privacy-security.md) | 隐私、安全、加密、权限与审计 |
| [docs/09-local-ai.md](docs/09-local-ai.md) | 本地 AI 架构与隐私策略 |
| [docs/10-desktop-mobile-remote.md](docs/10-desktop-mobile-remote.md) | 桌面端与手机远程端架构 |
| [docs/11-security-and-resource-optimization.md](docs/11-security-and-resource-optimization.md) | 安全与资源优化专项 |
| [docs/12-installer-and-uninstaller.md](docs/12-installer-and-uninstaller.md) | 安装与卸载体验详设 |
| [docs/13-settings-tutorial-help.md](docs/13-settings-tutorial-help.md) | 设置、教程与帮助系统 |
| [docs/14-performance-budget.md](docs/14-performance-budget.md) | 性能优化清单与量化指标 |
| [docs/15-open-source-governance.md](docs/15-open-source-governance.md) | 开源治理、许可证与插件生态 |
| [docs/16-roadmap-risks-acceptance.md](docs/16-roadmap-risks-acceptance.md) | 路线图、风险与验收标准 |
| [docs/17-tech-selection.md](docs/17-tech-selection.md) | 技术选型候选与取舍（不绑定） |
| [docs/18-visual-and-motion-design.md](docs/18-visual-and-motion-design.md) | 主题体系、宣纸质感、字体导入、动效与微交互规范 |
| [docs/adr/](docs/adr/) | 架构决策记录（ADR） |
| [docs/99-glossary.md](docs/99-glossary.md) | 术语表 |

---

## 参与贡献

我们欢迎一切形式的贡献：报告缺陷、提出需求、改进文档、提交代码、翻译、设计、写教程。

- 提交代码前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)
- 参与社区请遵守 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- 发现安全问题请**不要**开公开 Issue，按 [SECURITY.md](SECURITY.md) 私下报告
- 后续方向见 [ROADMAP.md](ROADMAP.md)，项目治理见 [GOVERNANCE.md](GOVERNANCE.md)

---

## 许可证

核心项目采用 **Apache License 2.0**——允许商用与闭源集成，同时提供明确的专利授权与免责条款，适合需要被广泛使用与二次分发的桌面工具。

许可证选择的完整理由、以及为何**不**采用 GPL / AGPL / SSPL 的讨论，见 [docs/15-open-source-governance.md](docs/15-open-source-governance.md)。

```
Copyright 2026 The DeskBase Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
```

---

<div align="center">

**DeskBase · 桌库** —— 你的数据，放在你自己的桌子上。

</div>
