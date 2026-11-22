# 内置软件包和应用模块

Rust 控制端从静态 schema 读取下列选项。带开关的模块在 `enable` 为 `true` 时生成资源。撤销声明会停止管理对应资源。每项资源保留参与生成它的生效定义来源。

| 公开 option | 类型 | 展开结果 |
|---|---|---|
| `environment.systemPackages` | 包名字符串列表 | 要安装的 Arch 官方仓库包。同一来源的同名包合并。 |
| `dek.arch.aurPackages` | 包名字符串列表 | 要安装的 AUR 包。dek 扩展，与官方仓库包分开标识。 |
| `programs.git.enable` | 布尔值，默认 `false` | 官方仓库 `git` 包。 |
| `programs.git.config` | 以节名为键的对象 | 已启用 Git 时生成系统级 `/etc/gitconfig`，权限为 `0644`。 |
| `services.openssh.enable` | 布尔值，默认 `false` | 官方仓库 `openssh` 包及其系统级 `sshd.service`，要求自启和运行。 |
| `services.caddy.enable` | 布尔值，默认 `false` | 官方仓库 `caddy` 包及其系统级 `caddy.service`，要求自启和运行。 |
| `services.caddy.configFile` | 相对源文件路径 | 已启用 Caddy 时生成目标文件 `/etc/caddy/Caddyfile`。 |

包列表使用集合合并策略。包名遵循 Arch `pkgname` 语法，版本由目标端规划。公开的官方软件包声明沿用 NixOS 路径。[Arch pkgname 规则](https://man.archlinux.org/man/PKGBUILD.5.en) · [NixOS environment.systemPackages](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/config/system-path.nix)

Git 使用原样拼写的节、子节和变量名。节的直接键可取字符串、布尔值、数字或这些值的数组。数组按声明顺序生成多条同名变量。节内对象表示带引号的子节。例如 `{"init":{"defaultBranch":"main"}}` 生成 `[init]` 下的 `defaultBranch`。省略 `config` 时 Git 模块只管理包。显式的空对象生成空配置文件。[NixOS Git 选项](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/programs/git.nix) · [Git 配置文件语法](https://git-scm.com/docs/git-config#_syntax)

OpenSSH 服务依赖其包，服务名称取自 Arch 包。Caddy 的静态文件在资源组装时读取，以生效定义的文件为相对路径基准。服务依赖安装好的包和已声明的配置文件。配置字节变化会登记一次对 `caddy.service` 的重载通知。[NixOS OpenSSH 选项](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/networking/ssh/sshd.nix) · [Arch OpenSSH 文件清单](https://archlinux.org/packages/core/x86_64/openssh/files/) · [NixOS Caddy 选项](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/web-servers/caddy/default.nix) · [Arch Caddy 文件清单](https://archlinux.org/packages/extra/x86_64/caddy/files/)
