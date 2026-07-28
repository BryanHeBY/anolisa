# ADR-012: Agent 边界归属

日期：2026-07-27

相关文档：[ADR-011](ADR-011-acp-unified-agent-integration.md)

## 背景

ADR-011 确立了 ACP 协议脊柱、`cosh-acp` 桥与双车道执行器。其余横切关切——认证、取证
访问、用户提问、审计、registry、配置传递与兼容性——每一项都需要唯一的 owner 和唯一的
通道。若不显式归属，这些关切在历史上会不断长出 provider 特异的旁路通道。

## 决策

### 平面判据

任何能力在实现前先归入两个平面之一，判据是机械的：

| 平面 | 通道 | 准入标准 | 成员 |
| --- | --- | --- | --- |
| 数据面 | MCP → unix socket | 幂等、可重试、UI 不可见、无 turn 耦合 | shell 取证工具、未来的只读 shell 状态 |
| 会话面 | ACP（标准方法或 `_cosh/*` 扩展） | 触碰 UI、用户注意力或 turn 生命周期 | ask_user、auth、permission、terminal |

不满足数据面标准的能力一律不得进入 MCP server，无论实现上多么方便。特别地，自由文本
提问永不进 MCP：不受信 agent 借原生外观 UI 发起自由文本输入会构成钓鱼面。

### 命令执行的例外：`cosh_terminal` 工具

命令执行是本判据唯一的有条件例外，理由是实测证伪了 ADR-011 的蜜罐前提。

ACP 的 `terminal/*` 是 agent 主动调用的 client 侧 RPC，LLM 的工具列表完全由 agent 侧
决定，client 无权注入。实测两个第三方 agent（qoder-cli 1.1.5、hermes-agent 0.18.2）：
即便 `initialize` 声明了 `terminal: true`，两者都用自带 shell 工具执行、全程未发出任何
`terminal/*` 调用；hermes 的 ACP 适配器（5282 行）中 `terminal/create` 出现 0 次，
qoder-cli 的模型自述"我没有 terminal/create，只有 Bash"。所以"声明能力即引导 agent
走受审计路径"没有落点：我们声明什么、定义什么 `_cosh/*` 扩展方法都不会被调用。

ACP 规范中 client 唯一能改变 agent 工具列表的口子是 `session/new` 的 `mcpServers`
字段——hermes 明确实现了它并据此重建工具列表。因此受审计执行要覆盖第三方 agent，只有
一条通路：把执行作为 MCP 工具 `cosh_terminal` 注入。

准入条件（全部必须满足，缺一即退回 Tier 3）：

- 工具由 shell 自有的 socket 服务承载，不由 MCP proxy 实现任何策略。proxy 保持无状态。
- 调用同步阻塞至审批与执行完成，审批卡由 shell 发起并保持"一条命令一张卡"不变量。
  MCP 侧无限期等待的风险由超时上界与显式取消覆盖，不靠 agent 配合。
- 命令走既有双车道执行器与安全门，与 `terminal/create` 完全同一条代码路径，不新增
  第二套执行语义。
- 审计记录仍以 shell 侧为权威，与 ACP 路径的记录形状一致。

已知代价：MCP 调用不携带 ACP 的 session/turn 身份，关联 id 必须经工具参数与 spawn 时
环境显式传入，比 `terminal/create` 天然带 `sessionId` 更脆。turn 取消需要 shell 侧按
关联 id 主动终止在飞的 socket 请求，而不能依赖 ACP 的 `session/cancel` 传导。这两点是
本例外的直接成本，接受它是为了让 Tier 2/3 agent 的执行进入审计视野。

`terminal/create` 仍是首选路径：agent 支持它时优先使用，`cosh_terminal` 只对不支持
委托的 agent 注入，避免同一 agent 出现两条执行入口。

### 取证：MCP 封装 + shell 自有 socket 服务

- cosh-shell 在 unix socket 上承载 Evidence Service
  （`$XDG_RUNTIME_DIR/cosh/<shell_session>/evidence.sock`，目录 0700，socket 0600）。
- MCP server 命名为 `cosh-shell`，由 `cosh-acp mcp-shell` 提供：一个无状态代理，对
  agent 说 stdio MCP，对 shell 说 socket 协议。它聚合所有只读 shell 状态工具（首期为
  `list_shell_commands`、`read_command_output`、`get_command_context`）。
