# 为指定账户部署用户级 systemd 单元

状态：接受（[#34](https://github.com/xingjianxu/dek/issues/34)）

使用 NSS 解析目标机的账户、UID、主组与 home。用户级定义发布到该 home 下的 `.config/systemd/user`。定义文件与自启链接由目标用户的权限写入。同计划声明的账户先由账户后端完成创建，缺少目标账户时规划报错。

按 scope 合并 manager 重载。自启链接是文件状态，可在 manager 离线时完成。需在线的动作持久记录为待激活，状态中仍可查询已完成的文件和启用操作。用户重新登录打开 manager，或由管理员以 `loginctl` 启用 linger 后，下一次应用重新观察运行状态。

曾考虑把 linger 纳入声明。管理员决定保留 `loginctl` 作为管理入口，账户的登录与退出策略继续由目标机管理员控制。
