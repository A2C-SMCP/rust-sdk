use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 基础设施工具模块 / Infrastructure utilities（原子 IO / 环境变量 / 路径；治理层 SET/SKL 共用）。
pub mod utils;

/// SKILL name 解析与合成（段数消歧 lexer）/ SKILL name parse & synthesis (segment-count lexer)。
pub mod skill_name;
pub use skill_name::{
    is_valid_skill_name, normalize_mcp_server_segment, parse_skill_name,
    synthesize_marketplace_name, synthesize_mcp_name, synthesize_user_name, ParsedSkillName,
    SkillNameError, SkillNameKind,
};

/// 协议版本解析与兼容性判定 / Protocol version parse & compatibility。
pub mod version;
pub use version::{
    is_compatible, ProtocolVersion, ProtocolVersionError, ProtocolVersionParseError,
};

/// SMCP协议的命名空间
pub const SMCP_NAMESPACE: &str = "/smcp";

/// A2C-SMCP 协议版本号 / A2C-SMCP protocol version
///
/// 锁定为 `MAJOR.MINOR` = `0.2.0`。SKILL（v0.2.1）与通用二进制传输（v0.2.1）等均为**加性升级**，
/// 不改变 `MAJOR.MINOR`，因此该常量保持 `"0.2.0"`，用于 HTTP 握手阶段的版本协商。
///
/// Locked to `MAJOR.MINOR` = `0.2.0`. SKILL (v0.2.1) and generic binary transfer (v0.2.1) are
/// **additive** upgrades that do not bump `MAJOR.MINOR`; this constant stays `"0.2.0"` and is used
/// for version negotiation during the HTTP handshake.
///
/// 协议依据 / Protocol: `a2c-smcp-protocol` versioning.md。
/// Python 参考 / Python reference: `a2c_smcp/smcp.py`。
pub const PROTOCOL_VERSION: &str = "0.2.0";

/// 标准错误码模块 / Standard error codes module
///
/// ⚠️ 与 [`ErrorCode`] 枚举是**两套有意不合并的命名空间**（对齐 Python `a2c_smcp/smcp.py`，
/// 其 `ErrorCode` 同样不含 4001–4005 / 4101–4104，合并会偏离参考实现）：
/// - 本模块 = **传输/管理层码** + 工具/房间码（400–500、4001–4005、4101–4104）。
/// - [`ErrorCode`] = **协议级闭集**（404、4006–4018），是 [`is_protocol_error_payload`] 识别的集合，
///   也是 `client:*` ack 协议级错误必用的码。
/// - 两者仅 `404` 重合。
pub mod error_codes {
    // 通用错误码 / General error codes
    pub const BAD_REQUEST: i32 = 400;
    pub const UNAUTHORIZED: i32 = 401;
    pub const FORBIDDEN: i32 = 403;
    pub const NOT_FOUND: i32 = 404;
    pub const TIMEOUT: i32 = 408;
    pub const INTERNAL_ERROR: i32 = 500;

    // 工具调用错误码 / Tool call error codes

    /// 工具调用**前**查找失败：目标工具在任一活动 MCP Server 上都不存在（路由 / 注册阶段）。
    /// 仅用于「调用前」的工具解析失败；工具**执行**阶段的失败必须使用
    /// [`TOOL_EXECUTION_FAILED`]（4003），二者语义严格区分。
    ///
    /// Tool lookup failed **before** invocation: the target tool is absent on every active MCP
    /// server (routing / registry stage). Use only for pre-call resolution failures; failures
    /// during tool **execution** MUST use [`TOOL_EXECUTION_FAILED`] (4003).
    pub const TOOL_NOT_FOUND: i32 = 4001;
    pub const TOOL_DISABLED: i32 = 4002;
    /// 工具**执行**失败：工具已成功解析并被调用，但在执行过程中返回错误或抛出异常。
    /// 与查找阶段失败的 [`TOOL_NOT_FOUND`]（4001）严格区分。
    ///
    /// Tool **execution** failed: the tool was resolved and invoked, but returned an error or
    /// raised during execution. Strictly distinct from the lookup-stage [`TOOL_NOT_FOUND`] (4001).
    pub const TOOL_EXECUTION_FAILED: i32 = 4003;
    pub const TOOL_TIMEOUT: i32 = 4004;
    pub const TOOL_REQUIRES_CONFIRMATION: i32 = 4005;

    // 房间管理错误码 / Room management error codes
    pub const ROOM_FULL: i32 = 4101;
    pub const ROOM_NOT_FOUND: i32 = 4102;
    pub const NOT_IN_ROOM: i32 = 4103;
    pub const CROSS_ROOM_ACCESS: i32 = 4104;
}

/// WebSocket 握手版本拒绝的 close code（RFC 6455 私有段 4000–4999）。
///
/// 用于 **WS-only 直连握手**在协议版本不匹配时的拒绝，是服务端运行栈不支持
/// ASGI WebSocket Denial Response 时的回退形态。`4900` 不携带结构化 body
/// （WS close reason ≤123 字节）。
///
/// ⚠️ **MUST NOT** 与 [`ErrorCode::ProtocolVersionMismatch`]（`4008`）混用或互转：
/// - `4008` 是 [`ErrorCode`] 值，作为 `ErrorPayload.code` 承载于 **HTTP 400 body**；
/// - `4900` 是 **WS close code**，不承载结构化 body。
///
/// 二者是不同命名空间、有意取不同数值。协议依据 / Protocol: versioning.md。
///
/// WebSocket close code (RFC 6455 private range 4000–4999) for rejecting a WS-only direct
/// handshake on protocol version mismatch (fallback when the server stack lacks ASGI WebSocket
/// Denial Response). MUST NOT be conflated/converted with [`ErrorCode::ProtocolVersionMismatch`]
/// (`4008`): 4008 is an `ErrorPayload.code` carried in the HTTP 400 body; 4900 is a WS close code
/// with no structured body. Different namespaces, intentionally different values.
pub const WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE: i32 = 4900;

