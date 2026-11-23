# 内置模块：账户、目录与 systemd

本页介绍 Linux 本地用户与组、受管目录以及系统级与用户级 systemd unit（服务和定时器）的声明方式与目标端行为。

> **自动依赖推导**：同一份配置中，用户的主组与附加组、目录的属主与属组、系统服务引用的运行用户/组、指定用户的 systemd unit，以及定时器指向的同作用域服务，都会自动建立依赖边并按正确顺序执行。

## 本地用户与组（`users.users` / `users.groups`）

### 选项一览

| 选项路径 | 类型 | 说明 |
|---|---|---|
| `users.groups.<name>` | `{}` | 仅声明本地组存在，不指定 GID |
| `users.groups.<name>.gid` | `integer`（u32） | 显式指定 GID；省略时由系统分配 |
| `users.users.<name>` | `{}` | 仅声明本地用户存在，不管理其余属性 |
| `users.users.<name>.isNormalUser` | `boolean` | 普通用户（与 `isSystemUser` 最多一项为 `true`） |
| `users.users.<name>.isSystemUser` | `boolean` | 系统服务账户 |
| `users.users.<name>.uid` | `integer`（u32） | 显式指定 UID；省略时由系统分配 |
| `users.users.<name>.group` | `string` | 主组名称 |
| `users.users.<name>.extraGroups` | `list<string>`（`set`） | 附加组集合，目标端**按增量方式添加** |
| `users.users.<name>.home` | `string` | 目标机主目录绝对路径 |
| `users.users.<name>.shell` | `string` | 目标机登录 shell 绝对路径 |
| `users.users.<name>.description` | `string` | 账户说明（GECOS） |

### 关键行为说明

- **增量与保守策略**：未声明的属性保持未受管；`extraGroups` 仅确保用户属于声明的组，**不会移除**用户在系统上原有的其他附加组。
- **冲突保护**：如果同名账户或组来自 LDAP 等外部目录服务，或已有本地账户的 UID/GID 与显式声明冲突，规划阶段会直接报错，避免覆盖或破坏现有权限归属。
- **不搬移数据**：修改已有账户的 `home` 不会隐式移动原有主目录文件；密码哈希和账户锁定状态继续由管理员直接管理。
- **停止管理**：删除账户或组声明不会删除系统上的账户和主目录。

### 示例

**Ruby：**
```ruby
group "research"
group "backup", gid: 980

user "alice",
  isNormalUser: true,
  group: "research",
  extraGroups: ["wheel"],
  shell: "/bin/bash",
  description: "Research Lead"
```

**JSON：**
```json
{
  "options": [
    { "path": "users.groups.research", "value": {} },
    { "path": "users.users.alice.isNormalUser", "value": true },
    { "path": "users.users.alice.group", "value": "research" },
    { "path": "users.users.alice.extraGroups", "value": ["wheel"] },
    { "path": "users.users.alice.shell", "value": "/bin/bash" }
  ]
}
```

## 受管目录（`systemd.tmpfiles.settings`）

目录通过 `systemd.tmpfiles.settings.<config>` 映射管理，键为目标机上的原始绝对路径（路径中的点号和空格原样保留），规则使用 `d` 对象，包含 `user`、`group`、`mode`（权限用字符串表示，如 `"0750"`）。

**Ruby（推荐使用 `directory` 简写）：**

```ruby
# 默认写入 systemd.tmpfiles.settings.dek，owner 自动映射到 d.user
directory "/srv/research", owner: "alice", group: "research", mode: "0750"
directory "/var/lib/myapp", owner: "myapp", mode: "0700", config: "myapp"
```

**JSON：**

```json
{
  "path": "systemd.tmpfiles.settings.dek",
  "value": {
    "/srv/research": {
      "d": { "user": "alice", "group": "research", "mode": "0750" }
    }
  }
}
```

### 目录管理行为
- 目录缺失时自动创建；已存在时核对并修正声明过的 `user`、`group` 与 `mode`。
- 只管理目录自身，**不递归改动**已有子文件及其权限，也不会触发 `systemd-tmpfiles --clean` 全局清理。
- 若目标路径已被普通文件或符号链接占用，规划阶段直接报告路径类型冲突。

