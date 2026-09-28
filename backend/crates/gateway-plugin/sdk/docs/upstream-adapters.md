# 受管上游适配器

`upstream_adapter` v1 是现有生成请求洋葱链的终端，只挂到内置 `openai` / `xai`，不替换独立图像或压缩端点。
插件定义 Base URL、业务路径、请求编码和响应解析；宿主继续持有身份、ACL、选号、租约、凭据、代理、重试和账本。
普通请求或响应加工使用 [middleware](capabilities.md#洋葱中间件)

## 声明与绑定

清单声明 `upstream_adapter`、版本 `1`、阶段 `upstream`，以及匹配的 `input_formats` / `output_formats`；
需要 `requests` 与 `upstream_connections`。使用 `PluginBuilder::on` 注册
`methods::UPSTREAM_ADAPTER_REGISTER` 与 `methods::UPSTREAM_ADAPTER_EXECUTE`

注册返回 `UpstreamAdapterRegistration`，包含 1 至 16 个适配器：

| 字段 | 含义 |
| --- | --- |
| `id` | 实例内唯一标识，不是 Provider 或账号类型 |
| `provider` | `openai` 或 `xai` |
| `base_url` | 以 `/` 结尾的 HTTP(S) 基址，无用户信息、查询串或 fragment |
| `paths` | 不以 `/` 开头的基址相对路径，含 `inference` 或 `auxiliary` 用途，无通配符 |
| `authentication_kinds` | 可使用的现有认证类型，宿主复核已选账号 |
| `transport` | `http_json`、`http_sse` 或 `websocket` |
| `protocol` | 请求和客户端 wire 使用的协议，须在清单输入、输出格式内 |
| `models` | 公开请求模型的精确匹配，空集合沿用绑定范围 |

实例绑定沿用 Key、账号组、Provider、模型范围，只允许一次 `upstream` 绑定和 `reject` 故障策略。
同一请求不能同时命中两个适配器；配置准备时拒绝重叠，执行时再次复核。适配器未命中时仍使用原生上游，
已命中但不可用时拒绝请求。安装、授权、配置、启停和升级使用已有插件管理

能力版本、清单版本、进程 RPC 版本和宿主兼容声明格式版本分别判断，见[清单](manifest.md)。
Provider、业务协议和传输名称不能写入宿主兼容清单的 capability / permission / RPC 版本位置

## 执行与受管出站

`TypedCall<UpstreamAdapterRequest>` 包含只读的 Key、账号、凭据版本、模型、协议、Header 和宿主续接投影；
原始请求正文位于 `call.payload`，原始账号凭据不进入插件输入。宿主首次消费冷流才调用插件

`disable_fast` 是宿主冻结的请求策略。OpenAI 请求正文在进入适配器前复用原生 Fast 策略处理；
适配器转换为其他上游协议时也须遵守该字段，不能重新启用 Fast / priority 档位

HTTP 请求使用 `call.host.upstream_http(request, body).await?`，返回 `HostHttpResponse`：

```rust,ignore
let response = call.host.upstream_http(UpstreamHttpRequest {
    method: "POST".into(), path: "responses".into(), query: vec![], headers: vec![],
}, encode_request(&call.payload)?).await?;
let status = response.status;
let mut body = response.body;
while let Some(bytes) = body.read().await? {
    // 分段不等于 SSE 事件；插件按自己的上游协议增量解码。
    decode_upstream(status, bytes)?;
}
```

正文与普通 `HostClient::http` 共用 `read`、`collect(maximum_bytes)` 和 `close`；每次读取至多 64 KiB，
不后台预读，不向作者暴露流 ID。提前关闭及收集超限会释放正文，父调用结束或取消时宿主兜底回收。
JSON 可有界收集，SSE 应增量解析，不能把读取块当成完整事件

WebSocket 使用 `call.host.upstream_websocket(request).await?`，返回
`UpstreamWebSocketUpgrade::Connected { headers, connection }` 或 `Rejected { status, headers, body }`。
连接提供 `send(kind, body)`、`read()`、`close()`，支持完整文本与二进制消息，Ping/Pong 由宿主处理。
同一连接一次只执行一个操作，握手拒绝保留 HTTP 状态和正文。底层 `host.upstream.*` 是 SDK 使用的资源协议，
不是额外的业务中间件链

宿主只对声明的基址和精确路径注入已选账号认证，禁止覆盖 Host、认证、Cookie 与握手保护头；
重定向不能携带凭据逃逸。出站复用该账号的代理，代理失败不能回退直连。
`auxiliary` 路径的出站计入请求副作用水位，防止附件上传等操作在失败后被透明重复执行

## 事件、结算和续接

执行返回 `TypedReply<Empty>` 的有界 `ResponseStream`。每个 chunk 使用 `UpstreamAdapterEvent::encode()`，
包含标准事实与可选原始 wire；事件需符合开始、内容、工具、用量和唯一结束的顺序。
原始 JSON / SSE 字节与标准事实分开传递，错误使用 `UpstreamFailure`；宿主网络观测决定发送状态，插件不能将其降为未发送

用量提交标准 `Usage`，费用由内置 Provider 按已冻结的上游模型与价格计算，响应回显模型不切换计价。
`service_tier` 记录上游回显的最新档位，允许上游在完成时确定实际值；OpenAI 计价沿用请求策略，回显档位仅作观测。
未知用量或价格保留未知。宿主确认 RPC 正常结束后才发布完成事实，插件不能独立结算或组织换号重试

成功终态可附带 `UpstreamContinuation`，包含上游响应 ID、不超过 32 KiB 的私有续接材料和作用域：

- `persisted`：续接材料随宿主会话保存，仍受实例、代次、Key、账号和凭据版本限制
- `connection_local`：还必须保有本次已建立的 WebSocket；宿主在成功终态后保存同一连接，续接时独占取回

连接内状态不能跨账号、Key、凭据版本、插件进程或代次复用，失效时拒绝续接，不重新建立连接假装原生续接成功。
每个适配器代次最多保留 64 条闲置连接，闲置期限 30 分钟；保留连接不占用上一轮账号租约。
显式 `close()` 放弃连接内续接。普通成功、错误、取消与退出释放未保留连接
