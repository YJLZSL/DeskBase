# AGENTS.md · DeskBase 的 AI 协作规范

> **任何 AI（或人）在这个仓库里动手之前，先读完本文件。**
> 本文件管**流程与纪律**：接手先读什么、每次必须更新什么、哪些线不能碰。
> 具体任务不在这里 —— 去 `local-docs/handoff/` 领。

**为什么有这份文件**：这个项目由多个 AI 会话接力开发。不把流程写死，
每一棒都会重新踩同一批坑 —— 改完不更新交接文档、把"测过后端"说成
"点过界面"、同一文件并发编辑互相覆盖、凭感觉报性能数字。
本文件把已经付过学费的教训固化下来，后面的会话不必再交一遍。

---

## 0. 三十秒认识这个项目

| 项 | 值 |
|----|-----|
| 项目 | DeskBase（桌库）· Apache-2.0 开源 |
| 定位 | 本地优先、隐私优先的办公工具箱 + 给非程序员用的轻量 SQL 数据库桌面工具 |
| 形态 | Windows 单机桌面应用（Rust + `tao`/`wry` + 系统 WebView2） |
| 代码 | `app/src/*.rs`（后端与全部能力）+ `app/ui/*.js|css`（WebView 里的手写前端） |
| 通道 | `window.__deskbase.call(cmd, args)` ↔ Rust `dispatch()`，命令在 `app/src/main.rs` 显式列出 |
| 存储 | 自研单文件存储引擎 `store`（append-only 日志 + 快照 + fsync，纯 Rust，无第三方数据库依赖） |
| 数据目录 | `data/main.dkb`、日志 `data/logs/app.log`（`DESKBASE_DATA_DIR` 可覆盖） |
| 当前版本 | 见 `local-docs/handoff/VERSION_PLAN.md` §1 事实面板（**别从别处猜版本**） |

---

## 1. 接手：按这个顺序读

**每一份都标了它的可靠性 —— 这个项目里"文档比代码旧"是常态。**

| 顺序 | 文件 | 角色 | 注意 |
|:--:|------|------|------|
| 1 | `local-docs/handoff/CONTEXT_SNAPSHOT.md` | **当前状态**（倒序，最新在最上面） | 读最上面一节即可，不必通读 |
| 2 | `local-docs/handoff/VERSION_PLAN.md` | **版本线 + 准入门槛 + 事实面板** | §0 维护规则必须先读 |
| 3 | `local-docs/handoff/DEVELOPMENT_PLAN.md` | **开工看它**：读顶部"⚠️ 读这一节就够" | 下面的 P0–P8 任务表是早期写的、状态已失真，只作线索 |
| 4 | `local-docs/handoff/AI_HANDOFF.md` | 三大原则（不丢数据 / 不泄露 / 本地优先） | ⚠️ §0 速览表停留在设计期，勿引用其数字 |
| 5 | `local-docs/handoff/OPEN_QUESTIONS.md` | 未决问题（Q-xxx 编号） | 动手前查一眼，别重复决策 |
| 6 | `local-docs/handoff/DECISIONS_LOG.md` | 已决事项与踩过的坑（D-xxx） | 修 bug 前先搜这里 |
| 7 | `local-docs/handoff/FILE_INDEX.md` | 文件清单（改哪个功能动哪些文件） | 新增文件后要更新它 |
| 8 | `local-docs/handoff/AI_CONTEXT.md` | 结构化上下文（带 `文件:行号`） | 版本基准可能偏旧，以代码为准 |
| 9 | `local-docs/handoff/ACCEPTANCE-*.md` | 验收覆盖**到哪一层** | 引用"验收过了"之前先看它 |
| 10 | `CONTRIBUTING.md` | 仓库红线（第十节）与提交流程 | 公开仓库，与 `local-docs/` 分开 |

**过时文件警告**：`NEXT_AI_GUIDE.md` 是**设计期**写的（开头还写着"代码未开始"），
只有第五节"文档修改规则"仍然有效；它的任务顺序已被 `DEVELOPMENT_PLAN.md` 重基线取代。

