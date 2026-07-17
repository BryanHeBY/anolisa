# Skill Ledger 与 SkillFS Canonical Path 及 Runtime Activation 接口

本文定义 Skill Ledger 与 SkillFS 之间的双向集成合同：SkillFS 负责维护
canonical path 到 live source 的映射并提供只读 resolver，Skill Ledger 负责安全账本、
扫描、决策和 activation。双方都不把 backing root 当作用户身份或配置对象。

本合同需要 SkillFS resolver provider、SkillFS notify v2 sender 与 Skill Ledger v2 consumer
协同部署。旧版 SkillFS 只发送 notify v1，不能直接与只接受 v2 的 Ledger 配合。

## 核心路径模型

统一模型如下：

```text
身份、配置、输出：canonicalSkillDir
实际文件操作：    ioSkillDir
展示名称：        skillName = basename(canonicalSkillDir)
```

| 名称 | 所有者 | 语义 |
| --- | --- | --- |
| `canonicalSkillDir` | SkillFS 与 Skill Ledger 的接口合同 | 用户和宿主看到的绝对、词法规范化 skill 路径；是配置、事件合并和结果输出的唯一权威身份 |
| `liveSkillDir` | SkillFS resolver 返回值 | daemon 或 CLI 当前可访问、可读写的 live source；可能位于 backing root |
| `ioSkillDir` | Skill Ledger 内部 | 本次操作实际使用的目录；有 SkillFS 时等于 `liveSkillDir`，host 模式下等于 `canonicalSkillDir` |
| `skillId` | SkillFS notify metadata | 可选 opaque 诊断信息，例如 Hermes 的 `apple/apple-notes`；Ledger 不用它解析路径、去重或做决策 |
| `skillName` | Skill Ledger manifest v1 与展示层 | `canonicalSkillDir` 的 basename；不是唯一身份，允许不同 canonical path 具有相同 basename |

`canonicalSkillDir` 只做绝对化和词法规范化，不通过 `realpath` 跟随 symlink，也不要求在 daemon
namespace 中可见。若配置的 SkillFS source 本身是 symlink，canonical identity 必须保留该 source
路径前缀。SkillFS 与 Hermes hook 可以另外使用 realpath 做 mount 判定、backing root 校验和越界
检查，但不能把 realpath 结果用于构造或匹配 `canonicalSkillDir`。Hermes nested 与 mixed layout
不进入 Ledger 的路径解析逻辑；它们只体现为不同的 canonical path。canonical path 必须使用单个
前导 `/`；`//skills/...` 等多前导斜杠形式会被明确拒绝。

路径解析关系为：

```text
canonicalSkillDir
    │
    ├─ SkillFS managed ── resolver ──> liveSkillDir ──> ioSkillDir
    │
    └─ host / managed=false ─────────────────────────> ioSkillDir
                                                       (= canonicalSkillDir)
```

`managedSkillDirs` 只保存 canonical path。Skill Ledger 不保存 canonical/live 映射，不把
backing root 写入配置、manifest 或用户输出，也不根据 basename、目录层级或 manifest 内容猜测
live root。scanner finding 在签名前使用本次 `ResolvedSkillRoot`，将已知 `ioSkillDir` 及其
symlink-resolved alias 下的路径投影回 canonical 空间；相对路径和其他外部路径保持不变。

## SkillFS 只读 Resolver 合同

### 传输与请求

Resolver 复用 SkillFS trusted control socket 的 JSONL 协议。M1 使用单一默认 endpoint：

```text
/run/user/<effective-uid>/skillfs/control.sock
```

Ledger 在运行时通过 `os.geteuid()` 计算该路径，不增加 resolver endpoint 配置，也不通过 notify
传递 endpoint、mount id 或 generation。SkillFS 与实际发起请求的 Skill Ledger daemon 或 CLI
必须运行在相同 effective UID 和安全域中，且请求进程必须通过 SkillFS trusted peer 校验。

每次连接发送一个 JSON request frame：

