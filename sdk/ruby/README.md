# Ruby 求值器与 DSL

`evaluator.rb` 随编译后的 CLI 分发。管理端在首次 Ruby 请求时从 `PATH` 启动 Ruby，同一次运行的采集和展开共用进程，每条请求拥有独立的求值对象，并恢复工作目录和环境变量。Ruby 库和脚本显式修改的全局对象仍处于同一进程内。纯 JSON 入口无需 Ruby。

协议 v1 按行交换紧凑 JSON。管理端请求带 `protocolVersion`、字符串 `id`、`operation`（`collect` 或 `expand`）以及含 `entry`、`context` 的 `payload`。求值器返回相同版本和请求 ID，成功带 `ok: true` 与 `result`，失败带 `ok: false` 与 `error`。配置输出写入 stderr，配置脚本读取的标准输入为空。

`target.fact`、`target.capability` 与 `target.read_text` 发出带同一 `parentId`、查询 `id`、`kind`、`key` 和来源的反向查询；回复使用同样的两个 ID。管理端将查询回复的观测追加到求值输出，供后续缓存判断使用。

背景和示例见[设计第 2 至 3 节](../../docs/design.md)。目标端使用 Rust 二进制，Ruby 在管理端执行。
