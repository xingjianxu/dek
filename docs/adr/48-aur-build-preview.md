# 用 paru 查询受管 AUR 候选

状态为已采纳。关联 [#48](https://github.com/xingjianxu/dek/issues/48)。

普通规划仅查询缺装的受管 AUR 包。完整升级预览同时查询已安装的受管包，按 Arch 版本比较规则列出待升级项。管理端在 AUR 资源中传递包名和来源。目标按这组包名批量调用 `paru -Si --aur`，从固定语言环境的输出读取版本及运行、构建、检查依赖。目标计划保留包声明来源和本地版本前置条件。

目标需要 paru、Git、makepkg、make、GCC、fakeroot、GnuPG 和 vercmp。普通账户使用自己的主目录和 paru 配置。以 root 运行目标端时，从目标机的 `/etc/dek/aur-build-user` 读一个非 root 用户名。该文件由 root 持有，组和其他账户没有写权限，解析出的用户须有有效主目录。目标端通过 setpriv 以该用户的身份和附加组调用 paru。软件源、代理和 paru 选项由目标账户通过其 paru 配置管理。

候选预检只读包数据库和受管状态。构建与安装由 [#49](../tasks/49.md) 接入，构建时沿用 paru 的 PKGBUILD 审阅与源码签名校验。
