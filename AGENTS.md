# 项目约定

- 独立的 Codex Proxy RS Gemini 原生 API 插件，生产代码仅通过公开 `gateway-plugin-sdk` 接入宿主，不修改宿主业务模块
- SDK 固定于宿主 v3.26.1 提交 `5358de53ddd4bda529cb63c422fd5c6af070f9c9`；清单和 RPC v2、`upstream_adapter` v2，Rust 1.97
- 支持自定义第三方 `baseUrl`、完整路径前缀、模型 ID，以及 `x-goog-api-key` / Bearer 鉴权，不限制为 Google 域名
- 凭据来自宿主已选 URL + Key 账号，必须核对账号 ID、revision 和 Base URL；不得读取本机 Gemini 登录文件，不记录 Key 或请求正文
- stdout 专用于 SDK 二进制 RPC；使用受管上游 HTTP，保留宿主代理、背压、取消、选号和计量责任
- 宿主 v3.26.1 的注册 `paths` 不接受冒号且最多 16 条；插件只登记合法的 `models` 标记，真实 `models/{model}:streamGenerateContent` 仍通过受管 HTTP 发送。声明路径不是访问白名单，未列出的调用由宿主按 inference 分类，不能改成普通 HTTP 绕开账号与代理
- 注册回归必须覆盖路径字符、唯一性和 16 条上限；RPC 握手或真实上游调用成功不能替代宿主注册校验
- `/v1/responses` 的非流式出口仍需 `response.completed` / `response.incomplete` wire 事件，由宿主聚合 JSON；不能直接用 `RawJson` 代替终态事件
- Gemini 思考签名按原始分段保存于 Responses reasoning 的不透明字段；调用方必须在完整历史中保留返回的 output 项，包括 reasoning
- 官方 Codex 协议参考 rust-v0.160.0 提交 `79b1b666f2e8551f8abbbca34957227f67f3f553` 的 `codex-api/src/common.rs`、`core/src/client.rs`、`codex-api/src/sse/responses.rs` 与 `protocol/src/models.rs`
- 请求回归使用包含 `client_metadata` 和 `stream_options` 的 Codex 请求形状，不只测试最小 input；客户端元数据只校验容器类型，不转发、回显或记录其值
- 可选 null 字段视为未设置；非空存储、续接、压缩与访问计划不能为兼容而静默丢弃。错误只点名代码中已知的协议选项，不回显未知字段名或字段值
- 客户端 `tool_search` 仅支持 `execution=client`，不能误判为托管搜索；声明及调用依据同一 Codex 提交的 `tools/src/tool_spec.rs`、`core/src/tools/router.rs` 与 `protocol/src/models.rs`
- 工具查找须保留对象形式的 arguments、call_id 和 Gemini 签名；将 `tool_search_output` 及 `role=developer` 的 `additional_tools` 声明加入后续工具表，相同定义去重、冲突报错，合计最多 128 个工具
- 工具回归覆盖包含真实工具声明的 Codex 请求与查找、加载、调用的三轮 SDK RPC，不以空工具表通过代替兼容性验收；插件兼容性移除可选顶层 `web_search` / `web_search_preview` 及已知日期版本声明，无需客户端或上游关闭开关，不执行或伪造原生联网搜索
- 仅在移除网页搜索声明时同步移除 `include` 中的 `web_search_call.action.sources`，响应 `tools` 只回显保留声明；按工具 type 精确匹配，不移除同名客户端函数、自定义工具或命名空间。强制网页搜索、实际搜索历史及动态加载的托管搜索工具仍须报错；`tool_choice=required` 仅在仍有可用客户端工具时接受

## 运行约束

- 在宿主创建 OpenAI 类型的 URL + Key 账号，地址与插件 `baseUrl` 一致，传输选择 HTTP/SSE，并关闭此 Key 的 OpenAI Fast/priority；账号分组、插件 upstream 绑定与客户端 Key 都限定到此上游
- 模型 ID 来自插件 `models` 配置，客户端手动指定；不实现 Google OAuth 或 `/v1/models` 自动发现，不把 Gemini 别名映射到 GPT 模型以冒用其价格
- 使用完整历史、`store=false` 和非后台请求；不支持 `previous_response_id`、托管搜索/文件工具、远程图片抓取、音视频输出或强制串行工具调用
- `stream_options` 接受 `reasoning_summary_delivery=sequential_cutoff`、布尔 `include_usage` 和 `include_obfuscation=false`，不实现混淆填充；`reasoning.context` 只接受省略、null 或 auto，不承诺 OpenAI 专属上下文选择语义
- 自定义工具 grammar 作为输入格式提示传给 Gemini，不提供原生 CFG 强制约束；自定义模型别名的 `reasoning.effort` 映射需明确模型代际
- 请求上限 16 MiB，回显设置上限 128 KiB，模型输出和签名状态各上限 256 KiB；超限返回错误，不截断成成功

## 验证与打包

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features --locked -- -D warnings`
- `cargo test --locked`：纯转换测试与真实插件子进程的离线 SDK RPC 测试，不调用外部模型
- 设置 `CPR_PLUGIN_TEST_BINARY` 为解包后的可执行文件，再运行 `cargo test --test rpc --locked`，可用同一套 RPC 测试校验发行产物
- Linux x86_64：安装匹配宿主 SDK 的 `cpr-plugin` 后执行 `bash scripts/package.sh`；可用 `CPR_PLUGIN_CLI` 指定 CLI 路径
- Windows 调用 WSL 构建时使用 PowerShell，避免 Git Bash 自动改写 `/mnt/...` 参数；Linux 与 Windows 使用独立的 Cargo 缓存及构建输出目录
- `dist/` 的 GNU Linux 包依赖 glibc 2.34+ 和 libgcc_s；目标系统更旧时应在对应基线环境重新编译，并以 `readelf --version-info` 核对实际要求
- 构建产物及临时工具链放入 `target/`，安装归档放入 `dist/`，不提交这些目录
- 真实宿主安装、第三方上游与 Google API 的付费调用需另获授权，离线测试不能替代这些验收
