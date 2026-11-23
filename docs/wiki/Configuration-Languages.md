# 配置语言与求值

dek 首版支持 **静态 JSON** 和 **Ruby DSL** 两种配置语言。它们共享同一套公开 option 命名、schema 校验与合并语义：同一台主机可以自由引用另一种语言编写的 profile，且两种语言对相同配置的求值结果完全等价。

## 静态 JSON 写法

仅使用静态 JSON 与内置模块时，管理机**无需安装 Ruby**。JSON 主机入口与 profile 文件共用相同结构：

```json
{
  "schemaVersion": 1,
  "imports": [
    "base",
    { "name": "role-web", "params": { "role": "edge", "httpPort": 80 } }
  ],
  "options": [
    { "path": "services.caddy.enable", "value": true },
    { "path": "services.caddy.configFile", "value": "./Caddyfile" },
    { "path": "environment.systemPackages", "value": ["ripgrep", "jq"] }
  ]
}
```

- `schemaVersion`：固定为 `1`。
- `imports`：引用的 profile 列表。可写 profile 名称字符串，也可写 `{"name": "...", "params": {...}}` 传入命名参数。
- `options`：逐条选项赋值，每条包含完整选项路径 `path` 与对应的 JSON 值 `value`。

## Ruby DSL 写法

管理机安装 Ruby 后，即可使用 `.rb` 文件编写主机入口、profile 与自定义模块。Ruby 求值器与 DSL 已内置在 `dek` 可执行文件中，单次命令中的多个未命中单元会复用同一个 Ruby 子进程。

```ruby
# hosts/server/default.rb
profile "base"
profile "role-web", role: "edge", httpPort: 80

configure "programs.git", enable: true
configure "environment.systemPackages", ["ripgrep", "jq"]

service :openssh, enable: true
service :caddy, enable: true, configFile: source_file("./Caddyfile")

group "research"
user "alice", isNormalUser: true, shell: "/bin/bash", extraGroups: ["research"]
directory "/srv/research", owner: "alice", group: "research", mode: "0750"
```

### 声明方法速查

| DSL 方法 | 展开后的规范选项路径 | 说明 |
|---|---|---|
| `profile "name", k: v` | 导入名为 `name` 的 profile | 参数可用关键字或 Hash 传入；在目标 profile 中通过 `params["k"]` 或 `params.fetch("k")` 读取 |
| `configure "a.b.c", value` | `a.b.c = value` | 直接为完整选项路径赋值 |
| `configure "a.b", c: v1, d: v2` | `a.b.c = v1`、`a.b.d = v2` | 关键字展开形式，不能与位置参数 `value` 同时使用 |
| `service :caddy, enable: true` | `services.caddy.enable = true` | `services.<name>.*` 的简写 |
| `user "alice", ...` | `users.users.alice.*` | 省略所有属性时生成 `{}`（声明账户存在） |
| `group "research", gid: 1200` | `users.groups.research.gid = 1200` | 省略属性时生成 `{}`（声明组存在） |
| `directory "/srv/data", owner: "alice", group: "research", mode: "0750"` | `systemd.tmpfiles.settings.dek` | 默认配置名为 `"dek"`（可用 `config:` 覆盖），`owner:` 自动映射为 `d.user` |

> **提示**：无论使用哪种简写，`dek show` 和错误诊断始终展示标准化后的完整选项路径（如 `services.caddy.enable`），并精确定位到源文件与行号。

### `require_relative` 与共享辅助代码

在 Ruby 配置中可以使用 `require_relative` 引入公共辅助脚本。dek 会自动追踪入口文件及所有通过 `require`/`require_relative`/`load` 加载的源码文件，将其摘要纳入当前单元的依赖清单。

## Profile 复用、参数与合并优先级

### Profile 引用图
- Profile 可以嵌套引用其他 profile，Rust 核心负责展开引用图并检测循环依赖。
- 带参数的 profile 引用按规范化后的参数值区分（比较参数时忽略对象键顺序，保留数组顺序）。同一主机内名称和参数相同的 profile 只求值一次。