> `local-docs/` 是**本地文档，已被 `.gitignore` 排除**，永不提交进仓库。
> 新会话如果拿不到它，问项目维护者。

---

## 1b. 目录职责表（别猜某个目录是干什么的）

```
DeskBase/
├── app/                    应用本体（唯一会进产物的东西）
│   ├── src/*.rs            Rust：外壳 + 全部能力。模块职责见 AI_CONTEXT 第 3 节
│   ├── ui/*.{js,css,html}  WebView 里的手写前端（零依赖、零构建）
│   └── target/             ⚠️ 构建缓存（~4 GB），可随时删，不要提交
├── docs/                   ★ 对外文档（会入库）：产品设计 00–19 + ADR。文档是契约
├── local-docs/             ⚠️ 本地专属，**已 gitignore、永不入库**
│   ├── reference/          调研与参考（26 篇，索引见其 README）
│   └── handoff/            AI 交接体系（快照/版本线/决策/待决/文件索引）
├── scripts/                构建、打包、发布、四道门禁、界面烟测
├── tests/                  端到端验收（e2e-import）、压力测试（stress-import）
│   └── crash/              崩溃一致性套件（P0-08 的产物）
├── tools/                  构建期工具（图标光栅化、字体校验）
├── testdata/               测试夹具（脏数据语料生成器 + GitHub 发布样本）
├── poc/                    ⚠️ 调研期产物（ADR-0010 的实测依据）。源码入库，
│                              `target-msvc/` 与 `node_modules/` 是构建缓存（约 700 MB），
│                              **不要删源码**（ADR-0010 引用它作为方案对比的依据）
├── dist/                   ⚠️ 发布产物归档（zip / SHA-256 / SBOM），已 gitignore
└── .workbuddy/             会话记忆（memory/ 是个人工作日志，已 gitignore）
```

**三条容易踩的边界**：

| 目录 | 规则 |
|------|------|
| `local-docs/` | **永不入库**（`git add -f` 也不行）。换机必须单独拷贝 |
| `docs/` | 是对外契约，改它要按 `CONTRIBUTING.md` 第八节补流程；结论性改动走 ADR |
| `poc/`、`dist/` | 本地产物。`poc/` 的**源码**留着（它是决策依据），缓存可删 |

---

## 2. 干活：命令速查

**不要绕过 `scripts/build.cjs` 直接跑 `cargo`** —— 本机有三处非标准
（MSVC 工具链 / Windows SDK 位置 / `reg.exe` 被安全策略拉黑），
脚本按「环境变量 → 常见位置 → 可操作的报错」三级回退兜住了它们。

| 目的 | 命令 |
|------|------|
| 只查编译（最快） | `node scripts/build.cjs --check` |
| 构建 release | `node scripts/build.cjs` |
| 构建 debug / 构建后启动 | `node scripts/build.cjs --debug` / `--run` |
| **测试**（唯一入口） | `node scripts/build.cjs --test` |
| 界面烟测（真实 WebView 里真实点击，31 步） | `node scripts/build.cjs --smoke` 或 `node scripts/ui-smoke.cjs <exe>` |
| **端到端验收**（真实数据走完整导入链路，28 步） | `node scripts/build.cjs --e2e` 或 `node tests/e2e-import.cjs <表格文件>` |
| **导入压力测试**（默认 1万+10万；可指定） | `node tests/stress-import.cjs --sizes 10000,100000,200000` |
| **崩溃恢复端到端**（强杀→重启验尸→复检，三阶段） | `node tests/crash-recovery.cjs` |
| **界面自动走查**（截图 + 文案导出 + 布局体检，出 HTML 报告） | `node tests/ui-walkthrough.cjs` |
| 门禁四件套 | `node scripts/check-motion.cjs` · `check-contrast.cjs` · `check-wiring.cjs` · `check-size.cjs` |
| 打包（zip + SHA-256 + SBOM） | `node scripts/package.cjs` |
| 发布（打标签即发布） | `node scripts/publish-release.cjs vX.Y.Z [--dry-run] [--prerelease]` |
| 发布说明 | `node scripts/gen-changelog.cjs [--to <ref>]` |

