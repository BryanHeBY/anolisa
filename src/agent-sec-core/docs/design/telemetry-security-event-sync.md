# Telemetry 安全事件同步设计规格

## 背景

`security_middleware` 当前在动作完成或异常时生成 `SecurityEvent`，并通过
`agent_sec_cli.security_events.log_event()` 写入：

- `security-events.jsonl`
- `security-events.db`

新增 telemetry 模块后，需要在不改变现有安全审计语义的前提下，从同一次 action 的
`actionResult` 派生一条 telemetry JSONL 记录。telemetry
记录不再复用原始 security event envelope，也不再以 `details` 字段筛选
作为核心模型；sanitizer 的职责调整为把各能力的 `actionResult` 映射到
Agentic OS 约定的 schema 字段。

Agentic OS 新增组件采集日志规范后，`agent-sec-core` 的 telemetry 记录还必须
写入统一 SLS ops JSONL 文件：

```text
/var/log/anolisa/sls/ops/agent-sec-core.jsonl
```

该目录和空文件由注册授权模块预创建并统一配置 logrotate。`agent-sec-core`
只负责按规范追加 JSONL 记录，不负责创建目录、初始化组件文件或配置轮转策略。

## 目标

1. 新增 `agent_sec_cli.telemetry` 模块，提供安全事件到 telemetry JSONL 的同步写入能力。
2. 不维护独立 telemetry 开关配置；目标组件日志文件存在则 best-effort 写入，
   不存在则跳过 telemetry 写入。
3. telemetry JSONL 使用独立文件，默认路径为
   `/var/log/anolisa/sls/ops/agent-sec-core.jsonl`。
4. 每次写入都必须通过路径重新打开文件、写入并关闭句柄，避免 logrotate 以
   rename 方式轮转后继续写旧 inode。
5. telemetry 写入必须是 best-effort，不影响原有 `security-events.jsonl` 和 SQLite 写入。
6. telemetry schema 使用 `seccore.*` 和 `baseline.*` 字段前缀表达数据分组，
   不再直接镜像原始 security event envelope，也不输出独立的数据分组字段。
7. 每条 telemetry JSONL 记录必须包含 Agentic OS 组件固定字段：
   `component.name`、`component.version`、`component.agent_name`。

## 非目标

- 不新增 telemetry SQLite 索引。
- 不修改各个 `security_middleware` backend 的执行语义。
- 不替换现有 `observability` 模块。
- 不新增 CLI 查询命令。
- 不在本阶段实现远端上传、批处理、重试队列或 OpenTelemetry exporter。
- 不设计通用脱敏 DSL。第一阶段只实现 `actionResult` 到目标 schema 的显式字段映射。
- 不创建或 chmod `/var/log/anolisa/sls/ops` 目录及其中的预置组件文件。
- 不写入 `instance.jsonl`、`llm.jsonl` 或其它组件的 JSONL 文件。
- 不由 `agent-sec-core` 配置 logrotate。轮转策略由注册授权模块统一设置。

## 现有基础

当前可复用组件：

- `agent_sec_cli.security_events.schema.SecurityEvent`
  - 当前安全事件 canonical envelope。
- `agent_sec_cli.security_events.writer.JsonlEventWriter`
  - 通用 JSONL writer，支持线程锁、flock、文件轮转、best-effort 错误处理。
  - 其现有实现已在每次写入时按路径 fresh open 并关闭文件句柄；Agentic OS
    组件日志可复用这一语义，但不能在 SLS ops 目录启用 agent-sec-core
    自有 size-based rotation。
- `agent_sec_cli.security_events.config.get_stream_log_path()`
  - 已支持逻辑 stream 到 JSONL 路径的解析，可作为测试或 legacy fallback
    能力；Agentic OS 生产路径应使用固定组件文件。
- `agent_sec_cli.security_events.log_event()`
  - 当前 security event 双写入口，是新增 telemetry 派生写入的最小侵入挂载点。

`observability` 已存在，但它是 agent hook metrics 的独立 schema 和 ingestion
通道。telemetry 本需求是 security event 的派生流，因此应新增 `telemetry`
模块，而不是复用 `observability` 的 record schema。

## 模块结构

新增目录：

```text
agent-sec-cli/src/agent_sec_cli/telemetry/
├── __init__.py          # public API: get_writer(), record_security_event_telemetry()
├── config.py            # path/component metadata 配置解析
├── sanitizer.py         # actionResult -> telemetry schema 字段映射
├── schema.py            # TelemetryRecord 记录构造
└── writer.py            # TelemetryWriter, close-on-write JSONL append
```