/// A2C-SMCP 协议错误码（v0.2.0 起；v0.2.1 加性追加 `4016` / `4017` / `4018`）。
///
/// 镜像 Python 参考实现 `a2c_smcp/smcp.py::ErrorCode`，**序列化 / 反序列化均为整数**
/// （与协议 `ErrorPayload.code` 的线格式一致）。协议依据 / Protocol: error-handling.md。
///
/// ⚠️ 与 [`error_codes`] 模块区分：本枚举是**协议级闭集**（[`is_protocol_error_payload`] 识别 +
/// `client:*` ack 必用）；[`error_codes`] 是传输/管理层与工具/房间码。两者仅 `404` 重合，有意不合并。
///
/// 语义要点 / Semantics:
/// - [`ErrorCode::NotFound`]（`404`）：通用「资源不存在」。本 SDK 用于 `client:*` 路由层
///   目标 Computer 名未命中（error-handling.md 明确「Computer 不存在」归 404）；镜像协议已有定义，非新增。
/// - [`ErrorCode::McpServerNotFound`]（`4014`）：v0.2.1 复用——SKILL `name` **格式合法但不存在**
///   （未注册 / 已卸载 / 孤儿）复用此码；`name` 格式非法 → [`ErrorCode::SkillNameInvalid`]（`4016`）；
///   `name` 有效但 `rel_path` 不可达 → [`ErrorCode::SkillResourceNotAccessible`]（`4017`）。
/// - SKILL 通道**不使用** `4015`：未声明 `resources` capability 的 server 在物化阶段即被排除，不上送 Agent。
/// - `4017` / `4018` 的 `details.reason` 为**开放枚举**：解析方 MUST 容忍未知值并兜底
///   （默认「不重试 + 诊断」），未来可非破坏地新增 reason。
///
/// Mirrors the Python reference `ErrorCode`; serializes to / from an **integer** matching the
/// protocol `ErrorPayload.code` wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ErrorCode {
    /// 通用「资源不存在」/ Generic "resource not found"（含 `client:*` 路由层 Computer 名未命中）。
    NotFound = 404,
    /// MCP 上游要求授权 / MCP upstream requires authorization。
    ToolAuthorizationRequired = 4006,
    /// MCP 上游授权失败 / MCP upstream authorization failed。
    ToolAuthorizationFailed = 4007,
    /// 协议版本不匹配（承载于 HTTP 400 body）/ Protocol version mismatch (carried in HTTP 400 body)。
    ///
    /// ⚠️ MUST NOT 与 WS close code [`WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE`]（`4900`）混用。
    ProtocolVersionMismatch = 4008,
    /// MCP Server 路由未命中 / MCP server not found（v0.2.1 复用于 SKILL name 合法但 Registry 未命中）。
    McpServerNotFound = 4014,
    /// MCP 能力不支持 / MCP capability not supported。
    McpCapabilityNotSupported = 4015,
    /// SKILL `name` 违反 lexer 规则（格式硬错）/ SKILL name violates lexer rules (hard format error)。
    SkillNameInvalid = 4016,
    /// SKILL 资源不可达 / SKILL resource not accessible（rel_path 穿越 / .skillenv forbidden / not_found / too_large）。
    SkillResourceNotAccessible = 4017,
    /// 二进制 blob 不可达 / Binary blob not accessible（invalid_handle / forbidden / gone / range）。
    BlobNotAccessible = 4018,
}

impl ErrorCode {
    /// 返回错误码的整数值 / Return the integer code value.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// 从整数值解析错误码；未知值返回 `None` / Parse from an integer; unknown values return `None`.
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            404 => Some(Self::NotFound),
            4006 => Some(Self::ToolAuthorizationRequired),
            4007 => Some(Self::ToolAuthorizationFailed),
            4008 => Some(Self::ProtocolVersionMismatch),
            4014 => Some(Self::McpServerNotFound),
            4015 => Some(Self::McpCapabilityNotSupported),
            4016 => Some(Self::SkillNameInvalid),
            4017 => Some(Self::SkillResourceNotAccessible),
            4018 => Some(Self::BlobNotAccessible),
            _ => None,
        }
    }
}

impl Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code = i32::deserialize(deserializer)?;
        Self::from_code(code)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown A2C-SMCP error code: {code}")))
    }
}

/// A2C-SMCP flat 错误负载 / A2C-SMCP flat error payload
///
/// 协议 0.2.2 统一错误形态：顶层 `code`/`message`，诊断信息置于可选的 `details` 容器。
/// **禁止**嵌套 `{"error": {...}}` envelope —— 所有 `client:*` ack 路由的协议级错误 MUST 为本结构。
/// 线格式 / Wire shape: `{ "code": <int>, "message": <str>, "details"?: <object> }`。
///
/// `details` 是诊断容器，Agent **MUST NOT** 原样透传给最终用户（防信息泄露）。
/// `details` is a diagnostic container; the Agent **MUST NOT** propagate it verbatim to end users.
///
/// 协议依据 / Protocol: `a2c-smcp-protocol` error-handling.md（flat ErrorPayload，禁止二次 unwrap）。
/// Python 参考 / Python reference: `a2c_smcp/smcp.py` 的 `ErrorPayload`。
///
/// 协议 0.2.x 对特定码在**顶层平铺**分流字段（对齐 Python `smcp.py` `ErrorPayload`
/// `total=False` TypedDict）：4008（协议版本不兼容）→ `server_version` / `client_version` /
/// `min_supported` / `max_supported`（HS-01 #21 / HS-02 #22，**已落地**，构造见
/// [`ErrorPayload::version_mismatch`]）。
/// 🚧 待补（latent，随对应码代码路径落地）：4014 / 4015 → `mcp_server_name` / `capability`
/// （SRV-01 #47 / AUTH-01 #23）。未知顶层字段在反序列化时被忽略（serde 默认行为）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// 错误码（协议 `ErrorCode` 取值；线格式为裸整数）/ Error code (a protocol `ErrorCode` value; bare int on the wire)
    pub code: i64,
    /// 人类可读的错误描述 / Human-readable error message
    pub message: String,
    /// 诊断容器（可选；为空时不序列化）/ Diagnostic container (optional; skipped when absent)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// 4008 顶层分流：服务端协议版本 / top-level for 4008: server protocol version。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// 4008 顶层分流：客户端协议版本 / top-level for 4008: client protocol version。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// 4008 顶层分流：服务端支持的最小版本 / top-level for 4008: min supported。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_supported: Option<String>,
    /// 4008 顶层分流：服务端支持的最大版本 / top-level for 4008: max supported。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_supported: Option<String>,
}

