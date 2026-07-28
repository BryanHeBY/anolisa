# ADR-011: ACP 统一 Agent 接入架构

日期：2026-07-27

相关文档：[ADR-012](ADR-012-agent-boundary-ownership.md)

## 背景

cosh-shell 目前通过 CLI stream-json 加一套私有控制协议接入 Agent 后端，存在结构性成本：

- 每接入一个新 Agent 都需要专属流解析器。Claude 解析器家族同时被 cosh-core 适配器复用，
  Claude 特有的帧格式事实上成了内部线协议；不输出 stream-json 的 Agent 完全无法接入。
- 会话身份（`--resume`、`--workspace`、`--approval-mode`）被编码为进程 spawn 参数。所谓
  持久 cosh-core runtime 实际是一个以 `(approval_mode, workspace, session_id)` 为 key 的
  进程缓存：key 任一变化即杀进程重启，会话连续性依赖磁盘重放而非进程内存。
- 每轮同步 spawn 与持久服务两条并行代码路径把同一套 turn 语义实现了两遍。
- Agent Client Protocol（ACP）已是发布的标准协议，具备官方 Rust SDK、强制的 stdio MCP
  传输、结构化 `session/update` 事件流、terminal 执行委托，以及 capability 门控的
  session load/resume/close。

## 决策

### 唯一协议脊柱

ACP 成为唯一的 Agent 接入协议。新增 `cosh-acp` crate，一个二进制两种形态：

- `cosh-acp bridge`：基于 tokio 的 ACP 客户端，是全项目唯一理解 ACP 的组件。cosh-shell
  保持纯同步，通过版本化的内部 JSONL 协议经 stdin/stdout 与桥通信（reader 线程 + mpsc +
  poll，复用现有 provider 进程基础设施）。
- `cosh-acp mcp-shell`：注入给 Agent 的 stdio MCP server（见 ADR-012）。

cosh-core 新增 `--acp` server 模式，与现有 headless JSONL 模式并列（后者保留给外部消费
者）。终态 shell 侧只有两个适配器：`Fake`（测试）与 `Acp`（其余一切，含 cosh-core）。

### 参数分层

- argv 只决定进程形态（`bridge`、`mcp-shell`）。
- 第一条 JSONL 消息（`initialize`）承载全部结构化配置：协议版本、agent 启动 spec、
  workspace cwd、MCP server spec、capability 请求、locale。
- 秘密只经 spawn 时环境变量传递，永不进 argv（`/proc/*/cmdline` 全局可读），永不进日志。

### 双车道命令执行器

Agent 请求的命令（`terminal/create`）统一路由到 cosh-shell：

- 后台车道（默认）：隐藏 PTY，多 terminal 并发，输出流式回传并带字节上限，完整映射
  terminal 生命周期（create/output/wait_for_exit/kill/release）。
- 前台车道（例外）：交互式命令与用户主动选择的执行，经现有 shell handoff 机制在用户
  PTY 中串行执行。
- `assess_shell_command` 安全门不变。审批卡排队：同一时刻至多一张。
- 取消为三级联动：`session/cancel` 通知、shell 侧 kill 活跃 terminal、桥侧对 agent
  进程组 SIGTERM/SIGKILL。

前台车道是所有交互式流程的指定逃生舱（agent 登录命令、TUI 向导等）；新交互类型必须
复用它，而不是新增协议消息。

### 信任分层

ACP 标准化的是协作而非约束，无法禁止 agent 本地执行。因此强制力分层并诚实声明：

- Tier 1（强制）：cosh-core。`--acp` 模式下检测到 client terminal capability 时禁用
  自身 shell 工具，执行一律路由到 `terminal/create`。
- Tier 2（观测）：经验证会把执行路由到 terminal 或 request_permission 的第三方 agent。
- Tier 3（警示）：其余 agent；启动 spec 注入该 agent 自身最严格的 permission/sandbox
  配置，UI 明示审计覆盖降级。