```json
{
  "schemaVersion": "1",
  "method": "skill.resolveLiveSource",
  "canonicalSkillDir": "/home/u/.hermes/skills/apple/apple-notes"
}
```

成功响应：

```json
{
  "schemaVersion": "1",
  "ok": true,
  "result": {
    "managed": true,
    "canonicalSkillDir": "/home/u/.hermes/skills/apple/apple-notes",
    "liveSkillDir": "/run/skillfs/backing/apple/apple-notes"
  }
}
```

明确未接管：

```json
{
  "schemaVersion": "1",
  "ok": true,
  "result": {
    "managed": false,
    "canonicalSkillDir": "/home/u/.hermes/skills/apple/apple-notes",
    "reason": "not_managed"
  }
}
```

Resolver 只查询路径，不执行 scan、decision、activation 或任何写操作。所有成功响应必须包含
boolean `managed` 并原样回显 `canonicalSkillDir`。`managed=true` 时还必须返回 Ledger namespace
中绝对、词法规范化且可访问的 `liveSkillDir`；`managed=false` 表示该 canonical path 未由当前
SkillFS 接管。`ok=false` 始终表示 resolver 错误，即使 error code 为 `not_managed` 也不能降级为
host 模式。

### Ledger 解析规则

| 结果 | Ledger 行为 |
| --- | --- |
| control socket 不存在（`ENOENT`） | 进入 host 模式，`ioSkillDir = canonicalSkillDir` |
| `ok=true` 且 `managed=false` | 校验 canonical echo 后进入 host 模式，`ioSkillDir = canonicalSkillDir` |
| `ok=true` 且 `managed=true` | 校验 canonical echo 与 `liveSkillDir`，随后只在 live 目录执行本次 I/O |
| `managed` 缺失或不是 boolean | 视为协议错误，返回 `skill_root_resolve_failed` |
| `ok=false`、连接拒绝、权限错误、1 秒超时、非法响应或其他错误 | 返回 `skill_root_resolve_failed`，不降级到 host |

Ledger 不重试、不缓存，也不持久化 resolver 结果。每个顶层操作只解析一次；内部的
`check`、scan 与 activation 调用复用同一个 resolved context，避免一次工作流观察到不同映射。
单 skill resolver 失败不影响批量任务中的其他 skill。

socket 不存在时自动进入 host 模式是 M1 的已知权衡：它让无 SkillFS 环境保持原有行为，但
SkillFS 异常退出并删除 socket 时可能被误判为未部署。除 `ENOENT` 和成功响应中的
`managed=false` 外，其他故障均禁止静默降级。

## Runtime Activation 合同

Skill Ledger 在本次解析得到的 I/O 根目录中写入：

```text
<io_skill_dir>/.skill-meta/activation.json
```

同时尽力在 `<io_skill_dir>` 上同步写入 xattr：

```text
user.agent_sec.skill_ledger.activation
```

`activation.json` 与 xattr 使用相同 UTF-8 JSON payload：

```json
{
  "schemaVersion": 1,
  "target": ".skill-meta/versions/v000002.snapshot"
}
```

无可激活版本时：

```json
{
  "schemaVersion": 1,
  "target": null
}
```

`target` 必须为相对 `ioSkillDir` 的 `.skill-meta/versions/<id>.snapshot` 路径。
`__pending_decision__.snapshot` 是保留的安全审查占位 target，不对应 manifest，也不进入版本链。

SkillFS 消费规则：

- 优先级可以是 xattr 或 `activation.json`；读取失败或文件系统不支持 xattr 时可以回退到文件。
- 两者同时存在但不一致时 fail-safe，不暴露该 skill，并记录诊断事件。
- `target` 为 `null`、越界、不是 snapshot 或不存在时 fail-safe。
- SkillFS 不解析 `latest.json`、`scanStatus`、activation policy、findings 或用户决策。
- SkillFS 读取 live source 中的 activation metadata，并将选中的 snapshot 暴露到 canonical 视图。

Skill Ledger 对外返回的 `canonicalSkillDir`、`activationPath`、rollback backup 和错误路径始终映射
回 canonical 空间，不公开 `liveSkillDir`。activation contract 的 schemaVersion 仍为数字 `1`，
与 resolver control schema 和 notify schema 独立演进。