impl ErrorPayload {
    /// 创建 flat 错误负载 / Create a flat error payload
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            server_version: None,
            client_version: None,
            min_supported: None,
            max_supported: None,
        }
    }

    /// 设置整个 `details` 诊断容器 / Set the whole `details` diagnostic container
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 向 `details` 对象插入单个字段（若 `details` 非对象则重置为对象）
    /// Insert a single field into the `details` object (reset to an object if it is not one)
    pub fn with_detail(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        let mut map = match self.details {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        map.insert(key.into(), value.into());
        self.details = Some(serde_json::Value::Object(map));
        self
    }

    /// 构造 4008（协议版本不兼容）flat ErrorPayload，顶层平铺 4 个版本分流字段。
    ///
    /// `min_supported` / `max_supported` 由 server 版本派生：同 `MAJOR.MINOR` 的整个 PATCH 段
    /// （`{maj}.{min}.0` ～ `{maj}.{min}.999`）。`message` 固定为 `"Protocol version mismatch"`。
    /// 与 Python middleware `_mismatch_body` 逐字段对齐。供 HS-01 (#21) 服务端握手中间件复用。
    pub fn version_mismatch(client: &ProtocolVersion, server: &ProtocolVersion) -> Self {
        Self {
            code: i64::from(ErrorCode::ProtocolVersionMismatch.code()),
            message: "Protocol version mismatch".to_string(),
            details: None,
            server_version: Some(server.to_string()),
            client_version: Some(client.to_string()),
            min_supported: Some(format!("{}.{}.0", server.major, server.minor)),
            max_supported: Some(format!("{}.{}.999", server.major, server.minor)),
        }
    }
}

/// 判定 `value` 是否为协议级 **flat ErrorPayload**：顶层 `code` 属协议错误码、且无嵌套 envelope。
///
/// - flat shape（顶层 `code` 为 [`ErrorCode`] 取值）→ `true`
/// - 嵌套 `{"error": {...}}`、未知码值、缺 `code`、`code` 非整数、或非对象 → `false`
///
/// server（透传判定）与 agent（抛协议错误）共用同一谓词，避免双重启发式漂移。
/// Shared by the server (passthrough decision) and the agent (raise on protocol error) so the two
/// never drift apart heuristically.
///
/// 对标 Python `a2c_smcp/smcp.py` 的 `is_protocol_error_payload`。
/// 协议依据 / Protocol: error-handling.md（禁止对 ack 负载二次 unwrap）。
pub fn is_protocol_error_payload(value: &serde_json::Value) -> bool {
    value
        .as_object()
        .and_then(|obj| obj.get("code"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .is_some_and(|code| ErrorCode::from_code(code).is_some())
}

/// 构造 `client:*` 路由层目标 Computer 名未注册时返回的 flat ErrorPayload(404)。
///
/// 与 Python 实现返回**逐字节一致**的负载（双实现镜像约束）：
/// `{ "code": 404, "message": "Computer with name '<name>' not found", "details": { "computer_name": "<name>" } }`。
///
/// 对标 Python `a2c_smcp/smcp.py` 的 `build_computer_not_found_error`。
/// 协议依据 / Protocol: error-handling.md §404（工具或 Computer 不存在）；所有 `client:*` ack 协议级错误 MUST 为 flat ErrorPayload。
pub fn build_computer_not_found_error(computer_name: &str) -> ErrorPayload {
    ErrorPayload::new(
        i64::from(ErrorCode::NotFound.code()),
        format!("Computer with name '{computer_name}' not found"),
    )
    .with_detail("computer_name", computer_name)
}

/// SMCP事件常量定义
pub mod events {
    /// 客户端请求获取工具列表
    pub const CLIENT_GET_TOOLS: &str = "client:get_tools";
    /// 客户端请求获取配置
    pub const CLIENT_GET_CONFIG: &str = "client:get_config";
    /// 客户端请求获取桌面信息
    pub const CLIENT_GET_DESKTOP: &str = "client:get_desktop";
    /// 客户端工具调用请求
    pub const CLIENT_TOOL_CALL: &str = "client:tool_call";
    /// 客户端通用二进制拉取请求（v0.2.1）/ Generic binary pull (v0.2.1)。
    pub const CLIENT_GET_BLOB: &str = "client:get_blob";

    /// 服务器加入办公室请求
    pub const SERVER_JOIN_OFFICE: &str = "server:join_office";
    /// 服务器离开办公室请求
    pub const SERVER_LEAVE_OFFICE: &str = "server:leave_office";
    /// 服务器更新配置请求
    pub const SERVER_UPDATE_CONFIG: &str = "server:update_config";
    /// 服务器更新工具列表请求
    pub const SERVER_UPDATE_TOOL_LIST: &str = "server:update_tool_list";
    /// 服务器更新桌面请求
    pub const SERVER_UPDATE_DESKTOP: &str = "server:update_desktop";
    /// 服务器取消工具调用请求
    pub const SERVER_TOOL_CALL_CANCEL: &str = "server:tool_call_cancel";
    /// 服务器列出房间请求
    pub const SERVER_LIST_ROOM: &str = "server:list_room";

    /// 通知取消工具调用
    pub const NOTIFY_TOOL_CALL_CANCEL: &str = "notify:tool_call_cancel";
    /// 通知进入办公室
    pub const NOTIFY_ENTER_OFFICE: &str = "notify:enter_office";
    /// 通知离开办公室
    pub const NOTIFY_LEAVE_OFFICE: &str = "notify:leave_office";
    /// 通知更新配置
    pub const NOTIFY_UPDATE_CONFIG: &str = "notify:update_config";
    /// 通知更新工具列表
    pub const NOTIFY_UPDATE_TOOL_LIST: &str = "notify:update_tool_list";
    /// 通知更新桌面
    pub const NOTIFY_UPDATE_DESKTOP: &str = "notify:update_desktop";

    /// 通用通知前缀
    pub const NOTIFY_PREFIX: &str = "notify:";
}

/// 请求ID，使用UUID确保全局唯一性
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReqId(pub String);

impl ReqId {
    /// 生成新的请求ID（使用hex格式以匹配Python的uuid.uuid4().hex）
    pub fn new() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// 从字符串创建请求ID
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// 获取请求ID的字符串引用
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ReqId {
    fn default() -> Self {
        Self::new()
    }
}

/// 角色类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Agent,
    Computer,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Agent => write!(f, "agent"),
            Role::Computer => write!(f, "computer"),
        }
    }
}

/// 用户信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub role: Role,
}

/// 工具调用请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    pub computer: String,
    pub tool_name: String,
    pub params: serde_json::Value,
    pub timeout: i32,
}

/// 获取计算机配置请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetComputerConfigReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    pub computer: String,
}

/// 更新计算机配置请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateComputerConfigReq {
    pub computer: String,
}

/// 获取计算机配置返回
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetComputerConfigRet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<serde_json::Value>>,
    pub servers: serde_json::Value,
}

