# #13 用共用 Ruby SDK 展开仓库模块

状态为已采纳，关联 [#13](../tasks/13.md)。

用户模块和内置模块使用同一套静态 schema 与资源校验。可让模块直接构造协议 JSON，也可由 SDK 生成序列化资源。前者适合外部适配器。SDK 为常用声明携带调用位置、依赖边和内容引用。

管理端先在统一注册表加载仓库模块的 schema。模块按 `<模块名>.enable` 启用，`readsOptions` 划定可见 option 路径和子树。展开脚本使用 `options` 和 `option(path)` 读取 Rust 合并的生效值。脚本可声明 `resource :directory, path: "/srv/backup"`，也可用 `file_resource "/etc/backup.conf", content: ...` 生成文件，或传入 `source:` 指向展开声明文件相对的源文件。内容对象可用 `content_object` 生成摘要引用。

声明采集与模块展开共享同一 Ruby 会话和 SDK 输入通道。资源通过 Rust 的结构校验和资源组装再交给规划器，声明的 `inputs: "sdk"` 保留给后续增量求值使用。
