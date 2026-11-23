# CLI 命令参考

dek 首版 CLI 每次命令针对单台主机运行。所有子命令均支持 `--help` 查看实时帮助，使用 `dek --version` 查看当前版本。

## 全局参数

以下参数可放在子命令之前或之后（`init` 不接受 `-H/--host`）：

| 参数 | 说明 |
|---|---|
| `--repo <目录>` | 显式指定配置仓库根目录；省略时从当前目录向上查找最近的 `dek.json` |
| `-H, --host <主机>` | 指定 `dek.json` 中的逻辑主机名；省略时仅自动匹配本机且名称与当前主机名一致的唯一条目 |
| `-h, --help` | 打印帮助信息 |
| `-V, --version` | 打印 `dek` 版本号 |

---

## 子命令一览

| 命令 | 主要用途 | 是否修改目标机 |
|---|---|---|
| `dek init` | 初始化配置仓库 | 仅创建仓库文件 |
| `dek hosts` | 列出清单中的主机与连接方式 | 否 |
| `dek show` | 查看合并后的生效配置及定义来源 | 否 |
| `dek target-info` | 核对目标身份、协议版本与资源能力 | 否 |
| `dek refresh` | 刷新主机显式声明的 HTTP 输入缓存 | 否（仅更新管理机缓存） |
| `dek plan` | 观测目标状态、计算差异并保存可审阅计划 | 否（仅写入计划与内容对象） |
| `dek apply` | 执行当次规划、已审阅计划或续办未完成运行 | **是** |
| `dek upgrade` | 预览完整 Arch 升级并在终端确认后执行 | 确认后执行 |
| `dek status` | 查看目标机最近一次或指定 ID 的执行记录 | 否 |

---

## 子命令详解

### `dek init`

初始化一个以当前主机名为默认入口的配置仓库。若目标目录已存在同名配置文件则报错，不覆盖已有文件。

```bash
dek init [目录] [--lang json|ruby]
```

| 参数 | 默认值 | 说明 |
|---|---|---|
| `[目录]` | `.`（当前目录） | 仓库初始化目录（不能同时与 `--repo` 指定） |
| `--lang <语言>` | `json` | 主机入口语言，支持 `json` 或 `ruby` |

---

### `dek hosts`

读取 `dek.json` 并列出已登记的主机名、入口路径和连接方式（`local` 或 `ssh <别名>`）。

```bash
dek hosts [-H <主机>] [--json]
```

---

### `dek show`

求值并展示目标主机合并后的生效配置，或追溯单个完整选项路径的最终值、优先级、生效位置及所有被覆盖的定义。

```bash
dek show [选项路径] [-H <主机>] [--target-snapshot] [--json]
```

| 参数 | 说明 |
|---|---|
| `[选项路径]` | 可选。省略时展示整机全部生效选项；指定路径（如 `services.caddy.enable`）时仅追溯该项 |
| `--target-snapshot` | 仅适用于 SSH 目标。使用管理机已记录的目标事实快照离线预览，并在输出中标注快照来源 |
| `--json` | 以 JSON 格式输出，`options` 以完整选项路径索引生效值与来源 |

---

### `dek target-info`

核对目标主机身份（含 SSH 握手与 `targetId` 校验），显示目标操作系统、发行版、架构、协议版本及支持的资源类型与属性。

```bash
dek target-info [-H <主机>] [--json]
```

---

### `dek refresh`

拉取并更新指定主机配置中通过 SDK `http_text(url)` 显式引用的全部 HTTP 外部输入，更新本地缓存并输出变化统计。

```bash
dek refresh [-H <主机>]
```

---

### `dek plan`

实时观测目标机状态，与声明的期望状态比对差异，在目标状态目录持久化一份版本化计划并返回计划 ID。输出同时提示目标机上尚未完成的待续办服务通知。

```bash
dek plan [-H <主机>] [--upgrade] [--rebuild-aur] [--state-dir <目录>] [--json]
```

| 参数 | 说明 |
|---|---|
| `--upgrade` | 预览完整的 Arch 滚动升级（官方仓库包 + 受管 AUR 包升级候选） |
| `--rebuild-aur` | 预览重建已安装的受管 AUR 包（可与 `--upgrade` 同时使用） |
| `--state-dir <目录>` | 仅本机模式有效。覆盖默认的目标状态目录 `/var/lib/dek` |
| `--json` | 输出 JSON 文档（顶层包含 `plan_id`、`host`、动作清单与可选 `pending_notifications`） |

---

### `dek apply`

按依赖顺序执行配置变更并在执行后逐项核验。开始执行时会打印 `run_id`。

```bash
dek apply [-H <主机>] [--plan <计划ID> [--request-id <标识>] | --run <执行ID>] [--interactive] [--state-dir <目录>]
```

| 参数 | 说明 |
|---|---|
| `--plan <计划ID>` | 严格按指定已审阅计划及其固化内容执行（与 `--run` 互斥） |
| `--run <执行ID>` | 续办指定的未完成执行记录（与 `--plan` 互斥） |
| `--request-id <标识>` | 需配合 `--plan` 使用。为本次执行指定稳定请求标识，断线重发时复用到原执行，避免重复执行 |
| `--interactive` | SSH 远程模式下允许需要交互的动作（如 `paru` 构建或 `sudo` 确认）开启独立 SSH 终端 |
| `--state-dir <目录>` | 仅本机模式有效。覆盖默认目标状态目录 `/var/lib/dek` |

> **不带 `--plan` 和 `--run` 运行 `dek apply` 时的默认行为**：
> 1. 若该主机存在最近一次未完成的执行记录，自动续办该执行。
> 2. 否则即时生成一份普通计划：若计划不涉及整机仓库升级则直接执行；若缺包补装触发了完整系统升级，则先输出升级计划并返回非零退出状态，待人工审阅后使用 `dek apply --plan <计划ID>` 执行。

---

### `dek upgrade`

快捷执行完整 Arch 系统升级的审阅与确认流程。

```bash
dek upgrade [-H <主机>] [--state-dir <目录>]
```

- **交互终端下**：先生成并展示完整升级计划，输入 `y` / `yes` 后立即执行（SSH 目标自动开启交互终端支持）。
- **非交互环境下**：只生成并输出升级计划与对应的 `dek apply -H <主机> --plan <计划ID>` 命令，不修改系统。

---

### `dek status`

直接读取目标机持久化的执行记录（不重新求值配置），展示每项动作的核验状态、失败或待激活原因以及待续办服务通知。

```bash
dek status [-H <主机>] [--run <执行ID>] [--state-dir <目录>] [--json]
```

| 参数 | 说明 |
|---|---|
| `--run <执行ID>` | 查询指定 `run_id`；省略时显示该主机最近一次执行记录 |
| `--state-dir <目录>` | 仅本机模式有效。覆盖默认目标状态目录 `/var/lib/dek` |
| `--json` | 输出 JSON 格式的状态记录（无记录时输出 `null`） |

---

## 脚本自动化示例（配合 `--json`）

```bash
# 1. 生成升级计划并提取 plan_id
PLAN_ID=$(dek plan --upgrade -H server --json | jq -r '.plan_id')

# 2. 经审批后按固定 request-id 执行
dek apply -H server --plan "$PLAN_ID" --request-id "release-20261123" --interactive

# 3. 检查执行是否全部完成
dek status -H server --json | jq '{run_id, finished, pending_notifications}'
```
