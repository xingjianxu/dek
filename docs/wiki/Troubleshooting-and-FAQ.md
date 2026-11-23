# 常见问题与排障

## 一、语义与日常疑问

### 1. 我从配置里删除了一个包、文件或服务，为什么 `dek apply` 没有把它删掉？
这是 dek 的核心安全设计：**移除配置声明仅代表“停止管理”**，不会自动卸载软件包、删除已有文件、删除账户或停止正在运行的服务。

| 想要达成的效果 | 正确做法 |
|---|---|
| 不再让 dek 干预该资源（保留现状） | 直接从配置中移除声明 |
| 停止服务 / 禁用开机自启 | 显式声明 `running: false` / `enabled: false` |
| 清空用户受管的 SSH 公钥 | 显式声明 `keys: []`（保留手工条目） |
| 显式删除文件或卸载包 | 由对应资源显式声明期望状态为 `absent` |

### 2. 纯 JSON 配置需要安装 Ruby 吗？
不需要。内置模块和 JSON 解析全部由 Rust 实现。只有当主机入口、引用的 profile 或自定义模块使用 `.rb` 文件时，**管理机**才需要安装 Ruby；**目标机**在任何情况下都不需要 Ruby。

### 3. 修改了 `Caddyfile` 之后，会不会重新运行 Ruby？
如果你在 Ruby 中使用 `source_file("./Caddyfile")` 声明配置文件，求值阶段不会读取其内容，修改 `Caddyfile` 后只会由 Rust 重新计算摘要并规划文件与服务重载，**不会重新启动 Ruby**。

### 4. 删除管理机缓存会弄丢正在进行的任务吗？
不会。管理机缓存（`$XDG_CACHE_HOME/dek/...`）只保存配置求值中间结果，随时可安全清空；目标机的已审阅计划、内容对象、执行记录与待重试服务通知独立保存在目标机的 `/var/lib/dek` 中。

---

## 二、常见报错排查

### 1. 状态目录权限错误（`无法打开 /var/lib/dek`）
- **原因**：默认目标状态目录 `/var/lib/dek` 要求归当前执行账户所有且无同组/其他用户写权限。普通用户直接运行 `dek plan` 访问 root 拥有的 `/var/lib/dek` 会被拒绝。
- **解决**：
  - 本机正式运维使用特权账户执行（如 `sudo dek plan` / `sudo dek apply`）。
  - 仅做本机只读预览或开发测试时，通过 `--state-dir "$HOME/.local/state/dek"` 指定当前用户的私有状态目录。

### 2. SSH 握手失败：目标主机标识不匹配
- **原因**：`dek.json` 中该主机的逻辑名称（或 `targetId`）与远端系统回报的实际主机名不一致。
- **解决**：
  1. 先确认 `~/.ssh/config` 中的别名确实指向预期机器，未发生 IP 或别名误指。
  2. 运行 `ssh <别名> hostname` 核对目标机真实主机名。
  3. 若逻辑名称刻意与实际主机名不同，在 `dek.json` 的对应 `hosts.<name>` 中显式添加 `"targetId": "<实际主机名>"`，再用 `dek target-info -H <name>` 复核。

### 3. SSH 连接失败或提示找不到 `dek` 命令
- **原因**：dek 通过非交互 SSH 会话调用远端 `dek` 并启用严格主机密钥检查。
- **解决**：
  1. 在管理机运行 `ssh <别名> dek --version` 排查：确认主机密钥已在 `~/.ssh/known_hosts` 中，且远端非交互登录的 `PATH` 包含 `dek`（建议安装在 `/usr/local/bin/dek` 或 `/usr/bin/dek`）。
  2. 确认管理机与目标机的 `dek` 协议版本兼容。

### 4. `dek apply` 报错并要求重新审阅新计划
- **常见触发原因**：
  - 缺包补装需要刷新软件源或触发了 Arch 完整系统升级。
  - 原计划已超过保留期（默认 7 天）。
  - 从 `dek plan` 到 `dek apply` 期间，目标文件被手工修改、上游软件源版本变化，或包升级后出现了计划外的新配置动作。
- **解决**：阅读输出中的变更原因，按提示的新 `plan_id` 审阅后执行 `dek apply -H <host> --plan <新plan-id>`。

### 5. `http_text(...)` 提示缺少外部 HTTP 输入
- **原因**：日常 `show` / `plan` / `apply` 严格离线复用本地缓存，不会隐式联网拉取外部 URL。
- **解决**：先运行 `dek refresh -H <host>` 拉取最新响应到管理机缓存，再重新执行规划。

### 6. AUR 规划或构建失败：缺少构建用户或工具
- **原因**：
  - 目标机缺少 `paru`、`git`、`makepkg`、`make`、`gcc`、`fakeroot`、`gnupg` 或 `vercmp`。
  - 目标端以 `root` 运行但未正确配置 `/etc/dek/aur-build-user`（或该文件权限允许非 root 写入、指定的账户不存在或无主目录）。
  - SSH 模式下执行需要交互审阅的 AUR 构建时未加 `--interactive`。
- **解决**：按 [Arch 滚动升级与 AUR](Arch-Upgrades-and-AUR) 检查构建工具与 `/etc/dek/aur-build-user`（属主 `root:root`、权限 `0644`），远程执行时附加 `--interactive`。

### 7. 用户级 systemd unit 显示“待激活（Deferred）”
- **原因**：目标用户的 systemd user manager 当时未运行（该用户尚未登录且未开启 linger）。此时 unit 文件和自启链接均已部署完毕，仅即时启动延后。
- **解决**：按需在目标机由管理员运行 `sudo loginctl enable-linger <用户名>`（或等待用户登录），随后重新运行 `dek apply` 核验激活状态。

### 8. `age` 加密文件部署被拒绝
- **常见原因**：
  - 目标机上的 identity 文件权限过宽（必须仅由目标端执行账户拥有，权限 `0600`）。
  - 落盘路径或 identity 路径链中存在符号链接，或目标路径与 identity 文件指向同一 inode。
  - 仓库中的密文未加密给目标机 identity 对应的公钥。
- **解决**：检查目标路径不含符号链接、核对 `ls -l` 权限，并用 `age -d -i <identity>` 在目标机排错确认密文与私钥匹配。

---

## 三、核心术语速查

| 术语 | 含义 |
|---|---|
| **管理机（Controller）** | 运行 `dek` 命令、存放配置仓库与求值缓存、执行 Ruby 求值的机器 |
| **目标机（Target）** | 实际被观测与修改的机器；本机模式下与管理机为同一台设备 |
| **Manifest（`dek.json`）** | 配置仓库根目录的清单文件，登记主机、profile 与自定义模块 |
| **Profile** | 可复用的共享选项集合，支持嵌套引用与命名参数 |
| **Option** | 模块声明的带类型配置项（如 `services.caddy.enable`） |
| **Resource** | 模块展开后的具体受管对象（包、用户、组、目录、文件、systemd unit、公钥、age 密钥） |
| **Plan（计划）** | 目标端比对实际状态后固化的带版本号变更方案，绑定文件摘要与前置条件 |
| **`run_id`** | 单次 `apply` / `upgrade` 执行的唯一标识，用于 `status` 查询与断线续办 |
