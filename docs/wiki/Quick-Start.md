# 快速上手

本指南将带你在本机完成一次完整流程：初始化配置仓库、编写主机配置、检查生效选项、预览执行计划、应用变更，以及验证幂等性与漂移检测。

> 本机实际写入系统配置（如 `/etc/gitconfig`、安装软件包、管理服务）需要相应权限；如果只想无副作用地体验配置解析和计划预览，可以先在测试目录运行 `dek show` 与 `dek plan --state-dir <私有目录>`。

## 1. 初始化配置仓库

在管理机上为你的基础设施创建一个独立仓库（推荐纳入 Git 版本控制）：

```bash
# 使用静态 JSON 作为入口语言（默认）
dek init my-machines

# 或者使用 Ruby DSL
dek init my-machines --lang ruby
```

进入仓库并查看生成的文件：

```bash
cd my-machines
dek hosts
```

`dek init` 会根据当前主机名自动生成清单 `dek.json` 和对应的主机入口文件：

```text
my-machines/
├── dek.json
└── hosts/
    └── <当前主机名>/
        └── default.json   # 若使用 --lang ruby 则为 default.rb
```

## 2. 编写第一份配置

下面以启用 Git、配置默认分支并安装常用工具 `ripgrep` 和 `jq` 为例。你可以根据初始化时选择的语言编辑对应入口文件（两种写法生成的配置完全等价）。

### 方式 A：静态 JSON（`hosts/<主机名>/default.json`）

```json
{
  "schemaVersion": 1,
  "imports": [],
  "options": [
    { "path": "programs.git.enable", "value": true },
    { "path": "programs.git.config", "value": { "init": { "defaultBranch": "main" } } },
    { "path": "environment.systemPackages", "value": ["ripgrep", "jq"] }
  ]
}
```

### 方式 B：Ruby DSL（`hosts/<主机名>/default.rb`）

```ruby
configure "programs.git",
  enable: true,
  config: { init: { defaultBranch: "main" } }

configure "environment.systemPackages", ["ripgrep", "jq"]
```

## 3. 检查合并后的生效配置

在触碰目标系统之前，先使用 `dek show` 验证语法、选项类型与生效值：

```bash
# 查看整机生效配置
dek show

# 追溯单个选项的生效值与定义来源
dek show programs.git.enable
```

如果写错了选项名或值类型，`dek show` 会直接定位到具体文件和行号（或 JSON Pointer）。

## 4. 生成并审阅执行计划

运行 `dek plan` 观测目标机当前状态并计算差异：

```bash
sudo dek plan
```

`dek plan` 会输出一份带唯一计划 ID 的变更清单（不修改任何受管资源）：

```text
主机 workstation（目标 workstation）
计划 <plan-id>

仓库软件包补装事务  [a-01]
  新增 extra/ripgrep ...
  新增 extra/jq ...
  原因 ...
更新文件 /etc/gitconfig  [a-02]
  原因 ...
  依赖 a-01
```

如果目标机上所需软件包已经安装，且 `/etc/gitconfig` 已与声明一致，则会显示：

```text
已符合声明，0 项变更
```

## 5. 应用计划

你可以按审阅过的计划 ID 精确执行，也可以直接运行 `dek apply` 当次规划并执行：

```bash
# 推荐：严格按刚才审阅的计划 ID 执行
sudo dek apply --plan <plan-id>

# 或一步完成规划与执行（若缺包需要整机升级，会先停在计划审阅阶段）
sudo dek apply
```

执行开始时会输出 `run_id`，执行完成后输出核验结果（如 `已核验完成，实际变更 2 项`）。

## 6. 验证幂等性与漂移修复

1. **验证幂等性**：立即再次运行 `sudo dek plan`，应看到 `已符合声明，0 项变更`。
2. **体验漂移检测**：如果有人手工修改了 `/etc/gitconfig`，再次运行 `sudo dek plan` 会立即检出差异并列出恢复动作。
3. **查看执行历史**：随时通过 `sudo dek status` 查看最近一次执行的每项动作结果。

## 7. 了解“停止管理”规则

如果你从配置中删掉 `programs.git.enable` 或 `ripgrep`，下一次 `dek apply` **只会停止管理它们**，不会擅自卸载软件包或删除 `/etc/gitconfig`。所有破坏性操作（卸包、删文件、停止服务、禁用自启）都必须显式声明，避免误删配置导致生产事故。

## 下一步

- 需要管理多台主机并抽取公共配置？阅读 **[配置仓库与 dek.json](Repository-and-Manifest)**。
- 想了解 Ruby DSL 的全部辅助函数和变量复用？阅读 **[配置语言与求值](Configuration-Languages)**。
- 想管理远端服务器？阅读 **[SSH 远程管理](SSH-Remote-Management)**。