/// 工具调用返回（符合 MCP CallToolResult 标准）
///
/// `meta` 为**结果级**元数据（线上 key 为 `meta`），承载 0.2.2 的 `a2c_*` 取消/超时标记
/// （[`tool_meta`] + [`ToolCallRet::mark_cancelled`] / [`ToolCallRet::mark_timeout`]）。
/// 二进制旁路句柄属**子级**标记，落在 content item 的 `_meta`（见 [`set_content_blob_sideband`]），
/// **不**放结果级 `meta`——区分见 data-structures.md §结果级 meta vs 子级 _meta。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub req_id: Option<ReqId>,
    /// 结果级 meta（承载 `a2c_cancelled` / `a2c_cancel_reason` / `a2c_timeout` 等）。
    ///
    /// 读写契约 / Read-write contract：标记落**结果级 `meta`**（A2C producer **MUST** 写此处，
    /// data-structures.md §结果级 `meta` vs 子级 `_meta`）。consumer reader（[`ToolCallRet::is_cancelled`]
    /// 等）据此读 `meta`。协议另有 SHOULD——consumer 宜对结果级 `_meta` 线上 key 兜底宽松读取（优先 `meta`）；
    /// 🚧 本结构暂只认 `meta`（当前无 producer 写结果级 `_meta`，Python 连 consumer 侧都未实现，见 #42 跟进），
    /// result-级 `_meta` 兜底**待补**，以免半实现 churn。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// CallToolResult 的 A2C 标记键 / A2C marker keys for CallToolResult。
///
/// 落位规则（data-structures.md §结果级 meta vs 子级 _meta）：
/// - **结果级**（`CallToolResult.meta`）：取消 / 超时标记 `a2c_cancelled` / `a2c_cancel_reason` / `a2c_timeout`。
/// - **子级**（content item `_meta`）：二进制旁路 `a2c_blob_handle` / `a2c_total_size` / `a2c_sha256`。
///
/// 对标 Python `a2c_smcp`（agent `_blob_sideband` / computer `socketio/client`）的同名键。
pub mod tool_meta {
    /// 结果级：取消标记（取消时 MUST = `true`）。
    pub const A2C_CANCELLED_KEY: &str = "a2c_cancelled";
    /// 结果级：取消原因（SHOULD，可选；当前恒为 [`A2C_DEFAULT_CANCEL_REASON`]）。
    pub const A2C_CANCEL_REASON_KEY: &str = "a2c_cancel_reason";
    /// 结果级：超时标记（超时时 SHOULD = `true`）。
    pub const A2C_TIMEOUT_KEY: &str = "a2c_timeout";
    /// `a2c_cancel_reason` 的现阶段恒定取值 / The current fixed cancel reason。
    pub const A2C_DEFAULT_CANCEL_REASON: &str = "agent_requested";
    /// 子级（content item `_meta`）：二进制旁路句柄。
    pub const A2C_BLOB_HANDLE_KEY: &str = "a2c_blob_handle";
    /// 子级：旁路资源总字节数。
    pub const A2C_TOTAL_SIZE_KEY: &str = "a2c_total_size";
    /// 子级：旁路资源全量 sha256（十六进制）。
    pub const A2C_SHA256_KEY: &str = "a2c_sha256";
}

impl ToolCallRet {
    fn meta_object_mut(&mut self) -> &mut serde_json::Map<String, serde_json::Value> {
        if !matches!(self.meta, Some(serde_json::Value::Object(_))) {
            self.meta = Some(serde_json::Value::Object(serde_json::Map::new()));
        }
        match self.meta.as_mut() {
            Some(serde_json::Value::Object(map)) => map,
            _ => unreachable!("meta 刚被规整为对象"),
        }
    }

    /// 标记取消（结果级 `meta.a2c_cancelled = true` + `a2c_cancel_reason`）。
    ///
    /// 取消语义 MUST `a2c_cancelled=true`；`reason` 缺省 [`tool_meta::A2C_DEFAULT_CANCEL_REASON`]。
    pub fn mark_cancelled(&mut self, reason: Option<&str>) {
        let reason = reason
            .unwrap_or(tool_meta::A2C_DEFAULT_CANCEL_REASON)
            .to_string();
        let map = self.meta_object_mut();
        map.insert(tool_meta::A2C_CANCELLED_KEY.to_string(), true.into());
        map.insert(tool_meta::A2C_CANCEL_REASON_KEY.to_string(), reason.into());
    }

    /// 标记超时（结果级 `meta.a2c_timeout = true`）/ 超时语义 SHOULD `a2c_timeout=true`。
    pub fn mark_timeout(&mut self) {
        self.meta_object_mut()
            .insert(tool_meta::A2C_TIMEOUT_KEY.to_string(), true.into());
    }

    /// 是否被取消（读结果级 `meta.a2c_cancelled`）/ Whether cancelled。
    pub fn is_cancelled(&self) -> bool {
        meta_bool(self.meta.as_ref(), tool_meta::A2C_CANCELLED_KEY)
    }

    /// 取消原因（读结果级 `meta.a2c_cancel_reason`）/ Cancel reason if present。
    pub fn cancel_reason(&self) -> Option<&str> {
        self.meta
            .as_ref()
            .and_then(|m| m.get(tool_meta::A2C_CANCEL_REASON_KEY))
            .and_then(serde_json::Value::as_str)
    }

    /// 是否超时（读结果级 `meta.a2c_timeout`）/ Whether timed out。
    pub fn is_timeout(&self) -> bool {
        meta_bool(self.meta.as_ref(), tool_meta::A2C_TIMEOUT_KEY)
    }
}