### 合并与覆盖规则

所有选项遵循统一优先级：**模块默认值 → profile 赋值 → host 入口显式赋值**。

| 合并策略 | 行为说明 | 典型示例 |
|---|---|---|
| `replace`（默认） | 高优先级覆盖低优先级；同级重复赋值若值相同自动合并，值不同则报错并列出双方来源 | `services.caddy.enable`、`users.users.<name>.shell` |
| `set` | 跨层级与同级合并去重，按集合语义处理 | `environment.systemPackages`、`extraGroups` |
| `ordered` | 保留声明顺序合并列表 | 自定义模块的有序列表选项 |
| `map` | 按键合并对象；不同键自动累积，同键按优先级规则处理或报同级冲突 | `systemd.tmpfiles.settings.<config>`、`systemd.services` |

随时可以通过 `dek show <option.path> -H <host>` 查看某个选项的最终生效值、生效定义位置以及所有被覆盖的历史赋值。

## 可追踪的外部输入与缓存策略（`inputs: "sdk"`）

在 Ruby 配置中，推荐使用 SDK 提供的输入方法读取外部数据，以便 dek 自动记录依赖摘要并精准控制缓存失效：

| 方法 | 作用 | 缓存与失效行为 |
|---|---|---|
| `source_file(path)` | 声明待部署的静态文件（相对当前脚本目录） | **求值阶段不读文件内容**，由 Rust 在规划阶段读取；修改该文件不会触发 Ruby 重新求值 |
| `read_text(path)` | 读取管理机上的 UTF-8 文本参与选项计算 | 记录文件内容摘要（文件缺失也会记录为 `missing`）；文件改动、创建或删除均触发重算 |
| `read_dir(path)` | 读取管理机目录内已排序的子项名称列表 | 记录目录成员清单摘要；增删或重命名成员均触发重算 |
| `env(name)` | 读取管理机环境变量（缺失返回 `nil`） | 记录变量值摘要或 `missing` |
| `target.fact(name)` | 向绑定目标机查询事实（如系统、发行版、架构） | 纳入目标依赖；SSH 正式规划时会在线复核 |
| `target.capability(name)` | 查询目标机是否支持特定资源能力 | 可用于跨平台/最小化环境按能力条件启用资源 |
| `target.read_text(path)` | 读取目标机上的文本文件（缺失返回 `nil`） | 绝不拿管理机文件顶替，纳入目标依赖 |
| `http_text(url)` | 读取已缓存的 HTTP UTF-8 响应文本 | 日常求值离线复用缓存；更新需显式运行 `dek refresh` |

### 为什么需要 `inputs: "sdk"`？

Ruby 本身允许直接执行原生 I/O、读取系统时间或调用外部进程，但 SDK 无法拦截绕过 SDK 的直接调用。因此：
- **默认情况**：未声明 `inputs: "sdk"` 的 Ruby 单元视为动态单元，每次规划都会重新求值。
- **启用跨运行缓存**：当你保证该 Ruby 单元仅通过上述 SDK 方法读取会影响结果的外部输入时，在 `dek.json` 对应条目添加 `"inputs": "sdk"`，dek 即可跨运行缓存该单元的求值结果。

### 使用外部 HTTP 输入与 `dek refresh`

当配置通过 `http_text("https://example.com/config.txt")` 引用远程内容时：
1. 首次规划前（或需要更新远程内容时），运行：
   ```bash
   dek refresh -H <host>
   ```
   dek 会拉取该主机显式引用的全部 HTTP 输入，更新本地缓存并报告更新数量与未变化数量。
2. 之后的 `dek show`、`dek plan` 和 `dek apply` 均直接离线复用本地已缓存的 HTTP 响应；若尚未拉取过某项 URL，dek 会明确提示缺少哪项 HTTP 输入并建议先执行 `dek refresh`。
