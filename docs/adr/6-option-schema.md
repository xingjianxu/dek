# 选项 schema 与合并规则

状态为已采纳，关联 [#6](https://github.com/xingjianxu/dek/issues/6)。

[设计 §2.2 和 §2.3](../design.md)要求内置模块和用户模块共享 schema，保留完整选项路径、值类型及定义来源。使用扁平的路径定义可直接匹配声明采集阶段的 `OptionPath`，并可用 JSON Pointer 定位每条 schema 定义。嵌套的模块专用结构也能表达相同类型，但每种语言和展开器都会需要重复实现从结构到规范路径的映射。

采用如下 JSON 格式。内置模块和用户模块均向 Rust 注册相同格式的文档。

```json
{
  "options": [
    {
      "path": "services.caddy.enable",
      "type": "boolean",
      "description": "是否启用 Caddy",
      "default": false
    },
    {
      "path": "environment.systemPackages",
      "type": { "list": "string" },
      "merge": "set",
      "description": "安装的仓库包"
    },
    {
      "path": "programs.git.config",
      "type": { "map": "any" },
      "description": "Git 配置"
    }
  ]
}
```

- 基本类型为 `any`、`boolean`、`string`、`integer`、`number` 和 `sourceFile`。用 `{"list": 类型}` 和 `{"map": 类型}` 递归校验元素或映射值。映射键保留原有拼写，适合容纳带点号的名称和第三方配置字段。
- 路径中的 `<name>` 等占位符匹配一个非空路径段。重叠定义会报告两个模块及各自的定义位置。具体路径接受声明后才应用包含占位符的默认值。
- 缺省策略 `replace` 选择最高优先级的完整值，保留其他优先级的定义，并报告任何一层上的不同值冲突。列表可选择 `set` 或 `ordered`，两者按模块默认、profile 遍历、host 声明的顺序累积，`set` 保留每个值的首次出现。
- `sourceFile` 接收相对路径字符串，用声明源文件的所在目录判断引用是否相同。合并配置保存原字符串和所有定义的来源，展开器据此查找实际内容。模块默认值的来源为 schema 文件中对应的默认值位置。