## systemd 服务与定时器

dek 支持三种作用域的 systemd unit（unit 名称必须包含完整后缀 `.service` 或 `.timer`）：

| 作用域 | 选项路径 | 目标部署位置 / 含义 |
|---|---|---|
| 系统级 | `systemd.services`、`systemd.timers` | 系统级 systemd manager |
| 全局用户级 | `systemd.user.services`、`systemd.user.timers` | 面向所有用户的全局用户级定义 |
| 指定账户用户级（dek 扩展） | `dek.users.<name>.systemd.services`、`dek.users.<name>.systemd.timers` | 指定账户的 `~/.config/systemd/user/`，以该用户权限部署与启用 |

### Unit 定义字段

每个 unit 对象支持以下字段：

| 字段 | 说明 |
|---|---|
| `description` | 写入 `[Unit]` 段的 `Description` |
| `wantedBy` | 字符串列表，写入 `[Install]` 段的 `WantedBy` |
| `unitConfig` | 写入 `[Unit]` 段的键值对 |
| `serviceConfig` | 写入 `[Service]` 段的键值对（服务专用） |
| `timerConfig` | 写入 `[Timer]` 段的键值对（定时器专用） |
| `installConfig` | 写入 `[Install]` 段的键值对 |
| `dropIns` | 以 `.conf` 文件名为键的文本映射，部署为 unit 的 drop-in 片段 |
| `enabled` | 独立布尔值：控制是否开机自启；省略时不干预 |
| `running` | 独立布尔值：控制是否处于运行状态；省略时不干预 |

> **指令格式说明**：段落内的指令值支持单行字符串、布尔值、数字以及它们的数组（数组按顺序生成多条同名指令）。字符串末尾禁止出现续行反斜杠 `\`。`serviceConfig.User` / `serviceConfig.Group` 与 `timerConfig.Unit` 需写在主定义中（不能放在内联 `dropIns`），以便展开阶段准确推导资源依赖。

### 示例：系统级定时备份任务

对于 `Type=oneshot` 的定时任务，通常只需为 `.timer` 声明 `enabled: true` 与 `running: true`，`.service` 由定时器触发，运行结束回到 `inactive` 属于正常状态，下次 `dek plan` 不会误判为漂移：

```ruby
configure "systemd.services", {
  "backup.service" => {
    description: "Run nightly backup",
    serviceConfig: {
      Type: "oneshot",
      User: "alice",
      Group: "research",
      ExecStart: "/usr/local/bin/backup"
    }
  }
}

configure "systemd.timers", {
  "backup.timer" => {
    description: "Nightly backup timer",
    timerConfig: { OnCalendar: "daily", Persistent: true },
    wantedBy: ["timers.target"],
    enabled: true,
    running: true
  }
}
```

### 示例：指定用户的 systemd 定时器

```ruby
configure "dek.users.alice.systemd.services", {
  "sync-notes.service" => {
    description: "Sync Alice's notes",
    serviceConfig: {
      Type: "oneshot",
      ExecStart: "/usr/bin/rclone sync /home/alice/notes remote:notes"
    }
  }
}

configure "dek.users.alice.systemd.timers", {
  "sync-notes.timer" => {
    timerConfig: { OnCalendar: "hourly" },
    wantedBy: ["timers.target"],
    enabled: true,
    running: true
  }
}
```

### 用户级 Manager 离线与“待激活”状态

- 即使以 `root` 或其他管理员账户连接目标机，dek 也会按目标账户 UID 定位并隔离处理，不会影响其他用户或系统级的同名 unit。
- 如果部署时目标用户的 systemd user manager 尚未运行（未登录且未开启 linger），dek 依然会完成 unit 文件落盘和自启软链接创建，并将需要在线 manager 才能执行的即时启动标记为 **“待激活（Deferred）”**。
- 账户级 `linger` 策略由管理员按需通过 `loginctl enable-linger <user>` 管理；用户重新登录或开启 linger 后，再次运行 `dek apply` 即可核验并激活。
