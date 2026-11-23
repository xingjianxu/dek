# dek GitHub Wiki 文档源码

主仓库的 `docs/wiki/` 是 dek 用户文档的维护位置。通过主仓库的 PR 更新文档，合入后从已审阅的提交单向发布到 GitHub Wiki。页面间链接遵循 GitHub Wiki 约定，省略 `.md` 后缀，例如 `[快速上手](Quick-Start)`。[dek GitHub Wiki](https://github.com/xingjianxu/dek/wiki)

## 页面结构

| 文件 | 对应 Wiki 页面 |
|---|---|
| `_Sidebar.md` | 全局右侧导航栏 |
| `_Footer.md` | 全局页脚 |
| `Home.md` | 首页与文档导览 |
| `Installation-and-Requirements.md` | 安装与环境要求 |
| `Quick-Start.md` | 快速上手 |
| `Repository-and-Manifest.md` | 配置仓库与 `dek.json` |
| `Configuration-Languages.md` | 配置语言与求值（静态 JSON / Ruby DSL） |
| `Builtin-Modules-Packages-Apps.md` | 内置模块：软件包与应用 |
| `Builtin-Modules-System.md` | 内置模块：账户、目录与 systemd |
| `Builtin-Modules-Keys-Secrets.md` | 内置模块：SSH 公钥与 age 密钥 |
| `SSH-Remote-Management.md` | SSH 远程管理 |
| `Arch-Upgrades-and-AUR.md` | Arch 滚动升级与 AUR |
| `Caching-State-and-Recovery.md` | 缓存、状态与故障恢复 |
| `Custom-Modules.md` | 自定义模块开发 |
| `CLI-Reference.md` | CLI 命令参考 |
| `Troubleshooting-and-FAQ.md` | 常见问题与排障 |

## 发布到 GitHub Wiki

首次发布前，在 GitHub Wiki 网页保存一个 `Home` 页面来初始化 `.wiki.git`。从主仓库选定的提交检出后，在仓库根目录执行下列命令。发布清单包含页面、侧栏和页脚，`README.md` 用于仓库内的维护说明。

```bash
wiki_dir=$(mktemp -d /tmp/dek-wiki.XXXXXXXX)
(
    set -e
    git clone https://github.com/xingjianxu/dek.wiki.git "$wiki_dir"
    for page in docs/wiki/*.md; do
        if [[ "$page" != docs/wiki/README.md ]]; then
            cp -- "$page" "$wiki_dir/"
        fi
    done
    git -C "$wiki_dir" add -- '*.md'
    git -C "$wiki_dir" diff --cached
)
```

从源码移除页面时，在 Wiki 工作副本中用 `git rm` 暂存对应页面的删除。核对全部暂存差异后，在同一 Bash 会话执行以下命令。差异为空时，命令会跳过提交和推送。

```bash
(
    set -e
    : "${wiki_dir:?请先在当前 Bash 会话准备 Wiki 工作副本}"
    wiki_origin=$(git -C "$wiki_dir" config --local --get remote.origin.url)
    if [[ "$wiki_origin" != https://github.com/xingjianxu/dek.wiki.git ]]; then
        printf '%s\n' '发布目录的远端地址与 dek Wiki 不符。' >&2
        exit 1
    fi
    if git -C "$wiki_dir" diff --cached --quiet; then
        exit 0
    fi
    git -C "$wiki_dir" commit -m "docs(wiki): 同步 dek 使用文档"
    git -C "$wiki_dir" push origin HEAD
)
```

推送后打开 Wiki，确认更新的页面和导航可访问。
