# Hermes Plugin — Agent-Sec-Core

Hermes Agent 安全插件，基于 `agent-sec-cli` 提供 OS 级安全防护能力。

## 架构概述

```
src/                          # 运行时文件（部署到 ~/.hermes/plugins/）
├── plugin.yaml               # Hermes 插件 manifest
├── __init__.py               # register(ctx) 入口
├── config.toml               # 能力开关与参数
├── registry.py               # 能力注册器 + safe-wrap
├── cli_runner.py             # agent-sec-cli subprocess 封装
└── capabilities/
    ├── __init__.py           # 能力清单
    └── code_scan.py          # Code Scanner 实现
```

采用 **capability 分层模式**：每个安全能力是独立模块，实现 `SecurityCapability` Protocol，
通过 `config.toml` 控制开关，`registry.py` 统一注册。

## 如何新增一个 Capability

### 1. 创建能力文件

在 `src/capabilities/` 下新建 `my_capability.py`：

```python
"""My new security capability."""
import logging

from cli_runner import call_agent_sec_cli
from registry import safe_hook_wrapper

logger = logging.getLogger("agent-sec-core")


class MyCapability:
    id = "my-capability"
    name = "My Capability"
    hooks = ["pre_tool_call"]  # 声明使用的 hook（仅日志用）

    def register(self, ctx) -> None:
        wrapped = safe_hook_wrapper(self._on_pre_tool_call, self.id)
        ctx.register_hook("pre_tool_call", wrapped)

    def _on_pre_tool_call(self, tool_name, args, **kwargs):
        # 实现逻辑...
        return None  # None = 放行
```

### 2. 导出能力

在 `src/capabilities/__init__.py` 中添加：

```python
from capabilities.my_capability import MyCapability

ALL_CAPABILITIES = [
    CodeScanCapability(),
    MyCapability(),  # 新增
]
```

### 3. 添加配置

在 `src/config.toml` 中添加：

```toml
[capabilities.my-capability]
enabled = true
```

## 可用 Hook 列表

Hermes 支持的 hook 及其回调签名：

| Hook | 签名 | 返回值 |
|------|------|--------|
| `pre_tool_call` | `(tool_name, args, **kwargs)` | `None` 放行 / `{"action": "block", "message": str}` 阻断 |
| `post_tool_call` | `(tool_name, params, result)` | 观测用，返回值忽略 |
| `pre_llm_call` | `(messages, **kwargs)` | `{"context": str}` 注入上下文 / `None` |
| `post_llm_call` | `(messages, response, **kwargs)` | 观测用 |
| `on_session_start` | `(**kwargs)` | 观测用 |
| `on_session_end` | `(**kwargs)` | 观测用 |
| `transform_tool_result` | `(tool_name, result, **kwargs)` | 修改后的 result / `None` |

完整列表参见 [Hermes 官方文档](https://hermes-agent.nousresearch.com/docs/zh-Hans/user-guide/features/plugins)。

## 开发与调试

### 本地测试

```bash
# 运行单元测试
cd agent-sec-core
uv run --project agent-sec-cli pytest tests/hermes-plugin/ -v
```

### 部署到本地 Hermes

```bash
# 从源码目录直接部署
./hermes-plugin/scripts/deploy.sh
```

deploy.sh 会自动推导 `src/` 路径并复制到 `~/.hermes/plugins/agent-sec-core-hermes-plugin/`。

## 注意事项

1. **Fail-open 原则** — 任何异常都不应阻塞 agent 运行。hook 内部捕获所有异常，返回 `None` 放行。
2. **零运行时依赖** — 仅使用 Python 3.11 标准库（tomllib、json、subprocess、logging、dataclasses）。RPM 分发不携带额外 pip 包。
3. **性能要求** — `pre_tool_call` 在热路径上同步执行。`cli_runner` 设置严格超时（默认 10s），超过 2s 的 hook 会记录慢日志告警。
4. **日志** — 使用 `logging.getLogger("agent-sec-core")`，Hermes 会自动捕获到 `~/.hermes/logs/agent.log`。
5. **导入方式** — Hermes 以包形式加载插件，因此模块间使用**相对导入**：

   ```python
   # 正确：相对导入
   from .registry import load_config              # 同级模块
   from .capabilities import ALL_CAPABILITIES     # 同级子包
   from ..cli_runner import call_agent_sec_cli    # 上级模块（在子包中）

   # 错误：裸名导入（插件目录不在 sys.path）
   # from registry import load_config
   ```

   依赖分层（无循环依赖）：
   - 底层：`cli_runner.py`（纯 stdlib，无内部依赖）
   - 中间层：`registry.py`（纯 stdlib）
   - 实现层：`capabilities/*.py`（依赖 cli_runner、registry）
   - 顶层：`__init__.py`（依赖 capabilities、registry）