## SkillFS 变更通知 v2

SkillFS 发现受管 source 变化后，通过现有 `agent-sec-daemon` method 通知 Ledger：

```text
skill_ledger.skillfs_notify_change
```

外层仍使用 daemon 的 Unix socket、单连接 NDJSON request frame 以及 `id`、`method`、
`params`、`trace_context`、`timeout_ms` 字段。daemon socket 由
`AGENT_SEC_DAEMON_SOCKET` 指定，未指定时使用
`$XDG_RUNTIME_DIR/agent-sec-core/daemon.sock`。

Hermes nested skill 的请求示例：

```json
{
  "id": "skillfs-01HX...",
  "method": "skill_ledger.skillfs_notify_change",
  "params": {
    "schemaVersion": 2,
    "canonicalSkillDir": "/home/u/.hermes/skills/apple/apple-notes",
    "skillId": "apple/apple-notes",
    "eventKind": "write",
    "paths": ["SKILL.md"]
  },
  "trace_context": {},
  "timeout_ms": 5000
}
```

`params` 字段：

| 字段 | 类型 | 约束 | Ledger 用途 |
| --- | --- | --- | --- |
| `schemaVersion` | number | 必须为 `2` | v1 不兼容，其他版本明确拒绝 |
| `canonicalSkillDir` | string | 绝对、词法规范化、单个前导 `/`；不支持 `~` 或 `//` | 唯一处理键；接收阶段不检查目录存在或 `SKILL.md` |
| `skillId` | 任意 JSON 值，可省略 | SkillFS 应发送稳定完整 id | 仅记录为 `reportedSkillId`；不校验、不解释、不参与处理 |
| `eventKind` | string | `mkdir` / `create` / `write` / `rename` / `unlink` / `rmdir` / `setattr` / `truncate` / `reconcile` | 诊断与事件合并 |
| `paths` | string[] | 相对 canonical skill 根；不得为空字符串、绝对路径或包含 `..` | 描述可能变化的文件；可为空 |

启动或重启后，SkillFS 可以发送 `eventKind="reconcile"`、`paths=[]`。通知中的
`canonicalSkillDir` 必须来自 SkillFS 自己维护的 canonical mapping，不能发送 live/backing path。

成功入队响应示例：

```json
{
  "id": "skillfs-01HX...",
  "ok": true,
  "data": {
    "schemaVersion": 2,
    "accepted": true,
    "ignored": false,
    "queued": true,
    "coalesced": false,
    "skill": {
      "canonicalSkillDir": "/home/u/.hermes/skills/apple/apple-notes",
      "skillName": "apple-notes",
      "eventKinds": ["write"],
      "paths": ["SKILL.md"],
      "reportedSkillId": "apple/apple-notes"
    }
  },
  "stdout": "",
  "stderr": "",
  "exit_code": 0
}
```

通知成功只表示 daemon 已接收或入队，不表示 scan 或 activation 已完成。
仅包含 `.skill-meta/**` 的事件在调用 resolver 前返回 `accepted=true, ignored=true`，避免 Ledger
写 metadata 形成通知循环。其余事件按 `canonicalSkillDir` debounce 和合并；事件允许重复、乱序
或合并，worker 始终根据当前 I/O 目录重新计算状态。

## Ledger daemon 执行与错误边界

daemon 对每个 canonical skill 执行以下流程：

1. 调用 resolver 一次，得到本次操作的 `ResolvedSkillRoot`。
2. 在同一个 context 中对 `ioSkillDir` 执行 scan。
3. 在签名前递归投影 scanner findings 中的已知 I/O path，并拒绝仍暴露内部 root 的结果。
4. 根据 manifest、当前状态、用户决策和 activation policy 刷新 activation contract。
5. 所有 job 结果和诊断使用 `canonicalSkillDir`；`skillId` 只作为 reported metadata 保留。

错误语义：