- 注入使用 ACP 稳定协议中 `session/new` / `session/load` / `session/resume` 的
  `mcpServers` 字段；stdio 传输是所有 ACP agent 的强制项。MCP-over-ACP 在其稳定前不
  使用；稳定后只有 `cosh-acp` 需要改动（自己成为 socket 客户端），工具 schema、socket
  协议与 cosh-shell 均不变。
- 访问控制：每 session 一次性 token 经环境变量传递，`SO_PEERCRED` 校验同 uid，session
  结束吊销 token。redaction 在 Evidence Service 出口统一执行；proxy 与 agent 均不受
  信。无有效 token 的连接 fail-closed。
- proxy 在 stdin EOF 或 socket 断开任一发生时退出，生命周期与双亲绑定。
- MCP 工具 schema 是对外契约（只允许增量变更）；socket 协议是内部契约，独立版本化。
- 降级：agent 未能连接 MCP 时取证工具消失，但基线 prompt 上下文（内联的失败上下文）
  不受影响。

### 认证：凭据跟随其所有者

- cosh-core（两端可控）：`_cosh/auth_challenge` ACP 扩展方法，承载现有的 provider/
  字段结构（secret、required、placeholder）与校验重试循环。shell 登录卡片不变。
- 声明 `authMethods` 的第三方 agent：标准 `authenticate(methodId)`，渲染为方法选择
  卡。交互式登录（OAuth、device code）作为前台车道命令在用户 PTY 中完成。
- 纯 env 型 agent：凭据在 `[acp.agents.<name>.env]` 中以不透明字符串配置，spawn 时
  注入，cosh 不解析、不校验。
- 规则：持久化归凭据所有者（cosh-core 自持配置；agent 自管；cosh 只记录所选方法）。
  secret 字段值在桥、journal、审计与 tracing 中一律打码。有界重试耗尽后以可恢复错误
  结束 turn，而不是挂在登录卡上。

### 审计：shell 侧记录为权威

审批、执行与取证访问对所有 tier 均由 cosh-shell 记录。关联 id（session、request、
terminal）经 ACP `_meta` 端到端透传，Tier 1 的 core 侧审计可据此 join。Tier 2/3 agent
只有 shell 侧记录，辅以进程树哨兵事件（`agent_local_exec`）。

### registry 不过桥

`/skills`、`/hooks`、`/extensions` 保持为 cosh-shell 与 `cosh-core --registry` 单次
调用之间的控制面。slash 层按 capability 门控，其他 agent 不可用。需要生效的 registry
变更在下一个 turn 边界触发 agent 进程重启（ADR-011 生命周期模型），不做进程内热重载。

### 数据出境与环境

- prompt 构造与 Evidence Service 是 shell 数据仅有的两个出境点，均应用 redaction。
  Tier 3 agent 可配置低证据模式（只给 exit code 与命令名，不给输出全文）。
- `session/new` 的 cwd 仅是初始值；每次 `terminal/create` 以用户当前 cwd 解析执行。
  为 agent 执行的命令使用环境变量白名单，排除 `COSH_*` 内部变量与无关的用户秘密。

### 错误与兼容

- 桥把 JSON-RPC 与进程故障映射为稳定的 cosh 错误码，附 `recoverable` 与 `hint` 字段；
  agent stderr 只进日志。`agent_failed` 区分可重试的崩溃与不可重试的协议不兼容。
- 三份契约独立版本化：内部 JSONL 协议（shell 与桥锁步出货，握手仍声明版本）、ACP
  （SDK 协商，方法按声明的 capability 门控）、MCP schema 与 socket 协议。

## 影响

- 每个关切有唯一 owner：执行与审批在 cosh-shell，只读取证在 MCP 之后的 Evidence
  Service，凭据在其所属后端，翻译与错误归一只在 `cosh-acp`，registry 走直连控制通道。
- 平面判据为未来能力提供机械化的准入测试，防止旁路通道再度累积。
- 代价：shell 需维护 socket 服务与 token 生命周期；启用第三方后端前需完成逐 agent 的
  信任分层评定。
