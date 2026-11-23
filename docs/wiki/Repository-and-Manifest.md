# 配置仓库与 dek.json

用户的主机配置应放在独立的 Git 仓库中维护，与 dek 源码仓库分离。仓库根目录的 **`dek.json`** 是整个配置仓库的清单（Manifest），负责登记主机、共享 profile 与自定义模块。

## 推荐目录结构

```text
my-machines/
├── dek.json                     # 仓库清单（必需）
├── hosts/
│   ├── laptop/
│   │   └── default.json         # 本机入口（静态 JSON）
│   └── server/
│       ├── default.rb           # 远端服务器入口（Ruby DSL）
│       └── Caddyfile            # 随主机部署的静态文件
├── profiles/
│   ├── base.json                # 跨主机复用的基础选项
│   └── web.rb                   # 支持传参的 Ruby profile
├── keys/
│   └── alice.pub                # SSH 授权公钥文件
├── secrets/
│   └── gitDeploy.age            # age 加密后的私钥密文
└── modules/
    └── lab-backup/
        ├── options.json         # 自定义模块选项 schema
        └── expand.rb            # 自定义模块资源展开脚本
```

- 清单中登记的所有路径均**相对于仓库根目录**，不能跳出仓库目录之外。
- 配置文件内引用的静态文件（如 `services.caddy.configFile`、`keyFiles`、`age.secrets.<name>.file`）默认以**声明所在的配置文件目录**为相对路径基准，允许用 `..` 引用仓库内的其他目录（例如从 `hosts/server/default.rb` 引用 `../../keys/alice.pub`），但解析后的真实文件仍必须在仓库根目录内。

## `dek.json` 清单格式

下面是一个包含多主机、共享 profile 和自定义模块的完整 `dek.json` 示例：

```json
{
  "schemaVersion": 1,
  "hosts": {
    "laptop": {
      "entry": "hosts/laptop/default.json",
      "target": { "local": true }
    },
    "server": {
      "entry": "hosts/server/default.rb",
      "inputs": "sdk",
      "target": { "ssh": "lab-server" },
      "targetId": "prod-server-01"
    }
  },
  "profiles": {
    "base": "profiles/base.json",
    "web": {
      "entry": "profiles/web.rb",
      "inputs": "sdk"
    }
  },
  "modules": {
    "lab.backup": {
      "schema": "modules/lab-backup/options.json",
      "expand": "modules/lab-backup/expand.rb",
      "readsOptions": ["lab.backup"],
      "inputs": "sdk"
    }
  }
}
```

### 顶层字段说明

| 字段 | 必填 | 说明 |
|---|---|---|
| `schemaVersion` | 是 | 清单格式版本，当前固定为 `1` |
| `hosts` | 是 | 逻辑主机名到主机配置的映射（键名不可重复、不可含控制字符或为空） |
| `profiles` | 否 | 可被主机或其他 profile 引用的公共配置映射 |
| `modules` | 否 | 仓库内自定义模块的注册表 |

### `hosts.<name>` 字段

| 字段 | 必填 | 说明 |
|---|---|---|
| `entry` | 是 | 主机入口文件路径（`.json` 或 `.rb`），相对仓库根目录 |
| `target` | 是 | 执行目标：本机使用 `{"local": true}`；远程使用 `{"ssh": "<SSH 别名>"}` |
| `targetId` | 否 | 仅用于 SSH 目标。当目标机实际主机名与清单逻辑名不同时，用该字段指定预期的目标标识（精确字符串匹配） |
| `inputs` | 否 | 输入策略，可选值 `"sdk"`。对 Ruby 入口声明此项后，允许跨运行复用求值缓存（详见 [配置语言与求值](Configuration-Languages)） |

### `profiles.<name>` 字段

profile 支持两种写法：
- **简写字符串**：直接写相对仓库根目录的文件路径，如 `"base": "profiles/base.json"`。
- **对象形式**：支持同时指定 `entry` 与 `inputs`（例如 `"web": { "entry": "profiles/web.rb", "inputs": "sdk" }`）。

### `modules.<name>` 字段

| 字段 | 必填 | 说明 |
|---|---|---|
| `schema` | 是 | 模块选项 schema 文件路径（JSON） |
| `expand` | 是 | 模块资源展开脚本路径（Ruby） |
| `readsOptions` | 否 | 声明模块展开时会读取的 option 路径或子树列表，用于缩小缓存失效范围 |
| `inputs` | 否 | 输入策略，可选 `"sdk"`，用于启用跨运行缓存 |

关于自定义模块的详细编写方式，请参阅 [自定义模块开发](Custom-Modules)。

## 仓库发现与主机选择规则

### 仓库发现
所有读取仓库的命令（如 `hosts`、`show`、`plan`、`apply`、`upgrade`、`refresh`、`status`、`target-info`）都会：
1. 若指定了全局参数 `--repo <目录>`，直接使用该目录作为配置仓库根目录。
2. 否则从当前工作目录逐级向上查找最近的 `dek.json`。

### 主机选择（`-H` / `--host`）
- **显式指定**：使用 `-H <逻辑主机名>` 或 `--host <逻辑主机名>`。主机名必须是 `dek.json` 的 `hosts` 中定义的逻辑名称（不是 SSH 别名）。
- **省略 `-H`**：仅当清单中存在**标记为 `{"local": true}`** 且逻辑名称与当前系统主机名匹配（忽略 ASCII 大小写）的唯一条目时，自动选中该主机；否则列出所有可选主机并报错退出，避免误操作远端主机。

```bash
# 查看清单中登记的所有主机
dek hosts

# 只查看单个主机条目（支持 --json）
dek hosts -H server --json

# 在仓库外指定仓库目录操作远端主机
dek plan --repo ~/infra/my-machines -H server
```
