# Codex 按对话绑定供应商与原对话续聊：部署与回滚

本文说明如何部署 CC Switch 的 Codex 按对话路由和可移植交接功能。Codex App 继续使用同一个会话目录和本地路由地址；不同对话可以同时使用不同供应商，手动改绑后仍在原 thread 中续聊，不需要复制对话或切换 `CODEX_HOME`。

## 行为边界

- 在 CC Switch“会话管理”页中，每个 Codex 对话都可以选择一家供应商，或选择“跟随全局路由”。绑定和上一次成功使用的供应商保存在 CC Switch 数据库中，代理或 CC Switch 重启后仍可继续使用。
- 已绑定的对话只请求所选供应商，不使用全局故障转移队列。该供应商返回 429、5xx 或超时会直接报错，不会消耗其他供应商额度。下拉框中的健康状态只供参考；手动绑定仍允许尝试健康状态异常的供应商。
- 未绑定的对话继续使用现有全局供应商和故障转移规则。清除绑定从下一次请求开始恢复全局路由，当前已经发出的请求不受影响。
- 如果数据库里残留无效绑定，或绑定供应商无法读取，代理返回 HTTP 503，错误码为 `session_provider_unavailable`，不会静默改用其他额度。通过 CC Switch 删除供应商时会清除指向它的绑定，但保留上一次成功供应商标识，用于下一次安全交接。
- 绑定请求成功后只更新该对话的上一次成功供应商，不修改全局当前供应商、托盘状态或其他对话的路由。删除 Codex 对话时会同时删除其路由记录。
- 设置项 `codexPortableHandoffOnProviderChange` 默认开启，也可在“设置 → 通用 → Codex 应用增强 → 跨供应商切换时保留可见上下文”中关闭。
- 同一对话的供应商发生变化时，代理根据持久化的上一次成功供应商判断边界，并删除旧供应商的 `previous_response_id`、响应项 ID、`encrypted_content` 和不可移植的推理/压缩状态。老对话第一次显式绑定且来源未知时，也按供应商边界处理。
- 开关开启时，可见用户与助手消息、附件引用、已完成的工具结果会保留。历史工具调用只作为文本上下文，不会再次执行；如果已压缩的对话无法从当前 Codex rollout 安全重建，代理返回 HTTP 503，错误码为 `portable_handoff_unavailable`，不会带残缺上下文请求新供应商。
- 开关关闭时，代理不读取本地 rollout，也不注入可移植 transcript；完成上述安全清洗后继续请求。早期已压缩的可见上下文可能丢失，但旧供应商的响应 ID、加密状态和不可移植推理状态仍不会转发。
- 只有完整响应成功后才更新该 session 的供应商归属。已向客户端输出部分内容或工具指令后，不会跨供应商重试。

## 部署前检查

1. 在 CC Switch 的备份管理页创建数据库备份。默认数据库位于用户目录下的 `.cc-switch\cc-switch.db`；如果设置过自定义应用配置目录，以界面显示的目录为准。
2. 备份当前 Codex 配置目录。默认是用户目录下的 `.codex`；如果设置了 `codex_config_dir`，备份该自定义目录。至少保留 `auth.json`、`config.toml`、`sessions`、`archived_sessions` 和状态数据库。
3. 记录当前 CC Switch 供应商、路由总开关、Codex 接管状态，以及“切换第三方时保留官方登录”的值。
4. 保留当前已安装版本的安装程序，或确认能从 CC Switch 的数据库备份与原安装包恢复。

不要复制、打印或提交 `auth.json`、API Key、Access Token、完整请求正文或 rollout 内容。

## 部署步骤

1. 完全退出 Codex App 和 CC Switch，确认两者不再持有配置文件或安装目录。
2. 使用本轮已核对版本与哈希的 3.18.0 NSIS 安装包升级 CC Switch（实际根目录取决于构建时的 `CARGO_TARGET_DIR`）：

   `<CARGO_TARGET_DIR>\release\bundle\nsis\CC Switch_3.18.0_x64-setup.exe`

