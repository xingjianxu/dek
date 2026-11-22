# Ruby 求值器与 DSL

`evaluator.rb` 随编译后的 CLI 分发。管理端在首次 Ruby 请求时从 `PATH` 启动 Ruby，同一次运行的采集和展开共用进程，每条请求拥有独立的求值对象，并恢复工作目录和环境变量。Ruby 库和脚本显式修改的全局对象仍处于同一进程内。纯 JSON 入口无需 Ruby。

协议 v1 按行交换紧凑 JSON。管理端请求带 `protocolVersion`、字符串 `id`、`operation`（`collect` 或 `expand`）以及含 `entry`、`context` 的 `payload`。求值器返回相同版本和请求 ID，成功带 `ok: true` 与 `result`，失败带 `ok: false` 与 `error`。配置输出写入 stderr，配置脚本读取的标准输入为空。

`target.fact`、`target.capability` 与 `target.read_text` 发出带同一 `parentId`、查询 `id`、`kind`、`key` 和来源的反向查询；回复使用同样的两个 ID。管理端将查询回复的观测追加到求值输出，供后续缓存判断使用。

## 声明和入参

`profile "base", role: "server"` 引用带参数的 profile，参数可通过 `params.fetch("role")` 读取。`configure "programs.git.enable", true` 写入完整选项路径，关键字写法 `configure "programs.git", enable: true` 生成相同赋值。`service`、`user`、`group` 将字段写入对应的规范路径，无字段的账户声明使用空对象。`directory` 默认写入 `systemd.tmpfiles.settings.dek`，目标路径保留为对象键，`owner` 映射为 `user`。

## 输入观测

- `source_file(path)` 保留声明文件相对的路径，供 Rust 在规划阶段读取。
- `read_text(path)` 返回 UTF-8 文本，`read_dir(path)` 返回排序的子项名称。相对路径以调用文件所在目录为基准，路径缺失时返回 `nil`。
- `env(name)` 读取指定环境变量，缺失时返回 `nil`。
- `target.fact(name)`、`target.capability(name)` 与 `target.read_text(path)` 查询绑定的目标。事实缺失时诊断指向读取处，缺失目标文件可由提供者报告为 `nil`。
- `http_text(url)` 读取管理端提供的 UTF-8 响应。空响应是已观察值，缺少响应时提示刷新。

带 `repoRoot` 的协议请求启用 SDK。采集请求返回声明，展开请求返回表达式结果，两者的输入观测与结果一同返回。入口和要求加载的代码计入输入，同一进程中的后续单元可沿用已加载代码及其依赖。参见[设计第 2 至 3 节](../../docs/design.md)和[输入观测决策](../../docs/adr/12-ruby-input-observations.md)。