**验收口径（重要）**：

| 改了什么 | 至少跑什么 |
|---------|-----------|
| 纯后端逻辑 | `--test` |
| 涉及界面 | `--test` + `--smoke` |
| 涉及样式 / 主题 | 再加 `check-motion` + `check-contrast` |
| 新增界面文件或 IPC 命令 | 再加 `check-wiring` |
| **涉及导入 / 大批量数据** | 再加 `--e2e` + `stress-import --sizes 10000,100000` |
| 动到体积（加依赖 / 加资源） | 再加 `check-size` |

- **别把"后端测过"说成"界面点过"** —— 这是本项目最忌讳的事。
  验收覆盖到哪一层，`ACCEPTANCE-*.md` 里逐项标着（已自动化 / 仅 IPC 层 / 未覆盖）。
- **测试必须可重复**：只断言确定的东西。网络结果（限流/超时/有新版）是**合法结果**，
  UI 如实展示即通过 —— 把网络抖动当红灯的测试会被无视。
- **改完连跑两次**确认稳定再提交：偶发通过的测试等于没有测试。

---

## 2b. 发布（Release）

**正路：推标签 → CI 自动发布。** 本地只做四件事 ——

1. 版本号两处一起改：`app/Cargo.toml` + `app/Cargo.lock`（不一致会让 `cargo build --locked` 直接失败）
2. 写 CHANGELOG 版本节 —— **它就是发布说明的唯一来源**（release.yml 从它抽取正文）
3. 本地验证：`node scripts/build.cjs --test && node scripts/build.cjs && node scripts/package.cjs`，
   并**解压打包产物用里面的 exe 实测**（烟测/崩溃恢复都能指定 exe 路径）
4. `git tag -a vX.Y.Z -m "..." && git push origin main && git push origin vX.Y.Z`

剩下的全自动：`.github/workflows/release.yml`（推 `v*` 触发）跑测试 + 门禁 +
构建 + 打包 + 建 Release（tag 含 `-` 自动预发布）+ 传 3 个产物。
**用 runner 自带 token，不依赖本机凭据 —— 本机无凭据时的正解就是这条路。**

**本机特殊情况**（无凭据 / 网络抽风 / Edge 锁 / shim 改写删除命令）——
**完整手册：`local-docs/handoff/RELEASE-RUNBOOK.md`。四个硬坑速记：**

1. **代理端口每会话变**（53248→61827…）：动态读 `$https_proxy`，**绝不写死**；
   HTTPS 一律加 `--ssl-no-revoke`；git 另需 **成对**的
   `-c http.schannelCheckRevoke=false -c http.sslBackend=schannel`
2. **仓库 Actions 默认权限必须是 write** —— 否则声明 `contents: write` 的
   workflow **0 job 直接失败**（`PUT /repos/{o}/{r}/actions/permissions/workflow`）
3. **workflow 注册缓存可能坏**（name 显示为路径 + 恒 0 job）→ 别恋战，**走 API 直发**
4. **禁用 `git rm`**：本机 shim 会把它变成递归删除（2026-09-19 真删过整个 `.github/`）。
   删文件一律 Node `fs.unlinkSync` + `git add <明确路径>`

**发布的两条落地路**：① 推 tag → CI（`release-publish.yml`）全自动；
② CI 不可用 → `node local-docs/tools/publish-via-api.cjs`（API 直发 + 本地产物，实测可用）。

**发布后回填**：VERSION_PLAN 的事实面板与发布记录（见第 3 节收工清单）。

## 3. 收工：必须更新的文档

**"改完代码不更新交接文档"= 把烂摊子留给下一棒。** 按此表勾一遍：

