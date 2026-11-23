# 自定义模块开发

当内置模块无法满足团队内部服务或通用组件的封装需求时，你可以在配置仓库内注册**自定义模块**。自定义模块与内置模块共用同一套选项 schema 校验、优先级合并、来源追溯与资源规划管线。

## 1. 在 `dek.json` 中注册模块

假设我们要编写一个名为 `lab.backup` 的自定义模块，存放在 `modules/lab-backup/` 目录下：

```json
{
  "schemaVersion": 1,
  "hosts": {
    "server": {
      "entry": "hosts/server/default.rb",
      "inputs": "sdk",
      "target": { "ssh": "lab-server" }
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

- `schema`：静态 JSON 文件，定义模块对外暴露的选项路径、类型、默认值与合并策略。
- `expand`：Ruby 脚本，读取 Rust 合并后的生效选项并展开为资源。
- `readsOptions`（强烈推荐）：声明模块需要读取的选项路径或前缀子树。填写后，只有这些生效选项发生变化时才重算模块展开缓存；省略则会兜底依赖主机的全部生效选项。
- `inputs: "sdk"`：声明展开脚本仅使用可追踪的 SDK 输入接口，从而启用跨运行缓存。

> **启用开关约定**：如果模块定义了 `<模块名>.enable`（例如 `lab.backup.enable`）布尔选项，当该选项的生效值为 `false` 时，Rust 核心会**自动跳过**执行 `expand.rb`，不启动 Ruby。

## 2. 编写选项 Schema（`options.json`）

```json
{
  "options": [
    {
      "path": "lab.backup.enable",
      "type": "boolean",
      "default": false,
      "description": "启用实验室定时备份服务"
    },
    {
      "path": "lab.backup.schedule",
      "type": "string",
      "default": "daily",
      "description": "systemd timer 的 OnCalendar 表达式"
    },
    {
      "path": "lab.backup.directories",
      "type": { "list": "string" },
      "default": [],
      "merge": "set",
      "description": "需要备份的目录列表"
    },
    {
      "path": "lab.backup.extraConfigFile",
      "type": "sourceFile",
      "description": "可选的附加配置文件（相对声明处路径）"
    }
  ]
}
```

### Schema 字段说明

| 字段 | 必填 | 说明 |
|---|---|---|
| `path` | 是 | 选项路径（支持 `<name>` 动态段占位符，如 `lab.jobs.<name>.schedule`）。不能与内置模块或其他自定义模块的选项重叠 |
| `type` | 是 | 值类型：基础类型 `any`、`boolean`、`string`、`integer`、`number`、`sourceFile`；复合类型 `{"list": <类型>}`、`{"map": <类型>}`（可嵌套） |
| `default` | 否 | 默认值（必须符合声明的 `type`；`sourceFile` 的默认值以 `options.json` 所在目录为基准） |
| `merge` | 否 | 合并策略：`replace`（默认）、`set`（集合去重）、`ordered`（有序追加）、`map`（对象按键合并） |
| `description` | 是 | 选项说明文本 |

## 3. 编写 Ruby 展开脚本（`expand.rb`）

展开脚本运行在管理机的 Ruby SDK 上下文中，读取由 Rust 合并和校验后的生效选项，并声明目标资源：

```ruby
# modules/lab-backup/expand.rb
schedule = option("lab.backup.schedule")
dirs = option("lab.backup.directories")

# 1. 生成配置文件（内联内容）
config_text = dirs.map { |d| "DIR=#{d}" }.join("\n") + "\n"
file_resource "/etc/lab-backup.conf",
  content: config_text,
  attributes: { owner: "root", group: "root", mode: "0644" }

# 2. 若用户提供了外部源文件，按声明处相对路径解析并部署
if option("lab.backup.extraConfigFile")
  file_resource "/etc/lab-backup.extra.conf",
    source: option_source("lab.backup.extraConfigFile"),
    attributes: { mode: "0644" }
end

# 3. 声明受管目录
resource :directory,
  path: "/var/backups/lab",
  attributes: { owner: "root", group: "root", mode: "0750" }

