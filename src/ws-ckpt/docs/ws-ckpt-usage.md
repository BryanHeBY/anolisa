# ws-ckpt CLI 用户手册

ws-ckpt 是一个基于 btrfs COW 快照的工作区状态管理工具，为 AI Agent 和用户提供微秒级 checkpoint/rollback 能力。

## 一、安装

Agentic OS 内置软件，无需安装，支持 yum 安装。

```shell
sudo yum install ws-ckpt
```

## 二、操作

### 2.1 创建快照

```bash
ws-ckpt checkpoint -w <workspace> [-s <id>] [-m <message>] [--metadata <json>]
```

| 参数            | 简写   | 必填 | 说明                   |
| --------------- | ------ | ---- | ---------------------- |
| `--workspace` | `-w` | 是   | 工作区路径或 ID       |
| `--snapshot`  | `-s` | 否   | 快照 ID；省略时由 CLI 自动生成，显式指定时须在工作区内唯一 |
| `--message`   | `-m` | 否   | 快照描述信息           |
| `--metadata`  |        | 否   | JSON 格式的附加元数据 |

**示例**：

```bash
# 基本用法
ws-ckpt checkpoint -w ./my-project -s test

# 自动生成快照 ID
ws-ckpt checkpoint -w ./my-project

# 带message
ws-ckpt checkpoint -w ./my-project -s test -m "initial state"

# 带元数据
ws-ckpt checkpoint -w ws-6d5aaa -s test --metadata '{"tool":"write","file":"main.py"}'

```

### 2.2 回滚快照

#### 2.2.1 回滚到指定快照

```bash
ws-ckpt rollback -w <workspace> -s <snapshot>

```

`--snapshot`简写 `-s` 接受快照 ID（如 `test`）

`--workspace`简写 `-w`, 工作区路径或 ID

**示例**：

```bash
# 按快照 ID 回滚
ws-ckpt rollback -w ./my-project -s test
```

#### 2.2.2 按祖先链回滚

```bash
ws-ckpt rollback -w <workspace> -n <N>
```

`--num-ancestors` 简写 `-n`，沿 parent 链回退 N 个祖先：
- `-n 1`：回退到 head（上次 checkpoint）
- `-n 2`：回退到 head 的 parent
- `-n 3`：回退到 head.parent.parent

与 `--snapshot/-s` 互斥。

#### 2.2.3 预览回滚变更

在上述两种目标选择方式后增加 `--preview`，可展示回滚将恢复的文件差异，但不会修改工作区：

```bash
ws-ckpt rollback -w ./my-project -s test --preview
ws-ckpt rollback -w ./my-project -n 2 --preview
```

预览与实际回滚使用相同的目标解析规则。输出包含目标快照，以及该快照之后发生、rollback 将撤销的文件变更；标记含义与 `ws-ckpt diff` 相同。

**示例**：

```bash
# 回退到上次快照
ws-ckpt rollback -w ./my-project -n 1
# 回退两步
ws-ckpt rollback -w ./my-project -n 2
```

> **注意**：对于升级前创建的旧快照（无血缘信息），`-n` 会返回错误，需使用 `-s` 指定目标快照 ID。

### 2.3 列出快照

```bash
ws-ckpt list [-w <workspace>] [--format <table|json>]

```

省略 `-w` 时列出所有工作区的快照。

**示例**：

```bash
# 列出所有工作区的快照
ws-ckpt list

# 列出指定工作区
ws-ckpt list -w ./my-project

# JSON 格式输出
ws-ckpt list -w workspace-6d5aaa --format json

```

### 2.4 删除指定快照

```bash
ws-ckpt delete [-w <workspace>] -s <snapshot> [--force]
```

* 必填 `--snapshot` / `-s`：指定要删除的快照 ID
* 可选 `--workspace` / `-w`：若快照 ID 全局唯一可省略；若跨工作区重复则必须指定

**示例**：

```bash
# 删除单个快照
ws-ckpt delete -w ./my-project -s test --force

# 按快照 ID 全局删除（无需 -w，若 ID 全局唯一）
ws-ckpt delete -s test

# 跳过确认
ws-ckpt delete -w ./my-project -s test --force

```

### 2.5 查看快照间差异

```bash
ws-ckpt diff -w <workspace> -f <snapshot> [-t <snapshot>]
```

| 参数            | 简写   | 必填 | 说明            |
| --------------- | ------ | ---- | --------------- |
| `--workspace` | `-w` | 是   | 工作区路径或 ID |
| `--from`      | `-f` | 是   | 起始快照 ID     |
| `--to`        | `-t` | 否   | 目标快照 ID；省略时与当前工作区比较 |

**示例**：

```bash
# 比较两个快照
ws-ckpt diff -w ./my-project -f msg1-step0 -t test

# 与当前工作区比较
ws-ckpt diff -w ./my-project -f msg1-step0
```