| 触发条件 | 必须更新 |
|----------|----------|
| **每次实质工作**（写代码/修 bug/调研/决策） | `CONTEXT_SNAPSHOT.md` **最上面加一节**（倒序） |
| **每次实质工作** | AI 会话记忆：WorkBuddy 工作区的 `.workbuddy/memory/YYYY-MM-DD.md`（在仓库外，只追加） |
| 发版 / 调整版本线 | `VERSION_PLAN.md` §1 事实面板 + §版本线；`CHANGELOG.md` |
| 做了技术决策、踩了值得记的坑 | `DECISIONS_LOG.md` 新条目（D-xxx） |
| 出现未决问题 | `OPEN_QUESTIONS.md` 新条目（Q-xxx），给出建议方案并标"待确认" |
| 新增/删除文件 | `FILE_INDEX.md` |
| 新增 ADR | `docs/adr/00xx-*.md` + 更新 `docs/adr/README.md` 索引 |
| 用户可见的行为变化 | `CHANGELOG.md`（口径见 `gen-changelog.cjs` 的用法） |

状态标记约定（全项目统一）：`☐` 待做 / `◐` 进行中 / `☑` 完成 / `⊘` 阻塞。

---

## 4. 硬性工程约定（违反 = 静默失效或门禁变红）

1. **IPC 命令必须是 `dispatch_sync` 里的 `match` 分支**。
   `check-wiring.cjs` 只认这个形式；写成 `if req.cmd == "..."` 门禁会误报（不是形式主义，是门禁的实现方式）。
2. **新增界面文件三步走**：写进 `app/ui/` → 登记到 `app/src/assets.rs` 资源表 → 在 `index.html` 挂载。
   漏任何一步 → 功能**静默失效**（不带报错的那种）。`check-wiring.cjs` 就是为这个存在的。
3. **serde 字段一律 `snake_case`，前端照读 `snake_case`**。
   历史上 `has_more` / `elapsed_ms` 两次因为前端写了驼峰，导致界面**毫无异常地**不显示数据。
   现在有专门的通用门禁在扫这条，但写的时候就要对。
4. **样式走 CSS 变量**（`--success` / `--warning` / `--danger` 等），
   动效只允许 `motion.css` 定义的档位与缓动。改样式要过 `check-motion` + `check-contrast`。
5. **体积**：release 产物要过 `check-size.cjs`（阈值 10 MB，当前约 6 MB）。加依赖前先想体积。
6. **新增依赖不联网**（见红线 R2）。引入前在 `DECISIONS_LOG.md` 写理由。
7. **注释用中文、解释"为什么"** —— 尤其是反直觉的取舍、绕过的坑、被否决的方案。
   这个代码库的注释是给下一个接手者读的，不是给编译器读的。
8. **不改已接受 ADR 的结论**。要改就走新 ADR（`docs/adr/` 下编号递增）。

---

## 5. 红线（碰之前先查 ADR）

完整 17 条在 `NEXT_AI_GUIDE.md` 第六节 + `CONTRIBUTING.md` 第十节。最容易踩的：

| # | 红线 |
|:-:|------|
| R1 | **用户数据不得离开本机**（逐次显式授权除外）；遥测/埋点/崩溃上报**不存在**（不是开关）—— ADR-0017 |
| R2 | 程序自主联网**默认关闭**、各自独立开关、全记审计（含被拒）、关掉时**出站 0 字节** —— ADR-0018/0019 |
| | 无账号、无登录、无强制云同步 |
| | 不以字符串拼接构造查询（值一律走类型校验）；不用 `innerHTML` 渲染用户数据 |
| | 密钥/令牌/口令不写日志；不把加密密钥放进备份包 |
| | **不把 `local-docs/` 提交进仓库**（`git add -f` 也不行）；不改 `.gitignore` 使其失效 |

> R1 与 R2 的区分很重要：更新检查只归 R2（可以做），别把 R2 的事挂到 R1 名下 ——
> 那会污染 R1 的可信度（ADR-0019 的原话）。

---

## 6. 验证纪律（本项目的文化核心）

1. **"状态必须与事实一致"** —— 写"已做"必须有实测或提交号支撑；**没验的就写"未验"**。
2. **数字要可复现** —— 每个数字后面标注怎么测出来的（命令/步骤）。禁止"凭感觉给性能数据"。
3. **禁止"基本完成"** —— 明确说清哪些**没**做，不要用含糊话糊过去。
4. **分层声明**：后端测过 ≠ 界面点过 ≠ 真机重启过。说清楚验到哪一层。
5. **测试要能重复运行**。界面/冒烟测试**只断言界面行为，不断言网络结果** ——
   限流、超时、有新版、已最新都是网络的合法结果，UI 如实展示即通过。
   "有时红有时绿"的测试等于没有测试。