建议职责：

| 文件 | 职责 |
| --- | --- |
| `config.py` | 解析 Agentic OS 组件文件路径、测试路径覆盖和组件 metadata |
| `sanitizer.py` | 将各能力 `actionResult` 映射为 `seccore.*` / `baseline.*` schema 字段 |
| `schema.py` | 组装包含组件固定字段和带分组前缀业务字段的 telemetry JSON record |
| `writer.py` | 将 telemetry record 以 close-on-write 方式追加到 JSONL |
| `__init__.py` | 维护 singleton writer，并暴露 best-effort 写入 API |

## 数据流

现有路径：

```text
security_middleware.lifecycle
  -> SecurityEvent(...)
  -> security_events.log_event(event)
       -> SecurityEventWriter.write(event)
       -> SqliteEventWriter.write(event)
```

新增后：

```text
security_middleware.lifecycle
  -> SecurityEvent(...)
  -> security_events.log_event(event)
       -> SecurityEventWriter.write(event)
       -> SqliteEventWriter.write(event)
       -> telemetry.record_security_event_telemetry(event)
            -> build_telemetry_security_event(event)
               -> map_action_result_to_schema(actionResult, RequestContext)
            -> TelemetryWriter.write(record)
```

集成点放在 `security_events.log_event()`，而不是 `security_middleware.lifecycle`。
原因：

- `log_event()` 是当前 security event 持久化边界，所有 security event 写入都会经过它。
- 不需要改动每个 backend。
- telemetry 与 JSONL/SQLite 一样是持久化 side effect，语义上同层。
- best-effort 异常隔离可与现有双写逻辑保持一致。

## 路径与启用条件

telemetry 不提供独立 enabled/disabled 开关。启用条件由目标组件日志文件是否存在决定：

- 目标 JSONL 文件存在：构造 telemetry record 并 best-effort 追加写入。
- 目标 JSONL 文件不存在：直接跳过 telemetry，不创建文件，不记录错误，不影响主流程。
- 写入失败：吞掉异常，不重试。

第一阶段只保留路径和 metadata 配置，避免引入新的配置文件格式。

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `AGENT_SEC_TELEMETRY_LOG_PATH` | `/var/log/anolisa/sls/ops/agent-sec-core.jsonl` | telemetry JSONL 路径；测试和本地开发可显式覆盖 |

路径解析：

1. 如果 `AGENT_SEC_TELEMETRY_LOG_PATH` 非空，使用该路径。
2. 否则使用 Agentic OS 固定组件文件：
   `/var/log/anolisa/sls/ops/agent-sec-core.jsonl`。
3. 生产环境不再通过 `AGENT_SEC_TELEMETRY_STREAM` 派生 `telemetry.jsonl`。
4. SLS ops 目录和组件文件应由注册授权模块预创建。telemetry writer 不应在生产路径
   下创建目录、修改权限或创建自有轮转文件。

## Agentic OS 组件日志规范

Agentic OS 组件日志统一写入：

```text
/var/log/anolisa/sls/ops/
├── instance.jsonl        # 设备基础信息，注册授权模块写入
├── llm.jsonl             # 模型调用信息，sight 监测写入
├── agentsight.jsonl      # agentsight 组件日志
├── agent-sec-core.jsonl  # agent-sec-core 组件日志
├── cosh.jsonl
├── tokenless.jsonl
├── ws-ckpt.jsonl
├── skillfs.jsonl
└── ...
```

目录和文件约束：

- `/var/log/anolisa/sls/ops` 已预创建，权限为 `0755`。
- 空组件日志文件已预创建，权限为 `0666`，任意用户可读写已有文件。
- `agent-sec-core` 只写
  `/var/log/anolisa/sls/ops/agent-sec-core.jsonl`。
- `instance.jsonl` 由注册授权模块写入。
- `llm.jsonl` 由 sight 监测写入。
- logrotate 策略由注册授权模块统一设置，组件只负责追加写文件。

每行 JSON object 必须包含固定组件字段：

| 字段 | 值来源 | 示例 |
| --- | --- | --- |
| `component.name` | 固定组件名 | `agent-sec-core` |
| `component.version` | 当前 `agent-sec-core` 发布版本，随包版本发布；当前版本为 `0.6.1` | `0.6.1` |
| `component.agent_name` | 当前阶段无法稳定获取，先输出空字符串；后续由 hook 调用上下文传入 | `""` |

字段命名规范：

