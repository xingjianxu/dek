# 账户、目录与 systemd

[设计 §2.3、§6.1 和 §6.2](../design.md)定义公共路径和目标端语义。内置模块在 Rust 中展开这些选项。声明可以来自 JSON 或 Ruby，权限、目标账户与 systemd manager 由目标后端核对。

| 公共 option 路径 | 值与含义 |
| --- | --- |
| `users.users.<name>`、`users.groups.<name>` | 空对象 `{}` 声明存在性，可用于省略所有属性的账户或组 |
| `users.users.<name>.isNormalUser`、`isSystemUser` | 布尔值，最多一项为 `true` |
| `users.users.<name>.uid`、`users.groups.<name>.gid` | 32 位无符号整数，省略时由目标端处理分配 |
| `users.users.<name>.group`、`home`、`shell`、`description` | 字符串，保持目标路径及原始拼写 |
| `users.users.<name>.extraGroups` | 字符串集合，目标端按增量方式添加 |
| `systemd.tmpfiles.settings.<config>` | 以目标绝对路径为键的映射，每项使用 `d` 规则 |
| `systemd.services`、`systemd.timers` | 以 unit 名为键的系统级定义 |
| `systemd.user.services`、`systemd.user.timers` | 以 unit 名为键的全局用户级定义 |
| `dek.users.<name>.systemd.services`、`dek.users.<name>.systemd.timers` | 针对指定账户的定义，属于 dek 扩展 |

未声明的账户属性保留为未受管。用户的主组与附加组、目录的属主与属组、指定用户的 unit 会依赖同份声明中管理的账户或组。已存在的外部账户和组保留为目标端按名称解析的引用。

主组 GID 匹配且用户已加入所有声明的附加组时，重复应用应跳过组信息写入。若手工修改受管主组或移除声明的附加组成员关系，下次 `dek plan` 会列出相应修正动作。

## 目录

目录选项使用 `systemd.tmpfiles.settings.<config>.<path>.d` 的结构。选项路径止于配置名，后续路径放在映射键中，目录名包含的点号和空格会按原样保留。以下条目可放入静态配置的 `options` 列表。

```json
{
  "path": "systemd.tmpfiles.settings.dek",
  "value": {
    "/srv/research.v1": {
      "d": { "user": "alice", "group": "research", "mode": "0750" }
    }
  }
}
```

Ruby 的 `directory` 默认使用配置名 `dek`，其 `owner:` 参数映射到 `d.user`。不同路径可以在多条赋值中累积。不同优先级声明同一路径时，较高优先级提供完整规则。权限以字符串传递。

## 服务与定时器

unit 映射使用包含 `.service` 或 `.timer` 后缀的完整名称作为键。重复声明同一 unit 时使用相同的完整名称。不同作用域可以管理同名 unit。`enabled` 和 `running` 是独立布尔值，省略任一项时保持未受管。

```json
[
  {
    "path": "systemd.services",
    "value": {
      "backup.service": {
        "description": "Run the backup once",
        "serviceConfig": { "Type": "oneshot", "ExecStart": "/usr/local/bin/backup" }
      }
    }
  },
  {
    "path": "systemd.timers",
    "value": {
      "backup.timer": {
        "timerConfig": { "OnCalendar": "daily" },
        "wantedBy": ["timers.target"],
        "enabled": true,
        "running": true
      }
    }
  }
]
```

`unitConfig`、`serviceConfig`、`timerConfig` 和 `installConfig` 分别生成对应的 systemd 段落。`description` 和 `wantedBy` 分别写入 `Description` 和 `WantedBy`。指令接受单行字符串、布尔值、数字及这些类型的列表。列表按顺序生成重复指令。字符串末尾的续行反斜杠会报错。`dropIns` 接受以 `.conf` 文件名为键的文本映射。

定时器默认触发同作用域的同名服务。可用 `timerConfig.Unit` 指明另一个完整服务名。若服务也由模块展开，定时器依赖该服务定义。示例只对 timer 声明自启和持续运行，oneshot 服务由定时器触发。

系统服务的 `serviceConfig.User` 和 `serviceConfig.Group` 引用同份配置管理的账户或组时，服务依赖相应资源。此类账户引用统一写在 `serviceConfig`，定时器的服务引用统一写在 `timerConfig.Unit`。内联 drop-in 中对这些字段的赋值会报错，便于在展开阶段确定依赖。