# 4. 结合 content_object 声明 systemd unit 与依赖
service_unit = <<~UNIT
  [Unit]
  Description=Lab backup job

  [Service]
  Type=oneshot
  ExecStart=/usr/local/bin/lab-backup /etc/lab-backup.conf
UNIT

timer_unit = <<~UNIT
  [Unit]
  Description=Lab backup timer

  [Timer]
  OnCalendar=#{schedule}
  Persistent=true

  [Install]
  WantedBy=timers.target
UNIT

resource :systemdUnit,
  unit: { scope: "system", name: "lab-backup.service" },
  definition: { content: content_object(service_unit) },
  depends_on: [
    { path: "/etc/lab-backup.conf" },
    { path: "/var/backups/lab" }
  ]

resource :systemdUnit,
  unit: { scope: "system", name: "lab-backup.timer" },
  definition: { content: content_object(timer_unit) },
  enabled: true,
  running: true,
  activates: { scope: "system", name: "lab-backup.service" },
  depends_on: [
    { systemdUnit: { scope: "system", name: "lab-backup.service" } }
  ]
```

### 展开脚本 SDK 方法速查

| 方法 | 说明 |
|---|---|
| `options` | 返回当前模块可见的全部生效选项（`Hash`） |
| `option(path)` | 读取指定路径的生效选项值，未定义时返回 `nil` |
| `option_source(path)` | 读取 `sourceFile` 选项及其生效定义来源，供 `file_resource(..., source:)` 使用，确保相对路径以声明处文件为基准 |
| `file_resource(path, content: \| source:, attributes: {}, depends_on: [], notifications: [])` | 声明一个受管文件资源；`content` 传入文本内容，`source` 可传入 `option_source(...)` 或相对当前 `expand.rb` 的路径字符串 |
| `resource(kind, depends_on: [], notifications: [], **attributes)` | 声明任意类型的资源（如 `:package`、`:user`、`:group`、`:directory`、`:file`、`:systemdUnit`、`:authorizedKeys`、`:ageSecret`） |
| `content_object(string)` | 将文本字节注册为内容对象并返回 `"sha256:..."` 摘要，常用于 `systemdUnit` 的 `definition.content` 或 `dropIns` |

此外，`expand.rb` 同样支持 `require_relative`、`read_text`、`read_dir`、`env`、`target.fact`、`target.capability`、`target.read_text` 与 `http_text`，其观测到的依赖会自动计入模块展开缓存。

### 常用资源类型与 `depends_on` 写法

| 资源类型（`kind`） | 对应的 `depends_on` 标识格式 |
|---|---|
| `:package` | `{ package: { source: "official", name: "rsync" } }`（或 `source: "aur"`） |
| `:user` | `{ user: "alice" }` |
| `:group` | `{ group: "research" }` |
| `:directory` / `:file` / `:ageSecret` | `{ path: "/etc/lab-backup.conf" }` |
| `:systemdUnit` | `{ systemdUnit: { scope: "system", name: "lab-backup.service" } }`（用户级作用域为 `{ user: { user: "alice" } }` 或 `"userGlobal"`） |
| `:authorizedKeys` | `{ authorizedKeys: "alice" }` |

### 文件触发服务通知示例

在 `file_resource` 中附加 `notifications` 可在文件内容变化时自动触发服务重载或重启：

```ruby
file_resource "/etc/myapp/config.toml",
  source: option_source("myapp.configFile"),
  attributes: { mode: "0644" },
  notifications: [
    {
      unit: { scope: "system", name: "myapp.service" },
      effect: "reload",          # 可选 "reload" 或 "restart"
      when: "contentChanged"     # 可选 "changed" 或 "contentChanged"
    }
  ]
```

## 4. 在主机配置中使用自定义模块

在主机入口或 profile 中像使用内置模块一样赋值即可：

```ruby
# hosts/server/default.rb
configure "lab.backup",
  enable: true,
  schedule: "02:30",
  directories: ["/srv/projects", "/srv/data"],
  extraConfigFile: source_file("./backup-extra.conf")
```

最后运行 `dek show -H server` 和 `dek plan -H server` 验证选项合并与资源展开结果。
