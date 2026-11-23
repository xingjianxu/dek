# SSH 远程管理

dek 支持在管理机上通过 OpenSSH 集中管理多台远端 Linux / Arch Linux 目标机。远程模式下，**配置仓库、Ruby 运行时与求值缓存全部留在管理机**；目标机只需在 SSH 会话的 `PATH` 中安装兼容版本的 `dek` 二进制。

## 1. 配置 OpenSSH 与 `dek.json`

### 在管理机配置 `~/.ssh/config`

dek 直接复用本机 OpenSSH 的主机别名、密钥、代理与 `known_hosts`（并强制开启严格主机密钥检查）：

```sshconfig
Host lab-server
    HostName 192.0.2.10
    User root
    IdentityFile ~/.ssh/id_ed25519_lab
```

> 请先手工用 `ssh lab-server dek --version` 验证免交互连接、主机密钥信任及远端 `PATH` 中能找到 `dek`。

### 在 `dek.json` 中登记远端主机

```json
{
  "schemaVersion": 1,
  "hosts": {
    "server": {
      "entry": "hosts/server/default.rb",
      "inputs": "sdk",
      "target": { "ssh": "lab-server" },
      "targetId": "arch-prod-01"
    }
  }
}
```

- **逻辑主机名 vs SSH 别名**：CLI 中始终通过 `-H <逻辑主机名>`（这里是 `-H server`）选择主机。
- **主机身份防误连（`targetId`）**：为防止 SSH 别名被改指到另一台同样受信任的主机，dek 握手时会核对目标机回报的 `TargetIdentity.id`（默认取目标机系统主机名）：
  - 省略 `targetId` 时，要求目标机实际主机名与逻辑主机名完全一致。
  - 若逻辑名与目标机实际主机名不同，通过 `"targetId": "arch-prod-01"` 显式绑定预期身份。

## 2. 核对目标身份与能力

首次接入新主机或升级 `dek` 后，建议先用 `dek target-info` 验证握手、协议版本和可用资源后端：

```bash
dek target-info -H server
# 脚本集成可使用 --json
dek target-info -H server --json
```

输出会显示目标主机标识、操作系统、发行版、架构、协议版本以及目标端已注册的资源类型和能力列表。

## 3. 日常远程规划与应用

### 预览差异并应用

```bash
# 1. 在管理机求值并按需向目标机补传缺失的文件内容，由远端生成并保存计划
dek plan -H server

# 2. 按已审阅的计划 ID 在远端串行执行
dek apply -H server --plan <plan-id>
```

**传输效率**：管理机只发送 `ResourceSet` 与文件 SHA-256 摘要清单，目标机对比本地已有内容对象后，**仅索取缺失的文件字节**。重复规划或弱网环境下可大幅减少传输量。

### 使用交互终端（`--interactive`）

默认情况下，dek 使用非交互 SSH 协议通道（`-T`）交换结构化 JSON 消息。当已审阅的计划包含需要交互输入或审阅的动作（例如通过 `paru` 构建 AUR 包或需要 `sudo` 确认）时，在 `apply` 中加上 `--interactive`：

```bash
dek apply -H server --plan <plan-id> --interactive
```

目标端会通过私有受保护的短期 Unix 套接字分配一次性连接令牌，由管理机按需拉起独立 SSH 终端会话（`-tt`）将交互画面直接呈现在你的当前终端，同时主协议通道继续传输结构化执行状态。

如果未加 `--interactive` 而遇到需要终端的动作，该动作会保留为待执行状态，随后可直接运行：

```bash
dek apply -H server --run <run-id> --interactive
```

> 使用 `dek upgrade -H server` 在交互终端确认后，会自动以交互模式应用对应的升级计划。

## 4. 读取远端目标事实与离线快照预览

在 Ruby 配置或自定义模块中，普通 `read_text` 读取的是**管理机**上的文件。如果你需要根据目标机的架构、能力或目标机上的某个文件动态生成配置，必须显式使用 `target.*` 接口：

```ruby
arch = target.fact("architecture")
has_systemd = target.capability("systemdUnit")
machine_id = target.read_text("/etc/machine-id")
```

- **正式远程规划（`dek plan -H server`）**：管理机会向绑定的目标机在线复核所有读取过的目标事实与文件，绝不会用管理机本地值顶替。
- **离线预览（`dek show --target-snapshot`）**：目标事实在在线求值时会被缓存为快照。当你断网或只想快速查看合并后的生效配置时，可使用快照离线预览：
  ```bash
  dek show -H server --target-snapshot
  ```
  文本输出与 `--json` 的 `targetSnapshot.label` 都会明确标注数据来自已记录的快照。

## 5. 断线重连、防重放与故障续办

远程执行开始后，管理机会在命令输出中尽早打印 `plan_id`、请求标识和 `run_id`。如果执行中途 SSH 断线或网络闪断：

1. **查询远端真实进度**（不重新求值配置）：
   ```bash
   dek status -H server
   # 或查询指定 run_id
   dek status -H server --run <run-id>
   ```
2. **续办未完成的执行**：
   ```bash
   # 直接续办指定 run_id（复用原已审阅计划与固化文件内容）
   dek apply -H server --run <run-id>
   ```
   或者直接运行不带 `--plan` 的 `dek apply -H server`，dek 会自动检测并续办该主机最近一次尚未结束的运行。
3. **脚本幂等重发（`--request-id`）**：
   在自动化流水线中调用 `dek apply --plan <plan-id>` 时，可传入稳定的 `--request-id <自定义标识>`。若因网络超时需要重试同一命令，目标端会识别相同请求标识并复用到原 `run_id`，避免重复执行包事务或服务重启：
   ```bash
   dek apply -H server --plan <plan-id> --request-id deploy-20261123-01
   ```