fn meta_bool(meta: Option<&serde_json::Value>, key: &str) -> bool {
    meta.and_then(|m| m.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// content item 的二进制旁路三元组 / The binary-sideband triple on a content item。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobSideband {
    /// 旁路句柄 / sideband handle（`_meta.a2c_blob_handle`）。
    pub blob_handle: BlobHandle,
    /// 全量字节数 / total bytes（`_meta.a2c_total_size`，可选）。
    pub total_size: Option<u64>,
    /// 全量 sha256 / full-content sha256（`_meta.a2c_sha256`，可选）。
    pub sha256: Option<String>,
}

/// 在 content item 上写入二进制旁路 / Write the binary sideband onto a content item。
///
/// 写 `item._meta.a2c_blob_handle`（+ 可选 `a2c_total_size` / `a2c_sha256`）。`item` 非对象时不动。
/// 对标 Python computer `socketio/client.py` 的旁路写入。
pub fn set_content_blob_sideband(
    item: &mut serde_json::Value,
    blob_handle: &str,
    total_size: Option<u64>,
    sha256: Option<&str>,
) {
    let serde_json::Value::Object(obj) = item else {
        return;
    };
    let meta = obj
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !meta.is_object() {
        *meta = serde_json::Value::Object(serde_json::Map::new());
    }
    if let serde_json::Value::Object(meta) = meta {
        meta.insert(
            tool_meta::A2C_BLOB_HANDLE_KEY.to_string(),
            blob_handle.into(),
        );
        if let Some(size) = total_size {
            meta.insert(tool_meta::A2C_TOTAL_SIZE_KEY.to_string(), size.into());
        }
        if let Some(hash) = sha256 {
            meta.insert(tool_meta::A2C_SHA256_KEY.to_string(), hash.into());
        }
    }
}

/// 读取 content item 的二进制旁路 / Read the binary sideband off a content item。
///
/// 命中 `item._meta.a2c_blob_handle`（非空字符串）→ `Some`；否则 `None`。对标 Python agent
/// `_blob_sideband.py` 的旁路枚举。
pub fn read_content_blob_sideband(item: &serde_json::Value) -> Option<BlobSideband> {
    let meta = item.get("_meta")?;
    let blob_handle = meta
        .get(tool_meta::A2C_BLOB_HANDLE_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(BlobSideband {
        blob_handle: blob_handle.to_string(),
        total_size: meta
            .get(tool_meta::A2C_TOTAL_SIZE_KEY)
            .and_then(serde_json::Value::as_u64),
        sha256: meta
            .get(tool_meta::A2C_SHA256_KEY)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// 获取工具请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetToolsReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    pub computer: String,
}

/// SMCP工具定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SMCPTool {
    pub name: String,
    pub description: String,
    pub params_schema: serde_json::Value,
    pub return_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// 获取工具返回
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetToolsRet {
    pub tools: Vec<SMCPTool>,
    pub req_id: ReqId,
}

/// 代理调用数据（基类）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCallData {
    pub agent: String,
    pub req_id: ReqId,
}

/// 进入办公室请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterOfficeReq {
    pub role: Role,
    pub name: String,
    pub office_id: String,
}

/// 离开办公室请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveOfficeReq {
    pub office_id: String,
}

/// 获取桌面请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDesktopReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    pub computer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_size: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

/// 桌面类型别名
pub type Desktop = String;

/// 获取桌面返回
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDesktopRet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktops: Option<Vec<Desktop>>,
    pub req_id: ReqId,
}

/// 列出房间请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRoomReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    pub office_id: String,
}

/// 会话信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub sid: String,
    pub name: String,
    pub role: Role,
    pub office_id: String,
}

/// 列出房间返回
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRoomRet {
    pub sessions: Vec<SessionInfo>,
    pub req_id: ReqId,
}

/// 进入办公室通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterOfficeNotification {
    pub office_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// 离开办公室通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveOfficeNotification {
    pub office_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// 更新MCP配置通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateMCPConfigNotification {
    pub computer: String,
}

/// 更新工具列表通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateToolListNotification {
    pub computer: String,
}

/// 通知类型枚举
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Notification {
    ToolCallCancel,
    EnterOffice(EnterOfficeNotification),
    LeaveOffice(LeaveOfficeNotification),
    UpdateMCPConfig(UpdateMCPConfigNotification),
    UpdateToolList(UpdateToolListNotification),
    UpdateDesktop,
}

// ── 通用二进制传输 / Generic binary transfer (v0.2.1) ──────────────────────
// 协议依据 / Protocol: blob-transfer.md（句柄契约 / 生产者-消费者模型 / 安全模型）；
// data-structures.md §GetBlobReq / §GetBlobRet。任何通道把大/二进制内容以句柄旁路，Agent 经
// 统一 `client:get_blob` 无状态分块拉取，`sha256` 自证完整性。
// Python 参考 / reference: a2c_smcp/smcp.py（BlobHandle / GetBlobReq / GetBlobRet）。

/// `client:get_blob` 事件名 / event name（顶层别名，等于 [`events::CLIENT_GET_BLOB`]）。
///
/// 对齐 Python `a2c_smcp.smcp.GET_BLOB_EVENT`。
pub const GET_BLOB_EVENT: &str = events::CLIENT_GET_BLOB;

/// 不透明、Computer 铸造、无状态可重解析的 blob 句柄（类型别名）。
/// Opaque, Computer-minted, stateless-reparseable blob handle (type alias)。
///
/// Agent **MUST** 视为不透明字符串：**MUST NOT** 解析 / 拼接 / 伪造 / 跨 Computer 复用
/// （blob-transfer.md §1）。
pub type BlobHandle = String;

/// 通用二进制拉取请求 / Generic binary pull request（`client:get_blob`）。
///
/// Computer 无服务端状态、幂等可并行（`chunk_offset` 为资源**解码后字节**的绝对偏移）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetBlobReq {
    #[serde(flatten)]
    pub base: AgentCallData,
    /// 目标 Computer 名 / target Computer name。
    pub computer: String,
    /// 来自某通道响应的不透明句柄 / opaque handle from a producer response。
    pub blob_handle: BlobHandle,
    /// 资源字节绝对偏移；**缺省 0**（无状态幂等 → 可续传 / 重试 / 并行）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_offset: Option<u64>,
    /// 客户建议单块上限（字节）；**缺省时由 Computer clamp 决定**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chunk_bytes: Option<u64>,
}