- 字段名全部小写，禁止驼峰和大写。
- 字段名只允许 `a-z`、`0-9`、`.`、`_`、`-`，不允许空格、中文、`@`、`$`
  等字符。
- 字段名不能以数字开头，例如 `2nd_attempt` 应改为 `second_attempt`。
- 字段名不能以 `_` 开头或结尾，避免与 SLS 保留字段混淆。
- 命名空间使用 `.` 分层，与 OTel 风格一致，例如 `gen_ai.usage.input_tokens`。
- 单段内多词使用 snake_case，例如 `input_tokens`、`finish_reasons`。
- 复合组件名只用于组件标识字段值，例如 `agent-sec-core`。
- 不合规的历史字段必须在 telemetry 输出侧改名或丢弃；新增字段必须先进入
  对应 schema 字段列表。

数据分组不是独立输出字段。业务字段必须直接携带分组前缀，例如
`seccore.event_id`、`baseline.event_id`；不输出 `schema.namespace`、
`data.group` 或其它表示分组的单独字段。

组件日志示例，两行分别为两条 JSONL 记录：

```jsonl
{"component.name":"agent-sec-core","component.version":"0.6.1","component.agent_name":"","seccore.event_id":"8e2e54e7-9f1a-45f5-8b3f-9ffb9f2a3f4a","seccore.event_type":"pii_scan","seccore.category":"pii_scan","seccore.result":"succeeded","seccore.timestamp":"2026-06-15T12:00:00.000000+00:00","seccore.trace_id":"trace-123","seccore.session_id":null,"seccore.run_id":null,"seccore.call_id":null,"seccore.tool_call_id":null,"seccore.request":{"source":"manual","text":"..."},"seccore.error":null,"seccore.error_type":null,"seccore.verdict":"deny","seccore.summary":{"total":1},"seccore.elapsed_ms":28,"seccore.asset_passed_count":null,"seccore.asset_failed_count":null,"seccore.details":{}}
{"component.name":"agent-sec-core","component.version":"0.6.1","component.agent_name":"","baseline.event_id":"b58ce11b-1a2d-4d8e-8ff8-47ce2a8d1761","baseline.result":"failed","baseline.timestamp":"2026-06-15T12:00:01.000000+00:00","baseline.request":{"args":["--scan","--config","agentos_baseline"]},"baseline.error":null,"baseline.error_type":null,"baseline.passed":12,"baseline.fixed":0,"baseline.failed":1,"baseline.total":13,"baseline.details":{}}
```

## Public API

`agent_sec_cli.telemetry.__init__` 暴露：

```python
def telemetry_log_path_exists() -> bool:
    """Return whether the configured Agentic OS telemetry JSONL file exists."""


def get_writer() -> TelemetryWriter:
    """Return the module-level telemetry JSONL writer."""


def record_security_event_telemetry(event: SecurityEvent) -> None:
    """Best-effort write of a telemetry record mapped from SecurityEvent/actionResult."""
```

`record_security_event_telemetry()` 语义：

- 如果目标 telemetry JSONL 文件不存在，直接 return。
- 构造或写入失败时吞掉异常。
- telemetry 写入失败不重试。
- 不加载 SQLAlchemy。
- 不写 `security-events.jsonl` 或 `security-events.db`。

## JSONL Schema

每行一条 JSON object。记录由 Agentic OS 组件固定字段和业务字段组成。业务字段
通过字段名前缀携带分组信息：

- `seccore.*`：agent-sec-core 安全事件字段。
- `baseline.*`：baseline / harden 能力字段。

数据分组不是独立字段，因此不能输出 `schema.namespace`、`data.group` 或类似字段。

```json
{
  "component.name": "agent-sec-core",
  "component.version": "0.6.1",
  "component.agent_name": "",
  "seccore.event_id": "8e2e54e7-9f1a-45f5-8b3f-9ffb9f2a3f4a",
  "seccore.event_type": "pii_scan",
  "seccore.category": "pii_scan",
  "seccore.result": "succeeded",
  "seccore.timestamp": "2026-06-15T12:00:00.000000+00:00",
  "seccore.trace_id": "trace-123",
  "seccore.session_id": "session-123",
  "seccore.run_id": "run-123",
  "seccore.call_id": "call-123",
  "seccore.tool_call_id": "tool-123",
  "seccore.request": {
    "source": "manual",
    "text": "..."
  },
  "seccore.error": null,
  "seccore.error_type": null,
  "seccore.verdict": "deny",
  "seccore.summary": {
    "total": 1,
    "by_type": {
      "api_key": 1
    }
  },
  "seccore.elapsed_ms": 28,
  "seccore.asset_passed_count": null,
  "seccore.asset_failed_count": null,
  "seccore.details": {}
}
```

