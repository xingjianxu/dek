# dek 开发协作

从[任务索引](docs/tasks/README.md)领取一个可独立验收的目标。先看任务卡指定的[设计章节](docs/design.md)和直接依赖，再读涉及的 crate。代码、测试、任务卡一起进入评审；当前进度、阻塞和验收结果记在 GitHub Issue。

## Issue、卡片与 worktree

1. 查找已有 Issue 和任务卡。新目标先在 `xingjianxu/dek` 用 `gh issue create --type Feature --title '交付目标' --body-file <正文文件>` 创建 Issue，填写原生 Issue Type。新增功能用 Feature，修复偏离预期的行为用 Bug，维护、测试、文档或 CI 用 Task。正文写清目标、范围、直接前置、实施步骤和可复现的验收条件。
2. 从 GitHub 返回的 URL 读取实际编号。卡片文件名固定为 `docs/tasks/<编号>.md`，标题为 `# #<编号> <Issue 标题>`。编号用十进制原样写入，不补零。PR 也会占用 GitHub 编号，遇到空号直接跳过。卡片写上 Issue 链接、Type、设计章节、前置卡、分支和 worktree、改动落点、步骤、验证场景与预期结果；在索引里加一行。跨仓库引用写作 `owner/repo#编号`。
3. 一项任务围绕一个可评审交付物。把正常路径和失败路径各写成可验证的例子；超出一次 agent 上下文的独立交付拆成新 Issue，分别取号。用 GitHub 的 blocked-by 关系记录直接依赖，规格变更时同步更新 Issue 正文、卡片及索引。父任务可记录跨卡的整体验收。
4. 开工时从最新默认分支创建关联分支及 worktree。分支用 `issue-<编号>-<slug>`，目录用 `/data/dek-dev/worktrees/<编号>-<slug>/`。slug 取自卡片，是交付目标的简短英文描述。先查看已有分支、worktree 和开发者接手记录，接续同一任务时复用已有工作。待领取的卡按需开分支。

在源码仓库主工作区执行以下 Bash 命令。用实际 Issue 返回的编号和卡片的 slug 设置变量，此处以初始化任务为例。

```bash
issue=1
slug=bootstrap-workspace
base=$(gh repo view --json defaultBranchRef --jq '.defaultBranchRef.name')
branch="issue-$issue-$slug"
tree="/data/dek-dev/worktrees/$issue-$slug"
git fetch origin
gh issue develop "$issue" --list
git worktree list
# 对尚未创建关联分支的任务执行
gh issue develop "$issue" --base "$base" --name "$branch"
git fetch origin "$branch"
git worktree add --track -b "$branch" "$tree" "origin/$branch"
cd "$tree"
```

创建后在 Issue 记录分支、worktree 和起点。实施中将影响方向的进展、反馈、阻塞和下一步写回 Issue。逐行意见留在 PR，改变验收标准的结论同步回 Issue 与卡片。

## 提交与评审

- 实现受管动作时，优先通过重新观测目标状态和跳过已达标动作保证幂等。确实无法保证时，向开发者说明触发条件、原因及重复执行的影响，请开发者确认处理方式，并将结论写入 Issue 和任务卡。
- 使用[仓库 git-commit skill](.agents/skills/git-commit/SKILL.md)。新克隆在仓库根目录执行 `git config --local core.hooksPath .githooks` 启用已有钩子。标题格式为 `<type>(<scope>): <中文摘要>`，scope 可省略，首行不超过 72 字符，末尾不加句号。正文用中文解释改动与原因，type、scope 和 footer token 用英文。
- Feature 通常对应 `feat`，Bug 对应 `fix`，Task 可用 `refactor/docs/test/build/ci/perf/style/chore`。辅助文档、测试、回滚提交可按实际内容选 type。提交引用 `Refs: #<编号>`；agent 按 skill 写入相邻的 `Co-authored-by:` 和 `Model:` 两条 trailer。squash 提交同样遵循语言、Type 映射及署名约定。
- PR 正文写明交付与验收证据。单个或最终 PR 使用 `Closes #<编号>`，中间 PR 普通引用 `#<编号>`。设计中的长期取舍随实现记录到 [docs/adr/](docs/adr/README.md)。
- 推送后，在干净的任务 worktree 核实当前 SHA 等于远端 PR head，记录默认分支 SHA，在开发机运行 `./scripts/check`。脚本依次检查格式、全 workspace/all targets 的 Clippy（警告即失败）和锁定依赖的 workspace 测试。PR 中记录 head、base、环境、命令、结果及未覆盖场景。GitHub Actions 同时运行 workspace 检查。
- 涉及真实账户、systemd、pacman、AUR、目标机解密或 SSH 的行为时，在专用 Arch VM 验证，把模拟和实机结果分别列出。初始化时骨架只做构建与入口验证。
- 处理评审与 CI 失败；head 更新则重新验证。默认分支前进时同步新基线再验证。常规 PR 已授权 agent 合并，按仓库规则采用 squash 或合并队列。使用 `gh pr merge <PR> --squash --match-head-commit <已验证SHA>` 锁定验证版本；指定符合以上约定的合并标题与正文。进入队列后持续跟踪至真正合入。
- 合入后将 PR 链接、合并 SHA 和验证摘要回写原 Issue，确认关闭。只清理本次已合入且干净的 worktree 和分支；squash 后先核对远端 PR 的 head 与本地分支指向一致、确实已合入，再清理。回到主工作区用 `git pull --ff-only` 同步。

## wks-lab 与开发数据

开发机为 NixOS 主机 `wks-lab`。先使用系统已有工具。需要安装软件时，在 `/etc/nixos` 中按该仓库说明修改 `hosts/wks-lab/` 对应的主机配置并按其流程验证、生效。dek 仓库只提交项目源码、Cargo 配置、脚本与测试数据定义，不加入 flake、shell.nix、NixOS 模块或 Nix 锁文件。

| 开发机目录 | 用途 |
|---|---|
| `/data/dek-dev/worktrees/<编号>-<slug>/` | 领取任务后的 Git worktree |
| `/data/dek-dev/cargo/target/<worktree>/` | 各 worktree 独立编译目录；手动运行 cargo 时也设置 CARGO_TARGET_DIR |
| `/data/dek-dev/cache/` | 下载缓存，Cargo 使用 cache/cargo |
| `/data/dek-dev/vm/base/<image-id>/` | 已校验的只读 Arch 基础镜像 |
| `/data/dek-dev/runs/<编号>/<run>/` | 单次验证的差分盘、临时目录、日志与报告 |

`scripts/check` 自动设置 Cargo 与临时目录；普通主工作区的验证记在 `runs/workspace/`。托管 CI 用 `DEK_DEV_ROOT` 指向 runner 临时目录。VM 测试每轮使用独立差分盘，成功只清理本轮数据，失败保留诊断。部署后的求值缓存与计划路径见设计第 4 节。