/// 通用二进制拉取响应 / Generic binary pull response（`GetBlobRet`）。
///
/// 完整性铁律：`eof` ⟺ `chunk_offset + len(decode(blob)) == total_size`；`eof` 后 Agent SHOULD
/// 校验重组 `sha256`；一次逻辑读取内 `sha256` / `total_size` MUST 稳定。空资源 = `total_size=0`，
/// 单响应 `eof=true`、`blob=""`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetBlobRet {
    /// 回显的句柄 / echoed handle。
    pub blob_handle: BlobHandle,
    /// 资源 MIME（可选）/ resource MIME (optional)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// 资源总字节数（首块即知；一次读取内恒定）/ total bytes。
    pub total_size: u64,
    /// 全量资源 sha256 十六进制（跨块恒定）/ full-content sha256 (hex)。
    pub sha256: String,
    /// 本块起始字节偏移 / this chunk's start byte offset。
    pub chunk_offset: u64,
    /// 末块标记 / end-of-file marker（⟺ `chunk_offset + 本块字节数 == total_size`）。
    pub eof: bool,
    /// 本块字节的 base64 编码 / base64 of this chunk's bytes。
    pub blob: String,
    /// 回显的请求 id（可选）/ echoed request id (optional)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_id: Option<ReqId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_req_id_helpers() {
        let req_id = ReqId::new();
        assert!(!req_id.as_str().is_empty());

        let req_id2 = ReqId::from_string("abc".to_string());
        assert_eq!(req_id2.as_str(), "abc");

        let req_id3 = ReqId::default();
        assert!(!req_id3.as_str().is_empty());
    }

    #[test]
    fn test_role_serde_lowercase() {
        let json = serde_json::to_string(&Role::Agent).unwrap();
        assert_eq!(json, "\"agent\"");

        let de: Role = serde_json::from_str("\"computer\"").unwrap();
        assert!(matches!(de, Role::Computer));
    }

    #[test]
    fn test_notification_serde() {
        let n = Notification::EnterOffice(EnterOfficeNotification {
            office_id: "office1".to_string(),
            computer: Some("c1".to_string()),
            agent: None,
        });

        let json = serde_json::to_string(&n).unwrap();
        let de: Notification = serde_json::from_str(&json).unwrap();
        match de {
            Notification::EnterOffice(p) => {
                assert_eq!(p.office_id, "office1");
                assert_eq!(p.computer.as_deref(), Some("c1"));
                assert!(p.agent.is_none());
            }
            _ => panic!("unexpected notification"),
        }
    }

    #[test]
    fn test_tool_call_ret_mcp_format() {
        // 测试成功的工具调用返回（MCP CallToolResult 格式）
        let success_ret = ToolCallRet {
            content: Some(vec![serde_json::json!({
                "type": "text",
                "text": "Operation completed successfully"
            })]),
            is_error: Some(false),
            req_id: Some(ReqId::from_string("test123".to_string())),
            meta: None,
        };

        let json = serde_json::to_string(&success_ret).unwrap();

        // 验证 JSON 包含正确的 MCP 字段
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("content").is_some());
        assert!(parsed.get("isError").is_some());
        assert_eq!(parsed.get("isError").unwrap(), false);
        assert_eq!(parsed.get("req_id").unwrap().as_str().unwrap(), "test123");

        // 验证字段名是 camelCase（isError 而不是 is_error）
        assert!(json.contains("isError"));
        assert!(!json.contains("is_error"));
        // 验证没有旧的 Rust 风格字段（检查字段名而不是整个字符串）
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("success").is_none());
        assert!(parsed.get("result").is_none());
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn test_tool_call_ret_error_format() {
        // 测试错误的工具调用返回
        let error_ret = ToolCallRet {
            content: Some(vec![serde_json::json!({
                "type": "text",
                "text": "Tool execution failed"
            })]),
            is_error: Some(true),
            req_id: None,
            meta: None,
        };

        let json = serde_json::to_string(&error_ret).unwrap();

        // 验证 JSON 格式
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("content").is_some());
        assert_eq!(parsed.get("isError").unwrap(), true);
        assert!(parsed.get("req_id").is_none());

        // 验证没有旧的 Rust 风格字段
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("success").is_none());
        assert!(parsed.get("result").is_none());
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn test_tool_call_ret_minimal() {
        // 测试最小化的工具调用返回
        let minimal_ret = ToolCallRet {
            content: None,
            is_error: None,
            req_id: None,
            meta: None,
        };

        let json = serde_json::to_string(&minimal_ret).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // 空对象应该序列化为 {}
        assert_eq!(parsed, serde_json::json!({}));
    }

    #[test]
    fn test_tool_call_ret_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = ToolCallRet {
            content: Some(vec![serde_json::json!({
                "type": "text",
                "text": "Test result"
            })]),
            is_error: Some(false),
            req_id: Some(ReqId::new()),
            meta: None,
        };

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ToolCallRet = serde_json::from_str(&json).unwrap();

        assert_eq!(original.content, deserialized.content);
        assert_eq!(original.is_error, deserialized.is_error);
        assert_eq!(original.req_id, deserialized.req_id);
    }

    #[test]
    fn test_error_payload_flat_serialization() {
        // flat 形态：顶层 code/message，无嵌套 "error" envelope；details 为 None 时不序列化
        let payload = ErrorPayload::new(404, "Resource not found");
        let v = serde_json::to_value(&payload).unwrap();

        assert!(v.get("error").is_none(), "禁止嵌套 envelope"); // 顶层平铺
        assert_eq!(v.get("code").unwrap(), 404);
        assert_eq!(v.get("message").unwrap(), "Resource not found");
        assert!(v.get("details").is_none()); // 没有 details 时不序列化
    }

    #[test]
    fn test_error_payload_with_details_serialization() {
        // details 以对象形式平铺在顶层 details 容器内
        let payload = ErrorPayload::new(4014, "boom")
            .with_detail("mcp_server_name", "srv-a")
            .with_detail("hint", "retry");
        let v = serde_json::to_value(&payload).unwrap();

        assert_eq!(v["code"], 4014);
        assert_eq!(v["details"]["mcp_server_name"], "srv-a");
        assert_eq!(v["details"]["hint"], "retry");
    }

    #[test]
    fn test_is_protocol_error_payload_flat_true() {
        // 顶层 code 为协议 ErrorCode 取值 → true
        for code in [404, 4006, 4007, 4008, 4014, 4015, 4016, 4017, 4018] {
            let v = serde_json::json!({ "code": code, "message": "x" });
            assert!(
                is_protocol_error_payload(&v),
                "code {code} 应判定为协议错误负载"
            );
        }
        // 构造器产出的 payload 同样应判定为 true
        let v = serde_json::to_value(build_computer_not_found_error("c1")).unwrap();
        assert!(is_protocol_error_payload(&v));
    }

    #[test]
    fn test_is_protocol_error_payload_false() {
        // 嵌套 envelope → false（禁止二次 unwrap 的关键防线）
        let nested = serde_json::json!({ "error": { "code": 404, "message": "x" } });
        assert!(!is_protocol_error_payload(&nested));

        // 非协议码（legacy 服务内部码 400 / 未知码 9999）→ false
        assert!(!is_protocol_error_payload(
            &serde_json::json!({ "code": 400 })
        ));
        assert!(!is_protocol_error_payload(
            &serde_json::json!({ "code": 9999 })
        ));

        // 缺 code / code 非整数 / 非对象 → false
        assert!(!is_protocol_error_payload(
            &serde_json::json!({ "message": "x" })
        ));
        assert!(!is_protocol_error_payload(
            &serde_json::json!({ "code": "404" })
        ));
        assert!(!is_protocol_error_payload(&serde_json::json!([
            "code", 404
        ])));
        assert!(!is_protocol_error_payload(&serde_json::json!("nope")));
    }

    #[test]
    fn test_build_computer_not_found_error() {
        let payload = build_computer_not_found_error("my-computer");
        assert_eq!(payload.code, 404);
        assert_eq!(payload.code, i64::from(ErrorCode::NotFound.code()));
        assert!(payload.message.contains("my-computer"));
        assert_eq!(
            payload
                .details
                .as_ref()
                .unwrap()
                .get("computer_name")
                .unwrap(),
            "my-computer"
        );
    }

    #[test]
    fn test_build_computer_not_found_error_python_byte_compat() {
        // 与 Python build_computer_not_found_error 逐字节一致（同字段名 / 层级 / 取值）
        let v = serde_json::to_value(build_computer_not_found_error("c1")).unwrap();
        let expected = serde_json::json!({
            "code": 404,
            "message": "Computer with name 'c1' not found",
            "details": { "computer_name": "c1" }
        });
        assert_eq!(v, expected);
    }

    #[test]
    fn test_error_payload_roundtrip() {
        // ErrorPayload 对任意 code 通用；序列化/反序列化往返一致
        let original = ErrorPayload::new(500, "Internal error").with_detail("trace_id", "abc123");

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ErrorPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_protocol_version_constant() {
        // PROTOCOL_VERSION 锁定 MAJOR.MINOR = 0.2.0（SKILL/blob 为加性升级，不改主次版本）
        assert_eq!(PROTOCOL_VERSION, "0.2.0");
    }

    #[test]
    fn test_error_code_values() {
        // 枚举值与协议 error-handling.md / Python ErrorCode 全表完全一致
        assert_eq!(ErrorCode::NotFound.code(), 404);
        assert_eq!(ErrorCode::ToolAuthorizationRequired.code(), 4006);
        assert_eq!(ErrorCode::ToolAuthorizationFailed.code(), 4007);
        assert_eq!(ErrorCode::ProtocolVersionMismatch.code(), 4008);
        assert_eq!(ErrorCode::McpServerNotFound.code(), 4014);
        assert_eq!(ErrorCode::McpCapabilityNotSupported.code(), 4015);
        assert_eq!(ErrorCode::SkillNameInvalid.code(), 4016);
        assert_eq!(ErrorCode::SkillResourceNotAccessible.code(), 4017);
        assert_eq!(ErrorCode::BlobNotAccessible.code(), 4018);
    }

    #[test]
    fn test_error_code_serializes_as_int() {
        // 必须序列化为裸整数（而非字符串 / 标签对象）
        assert_eq!(
            serde_json::to_string(&ErrorCode::ProtocolVersionMismatch).unwrap(),
            "4008"
        );
        assert_eq!(serde_json::to_string(&ErrorCode::NotFound).unwrap(), "404");

        // 作为结构体字段时同样是裸整数（对齐 ErrorPayload.code 线格式）
        let v = serde_json::json!({ "code": ErrorCode::BlobNotAccessible });
        assert_eq!(v["code"], serde_json::json!(4018));
    }

    #[test]
    fn test_error_code_deserializes_from_int() {
        let c: ErrorCode = serde_json::from_str("4014").unwrap();
        assert_eq!(c, ErrorCode::McpServerNotFound);
        // 未知码值必须报错（解析方对已知集合是封闭的）
        assert!(serde_json::from_str::<ErrorCode>("9999").is_err());
        assert_eq!(ErrorCode::from_code(9999), None);
    }

    #[test]
    fn test_error_code_int_roundtrip() {
        for code in [
            ErrorCode::NotFound,
            ErrorCode::ToolAuthorizationRequired,
            ErrorCode::ToolAuthorizationFailed,
            ErrorCode::ProtocolVersionMismatch,
            ErrorCode::McpServerNotFound,
            ErrorCode::McpCapabilityNotSupported,
            ErrorCode::SkillNameInvalid,
            ErrorCode::SkillResourceNotAccessible,
            ErrorCode::BlobNotAccessible,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, back);
            assert_eq!(ErrorCode::from_code(code.code()), Some(code));
        }
    }

    #[test]
    fn test_ws_close_code_distinct_from_protocol_mismatch() {
        // 4900（WS close code）与 4008（ErrorPayload.code）是不同命名空间的不同值，MUST NOT 混用
        assert_eq!(WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE, 4900);
        assert_eq!(ErrorCode::ProtocolVersionMismatch.code(), 4008);
        assert_ne!(
            WS_VERSION_HANDSHAKE_REJECTED_CLOSE_CODE,
            ErrorCode::ProtocolVersionMismatch.code()
        );
    }

    #[test]
    fn test_tool_lookup_vs_execution_boundary() {
        // 4001/4003 语义边界：调用前查找失败 vs 执行失败，必须是不同的码
        assert_eq!(error_codes::TOOL_NOT_FOUND, 4001);
        assert_eq!(error_codes::TOOL_EXECUTION_FAILED, 4003);
        assert_ne!(
            error_codes::TOOL_NOT_FOUND,
            error_codes::TOOL_EXECUTION_FAILED
        );
    }

    // ── #39 通用二进制传输 / Generic binary transfer ──────────────────────

    #[test]
    fn test_get_blob_event_constant() {
        // GET_BLOB_EVENT 字符串精确匹配，且与 events 模块同源
        assert_eq!(GET_BLOB_EVENT, "client:get_blob");
        assert_eq!(GET_BLOB_EVENT, events::CLIENT_GET_BLOB);
    }

    fn sample_blob_req(chunk_offset: Option<u64>, max_chunk_bytes: Option<u64>) -> GetBlobReq {
        GetBlobReq {
            base: AgentCallData {
                agent: "agent-1".to_string(),
                req_id: ReqId::from_string("r1".to_string()),
            },
            computer: "comp-1".to_string(),
            blob_handle: "opaque-handle".to_string(),
            chunk_offset,
            max_chunk_bytes,
        }
    }

    #[test]
    fn test_get_blob_req_with_offset_serde() {
        let req = sample_blob_req(Some(1024), Some(65536));
        let v = serde_json::to_value(&req).unwrap();
        // flatten base（agent / req_id）+ computer + blob_handle 平铺在顶层
        assert_eq!(v["agent"], "agent-1");
        assert_eq!(v["req_id"], "r1");
        assert_eq!(v["computer"], "comp-1");
        assert_eq!(v["blob_handle"], "opaque-handle");
        assert_eq!(v["chunk_offset"], 1024);
        assert_eq!(v["max_chunk_bytes"], 65536);
        let back: GetBlobReq = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn test_get_blob_req_without_offset_omits_optional_keys() {
        let req = sample_blob_req(None, None);
        let v = serde_json::to_value(&req).unwrap();
        // 可选字段 None → skip_serializing_if 不出现在线格式
        assert!(
            v.get("chunk_offset").is_none(),
            "缺省 chunk_offset 不应序列化"
        );
        assert!(
            v.get("max_chunk_bytes").is_none(),
            "缺省 max_chunk_bytes 不应序列化"
        );
        // 反序列化缺省 → None（缺省 offset 语义 = 0，由消费方解释）
        let back: GetBlobReq = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
        assert!(back.chunk_offset.is_none());
    }

    #[test]
    fn test_get_blob_ret_first_and_last_chunk_roundtrip() {
        // 首块（eof=false）
        let first = GetBlobRet {
            blob_handle: "h".to_string(),
            mime_type: Some("application/octet-stream".to_string()),
            total_size: 10,
            sha256: "abc123".to_string(),
            chunk_offset: 0,
            eof: false,
            blob: "aGVsbG8=".to_string(), // base64("hello")
            req_id: Some(ReqId::from_string("r1".to_string())),
        };
        let back: GetBlobRet =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(back, first);
        assert!(!back.eof);

        // 末块（eof=true）+ 省略 mime_type / req_id（可选）
        let last = GetBlobRet {
            blob_handle: "h".to_string(),
            mime_type: None,
            total_size: 10,
            sha256: "abc123".to_string(),
            chunk_offset: 5,
            eof: true,
            blob: "d29ybGQ=".to_string(),
            req_id: None,
        };
        let v = serde_json::to_value(&last).unwrap();
        assert!(v.get("mime_type").is_none());
        assert!(v.get("req_id").is_none());
        assert_eq!(v["eof"], true);
        let back: GetBlobRet = serde_json::from_value(v).unwrap();
        assert_eq!(back, last);
    }

    // ── #42 CallToolResult meta / _meta 标记 ─────────────────────────────

    fn empty_ret() -> ToolCallRet {
        ToolCallRet {
            content: None,
            is_error: Some(true),
            req_id: None,
            meta: None,
        }
    }

    #[test]
    fn test_mark_cancelled_default_and_custom_reason() {
        // 默认原因 = agent_requested
        let mut ret = empty_ret();
        ret.mark_cancelled(None);
        assert!(ret.is_cancelled());
        assert_eq!(ret.cancel_reason(), Some("agent_requested"));
        // 结果级 meta 顶层 key 为 "meta"
        let v = serde_json::to_value(&ret).unwrap();
        assert_eq!(v["meta"]["a2c_cancelled"], true);
        assert_eq!(v["meta"]["a2c_cancel_reason"], "agent_requested");

        // 自定义原因
        let mut ret2 = empty_ret();
        ret2.mark_cancelled(Some("operator_abort"));
        assert_eq!(ret2.cancel_reason(), Some("operator_abort"));
    }

    #[test]
    fn test_mark_timeout() {
        let mut ret = empty_ret();
        assert!(!ret.is_timeout());
        ret.mark_timeout();
        assert!(ret.is_timeout());
        assert!(!ret.is_cancelled()); // 超时 ≠ 取消
        let v = serde_json::to_value(&ret).unwrap();
        assert_eq!(v["meta"]["a2c_timeout"], true);
    }

    #[test]
    fn test_content_blob_sideband_placement_and_roundtrip() {
        // 二进制旁路是**子级** _meta，落在 content item，而非结果级 meta
        let mut item = serde_json::json!({ "type": "text", "text": "" });
        set_content_blob_sideband(&mut item, "blob-xyz", Some(2048), Some("deadbeef"));
        assert_eq!(item["_meta"]["a2c_blob_handle"], "blob-xyz");
        assert_eq!(item["_meta"]["a2c_total_size"], 2048);
        assert_eq!(item["_meta"]["a2c_sha256"], "deadbeef");

        let sb = read_content_blob_sideband(&item).expect("应读到旁路句柄");
        assert_eq!(sb.blob_handle, "blob-xyz");
        assert_eq!(sb.total_size, Some(2048));
        assert_eq!(sb.sha256.as_deref(), Some("deadbeef"));

        // 无 _meta.a2c_blob_handle → None
        let plain = serde_json::json!({ "type": "text", "text": "hi" });
        assert!(read_content_blob_sideband(&plain).is_none());
    }

    #[test]
    fn test_result_meta_vs_child_meta_separation() {
        // 取消标记落结果级 meta；blob 句柄落 content item _meta —— 两层不混淆
        let mut item = serde_json::json!({ "type": "text", "text": "" });
        set_content_blob_sideband(&mut item, "h", None, None);
        let mut ret = ToolCallRet {
            content: Some(vec![item]),
            is_error: Some(true),
            req_id: None,
            meta: None,
        };
        ret.mark_cancelled(None);

        let v = serde_json::to_value(&ret).unwrap();
        // 结果级 meta 只有取消标记，没有 blob 句柄
        assert_eq!(v["meta"]["a2c_cancelled"], true);
        assert!(v["meta"].get("a2c_blob_handle").is_none());
        // content item 的 _meta 只有 blob 句柄，没有取消标记
        assert_eq!(v["content"][0]["_meta"]["a2c_blob_handle"], "h");
        assert!(v["content"][0]["_meta"].get("a2c_cancelled").is_none());
    }

    #[test]
    fn test_tool_call_ret_deserialize_external_meta() {
        // 反向：消费外部形态 wire JSON（producer 把标记写在结果级 meta）→ reader 正确读取
        let cancelled_wire = r#"{"content":[{"type":"text","text":"cancelled"}],"isError":true,"meta":{"a2c_cancelled":true,"a2c_cancel_reason":"agent_requested"}}"#;
        let ret: ToolCallRet = serde_json::from_str(cancelled_wire).unwrap();
        assert!(ret.is_cancelled());
        assert_eq!(ret.cancel_reason(), Some("agent_requested"));
        assert!(!ret.is_timeout());

        let timeout_wire = r#"{"isError":true,"meta":{"a2c_timeout":true}}"#;
        let ret2: ToolCallRet = serde_json::from_str(timeout_wire).unwrap();
        assert!(ret2.is_timeout());
        assert!(!ret2.is_cancelled());
        assert_eq!(ret2.cancel_reason(), None);

        // 无 meta → 全 false（不 panic）
        let plain: ToolCallRet = serde_json::from_str(r#"{"isError":false}"#).unwrap();
        assert!(!plain.is_cancelled() && !plain.is_timeout());
    }
}