字段规则：

- `component.name` 固定为 `agent-sec-core`。
- `component.version` 来自当前 `agent-sec-core` 发布版本，随包版本发布；当前版本为
  `0.6.1`，字段必须保持存在。
- `component.agent_name` 当前阶段固定输出空字符串 `""`，字段必须保持存在；后续由 hook
  调用上下文传入稳定 agent 名称后再改造映射。
- `seccore.*` 和 `baseline.*` 前缀标识当前字段所属业务分组。
- `seccore.event_id` / `baseline.event_id` 优先沿用原始 security event，缺失时自动生成 UUID v4。
- `seccore.timestamp` / `baseline.timestamp` 优先沿用原始 security event，缺失时自动生成 UTC ISO-8601 时间戳。
- `seccore.request` / `baseline.request` 来自各能力的 `actionResult` 请求信息，可能包含 prompt、路径等敏感信息。
- `seccore.details` / `baseline.details` 是可选扩展字段，当前阶段固定输出空对象 `{}`。
- 除 `*.event_id`、`*.timestamp` 自动生成和 `*.details={}` 外，目标 schema 字段如果找不到
  source field，输出值必须为 `null`，不要省略字段。
- 输出字段名必须满足 Agentic OS 字段命名规范；不合规字段不能原样进入 telemetry。

### `seccore.*` 记录字段

| 字段 | 说明 |
| --- | --- |
| `component.name` | 固定组件名：`agent-sec-core` |
| `component.version` | 当前 `agent-sec-core` 发布版本，随包版本发布；当前版本为 `0.6.1` |
| `component.agent_name` | 当前阶段固定为空字符串 `""`，后续由 hook 调用上下文传入 |
| `seccore.event_id` | 事件唯一标识，UUID v4 自动生成 |
| `seccore.event_type` | 事件类型：`sandbox_prehook` / `verify` / `code_scan` / `prompt_scan` / `pii_scan` / `skill_ledger` |
| `seccore.category` | 事件类别：`sandbox` / `asset_verify` / `code_scan` / `prompt_scan` / `pii_scan` / `skill_ledger` |
| `seccore.result` | 执行结果：`succeeded` / `failed` |
| `seccore.timestamp` | ISO-8601 格式 UTC 时间戳，自动生成 |
| `seccore.trace_id` | 追踪 ID，来自 `RequestContext` |
| `seccore.session_id` | 会话级关联 ID，可为 `null` |
| `seccore.run_id` | Agent run/turn 关联 ID，可为 `null` |
| `seccore.call_id` | LLM 调用关联 ID，可为 `null` |
| `seccore.tool_call_id` | 工具调用关联 ID，可为 `null` |
| `seccore.request` | 安全事件请求内容，按照各能力不同可能包含 prompt、路径等敏感信息 |
| `seccore.error` | 安全事件错误详情 |
| `seccore.error_type` | 安全事件错误类型 |
| `seccore.verdict` | scan 能力判断结果 |
| `seccore.summary` | scan 能力判断总结 |
| `seccore.elapsed_ms` | 耗时 |
| `seccore.asset_passed_count` | asset verify 扫描通过数量 |
| `seccore.asset_failed_count` | asset verify 扫描未通过数量 |
| `seccore.details` | 可选扩展字段，暂时为空 |

### `baseline.*` 记录字段

`baseline.*` 用于 `harden` 能力结果映射。

| 字段 | 说明 |
| --- | --- |
| `component.name` | 固定组件名：`agent-sec-core` |
| `component.version` | 当前 `agent-sec-core` 发布版本，随包版本发布；当前版本为 `0.6.1` |
| `component.agent_name` | 当前阶段固定为空字符串 `""`，后续由 hook 调用上下文传入 |
| `baseline.event_id` | 事件唯一标识，UUID v4 自动生成 |
| `baseline.result` | 执行结果：`succeeded` / `failed` |
| `baseline.timestamp` | ISO-8601 格式 UTC 时间戳，自动生成 |
| `baseline.request` | 基线扫描或修复请求信息，可能包含敏感信息 |
| `baseline.error` | 基线扫描错误信息 |
| `baseline.error_type` | 基线扫描错误类型 |
| `baseline.passed` | 扫描项通过数量 |
| `baseline.fixed` | 扫描项修复成功数量 |
| `baseline.failed` | 扫描项修复失败或未通过数量 |
| `baseline.total` | 扫描总数量 |
| `baseline.details` | 可选扩展字段。留作云安全基线能力接入备用字段，暂时为空 |

