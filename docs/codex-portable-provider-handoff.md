# Codex 原对话跨供应商续聊：部署与回滚

本文说明如何部署 CC Switch 的 Codex 可移植交接功能。目标是让 Codex App 固定使用一个会话目录和本地路由地址；在 CC Switch 中切换真实供应商后，继续使用原 thread，而不是复制对话或切换 `CODEX_HOME`。

## 行为边界

- 设置项 `codexPortableHandoffOnProviderChange` 默认开启，也可在“设置 → 通用 → Codex 应用增强 → 跨供应商切换时保留可见上下文”中关闭。
- 供应商发生变化时，代理删除旧供应商的 `previous_response_id`、响应项 ID、`encrypted_content` 和不可移植的推理/压缩状态。
- 可见用户与助手消息、附件引用、已完成的工具结果会保留。历史工具调用只作为文本上下文，不会再次执行。
- 如果已压缩的对话无法从当前 Codex rollout 安全重建，代理返回 HTTP 503，错误码为 `portable_handoff_unavailable`，不会带残缺上下文请求新供应商。
- 只有完整响应成功后才更新该 session 的供应商归属。已向客户端输出部分内容或工具指令后，不会跨供应商重试。

## 部署前检查

1. 在 CC Switch 的备份管理页创建数据库备份。默认数据库位于用户目录下的 `.cc-switch\cc-switch.db`；如果设置过自定义应用配置目录，以界面显示的目录为准。
2. 备份当前 Codex 配置目录。默认是用户目录下的 `.codex`；如果设置了 `codex_config_dir`，备份该自定义目录。至少保留 `auth.json`、`config.toml`、`sessions`、`archived_sessions` 和状态数据库。
3. 记录当前 CC Switch 供应商、路由总开关、Codex 接管状态，以及“切换第三方时保留官方登录”的值。
4. 保留当前已安装版本的安装程序，或确认能从 CC Switch 的数据库备份与原安装包恢复。

不要复制、打印或提交 `auth.json`、API Key、Access Token、完整请求正文或 rollout 内容。

## 部署步骤

1. 完全退出 Codex App 和 CC Switch，确认两者不再持有配置文件或安装目录。
2. 使用已验证的 NSIS 安装包升级 CC Switch：

   `src-tauri\target\release\bundle\nsis\CC Switch_3.16.5_x64-setup.exe`

3. 启动 CC Switch，在“设置 → 路由”打开路由总开关并启用 Codex 接管。
4. 在“设置 → 通用 → Codex 应用增强”保持“跨供应商切换时保留可见上下文”开启。需要保留官方插件或远程能力时，同时保持“切换第三方时保留官方登录”开启。
5. 启动 Codex App。首次部署需要这次重启，使 App 加载指向本地路由的 live `config.toml`；之后在 CC Switch 中切换供应商不需要新建对话。
6. 保持 Codex App 的会话目录不变。CLI 已有的 run-scoped HOME 继续独立使用，不迁移、不合并，也不改写。

## 验收

先使用本地或免计费的模拟上游，不要在未单独授权时发送真实付费请求。

1. 在同一 Codex thread 中完成一次 A → B 手动切换，确认 thread/session UUID 和 rollout 文件未变化。
2. 触发本地模拟的 A 端 HTTP 429 → B 故障转移，确认客户端只收到一份助手输出，工具只执行一次。
3. 再执行 B → C 手动切换，确认可见历史、附件引用和已完成工具结果仍可用。
4. 检查 CC Switch 日志：不得出现 API Key、Access Token、完整可移植 transcript 或 `encrypted_content`。
5. 对无法安全读取 rollout 的压缩会话，确认返回 `portable_handoff_unavailable`，并且没有请求到新上游。

当前仓库验证结果：

- TypeScript：项目内 `tsc --noEmit` 通过。
- Rust：库测试 `1785 passed / 2 ignored / 0 failed`，其余集成测试目标全部通过；包含 proxy、failover、settings、provider service 和 Codex history migration 覆盖。
- 三供应商本地 HTTP fixture：`959` 个 proxy 测试通过，无外部模型请求。
- Production：Vite 和 Rust release 构建通过，生成 `src-tauri\target\release\cc-switch.exe`；NSIS 安装包已生成。默认全 bundle 命令的 MSI 阶段因下载 WiX 超时而失败，因此本次只把 NSIS 作为已验证的 Windows 安装产物。

## 回滚

1. 完全退出 Codex App 和 CC Switch。
2. 卸载或覆盖安装回原 CC Switch 版本。
3. 如果新版本已经改变 CC Switch 数据，使用备份管理页恢复部署前数据库；恢复操作本身会先创建安全备份。
4. 关闭 Codex 接管并停止本地路由，让 CC Switch 恢复接管前保存的 live 配置。
5. 如果 live 配置未正常恢复，才从部署前备份恢复 Codex 配置目录中的 `auth.json` 和 `config.toml`。不要覆盖 `sessions` 或状态数据库，除非已经确认会话文件本身损坏。
6. 启动原 CC Switch，再启动 Codex App，确认原供应商、官方登录状态和历史列表可用。

回滚不需要修改 CLI 的独立 HOME，也不应删除或重写 Codex 对话文件。