配套机制：

- 受审计执行入口。原定的蜜罐策略——主动声明 terminal capability，让受审计路径成为
  agent 最顺手的路径——已被实测证伪：LLM 的工具列表由 agent 侧决定，client 声明能力
  不会让 agent 多出一个工具，实测的两个第三方 agent 均无 `terminal/*` 实现（证据见
  ADR-012）。因此对不支持委托的 agent，改由 `session/new` 注入 MCP 工具
  `cosh_terminal`，这是 ACP 中 client 唯一能改变 agent 工具列表的口子；
  `terminal/create` 仍是支持它的 agent 的首选路径。
- 进程树哨兵。桥观测 agent 的直接子进程，出现 MCP proxy 之外的进程即产生
  `agent_local_exec` 审计事件。仅对 Tier 2/3 启用：cosh-core 本身就会 spawn hook、
  扩展与压缩器，且其自有审计已记录所执行的命令，对它启用只会产生噪音。哨兵是告警
  信号而非强制边界——短命命令可能落在采样间隔之间。
- 启动 spec 预留按 tier 配置的 OS 沙箱钩子，首期不实现。

### 生命周期模型

三条生命周期解耦：

- 进程生命周期：`cosh-core --acp`（及任何 agent）是长驻 server，argv 中不含任何会话
  身份。进程重启对会话连续性不可见。
- 会话生命周期：经 ACP `session/new` / `session/load` 创建与恢复；approval mode 是每轮
  prompt 的协议字段；进程存活期间内存 transcript 是权威。
- 持久化生命周期：session store 降级为 write-behind journal，只服务崩溃恢复、跨重启
  resume 与 shell 侧会话账本。落盘成功仍是 turn 的 commit 点。store 增加 advisory
  单写者锁。

shell 侧状态机：`NotSpawned → Spawning → Ready ⇄ Busy`，附 idle 超时回收、崩溃后退避
重启 + `session/load`，registry/扩展变更以进程重启为统一 reload 边界。

## 迁移

旁路优先：每个阶段现有路径保持可用，删除放在最后。

- S0：本 ADR 与 ADR-012。
- S1：`cosh-acp` crate 骨架、JSONL 协议 v1、shell 侧 `AcpAdapter`（仅新增枚举变体）、
  scripted fake ACP agent 测试。Gate：流式/取消/审批/崩溃重启通过；零回归。
- S2：shell 侧 Evidence Service（unix socket）与 `cosh-acp mcp-shell`。Gate：token
  fail-closed PoC；工具 schema 冻结。
- S3：双车道执行器。Gate：并发 terminal、三级取消、tokenizer 安全门回归变体。
- S4：`cosh-core --acp` 模式、`_cosh/*` 扩展、store 锁。Gate：与现有 cosh-core 路径
  逐项对照的六通路 parity 清单（流式、审批、取消、resume、auth、evidence）。
- S5：`adapter_default = "acp"`、第三方 agent 配置、哨兵、崩溃恢复、会话账本；环境
  变量一键回退开关。Gate：观察期内未被迫启用回退。
- S6：删除每轮同步 spawn 路径、进程缓存 reset 逻辑、qwen/claude 直连适配器、Claude
  流解析器、shell 侧控制协议；slash 层 `AdapterInstance::CoshCore` 特判替换为
  capability 查询。Gate：布局检查无新增 violation group、`clippy --all-targets` 干净、
  workspace 测试计数核对。

## 影响

- 新 agent 仅需配置即可接入；协议演进被隔离在 `cosh-acp` 内。
- cosh-core 可被任何 ACP 客户端使用，与其"Agent 生态执行后端"的定位一致。
- shell 保持唯一策略执行点，安全声明从不可验证的"无法绕过"改为显式的分层保证。
- 代价：进程链多一个二进制、需维护一个桥 crate、S6 之前存在协议共存过渡期。