## ActionResult 映射策略

### 总原则

sanitizer 不再做 `details` 字段筛选。它的职责是从各能力返回的
`actionResult` 中提取字段并映射到目标 schema：

- 根据能力类型选择字段前缀：`harden` 输出 `baseline.*` 字段，其它 action 输出
  `seccore.*` 字段。
- `*.event_id`、`*.timestamp` 和 `seccore.trace_id`、`seccore.session_id`、
  `seccore.run_id`、`seccore.call_id`、`seccore.tool_call_id` 优先从
  `SecurityEvent` / `RequestContext` 获取。
- `*.result` 根据 action 执行是否成功映射为 `succeeded` 或 `failed`。
- `*.request` 从 `actionResult.request` 或原始 action 入参映射，保持 JSON-safe
  结构，允许包含敏感内容。
- `*.error`、`*.error_type` 从 action 失败信息映射，成功时为 `null`。
- scan 类能力从 `actionResult.data` 映射判断结果，例如
  `code_scan.actionResult.data.verdict` 映射到 `seccore.verdict`。
- `verify` / `asset_verify` 的 `seccore.asset_passed_count` 来自
  `actionResult.data.passed`，`seccore.asset_failed_count` 来自
  `actionResult.data.failed`。
- `harden` 的 `baseline.passed`、`baseline.fixed`、`baseline.failed`、
  `baseline.total` 来自 `actionResult.data` 下同名字段。
- `seccore.details` / `baseline.details` 当前输出 `{}`，后续扩展字段只能通过显式
  schema 评审加入。
- 所有映射必须防御编程：如果 source field 不存在、结构不匹配或类型不可安全转换，
  telemetry 中对应字段置为 `null`，不能因为单个字段缺失丢弃整条记录。
- 输出对象必须 deep-copy，不能修改原始 `SecurityEvent.details` 或
  `actionResult`。
- 所有输出字段名必须满足 Agentic OS 命名规范；来自旧 schema 的不合规字段
  必须显式改名或丢弃。

### 字段映射示例

| 目标字段 | 来源 |
| --- | --- |
| `seccore.event_id` / `baseline.event_id` | `SecurityEvent.event_id`；缺失时生成 UUID v4 |
| `seccore.event_type` | `SecurityEvent.event_type` 或 action name |
| `seccore.category` | `SecurityEvent.category` 或 action category |
| `seccore.result` / `baseline.result` | `SecurityEvent.result` / action success flag |
| `seccore.timestamp` / `baseline.timestamp` | `SecurityEvent.timestamp`；缺失时生成 UTC ISO-8601 |
| `seccore.trace_id` | `RequestContext.trace_id` |
| `seccore.session_id` | `RequestContext.session_id` |
| `seccore.run_id` | `RequestContext.run_id` |
| `seccore.call_id` | `RequestContext.call_id` |
| `seccore.tool_call_id` | `RequestContext.tool_call_id` |
| `seccore.request` / `baseline.request` | `actionResult.request` / action input |
| `seccore.error` / `baseline.error` | `actionResult.error` |
| `seccore.error_type` / `baseline.error_type` | `actionResult.error_type` 或异常类型名 |
| `seccore.verdict` | `actionResult.data.verdict` |
| `seccore.summary` | `actionResult.data.summary` |
| `seccore.elapsed_ms` | `actionResult.data.elapsed_ms` |
| `seccore.asset_passed_count` | `verify.actionResult.data.passed` |
| `seccore.asset_failed_count` | `verify.actionResult.data.failed` |
| `baseline.passed` | `harden.actionResult.data.passed` |
| `baseline.fixed` | `harden.actionResult.data.fixed` |
| `baseline.failed` | `harden.actionResult.data.failed` |
| `baseline.total` | `harden.actionResult.data.total` |

## Writer 设计

`TelemetryWriter` 采用 close-on-write JSONL writer。它可以通过扩展
`JsonlEventWriter` 增加 `rotation_enabled=False` 能力实现，也可以单独实现一个
轻量 writer；关键约束是 Agentic OS SLS ops 组件文件不能由 `agent-sec-core`
自行轮转。

