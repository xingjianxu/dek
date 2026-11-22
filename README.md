# dek

dek 用声明式配置管理主机。首版面向 Linux 管理机和 Arch Linux 目标机，支持静态 JSON 与 Ruby 配置，以及本机与 SSH 两种执行方式。

当前仓库提供可编译的项目骨架。可以运行 `dek --help` 和 `dek --version`；`plan`、`apply` 等产品命令的实现进度见任务索引。

```sh
./scripts/check
CARGO_TARGET_DIR="/data/dek-dev/cargo/target/$(basename "$PWD")" cargo run --locked -- --version
```

| 入口 | 内容 |
|---|---|
| [设计文档](docs/design.md) | 产品语义、五阶段验收、架构边界和待确认事项 |
| [任务索引](docs/tasks/README.md) | 按交付阶段查找 Issue、前置任务与可单独领取的任务卡 |
| [协作约定](AGENTS.md) | 从创建 Issue、准备 worktree，到验证 PR、合并及收尾 |
| [架构决策](docs/adr/README.md) | 记录需要长期保留的取舍 |
| [最小配置](examples/minimal/dek.json) | 本地主机 demo 引用一个静态 JSON profile |
| [Ruby SDK](sdk/ruby/README.md) | 管理端求值器与 DSL 的代码位置 |

Rust workspace 位于 `crates/`。`dek-core` 定义共享模型，`dek-controller` 读取仓库和求值，`dek-target` 规划并执行动作，`dek-linux` 提供 Linux/Arch 后端，`dek-cli` 装配以上组件并生成 `dek`。用户的主机配置仓库与源码分开维护，仓内示例集中在 `examples/`。