- resolver 失败记录 per-skill `status=skipped`、`reasonCode=skill_root_resolve_failed`，job 保持
  `running`。
- scanner、路径或 activation 失败记录 per-skill `status=error`，不把整个 job 标记为不健康。
- 只有 worker loop、初始化或队列基础设施故障可以使 job 进入 `state=error`。
- startup reconcile 只枚举 `managedSkillDirs`，不把默认发现目录扩大为后台写入范围。
- `managedSkillDirs` 中的 glob 仍依赖 daemon namespace 的目录可见性；不可见的 canonical skill
  可能需要等待下一次 SkillFS notify。后续可通过 canonical replay 或只读枚举接口补齐。

## SkillFS 本地事件日志

SkillFS 现有 append-only protocol event log 是内部诊断机制，不是 notify v2，也不是 Ledger 的
canonical 身份来源。它可以继续使用独立的 schemaVersion `1` 和内部 source 路径：

```json
{
  "schemaVersion": 1,
  "time": "2026-06-11T10:00:00.000Z",
  "skillDir": "/path/to/live/source/tianqi-weather",
  "skillName": "tianqi-weather",
  "eventKind": "write",
  "paths": ["SKILL.md"]
}
```

Ledger v2 不直接读取该日志。若未来增加 replay，SkillFS 必须在发送边界把内部事件转换为
`canonicalSkillDir` + 可选 `skillId` 的 notify v2，而不能把日志中的 live `skillDir` 直接交给
Ledger。日志写入失败只影响诊断，不改变当前运行视图。

## Activation 策略

activation policy 是 Skill Ledger 配置项，SkillFS 不感知策略，只消费最终 target。当前统一为：

```json
{
  "activationPolicy": "pass_warn_only"
}
```

| 场景 | activation target |
| --- | --- |
| 用户决策 `allow` / `always_allow` / `rollback` | 该决策允许的真实 snapshot |
| 用户决策 `block` | `null` |
| 无用户决策，latest 为可信 `pass` / `warn` | latest snapshot |
| latest 为风险状态，存在历史可信 `pass` / `warn` | 最近的历史可信 snapshot |
| latest 为风险状态且无可信 fallback | `.skill-meta/versions/__pending_decision__.snapshot` |

历史配置值 `pass_only` / `latest_scanned` 会由 Ledger 兼容读取并归一化为
`pass_warn_only`。策略不会激活 source/current workspace；SkillFS 也不根据扫描状态自行选择版本。

## 部署边界与 M1 限制

- SkillFS 与 Skill Ledger 必须协调升级：Ledger 明确拒绝 notify v1。
- 既有 `managedSkillDirs` 中的 live/backing path 必须迁移为 canonical path；Ledger 不自动猜测。
- M1 只支持一个默认 resolver endpoint，不引入 mount registry、mountId、generation 或 endpoint
  传递。
- M1 不缓存 canonical/live 映射；每个顶层操作实时查询一次。
- SkillFS 与 Skill Ledger 必须使用相同 effective UID；否则双方会计算出不同的 per-user endpoint，
  Ledger 可能把 `ENOENT` 误判为未部署 SkillFS。
- SkillFS control socket 必须信任实际调用 resolver 的 daemon 与 CLI 可执行进程。对于 Python
  console script，内核 peer identity 和 `/proc/<pid>/exe` 通常指向 Python interpreter，而不是
  console script 文件；配置 `--trusted-peer-exe` 时应使用实际 interpreter 路径和 identity。
  信任通用 Python interpreter 等价于信任同一安全域内可使用该 interpreter 并访问 socket 的
  进程，因此生产部署应使用专用运行环境或明确接受该 same-UID 安全边界。
- canonical source 可以是 symlink；notify、resolver request 与 canonical echo 均保留绝对、词法
  规范化的 source 前缀。realpath 只用于双方各自的内部安全检查。
- 无 SkillFS 时 canonical path 直接作为 I/O 路径；有 SkillFS 时差异只存在于 resolver 和
  `ResolvedSkillRoot` 内部，CLI、hook、Cosh、OpenClaw 与 Hermes 不接触 backing root。