3. 启动 CC Switch，在“设置 → 路由”打开路由总开关并启用 Codex 接管。
4. 在“设置 → 通用 → Codex 应用增强”保持“跨供应商切换时保留可见上下文”开启。需要保留官方插件或远程能力时，同时保持“切换第三方时保留官方登录”开启。
5. 启动 Codex App。首次部署需要这次重启，使 App 加载指向本地路由的 live `config.toml`；之后在 CC Switch 中切换供应商不需要新建对话。
6. 打开 CC Switch“会话管理”，选中一个 Codex 对话，在右侧标题栏的“供应商”下拉框中选择目标供应商。保存后下一次请求生效；需要恢复现有全局路由时选择“跟随全局路由”。
7. 保持 Codex App 的会话目录不变。CLI 已有的 run-scoped HOME 继续独立使用，不迁移、不合并，也不改写。

## 验收

先使用本地或免计费的模拟上游，不要在未单独授权时发送真实付费请求。

1. 准备三个本地模拟供应商，把三个不同 Codex 对话分别绑定到 A、B、C，同时发送请求，确认每个请求只到自己的供应商，并且全局当前供应商没有变化。
2. 在同一 Codex thread 中完成一次 A → B 手动改绑，确认 thread/session UUID 和 rollout 文件未变化，可见历史仍可使用。
3. 让已绑定供应商返回 429、5xx 或超时，确认请求直接失败且没有到达全局备用供应商。
4. 清除一个对话的绑定，确认它的下一次请求恢复使用全局路由；重启代理后重复改绑，确认绑定和上一次成功供应商仍有效。
5. 在未绑定对话上触发本地模拟的 A 端 HTTP 429 → B 全局故障转移，确认客户端只收到一份助手输出，工具只执行一次。
6. 检查 CC Switch 日志：不得出现 API Key、Access Token、完整可移植 transcript 或 `encrypted_content`。
7. 对无法安全读取 rollout 的压缩会话，确认返回 `portable_handoff_unavailable`，并且没有请求到新上游。
8. 关闭“跨供应商切换时保留可见上下文”后再次改绑，确认请求经过安全清洗后继续，不读取 rollout、不出现 portable transcript marker；此模式允许早期压缩上下文缺失。

当前仓库验证结果以本轮发布复验记录为准：

- TypeScript：项目内 `tsc --noEmit` 通过；完整前端测试 `525 passed / 0 failed`，Vite production build 和 Prettier 检查通过。
- Rust：库测试 `2156 passed / 2 ignored / 0 failed`，其余独立集成测试目标全部通过；`cargo clippy --all-targets --all-features -- -D warnings`、Rust 格式和 `git diff --check` 通过。
- 会话路由本地 HTTP fixture：三个不同 session 同时固定到 A、B、C，代理重启后 A 改绑 B，B 清除绑定后走全局 C，全局供应商不被绑定请求修改；已有绑定失败不回退、工具不重复和日志防泄漏回归继续通过。所有上游均为本地 fixture，没有发送外部模型请求。
- Production：在干净工作区从功能代码提交 `fd2e3b96506141e9b91ff4b543b0e7e8a4b70604` 执行 Vite production build 和 Tauri release + NSIS build，均成功。由于本机没有 updater 私钥，本轮构建通过临时 config override 设置 `createUpdaterArtifacts=false`，没有生成 updater 签名产物。MSI/WiX、updater 签名与 updater 兼容性均未验证；NSIS 仍有既有 `__TAURI_BUNDLE_TYPE` 未找到警告。EXE 和 NSIS 的 PE 版本均为 3.18.0，且均未做 Authenticode 签名。
- 最终 Windows 产物：EXE SHA-256 `8C2979EB4D568EC43637779C238B1C4EBC910C1C9F87443F991F0DD61AC8A583`；NSIS SHA-256 `0909D97929369E6925F5A60A497776B1114C3C4F54AFD2B21CD5537DFD8EF099`。

## 回滚

1. 完全退出 Codex App 和 CC Switch。
2. 卸载或覆盖安装回原 CC Switch 版本。
3. 如果新版本已经改变 CC Switch 数据，使用备份管理页恢复部署前数据库；恢复操作本身会先创建安全备份。
4. 关闭 Codex 接管并停止本地路由，让 CC Switch 恢复接管前保存的 live 配置。
5. 如果 live 配置未正常恢复，才从部署前备份恢复 Codex 配置目录中的 `auth.json` 和 `config.toml`。不要覆盖 `sessions` 或状态数据库，除非已经确认会话文件本身损坏。
6. 启动原 CC Switch，再启动 Codex App，确认原供应商、官方登录状态和历史列表可用。

回滚不需要修改 CLI 的独立 HOME，也不应删除或重写 Codex 对话文件。