```python
class TelemetryWriter:
    def __init__(
        self,
        path: str | Path | None = None,
    ) -> None:
        self._path = Path(path or get_telemetry_log_path())
        self._lock = threading.Lock()

    def write(self, record: Mapping[str, Any]) -> None:
        with self._lock:
            try:
                if not self._path.exists():
                    return
                line = json.dumps(record, ensure_ascii=False) + "\n"
                self._append_line(line)
            except Exception as exc:  # noqa: BLE001
                _log_telemetry_write_failure(exc)

    def _append_line(self, line: str) -> None:
        # Open by path for every write and close before returning.
        fd = os.open(self._path, os.O_WRONLY | os.O_APPEND | os.O_CLOEXEC)
        with os.fdopen(fd, "a", encoding="utf-8") as fh:
            fh.write(line)
            fh.flush()
```

写入语义：

- 每条记录以单次 append 写入 JSONL。
- 写入前检查目标文件是否存在；不存在则直接跳过。
- 每次写入都按路径重新打开文件，写入完成后立即关闭句柄。
- 失败不抛出，所有异常只进入 best-effort diagnostic logger。
- 写入失败不重试。
- 不调用 `write_or_raise()`，不向 caller 暴露写入失败。
- 不使用 `O_CREAT` 创建目标文件；目标文件缺失时本次 telemetry 写入 fail-open。
- 不在 `/var/log/anolisa/sls/ops` 下创建 `.lock`、backup 或临时轮转文件。该目录
  权限为 `0755`，组件进程只应依赖已存在且 `0666` 的目标 JSONL 文件。
- 如果需要跨进程互斥，只能在本次打开的文件句柄上做短生命周期 advisory lock，
  并且必须在同一次写入结束前释放和关闭。
- logrotate 以 rename 方式轮转后，下一次写入必须重新解析路径并写入新文件；
  不能持有长期文件句柄。

## 与 security_events 集成

修改 `agent_sec_cli.security_events.__init__.log_event()`：

```python
def log_event(event: SecurityEvent) -> None:
    try:
        get_writer().write(event)
    except Exception:
        pass

    try:
        get_sqlite_writer().write(event)
    except Exception:
        pass

    try:
        from agent_sec_cli.telemetry import record_security_event_telemetry

        record_security_event_telemetry(event)
    except Exception:
        pass
```

注意：

- 可以使用函数体内 import 避免 `security_events` import 时加载 telemetry，但当前 ruff
  禁止函数体内导入。实现时可选择文件顶部导入轻量 telemetry API，前提是 telemetry
  import 不加载重模块。
- 如果使用函数体内 import，需要按项目规则增加明确豁免，或避免此方案。
- `security_events` 包导入测试当前要求不加载 SQLAlchemy；telemetry 集成不能破坏该性质。

推荐实现：

```python
from agent_sec_cli.telemetry import record_security_event_telemetry
```

并确保 `agent_sec_cli.telemetry` 顶层只导入轻量模块。

## 错误处理

所有 telemetry 路径都必须 fail-open：

| 阶段 | 错误 | 行为 |
| --- | --- | --- |
| 路径解析 | 非法路径 | 回退默认 telemetry 路径，best-effort warning |
| schema 构造 | 非 JSON-safe 值、缺少组件版本、缺少 agent name | 转成 JSON-safe；`component.version` 使用当前发布包版本，`component.agent_name` 使用空字符串 |
| 字段命名校验 | 输出字段名不满足 Agentic OS 规范 | 显式改名或丢弃字段 |
| actionResult 映射 | 未知 action 类型、未知结构、字段类型不匹配 | 映射已知字段，缺失 source field 对应目标字段置为 `null` |
| JSONL 文件检查 | 目标文件缺失 | 跳过 telemetry 写入，不创建文件，不记录错误 |
| JSONL 写入 | logrotate rename/create 间隙、磁盘满、权限不足、序列化失败 | 吞掉异常，不重试，不影响 caller |

telemetry write failure 可通过 `agent_sec_cli` logger tree 记录到 diagnostic stream，
但不能回写 security event，避免递归。

## 性能和并发

- 每个 security event 最多额外做一次 actionResult 字段映射和一次 JSONL append。
- 目标 telemetry 文件存在时会增加一次按路径 exists check、open、append、flush、close。
- 如果实现短生命周期 advisory lock，锁必须只覆盖本次写入，并随文件句柄关闭释放。
- 当前 CLI 多为短生命周期进程，这一同步开销可接受。
- 如果 daemon 高频写入成为瓶颈，再评估异步队列或批量写入。
- 不允许为了吞吐持有长期文件句柄；logrotate 兼容性优先于单次写入微优化。

## 安全与隐私

