# DeskBase 零污染快照工具（snapshot）

用于证明 DeskBase 的「零污染 / 零残留」底线：

- **便携版**：解压即用，不写注册表、不写系统目录、不装服务、不建计划任务、不改文件关联、不开机自启。
- **安装版**：用户级安装（只写 `HKCU`），卸载后残留必须为 0。

唯一的证明手段是**快照对比**：操作前拍一张、操作后拍一张，逐项比对差异。差异为 0 即为达标。

本工具**零第三方依赖**，只需系统自带 Node（以及 Windows 自带的 `reg.exe` / `schtasks.exe` / `sc.exe`）。

---

## 运行环境

- Node.js ≥ 16（本机使用 `C:\Users\项目发起人\node.exe`）
- 仅支持 Windows 采集（注册表 / 任务 / 服务依赖系统命令）。在非 Windows 上 `take` 会失败并记入错误。

调用示例：

```bat
set NODE=C:\Users\项目发起人\node.exe
%NODE% tools/snapshot/snapshot.mjs help
```

> 给 Windows 程序传路径请用 `C:/...` 形式，不要用 Git Bash 的 `/c/...`。

---

## 三个子命令

### 1. `take` —— 拍快照

```
node snapshot.mjs take --out <快照文件.json> [--label 标签] [--dirs "dir1,dir2"] [--reg-keys "k1,k2"]
```

| 参数 | 必填 | 说明 |
|------|------|------|
| `--out` | 是 | 快照输出路径（JSON） |
| `--label` | 否 | 快照标签，记进元数据 |
| `--dirs` | 否* | 要监控的文件目录列表，逗号分隔。**默认不扫全盘** |
| `--reg-keys` | 否 | 覆盖默认注册表采集根键（逗号分隔） |

\* 不传 `--dirs` 则文件系统部分为空，仅对比注册表/任务/服务/自启。

#### 默认采集范围

**注册表**（递归 `/s`）：

- `HKCU\Software`
- `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`
- `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
- `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
- `HKLM\SYSTEM\CurrentControlSet\Services`

> `HKCU\Software` 已包含 Run 与 Uninstall 子键；后两者单独列出是为冗余保险与范围清晰。

**自启项**（读取键值，非递归）：

- `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
- `HKLM\Software\Microsoft\Windows\CurrentVersion\Run`

**文件系统**：默认不扫，需 `--dirs` 显式指定（如便携版解压目录、安装目标目录）。

**计划任务**：`schtasks /query /fo LIST`，取所有 `TaskName`。

**服务**：`sc query type= service state= all`，记录服务名、状态、启动类型。

### 2. `diff` —— 对比快照

```
node snapshot.mjs diff <快照A> <快照B> [--json <输出.json>]
```

输出五类差异，每类都给出**精确路径**（非空时）：

- **新增**：键/值/文件/任务/服务/自启项
- **删除**：同上
- **修改**：注册表值变了、文件大小或修改时间变了、服务状态/启动类型变了

无 `--json` 时打印人类可读的分组缩进文本；加 `--json` 把结构化结果另存给 CI。

### 3. `verify` —— 判定零残留

```
node snapshot.mjs verify <快照A> <快照B> [--allow <白名单.txt>] [--json <输出.json>]
```

- 差异为 0（或全被白名单容忍）→ **退出码 0**，打印「未发现残留」。
- 存在真实残留 → **退出码 1**，逐条列出。
- `--allow <白名单>`：文本文件，每行一个正则（`#` 开头为注释）。命中正则的差异项归入「可接受变化」，其余为「真实残留」。输出会**明确区分两者**。

---

## 白名单文件怎么写

每行一个 JS 正则，用于容忍系统自身的临时变化。示例见 `allowlist-default.txt`，覆盖了：

- Windows 自身写入：Prefetch、事件日志、Explorer 下的时间戳/MRU 类值。
- 系统服务状态自然变化：时间同步（W32Time）、Windows Update（wuauserv / UsoSvc）。
- 系统内置计划任务（`Microsoft\Windows\` 下的任务目录）。

白名单**只应容忍非 DeskBase 写入、且必然被系统自身改写**的噪声；任何由 DeskBase 自身产生的项都不应进白名单，否则会掩盖真实污染。

---

## 在 P0-05 / P8-20 里怎么用

### P0-05：验证便携版运行后注册表差异 = 0

```bat
:: 运行便携版前
%NODE% tools/snapshot/snapshot.mjs take --out before.json --dirs "D:/path/to/portable"
:: 解压并运行便携版，关闭退出
%NODE% tools/snapshot/snapshot.mjs take --out after.json --dirs "D:/path/to/portable"
:: 判定（容忍系统噪声）
%NODE% tools/snapshot/snapshot.mjs verify before.json after.json --allow allowlist-default.txt
:: 退出码 0 即证明便携版零污染
```

### P8-20：验证安装版「安装 → 使用 → 彻底卸载」后残留 = 0

```bat
:: 安装前
%NODE% tools/snapshot/snapshot.mjs take --out s0.json --dirs "C:/Users/<you>/AppData/Local/DeskBase"
:: 安装、使用、彻底卸载（含卸载器清理）
%NODE% tools/snapshot/snapshot.mjs take --out s1.json --dirs "C:/Users/<you>/AppData/Local/DeskBase"
:: 判定残留必须为 0
%NODE% tools/snapshot/snapshot.mjs verify s0.json s1.json --allow allowlist-default.txt
:: 退出码 0 即证明卸载后残留为 0
```

---

## 已知限制

- **权限**：`HKLM` 下部分键需管理员权限才能读；无权限时该键采集失败并记入快照 `errors`，diff 时会显示为「该键缺失」，需以管理员身份重拍快照。
- **长路径**：Windows `MAX_PATH` 限制由 `\\?\` 前缀绕过；但个别系统命令（reg/schtasks）对超长输出有自身缓冲上限。
- **被占用的键/文件**：被其他进程独占占用的键或文件无法读取，会被跳过并记录到 `errors`，不会让工具崩溃。
- **注册表大数据量**：`HKCU\Software` 递归采集可能较慢且快照较大；在 CI 中建议仅对关心的子键用 `--reg-keys` 收窄。
- **文件比对维度**：仅比大小与修改时间，不比对内容哈希（避免大目录过慢）；若需内容级比对可后续扩展。
- **服务/任务实时性**：`sc` / `schtasks` 反映的是拍快照瞬间的运行时状态，系统自身服务状态波动会进入差异，应通过白名单容忍。
