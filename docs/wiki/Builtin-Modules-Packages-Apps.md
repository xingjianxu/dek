# 内置模块：软件包与应用

内置模块以静态 schema 和 Rust 资源展开器的形式随 `dek` 发布，不依赖 Ruby。公开选项路径优先沿用 NixOS 命名规范，dek 扩展项放在 `dek.*` 命名空间下。

> 撤销某项软件包或应用的声明后，下一次 `dek apply` **仅停止管理**，不会自动卸载软件包、删除配置文件或停止已有服务。

## 选项总览

| 选项路径 | 类型 | 默认值 | 合并策略 | 展开结果 |
|---|---|---|---|---|
| `environment.systemPackages` | `list<string>` | — | `set` | 要安装的 Arch 官方仓库包列表 |
| `dek.arch.aurPackages` | `list<string>` | — | `set` | 要安装的 AUR 软件包列表（与官方包分开标识） |
| `programs.git.enable` | `boolean` | `false` | `replace` | 安装官方仓库 `git` 包 |
| `programs.git.config` | `map<map<any>>` | — | `replace` | 启用 Git 时生成系统级 `/etc/gitconfig`（权限 `0644`） |
| `services.openssh.enable` | `boolean` | `false` | `replace` | 安装 `openssh` 包，要求 `sshd.service` 自启并运行 |
| `services.caddy.enable` | `boolean` | `false` | `replace` | 安装 `caddy` 包，要求 `caddy.service` 自启并运行 |
| `services.caddy.configFile` | `sourceFile` | — | `replace` | 启用 Caddy 时部署到 `/etc/caddy/Caddyfile`，变更自动重载 `caddy.service` |

## 软件包声明

### 官方仓库包与 AUR 包

包名需符合 Arch `pkgname` 规范，配置文件不记录版本号（版本号由目标端按 Arch 滚动更新实时规划）。

**Ruby 示例：**

```ruby
configure "environment.systemPackages", [
  "ripgrep",
  "jq",
  "htop",
  "tmux"
]

configure "dek.arch.aurPackages", [
  "paru-bin"
]
```

**JSON 示例：**

```json
{
  "schemaVersion": 1,
  "imports": [],
  "options": [
    { "path": "environment.systemPackages", "value": ["ripgrep", "jq", "htop", "tmux"] },
    { "path": "dek.arch.aurPackages", "value": ["paru-bin"] }
  ]
}
```

- 多个 profile 和主机入口中声明的包列表会自动按集合合并去重。
- 目标端首先查询本地 pacman 数据库：如果所有声明的包都已安装，普通 `dek plan` / `dek apply` 可以**完全离线**完成，跳过软件源刷新。
- 缺包补装、系统升级与 AUR 构建的详细流程请参阅 [Arch 滚动升级与 AUR](Arch-Upgrades-and-AUR)。

## Git（`programs.git`）

启用 `programs.git.enable = true` 会声明安装 `git` 官方仓库包。若同时提供 `programs.git.config`，则生成系统级 `/etc/gitconfig`（权限 `0644`）。

- 节名、子节名与变量名保留原始拼写。
- 节内直接键支持字符串、布尔值、数字或由这些类型组成的数组（数组按顺序生成多条同名配置项）。
- 节内嵌套对象表示带引号的子节（如 `[url "ssh://git@github.com/"]`）。
- 省略 `programs.git.config` 时只管理 `git` 软件包；显式传入 `{}` 会生成空的 `/etc/gitconfig`。

**Ruby 示例：**

```ruby
configure "programs.git",
  enable: true,
  config: {
    init: { defaultBranch: "main" },
    pull: { rebase: true },
    url: {
      "ssh://git@github.com/" => { insteadOf: "https://github.com/" }
    }
  }
```

## OpenSSH（`services.openssh`）

```ruby
service :openssh, enable: true
```

启用后展开为：
1. 安装 Arch 官方仓库 `openssh` 包。
2. 要求系统级 `sshd.service` 开机自启（`enabled: true`）且处于运行状态（`running: true`），服务动作自动依赖 `openssh` 包安装完成。

> 若需要管理用户的 SSH 登录授权公钥，请查看 [SSH 公钥与 age 密钥](Builtin-Modules-Keys-Secrets)。

## Caddy（`services.caddy`）

```ruby
service :caddy,
  enable: true,
  configFile: source_file("./Caddyfile")
```

启用后展开为：
1. 安装 Arch 官方仓库 `caddy` 包。
2. 若声明了 `configFile`，将相对路径指向的源文件部署到 `/etc/caddy/Caddyfile`（相对路径以生效定义的配置文件所在目录为基准）。
3. 要求系统级 `caddy.service` 开机自启且处于运行状态，依赖 `caddy` 包和配置文件就绪。
4. 当 `/etc/caddy/Caddyfile` 的实际内容字节发生变化时，自动触发一次 `caddy.service` 重载（`reload`）；如果服务在同次执行中是首次启动，则省略多余的重载。