**输出标记说明**：

| 标记  | 含义                     | 颜色 |
| ----- | ------------------------ | ---- |
| `+` | 新增文件/目录 (Added)   | 绿色 |
| `-` | 删除文件/目录 (Deleted) | 红色 |
| `M` | 内容修改 (Modified)     | 黄色 |
| `R` | 重命名 (Renamed)        | 青色 |

> diff 内置智能解析器，自动将 btrfs 底层的临时 inode 引用（如 `o261-118-0`）解析为真实文件路径，并对同一文件的多个操作去重合并。

---

### 2.6 批量清理早期快照

```bash
ws-ckpt cleanup -w <workspace> [--keep <N>]

```

保留最近 N 个普通快照（默认 20）。

**示例**：

```bash
# 保留最近 5 个
ws-ckpt cleanup -w ./my-project --keep 5

# 使用默认值（保留 20 个）
ws-ckpt cleanup -w workspace-6d5aaa

```

---

### 2.7 查看状态

```bash
ws-ckpt status [-w <workspace>] [--format <table|json>]

```

**示例**：

```bash
# 全局状态
ws-ckpt status

# 指定工作区
ws-ckpt status -w ./my-project

```

### 2.8 查看或修改配置

配置分两层:**全局**(`/etc/ws-ckpt/config.toml`,daemon-wide 默认值)和**局部**(`/var/lib/ws-ckpt/indexes/<ws_id>/policy.toml`,per-workspace 覆盖)。`ws-ckpt config` 通过 scope 决定作用范围:

- 不带 scope:打印只读 overview(全局配置 + workspace 覆盖统计),修改类 flag 会被拒绝
- `-g` / `--global` 查看或修改全局
- `-w <workspace>` / `--workspace <workspace>` 查看或修改单个 workspace 的 `policy.toml`

`-w` 只能覆盖 `auto_cleanup` 与 `auto_cleanup_keep`,其他字段(interval / image / health check)是 daemon-wide,只能 `-g` 设置。

```bash
# === 全局 ===
# 查看
ws-ckpt config -g

# 开/关后台 auto-cleanup
ws-ckpt config -g --enable-auto-cleanup
ws-ckpt config -g --disable-auto-cleanup

# 保留策略:整数=按数量,时长=按时间(单位 s/m/h/d/w)
ws-ckpt config -g --auto-cleanup-keep 10
ws-ckpt config -g --auto-cleanup-keep 30d

# 调度 / 健康检查间隔(秒,0 禁用)
ws-ckpt config -g --auto-cleanup-interval 3600
ws-ckpt config -g --health-check-interval 300

# BtrfsLoop 镜像容量(指定后需要重启 daemon 生效)
ws-ckpt config -g --img-size 30 --img-max-percent 40

# === 局部(per-workspace 覆盖) ===
# 三栏视图: effective / local / global
ws-ckpt config -w ~/proj

# 这个 workspace 单独保留 5 份
ws-ckpt config -w ~/proj --auto-cleanup-keep 5

# 这个 workspace 关掉 auto-cleanup,即便全局是开的
ws-ckpt config -w ~/proj --disable-auto-cleanup

# 这个 workspace 反之: 全局关闭时单独打开
ws-ckpt config -w ~/proj --enable-auto-cleanup

# 删除该 workspace 的 policy.toml,回到沿用全局
ws-ckpt config -w ~/proj --reset
```

### 2.9 重新加载配置

```bash
ws-ckpt reload        # 等价于 systemctl reload ws-ckpt
```

## 典型工作流

### Agent Checkpoint/Rollback 流程

```bash
# 1. 初始化工作区（模拟 OpenClaw session 启动时初始化工作区）
ws-ckpt init --workspace ~/.openclaw/workspace/

# 2. 模拟 OpenClaw tool call 前后的 checkpoint
echo "v1" > ~/.openclaw/workspace/main.py
ws-ckpt checkpoint --workspace ~/.openclaw/workspace/ -s test-v1 -m "write main.py v1"

echo "v2 - bad change" > ~/.openclaw/workspace/main.py
ws-ckpt checkpoint --workspace ~/.openclaw/workspace/ -s test-v2 -m "write main.py v2"

# 3. 发现改坏了，回滚
ws-ckpt rollback --workspace ~/.openclaw/workspace/ --snapshot test-v1

# 4. 验证回滚成功
cat ~/.openclaw/workspace/main.py  # 应输出 "v1"

# 5. 清理
ws-ckpt delete --workspace ~/.openclaw/workspace/ -s test-v2 --force

```

### 日常维护

```bash
# 查看所有工作区状态
ws-ckpt status

# 清理旧快照，释放空间
ws-ckpt cleanup -w workspace-6d5aaa --keep 10

# 查看 btrfs 空间使用
ws-ckpt status
```