6. **修 bug 先有复现用例**；改数据/并发/崩溃相关代码，先写测试再写实现。
7. **版本号不预支**：不到发布条件不升版本号，也不要跳级到 v1.0.0 之类的"正式版"充数。

---

## 7. 已知的坑（已付过学费，别重复交）

| 坑 | 事实与对策 |
|----|-----------|
| **同一文件并发编辑会静默覆盖** | 多个编辑同时作用于一个文件时，会出现"工具报成功、内容没落盘"。**逐条改、改完立刻验证**（`cargo check` / grep 复核），不要并行编辑同一个文件 |
| **`evaluate_script` 在 IPC 回调栈里会被吞** | WebView2 的 `WebMessageReceived` 回调里再调 `ExecuteScript`，脚本不会执行（日志却"看起来"成功）。注入类操作**走事件循环**（见 `main.rs` 的 `AppEvent::SmokeInject` 注释） |
| `evaluate_script` 的返回值只代表"调用被受理" | 脚本执行的真实结果在回调里（wry 的 `evaluate_script_with_callback`）。想确认执行成功，必须用回调或让脚本自己回报 |
| `reg.exe` 被本机安全策略拉黑 | 见到 `LNK1181: cannot open input file 'kernel32.lib'` 先想这条；构建脚本已用"扫盘符"兜住，**不要自己写注册表方案** |
| 界面烟测会**短暂弹窗** | 正常现象，烟测用临时数据目录（不碰真实数据），结束自动清理 |
| 交互式命令会挂 | `Read-Host`、`git rebase -i` 等在非交互环境会挂死；PowerShell 5.1 不支持 `&&` |
| **数据文件已从 SQLite 变为自研格式** | 旧库 `data/main.db`（SQLite）→ 新库 `data/main.dkb`（自研 append-only 日志 + 快照）；旧库不直读，迁移走「旧版导出 JSON/CSV → 新版导入」。主程序不再依赖 SQLite（`rusqlite` / `libsqlite3-sys` 已从依赖树移除），`Db` 句柄现在指向 `store` / `model` 而非 SQLite 连接 |

---

## 8. 当前进度与下一步

> **最新状态（2026-09-19 深夜）**：`v0.3.0-beta.2` 已发布（Release 3 产物）。
> 测试 **272** · 烟测 **37** · 崩溃 **16** · 联网 **2/2**。
> v0.3.0 正式版还差 5 项 —— **接手的 AI 请先读
> `local-docs/handoff/NEXT-STEPS.md`**（现状 / 剩余包 / 验收口径 / 三个必踩的坑）。

**不要从本文件猜进度** —— 两个地方是权威，且每次收工都会被更新：

- `local-docs/handoff/CONTEXT_SNAPSHOT.md`（最上面一节）：刚发生了什么、有什么没解决
- `local-docs/handoff/DEVELOPMENT_PLAN.md`（顶部重基线节）：**下一步按什么顺序做**

---

## 9. 交付格式（给用户的执行摘要）

用户看不到你的思考过程与工具输出，**只能看到最终回复**。收工时必须给四件事：

1. **做了什么**（结论先行，别铺垫）
2. **改了什么**（具体文件 + 一句话说明改了什么）
3. **还剩什么**（没做的、没验的、被阻塞的，如实列出）
4. **有什么坑**（下一棒需要注意的）

不要只贴一份长报告；不要把"打算做"写成"做完了"。

---

## 相关文件

- 仓库内：`CONTRIBUTING.md`（红线与流程）· `README.md`（对外说明）· `CHANGELOG.md`（用户可见变化）· `ROADMAP.md` · `docs/`（设计契约）· `docs/adr/`（已接受决策）
- 本地（不上传）：`local-docs/handoff/`（交接全套）· `local-docs/reference/`（调研与参考）
