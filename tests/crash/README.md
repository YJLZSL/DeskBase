# 崩溃一致性强杀循环骨架（tests/crash）

> 对应任务 **P0-08**，对应规范 **`docs/19-test-strategy.md` 第 5.1 节 F01** 与
> 设计文档 **`local-docs/reference/13-crash-test-and-corpus.md`** 第四节。
> 配套语料库见 **`testdata/`**。

## 一、作用

这是一个**先于实现写好**的骨架：业务代码还是 0 行时，它就能把「写入中强杀」这条
破坏性测试的流程跑通。等 P1 内核实现后，只需替换 `--target` / `--verify`，方法不变
（见 `13-crash-test-and-corpus.md` 第八节）。

流程：**启动目标进程 → 随机延迟 → 强杀 → 重启 → 校验 → 记录**，循环 N 次并汇总。

## 二、约束

- **不依赖任何第三方包**，只用系统自带 Node。
- 真实应用没写，因此默认提供 `--dry-run`：内置一个「假目标」（反复写文件的小脚本）
  和一个内置校验，演示整条链路能跑通。
- 真实接入时用 `--target` 指向被测进程、`--verify` 指向校验命令。

## 三、用法

```bash
# 1) 演示模式（无需任何真实程序即可跑通）
node kill-loop.mjs --dry-run --rounds 5

# 2) 真实接入（等 P1 内核后）
node kill-loop.mjs \
  --target "node path/to/writer.mjs" \
  --verify "node path/to/check.mjs" \
  --rounds 100 \
  --min-delay 1000 --max-delay 5000 \
  --out ./crash-data \
  --report ./crash-report.json
```

### 参数

| 参数 | 默认 | 说明 |
|------|------|------|
| `--dry-run` | 关 | 使用内置假目标与内置校验，演示流程。 |
| `--rounds` | 100 | 迭代次数（F01 规范为 100；CI 抽样可缩到 20）。 |
| `--min-delay` | 1000 | 随机强杀延迟下限（ms）。 |
| `--max-delay` | 5000 | 随机强杀延迟上限（ms）。F01 规范为 1–5 s。 |
| `--target` | 空 | 目标进程启动命令（真实模式必填）。 |
| `--verify` | 空 | 校验命令；退出码 0 = 通过（真实模式必填）。 |
| `--out` | dry-run 用系统 Temp；否则 `./crash-data` | 目标进程的数据目录。 |
| `--report` | 数据目录同级的 `*.report.json` | JSON 报告输出路径。 |

## 四、判据

- **通过**：校验命令退出码为 0（重启后数据库可打开、完整性检查通过、已提交数据无损、
  最多丢失最后一个未提交事务）。
- **失败（P0）**：校验命令非 0，且非环境问题——即真实数据损坏，阻断发布。
- **环境问题**：目标进程未能启动 / 校验超时 / 临时盘满等外部原因，需重跑，不计入失败。

详见 `13-crash-test-and-corpus.md` 第七节「测试通过判据」。

## 五、输出

每轮打印一行摘要；结束打印汇总（总次数 / 通过数 / 失败数 / 环境问题数 / 失败详情），
并写出结构化 JSON 报告（含随机种子、每轮延迟、退出信号、校验结果），供 CI 门禁读取：

```json
{
  "scenario": "F01",
  "mode": "dry-run",
  "seed": 123456,
  "rounds": 5,
  "passed": 5,
  "failed": 0,
  "envIssues": 0,
  "failures": [],
  "perRound": [ { "round": 1, "killDelayMs": 3120, "signal": "SIGKILL", "verifyExit": 0, "pass": true } ]
}
```

CI 判门禁规则：`failed === 0 且 passed === rounds` 才通过。
