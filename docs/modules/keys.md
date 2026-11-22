# SSH 授权公钥与 age 文件

内置模块把账户的授权条目与加密文件声明展开为目标端资源。Ruby 和 JSON 使用下列公共选项。[设计 §6.3](../design.md#63-ssh-授权公钥与加密私钥)

| 公共 option 路径 | 值与含义 |
| --- | --- |
| `users.users.<name>.openssh.authorizedKeys.keys` | 内联公钥字符串集合 |
| `users.users.<name>.openssh.authorizedKeys.keyFiles` | 相对声明文件的仓库公钥文件路径集合 |
| `age.identityPaths` | 目标机解密 identity 的路径列表，保留声明顺序和拼写 |
| `age.secrets.<name>.file` | 相对声明文件的仓库密文文件路径 |
| `age.secrets.<name>.path` | 解密内容的目标路径，保留原始拼写 |
| `age.secrets.<name>.owner`、`group`、`mode` | 目标机属主、属组和权限字符串，例如 `"0600"` |

`keys` 与 `keyFiles` 可以同时声明。它们汇集有效的 profile 和主机赋值。任一字段存在即可纳管该账户的授权条目。合并后的集合为空时清除受管条目，目标端保留手工添加的条目。省略两个字段会停止管理该账户的授权条目。

`file` 与 `path` 是每个命名 age 文件的必填字段。`identityPaths` 与目标路径是传给目标端的声明数据。目标后端负责选择 identity、解密与核对目标状态。

从 `hosts/server/default.json` 引用仓库根的 `keys/alice.pub` 和 `secrets/gitDeploy.age` 时，可以在 `options` 列表中写入以下条目。

~~~json
[
  {
    "path": "users.users.alice.openssh.authorizedKeys.keyFiles",
    "value": ["../../keys/alice.pub"]
  },
  {
    "path": "age.identityPaths",
    "value": ["/etc/dek/identities/server.agekey"]
  },
  {
    "path": "age.secrets.gitDeploy.file",
    "value": "../../secrets/gitDeploy.age"
  },
  {
    "path": "age.secrets.gitDeploy.path",
    "value": "/home/alice/.ssh/id_git_deploy"
  },
  {
    "path": "age.secrets.gitDeploy.mode",
    "value": "0600"
  }
]
~~~

管理端在组装资源清单时读取公钥和密文，并按内容生成 SHA-256 摘要。仓库源文件可用 `..` 到达同一仓库内的目录。实际解析到的源文件也必须位于仓库内。若属主、属组或输出文件的直接父目录在清单内受管，加密文件将依赖相关资源。授权条目依赖受管账户，且在已声明账户家目录并管理其 `.ssh` 目录时，依赖该目录。

用户的授权公钥控制谁能以该账户登录。客户端私钥代表该账户登录其他机器的身份，可作为 age 加密文件分发。SSH 服务端主机键负责向客户端证明服务器身份，对应设计中的 `services.openssh.hostKeys`。age identity 用于在目标机解密 age 文件，独立于 SSH 登录与主机身份。
