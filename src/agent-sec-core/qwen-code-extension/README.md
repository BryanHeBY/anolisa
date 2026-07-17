# Qwen Code extension

全本地、零 Token 成本地为 Qwen Code 提供 PII/凭据扫描和 Observability。
扩展通过 Qwen Code 原生 command hook 挂载，不自行实现 HookRegistry 或事件聚合器；
PII policy hook 同步返回安全决策，Observability hook 异步记录生命周期事件。

协议依据是 Qwen Code 官方的
[Hooks 文档](https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/)；
extension manifest、变量替换和安装行为同时依据
[Extensions 文档](https://qwenlm.github.io/qwen-code-docs/en/developers/extensions/extension/)
及当前 Qwen Code 源码校验。

## 前置条件

- `qwen`、`python3` 和 `agent-sec-cli` 均在 `PATH` 中。
- 当前目录已被 Qwen Code 信任；Qwen Code 会拒绝从不受信任的工作区安装扩展。

当前实现与真实安装测试以 Qwen Code `0.19.9` 源码为基线；该版本声明需要
Node.js `>=22`。与仓库内其他插件部署脚本一致，本脚本仅通过 `command -v` 检查
`qwen`、`python3` 和 `agent-sec-cli` 是否存在，不绑定或推断其版本和接口实现。

存在性检查不能证明二进制来源。部署前应通过系统包管理、制品签名或校验和确认
`qwen`、`python3`、`node` 和 `agent-sec-cli` 来自受信源；运行中的 hook 也会从
Qwen Code 进程的 `PATH` 查找 `python3` 和 `agent-sec-cli`，因此该运行时 PATH
必须只包含受信目录。

## 部署

```bash
./qwen-code-extension/scripts/deploy.sh
```

脚本调用 Qwen Code 的 `extensions install`、`extensions update` 和
`extensions enable` 命令，安装到 user scope。可通过 `QWEN_BIN` 和 `QWEN_HOME`
覆盖 Qwen Code 可执行文件及配置目录。也可以把扩展目录作为第一个参数传入：

```bash
./qwen-code-extension/scripts/deploy.sh /path/to/qwen-code-extension
```

Qwen Code 对本地扩展按 manifest 版本判断是否更新。因此修改扩展内容后必须同步提升
`qwen-extension.json` 中的 `version`，部署脚本才会执行更新。

## PII 扫描与阻断

| Qwen Code event | 扫描内容 | `scan-pii --source` | 阻断边界 |
| --- | --- | --- | --- |
| `UserPromptSubmit` | `prompt` | `user_input` | 可阻止 prompt 提交 |
| `PreToolUse` | `tool_input` | `tool_input` | 可阻止工具执行 |
| `PostToolUse` | `tool_response` | `tool_output` | `continue:false` 阻止正常结果的后续处理；不能撤销工具副作用 |
| `PostToolUseFailure` | `error` | `tool_output` | 仅扫描和审计；v0.19.9 不消费该事件的阻断字段 |
| `Stop` | `last_assistant_message` | `model_output` | 首次命中可要求模型重写 |
| `StopFailure` | `last_assistant_message` | `model_output` | 仅审计；Qwen Code 忽略该事件的输出 |

敏感原文只通过 stdin 传给
`agent-sec-cli scan-pii --stdin --format json --redact-output`，不会作为命令行参数。
用户可见告警和阻断理由只使用 `evidence_redacted`。CLI 缺失、超时、非零退出、非法
JSON、`error` 或未知 verdict 均 fail-open。

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PII_CHECKER_ENABLED` | `true` | 设为 `false`、`0`、`no` 或 `off` 时跳过扫描 |
| `PII_CHECKER_MODE` | `observe` | `observe` 只告警；`block` 显式阻断 scanner `deny`；兼容 `deny` 别名 |
| `PII_CHECKER_INCLUDE_LOW_CONFIDENCE` | `false` | 开启后传递 `--include-low-confidence` |
| `PII_CHECKER_TIMEOUT` | `5` | 子进程超时秒数，最大 8 秒；非法或非正值回退到 5 秒 |

例如，显式开启阻断后再部署或启动 Qwen Code：

```bash
export PII_CHECKER_MODE=block
export PII_CHECKER_TIMEOUT=5
./qwen-code-extension/scripts/deploy.sh
```

只有 scanner `deny` 会在 block 模式下阻断，`warn` 始终仅告警。`PreToolUse` 的
observe/pass 结果不会返回 `permissionDecision: allow`，因此不会绕过 Qwen Code 原有的
工具权限确认。

`PostToolUse` 在工具成功执行后触发，无法撤销已经发生的副作用。Qwen Code `0.19.9`
只通过 `shouldStopExecution()` 检查 `continue:false`；单独返回 `decision:block` 不会
停止执行。命中后正常工具结果会被 Qwen 转为 hook-stopped error，不再按成功结果继续
处理。`PostToolUseFailure` 的消费路径只读取 additional context 和 artifacts，不读取
阻断字段，因此失败信息只能扫描并产生审计事件，仍进入既有错误处理链。

`Stop` 首次在 block 模式命中 `deny` 时要求模型移除敏感信息、使用占位符并重写；如果
重写后 `stop_hook_active=true` 仍命中，只输出脱敏告警而不再次阻断，避免 Stop 循环。
Qwen Code 当前没有 pre-render/output-transform hook，因此该处理是尽力阻断，不能保证
原始模型文本从未出现在终端或 transcript 中。

每次 source-specific 扫描由 `scan-pii` 产生一个脱敏 `pii_scan` SecurityEvent，并关联
可用的 session/tool call ID。Observability 对敏感指标的脱敏会另行产生
`source=observability` 的扫描事件，两类事件职责不同但使用相同关联上下文。

## 可观测事件映射

| Qwen Code event | agent-sec-core hook |
| --- | --- |
| `UserPromptSubmit` | `before_agent_run` |
| `PreToolUse` | `before_tool_call` |
| `PostToolUse` | `after_tool_call`（success） |
| `PostToolUseFailure` | `after_tool_call`（error/interrupted） |
| `Stop` | `after_agent_run`（success） |
| `StopFailure` | `after_agent_run`（failure） |

Qwen Code 当前没有与 `before_llm_call` / `after_llm_call` 对应的模型调用 hook，
所以本扩展不会伪造这两类记录。当前直接 HookInput 也没有贯穿 prompt、tool 和 stop
事件的稳定 run 标识，因此现阶段所有记录的 `runId` 统一使用全零 GUID；
`tool_call_id` 优先作为 `toolCallId`，缺失时回退到 `tool_use_id`。

Qwen Code 的工具结果回填会再次触发 `UserPromptSubmit`，其中仅包含 function response
的回填会被序列化为空 `prompt`。扩展直接检查 HookInput：缺失或空白的 `prompt` 不记录，
非空 `prompt` 记录为 `before_agent_run`。该判断不创建本地 active-run 状态，因此不会因
并发或缺失 `Stop` / `StopFailure` 而影响后续用户 prompt。

`SessionStart`、`SessionEnd`、compact、notification、subagent 和 todo 等官方事件目前
没有与 agent-sec-core Observability schema 含义一致的记录类型，因此不会被错误映射到
agent/tool run；后续应先扩展 schema，再单独挂载。

`agent-sec-prompt-scanner` 是同步安全 hook：默认 `PROMPT_SCANNER_MODE=observe`，只记录
扫描事件且不阻断；设置为 `deny` 后，`agent-sec-cli scan-prompt` 返回 `warn` 或 `deny`
时会向 Qwen Code 返回拒绝决策并阻断该 prompt。`PROMPT_SCANNER_TIMEOUT` 控制内部
`agent-sec-cli` 调用超时，默认 10 秒；外层 manifest 为 prompt scanner 预留 15 秒
command-hook 超时。

`agent-sec-pii-checker` 也是同步安全 hook，但默认 `observe`；只有显式配置 block 且
scanner 返回 `deny`，才会按上述点位阻断。

`agent-sec-observability` 仍异步执行并保持 fail-open：任何脚本、PII 扫描或记录写入异常
都不会改变 Qwen Code 的执行决策。敏感指标在写入前由本地 `scan-pii` 脱敏；脱敏失败时
直接丢弃对应敏感字段。

## 测试

```bash
QWEN_CODE_EXTENSION_E2E_DIR="$PWD/qwen-code-extension" \
  uv run --project agent-sec-cli pytest \
  tests/unit-test/qwen-code-extension \
  tests/e2e/qwen-code-extension -v
```

E2E 测试从 `qwen-extension.json` 读取并直接执行 command hook，使用独立进程和隔离的
`AGENT_SEC_DATA_DIR` 验证 PII source、脱敏输出、Qwen v0.19.9 输出协议、SecurityEvent、
Observability JSONL、全零 `runId`、tool call 关联以及空 prompt 工具结果回填的过滤。
它不安装或启动 Qwen Code。
