# Arch 滚动升级与 AUR

dek 贴合 Arch Linux 的滚动更新模型：配置仓库**只声明包名和来源**（官方仓库包或 AUR 包），不维护软件包版本锁文件，避免逆着 Arch 滚动更新做部分升级（partial upgrade）。

## 软件包来源划分

- **官方二进制仓库包**：声明在 `environment.systemPackages`（或内置应用模块如 `programs.git`、`services.caddy`、`services.openssh` 自动引入的包），由目标端 **`pacman`** 管理。
- **AUR 软件包**：声明在 `dek.arch.aurPackages`，由目标端 **`paru`** 以非 root 用户构建、安装与升级。

## 1. 日常缺包补装与离线行为

1. **全部已安装（离线零开销）**：目标端先读取本地 pacman 数据库。若所有声明的软件包均已安装，普通 `dek plan` / `dek apply` 会**跳过软件源刷新与升级检查**，完全离线运行。
2. **仅缺官方仓库包且与当前系统一致**：`dek plan` 列出解析出的补装事务（新增包与依赖），`dek apply` 在不破坏当前系统一致性的前提下直接补装，然后再执行依赖这些包的文件与服务动作。
3. **补装需要升级系统时（自动拦截保护）**：
   - Arch Linux 不支持“刷新软件源后只装新包而不升级旧包”。
   - 若补装需要刷新软件源，或目标机此前已被手工刷新过数据库且存在待升级包，或无法确认与当前系统一致，dek 会**自动转为规划完整系统升级**。
   - 预览过程使用**隔离的临时 pacman 数据库**，绝不污染系统主数据库 `/var/lib/pacman/sync/`。
   - 如果你直接运行 `dek apply` 触发了整机升级，dek 会在做任何写入前**暂停并返回升级计划 ID**，要求你先审阅完整变更，再通过 `dek apply --plan <plan-id>` 显式放行。

## 2. 主动预览与执行完整系统升级

### 方式 A：交互式一步升级（`dek upgrade`）

```bash
# 本机
sudo dek upgrade

# 远端主机
dek upgrade -H server
```

在交互终端中，`dek upgrade` 会：
1. 在隔离数据库中计算完整的仓库包变更（新增、升级、替换、移除）以及受管 AUR 包的待升级候选。
2. 展示完整升级计划并提示确认：`执行已审阅的升级计划？[y/N]`。
3. 确认后先执行一次完整的 `pacman` 仓库事务，随后按需执行受管 AUR 包构建，并在包升级完成后复核后续配置动作。
4. 如果在非交互环境（如脚本或 CI）中运行 `dek upgrade`，它不会修改系统，而是输出计划并打印对应的 `dek apply -H <host> --plan <plan-id>` 命令供后续人工确认执行。

### 方式 B：分步规划与执行（`dek plan --upgrade`）

适合需要先审阅、导出 JSON 或在变更窗口执行升级的场景：

```bash
# 1. 预览完整系统升级（支持 --json 提取 plan_id）
dek plan --upgrade -H server

# 2. 审阅满意后按计划 ID 执行（含 AUR 时建议加 --interactive）
dek apply -H server --plan <plan-id> --interactive
```

> **安全前置复核**：从审阅计划到实际执行期间，如果软件源又发生了变化导致解析出的包事务范围或候选版本与已审阅计划不一致，或者包升级后产生了原计划之外的新配置动作，dek 会拒绝静默扩大范围，而是生成新的计划 ID 并请你重新审阅。

## 3. 配置与使用受管 AUR 包

### 目标机环境要求

目标机需具备以下工具：
- `paru`、`git`、`makepkg`、`make`、`gcc`、`fakeroot`、`gnupg`、`vercmp`

### 配置非 root 构建用户（目标端以 root 运行时必需）

Arch 的 `makepkg` 禁止以 `root` 身份构建软件包。当目标端以 `root` 运行（例如 `sudo dek apply` 或 SSH 以 `root` 登录）并管理 AUR 包时，必须在目标机创建 **`/etc/dek/aur-build-user`** 文件，写入一个用于运行 `paru` 的非 root 用户名：

```bash
# 在目标机上执行（假设使用 builder 账户）
echo "builder" | sudo tee /etc/dek/aur-build-user
sudo chown root:root /etc/dek/aur-build-user
sudo chmod 0644 /etc/dek/aur-build-user
```

安全约束：
- `/etc/dek/aur-build-user` 必须由 `root` 所有，且**同组和其他用户均无写权限**。
- 指定的账户必须是真实存在的非 root 用户，且拥有有效主目录。
- 目标端通过 `setpriv` 以该用户的身份和附加组调用 `paru`；代理、缓存目录和 `paru` 选项均沿用该账户自己的 `paru` 配置。
- 请确保该构建用户拥有安装本地构建包所需的授权方式（例如通过 `sudo` 交互授权，配合 `--interactive`）。

### AUR 候选预检、审阅与构建

- **规划阶段**：普通 `dek plan` 只查询缺装的受管 AUR 包；`dek plan --upgrade` 还会查询已安装受管 AUR 包的新版本，并列出每个包的候选版本以及运行依赖、构建依赖和检查依赖。若目标机缺少 `paru` 或构建工具链，预检会提前报错。
- **执行阶段**：目标端按审阅时的精确候选版本调用 `paru -S --aur --review`，完整保留 `paru` 的 PKGBUILD 审阅、依赖处理与源码签名校验流程。
  - 本机终端中直接交互。
  - SSH 远程执行时请附加 `--interactive`（或使用交互式 `dek upgrade`）。

### 显式重建已安装的受管 AUR 包（`--rebuild-aur`）

当系统底层动态库（如 Python、ICU、OpenSSL 等）随滚动更新升级，导致某些已安装的 AUR 包需要重新编译链接时，可以使用 `--rebuild-aur`：

```bash
# 仅预览重建已安装的受管 AUR 包
dek plan --rebuild-aur -H server

# 将完整系统升级与 AUR 重建组合在同一计划中
dek plan --upgrade --rebuild-aur -H server

# 审阅后执行
dek apply -H server --plan <plan-id> --interactive
```

dek 会记录本地 pacman 数据库中已安装包的设备号、inode 与纳秒级变更时间，即使版本号未变也能准确核验同版本重装是否真正完成。

## 4. 部分失败的恢复

如果一次升级中官方仓库包事务已成功完成，但在后续某个 AUR 包构建或服务重载阶段失败：
1. 目标端会持久化记录已核验完成的仓库事务以及失败的具体动作。
2. 修复构建问题（或解决网络/PKGBUILD 问题）后，运行 `dek status -H <host>` 查看失败详情。
3. 运行 `dek apply -H <host> --run <run-id> --interactive` 续办：目标端会先核查实际安装状态，自动跳过已完成的仓库包和已安装好的 AUR 包，只重试尚未完成的动作。

> **整机回滚提示**：dek 负责可追踪、可续办的增量收敛；整机级文件系统快照与回滚（如内核或关键系统库升级异常）建议继续配合你现有的 **Snapper** / Btrfs 快照工作流使用。
