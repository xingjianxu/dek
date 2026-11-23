# 安装与环境要求

dek 以单个可执行文件 `dek` 分发，内置了全部核心模块以及 Ruby 求值器 SDK，管理机与目标机使用同一个二进制文件。

## 环境要求

### 管理机（Controller）

| 组件 | 要求 | 说明 |
|---|---|---|
| 操作系统 | Linux（首版交付） | 负责读取配置仓库、求值、合并选项与展示计划 |
| Ruby | 可选（使用 Ruby 配置或自定义模块时需在 `PATH` 中可用） | 仅使用静态 JSON 与内置模块时**无需安装 Ruby** |
| OpenSSH 客户端（`ssh`） | 管理远端主机时需要 | 复用本机 `~/.ssh/config`、密钥与 SSH Agent |
| `age` 命令行工具 | 管理加密密钥时需要 | 仅用于在管理机离线加密文件入库 |

### 目标机（Target）

| 组件 | 要求 | 说明 |
|---|---|---|
| 操作系统 | Linux（首个完整支持的发行版为 Arch Linux） | 负责观测系统状态、生成并持久化计划、执行变更 |
| `dek` 可执行文件 | 必须位于登录会话的 `PATH` 中 | 远程模式通过 SSH 按需调用 `dek` 内部目标端入口，目标机无需 Ruby 和配置仓库 |
| 系统工具 | 按声明的资源类型按需使用 | 账户管理依赖 `useradd`/`usermod`/`groupadd`/`groupmod`；目录管理依赖 `systemd-tmpfiles`；服务管理依赖 `systemctl` |
| Arch 软件包工具 | 管理 Arch 仓库包时需要 `pacman` | 管理 AUR 包时额外需要 `paru`、`git`、`makepkg`、`make`、`gcc`、`fakeroot`、`gnupg` 与 `vercmp` |

## 从源码构建

在具备 Rust 工具链（支持 Rust Edition 2024）的环境中克隆仓库并编译：

```bash
git clone https://github.com/xingjianxu/dek.git
cd dek
cargo build --locked --release -p dek-cli
```

编译产物位于 `target/release/dek`。将其安装到 `PATH` 中的目录（例如 `/usr/local/bin`）：

```bash
sudo install -m 0755 target/release/dek /usr/local/bin/dek
dek --version
```

> **提示**：如果你在多台同架构（如 `x86_64`）Arch Linux 目标机上部署，可直接将编译好的同一个 `dek` 二进制文件复制到各目标机的 `/usr/local/bin/dek`。管理机与目标机的 `dek` 协议版本需保持兼容，可通过 `dek target-info -H <host>` 校验。

## 目标机状态目录与权限

目标端默认将已审阅计划、内容对象、执行记录与 `age` 验证标记存放在 **`/var/lib/dek`**。

- **安全权限要求**：状态目录必须归运行目标端的账户所有，且不对同组或其他用户开放写权限（目录权限为受限私有目录）。
- **本机特权执行**：修改 `/etc/` 文件、管理系统级 systemd 服务或执行 `pacman` 事务通常需要以 `root` 身份运行（例如 `sudo dek plan` / `sudo dek apply`）。
- **自定义状态目录**：在本机测试或非 root 场景下，可以通过 `--state-dir <目录>` 指定当前用户私有的状态目录（远程模式不支持从管理机传递 `--state-dir`）。

```bash
# 示例：在本机使用普通用户私有目录预览计划（只读观测）
dek plan --state-dir "$HOME/.local/state/dek"
```

## 远程目标机前置准备（SSH 模式）

在使用 SSH 管理远端 Arch Linux 主机之前，请确保完成以下准备：

1. 在目标机安装 `dek` 二进制，确保非交互 SSH 会话的 `PATH` 能找到 `dek`（例如放在 `/usr/local/bin/dek` 或 `/usr/bin/dek`）。
2. 在管理机的 `~/.ssh/config` 中配置好目标主机的 SSH 别名、登录用户和私钥，并确保主机密钥已加入 `known_hosts`（dek 使用严格主机密钥检查）。
3. 若目标机需管理受管 AUR 包且目标端以 `root` 运行，请提前配置 `/etc/dek/aur-build-user`（详见 [Arch 滚动升级与 AUR](Arch-Upgrades-and-AUR)）。
4. 若目标机需解密 `age` 密钥文件，请提前将解密私钥（age identity）放置到目标机受保护路径（详见 [SSH 公钥与 age 密钥](Builtin-Modules-Keys-Secrets)）。

## 下一步

环境就绪后，前往 **[快速上手](Quick-Start)** 创建第一个配置仓库并完成首次部署。
