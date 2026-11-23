# 内置模块：SSH 授权公钥与 age 密钥

dek 内置了用户 SSH 授权公钥管理以及基于 `age` 的加密文件部署能力，严格区分**授权公钥**、**客户端私钥/机密文件**与**目标机解密 identity**。

## 选项总览

| 选项路径 | 类型 | 合并策略 | 说明 |
|---|---|---|---|
| `users.users.<name>.openssh.authorizedKeys.keys` | `list<string>` | `set` | 内联 SSH 公钥字符串集合 |
| `users.users.<name>.openssh.authorizedKeys.keyFiles` | `list<sourceFile>` | `set` | 相对声明文件的仓库公钥文件路径集合 |
| `age.identityPaths` | `list<string>` | `replace` | 目标机上预置的 age 解密私钥路径列表（按顺序尝试） |
| `age.secrets.<name>.file` | `sourceFile` | `replace` | 必填。相对声明文件的仓库密文文件路径（`.age`） |
| `age.secrets.<name>.path` | `string` | `replace` | 必填。解密后在目标机上的落盘绝对路径 |
| `age.secrets.<name>.owner` | `string` | `replace` | 目标机属主 |
| `age.secrets.<name>.group` | `string` | `replace` | 目标机属组 |
| `age.secrets.<name>.mode` | `string` | `replace` | 目标机权限字符串（默认 `"0600"`） |
| `age.secrets.<name>.reloadUnits` | `list<string>` | `set` | 解密后**实际明文变化**时重载的系统级服务列表 |
| `age.secrets.<name>.restartUnits` | `list<string>` | `set` | 解密后**实际明文变化**时重启的系统级服务列表 |

---

## SSH 授权公钥（`authorized_keys`）

### 配置示例

**Ruby：**
```ruby
user "alice", isNormalUser: true, group: "research"
directory "/home/alice/.ssh", owner: "alice", group: "research", mode: "0700"

configure "users.users.alice.openssh.authorizedKeys",
  keys: ["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... alice@laptop"],
  keyFiles: ["../../keys/alice-workstation.pub"]
```

**JSON：**
```json
{
  "options": [
    {
      "path": "users.users.alice.openssh.authorizedKeys.keyFiles",
      "value": ["../../keys/alice-workstation.pub"]
    }
  ]
}
```

### 管理规则与安全保护
- **受管块与手工条目并存**：目标端在 `~/.ssh/authorized_keys` 中使用一对 dek 标记界定受管区域，标记之外的手工条目按原字节保留，不会被覆盖。
- **清空 vs 停止管理**：
  - 声明 `keys: []` 或 `keyFiles: []`（使合并结果为空集）会**清除 dek 受管块中的所有公钥**，但保留手工条目。
  - 同时省略 `keys` 和 `keyFiles` 会**停止管理**该用户的 `authorized_keys`，已有内容保持原状。
- **冲突检查**：如果标记损坏、路径存在不安全的符号链接/权限问题，或者声明的公钥与手工条目重复（避免绕过手工条目上的 OpenSSH 授权选项），规划阶段会报错并要求先处理冲突。

---

## `age` 加密文件部署

对于需要随配置下发到目标机的敏感文件（例如客户端 SSH 私钥、API Token、TLS 证书私钥或应用配置文件），可以使用 `age` 加密后提交到 Git 仓库。**明文永远不进入 Git 仓库、管理机缓存、执行计划、日志或错误输出。**

### 第一步：在目标机准备解密私钥（identity）

在每台目标机上生成一把专用的原生 X25519 `age` 密钥，并妥善限制文件权限（必须归执行目标端的账户所有，且组和其他用户无任何访问权限）：

```bash
# 在目标机上执行
sudo install -d -m 0700 /etc/dek/identities
sudo age-keygen -o /etc/dek/identities/server.agekey
sudo chmod 0600 /etc/dek/identities/server.agekey

# 记录输出中的公钥（以 age1... 开头），保存到管理机仓库的收件人文件
sudo age-keygen -y /etc/dek/identities/server.agekey
```

### 第二步：在管理机离线加密入库

将目标机的 `age1...` 收件人公钥写入仓库（如 `keys/server-recipients.txt`），然后离线加密敏感文件（支持二进制与 ASCII armor 格式）：

```bash
# 在配置仓库根目录执行
age -R keys/server-recipients.txt \
  -o secrets/gitDeploy.age \
  /secure/outside-repo/id_git_deploy
```

> 同一份 `.age` 密文可以同时加密给多台主机的收件人公钥；明文源文件不要放进 Git 仓库。

### 第三步：在主机配置中声明加密文件

```ruby
configure "age", identityPaths: ["/etc/dek/identities/server.agekey"]

configure "age.secrets.gitDeploy",
  file: "../../secrets/gitDeploy.age",
  path: "/home/alice/.ssh/id_git_deploy",
  owner: "alice",
  group: "research",
  mode: "0600"

# 需要联动服务重载或重启的机密配置
configure "age.secrets.appEnv",
  file: "../../secrets/app.env.age",
  path: "/etc/myapp/secrets.env",
  owner: "myapp",
  group: "myapp",
  mode: "0600",
  restartUnits: ["myapp.service"]
```

如果 `owner`、`group` 或输出路径的直接父目录也在同份配置中声明，dek 会自动确保先创建账户/组和父目录，再部署加密文件。

### 规划与目标端解密机制

1. **规划阶段不解密**：管理机只读取密文并计算 SHA-256 摘要，传输给目标端。目标端使用受保护目录（`/var/lib/dek/secrets/`，权限 `0700`）中的独立随机密钥生成 HMAC-SHA256 验证标记，用于检测目标明文是否与上次部署一致或发生漂移。
   - 若首次部署、验证标记缺失，或密文 / `identityPaths` 发生变化，计划会标注需在应用时核对并解密。
2. **原子解密与安全防线**：
   - 目标端逐级校验目录描述符，**拒绝跟随符号链接**，并**禁止将解密输出写入 identity 文件自身**（含硬链接检测）。
   - 完整解密成功并确认内容有变化后，才在目标目录同文件系统内以受限权限写入临时文件，设定最终属主与权限，再原子替换并更新验证标记。
   - 解密失败或校验失败时保留原有目标文件、清理临时文件，并中止依赖该文件的后续动作。
3. **精准区分“重加密”与“内容变化”**：
   - 当你为了新增一台主机而用更新后的收件人列表重新加密了同一份明文时，目标机解密比对明文字节后，会识别出明文未变：仅更新内部验证标记，**不会触发** `reloadUnits` / `restartUnits` 或不必要的服务重启。

### 密钥轮换建议

- **新增授权主机**：更新收件人列表，对同一明文重新运行 `age -R ...` 覆盖 `.age` 文件并提交即可。
- **吊销已下发密钥**：仅从收件人列表移除旧主机并不能收回已经落盘的明文，必须轮换业务密钥本身（例如重新生成新的 SSH 密钥对或 API Token），加密提交后部署，并在远端撤销旧密钥的授权。
- **轮换目标机 age identity**：可在 `age.identityPaths` 中按顺序同时列出新旧两把 identity 路径，待全部密文使用新公钥重新加密部署完毕后，再移除旧 identity。