1. 不维护独立 telemetry 开关；是否写入由目标文件是否存在决定。
2. telemetry 文件默认位于 `/var/log/anolisa/sls/ops`，组件文件权限为 `0666`。
   这意味着 telemetry 内容必须按可被本机任意用户读取来处理。
3. `seccore.request` / `baseline.request` 字段按 schema 要求可能包含 prompt、路径等敏感内容；
   预置组件文件和启用 SLS 采集链路前必须确认该数据面符合安全要求。
4. sanitizer 不再承担脱敏职责，它负责 `actionResult` 到 schema 的结构化映射。
5. 所有输出字段名必须满足 Agentic OS 命名规范，避免 SLS 字段解析歧义。
6. 保持 `seccore.event_id` / `baseline.event_id` 和 correlation IDs，便于回查原始
   security event。

## 测试计划

新增测试目录：

```text
tests/unit-test/telemetry/
├── test_config.py
├── test_sanitizer.py
├── test_schema.py
└── test_writer.py
```

修改：

```text
tests/unit-test/security_events/test_log_event.py
```

覆盖项：

1. 目标 telemetry JSONL 文件不存在时跳过写入，不创建文件。
2. 目标 telemetry JSONL 文件存在时写入 telemetry JSONL。
3. 未设置 `AGENT_SEC_TELEMETRY_LOG_PATH` 时默认路径为
   `/var/log/anolisa/sls/ops/agent-sec-core.jsonl`。
4. `AGENT_SEC_TELEMETRY_LOG_PATH` 可覆盖路径，供测试和本地开发使用。
5. telemetry record 总是包含 `component.name`、`component.version`、
   `component.agent_name`。
6. `component.name` 固定为 `agent-sec-core`。
7. `component.version` 跟随当前发布包版本；当前版本为 `0.6.1`。
8. `component.agent_name` 当前固定为空字符串 `""`，后续由 hook 调用上下文传入。
9. `seccore.*` 和 `baseline.*` 两类记录都包含三个 `component.*` 固定字段。
10. telemetry record 不包含 `schema.namespace`、`data.group` 或其它独立分组字段。
11. `sandbox_prehook` / `verify` / `code_scan` / `prompt_scan` / `pii_scan` /
   `skill_ledger` 映射到合法的 `seccore.event_type` 和 `seccore.category`。
12. `seccore.event_id` / `baseline.event_id`、`seccore.timestamp` /
   `baseline.timestamp` 缺失时自动生成；存在时沿用原始值。
13. `seccore.trace_id`、`seccore.session_id`、`seccore.run_id`、`seccore.call_id`、`seccore.tool_call_id` 从
    `RequestContext` / `SecurityEvent` 映射，可为 `null`。
14. `seccore.request` / `baseline.request` 从 `actionResult.request` 或 action input 映射并保持 JSON-safe，允许包含 prompt、路径等内容。
15. `seccore.error` / `baseline.error`、`seccore.error_type` /
    `baseline.error_type` 从失败结果映射；成功时为 `null`。
16. scan 类能力从 `actionResult.data` 映射 `seccore.verdict`、`seccore.summary`、
    `seccore.elapsed_ms`，例如 `code_scan.actionResult.data.verdict -> seccore.verdict`。
17. `verify` 的 `seccore.asset_passed_count` 映射自 `actionResult.data.passed`，
    `seccore.asset_failed_count` 映射自 `actionResult.data.failed`。
18. `harden` 映射到 `baseline.*` 字段，不输出 `seccore.*` 业务字段。
19. `harden.actionResult.data.passed/fixed/failed/total` 映射到
    `baseline.passed`、`baseline.fixed`、`baseline.failed`、`baseline.total`。
20. 任一 source field 缺失、结构不匹配或类型不可安全转换时，对应 telemetry
    字段置为 `null`。
21. `seccore.details` / `baseline.details` 当前为空对象 `{}`。
22. 输出字段名不包含大写、驼峰、中文、空格、`@`、`$`，且不以数字或 `_` 开头。
23. telemetry writer 每次写入都会关闭文件句柄；模拟 logrotate rename 后不会继续写旧文件。
24. telemetry writer 不使用 `O_CREAT` 创建缺失的目标 JSONL 文件。
25. telemetry writer 不创建 `.lock`、backup 或自有轮转文件。
26. telemetry writer 失败不影响 security event JSONL 和 SQLite writer 调用。
27. telemetry writer 失败只尝试一次，不重试。
28. `import agent_sec_cli.telemetry` 不加载 SQLAlchemy。
29. `import agent_sec_cli.security_events` 仍不加载 SQLAlchemy。

建议针对 `log_event()` 新增断言：

- JSONL 失败不阻断 SQLite，也不阻断 telemetry。
- SQLite 失败不阻断 JSONL，也不阻断 telemetry。
- telemetry 失败不阻断 JSONL 和 SQLite。

## 兼容性

- 原 `security-events.jsonl` schema 不变。
- 原 `security-events.db` schema 不变。
- `SecurityEvent` schema 不变。
- 未预置目标 telemetry JSONL 文件时默认行为不变。
- 预置目标 telemetry JSONL 文件后新增的 side effect 是向
  `/var/log/anolisa/sls/ops/agent-sec-core.jsonl` 追加一条 JSONL 记录，或写入
  `AGENT_SEC_TELEMETRY_LOG_PATH` 指定的测试/本地路径。
- Agentic OS 组件日志不使用 agent-sec-core 自有 size-based rotation。

## 实施步骤

1. 新增 `telemetry.config`，实现 path/component metadata 配置解析。
2. 新增 `telemetry.sanitizer`，实现 `actionResult` 到
   `seccore.*` / `baseline.*` schema 的字段映射。
3. 新增 `telemetry.schema`，实现 `build_telemetry_security_event(event)`。
4. 新增 `telemetry.writer`，实现 close-on-write JSONL append，不做内部轮转。
5. 新增 `telemetry.__init__` public API 和 singleton writer。
6. 在 `security_events.log_event()` 增加 telemetry best-effort 写入。
7. 补充单元测试。
8. 运行相关测试：

```bash
uv run --project agent-sec-cli pytest \
  tests/unit-test/telemetry/ \
  tests/unit-test/security_events/test_log_event.py \
  tests/unit-test/security_events/test_writer.py \
  tests/unit-test/security_middleware/test_lifecycle.py \
  -v
```

## 验收标准

- 目标 telemetry JSONL 文件不存在时不写入、不创建文件，现有测试行为不变。
- 目标 telemetry JSONL 文件存在时，每次 `security_events.log_event(SecurityEvent(...))`
  都会 best-effort 向 Agentic OS 组件 JSONL 追加一条 telemetry 记录。
- telemetry JSONL 默认路径为 `/var/log/anolisa/sls/ops/agent-sec-core.jsonl`。
- telemetry writer 每次写入都会重新打开并关闭目标文件句柄，不持有长期 fd。
- telemetry writer 不使用 `O_CREAT` 创建缺失的目标 JSONL 文件。
- telemetry writer 不创建 `.lock`、backup 或自有轮转文件。
- telemetry record 包含 `component.name`、`component.version`、
  `component.agent_name`。
- `component.name` 固定为 `agent-sec-core`。
- `component.version` 跟随当前发布包版本；当前版本为 `0.6.1`。
- `component.agent_name` 当前固定为空字符串 `""`，后续由 hook 调用上下文传入。
- `seccore.*` 和 `baseline.*` 两类记录都包含三个 `component.*` 固定字段。
- telemetry record 不包含 `schema.namespace`、`data.group` 或其它独立分组字段。
- telemetry record 的业务字段只使用 `seccore.*` 或 `baseline.*` 前缀。
- `harden` 映射到 `baseline.*`，其它 action 映射到 `seccore.*`。
- sanitizer 从 `actionResult` / `actionResult.data` 映射 `*.request`、`*.error`、
  `seccore.verdict`、`seccore.summary`、`seccore.elapsed_ms`、asset count 和 baseline count 字段。
- 映射 source field 不存在时，对应 telemetry 字段置为 `null`。
- telemetry record 所有输出字段名满足 Agentic OS 字段命名规范。
- telemetry record 可通过 `seccore.event_id` / `baseline.event_id` 与原始 security event 关联。
- telemetry `seccore.details` / `baseline.details` 当前固定为空对象 `{}`。
- telemetry 任意失败不重试，不改变 CLI exit code，不影响 security event JSONL/SQLite 写入。
- 新增/修改代码满足 ruff 规则，包括类型注解、绝对导入、无函数体内 import。

## 后续扩展

- 支持为 `seccore.details` / `baseline.details` 增加经过 schema 评审的扩展字段。
- 支持 `findings_count`、`findings_by_severity` 等派生摘要字段。
- 支持 telemetry SQLite 或远端 exporter。
- 支持 daemon 模式下的 request-aware telemetry 配置。
- 支持采样率和类别过滤，例如只记录 `*.result=failed` 或特定 `seccore.category`。
