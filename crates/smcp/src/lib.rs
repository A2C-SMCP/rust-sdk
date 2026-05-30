use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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

/// 错误详情结构 / Error detail structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// 错误码 / Error code
    pub code: i32,
    /// 人类可读的错误描述 / Human readable error message
    pub message: String,
    /// 结构化调试信息（可选）/ Structured debug info (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<HashMap<String, serde_json::Value>>,
}

impl ErrorDetail {
    /// 创建新的错误详情 / Create new error detail
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// 添加详情字段 / Add detail field
    pub fn with_detail(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        let details = self.details.get_or_insert_with(HashMap::new);
        details.insert(key.into(), value.into());
        self
    }

    /// 添加多个详情字段 / Add multiple detail fields
    pub fn with_details(mut self, details: HashMap<String, serde_json::Value>) -> Self {
        self.details = Some(details);
        self
    }
}

/// 标准错误响应格式 / Standard error response format
/// 格式: { "error": { "code": i32, "message": str, "details": object? } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// 错误详情 / Error detail
    pub error: ErrorDetail,
}

impl ErrorResponse {
    /// 创建新的错误响应 / Create new error response
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail::new(code, message),
        }
    }

    /// 添加详情字段 / Add detail field
    pub fn with_detail(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.error = self.error.with_detail(key, value);
        self
    }

    // 便捷构造方法 / Convenience constructors

    /// Bad Request 错误 / Bad Request error
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(error_codes::BAD_REQUEST, message)
    }

    /// Unauthorized 错误 / Unauthorized error
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(error_codes::UNAUTHORIZED, message)
    }

    /// Forbidden 错误 / Forbidden error
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(error_codes::FORBIDDEN, message)
    }

    /// Not Found 错误 / Not Found error
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(error_codes::NOT_FOUND, message)
    }

    /// Timeout 错误 / Timeout error
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(error_codes::TIMEOUT, message)
    }

    /// Internal Error 错误 / Internal Error error
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(error_codes::INTERNAL_ERROR, message)
    }

    /// Tool Not Found 错误 / Tool Not Found error
    pub fn tool_not_found(tool_name: impl Into<String>) -> Self {
        let name = tool_name.into();
        Self::new(
            error_codes::TOOL_NOT_FOUND,
            format!("Tool '{}' not found", name),
        )
        .with_detail("tool_name", serde_json::Value::String(name))
    }

    /// Tool Execution Failed 错误 / Tool Execution Failed error
    pub fn tool_execution_failed(message: impl Into<String>) -> Self {
        Self::new(error_codes::TOOL_EXECUTION_FAILED, message)
    }

    /// Tool Timeout 错误 / Tool Timeout error
    pub fn tool_timeout(timeout_secs: u64) -> Self {
        Self::new(
            error_codes::TOOL_TIMEOUT,
            format!("Tool execution timed out after {} seconds", timeout_secs),
        )
        .with_detail(
            "timeout",
            serde_json::Value::Number(serde_json::Number::from(timeout_secs)),
        )
    }

    /// Room Full 错误 / Room Full error
    pub fn room_full(office_id: impl Into<String>) -> Self {
        let id = office_id.into();
        Self::new(
            error_codes::ROOM_FULL,
            format!("Room '{}' already has an agent", id),
        )
        .with_detail("office_id", serde_json::Value::String(id))
    }

    /// Not In Room 错误 / Not In Room error
    pub fn not_in_room() -> Self {
        Self::new(error_codes::NOT_IN_ROOM, "Session is not in any room")
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub req_id: Option<ReqId>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        };

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ToolCallRet = serde_json::from_str(&json).unwrap();

        assert_eq!(original.content, deserialized.content);
        assert_eq!(original.is_error, deserialized.is_error);
        assert_eq!(original.req_id, deserialized.req_id);
    }

    #[test]
    fn test_error_response_format() {
        // 测试标准错误响应格式 / Test standard error response format
        let error_resp = ErrorResponse::new(404, "Resource not found");

        let json = serde_json::to_string(&error_resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // 验证格式: { "error": { "code": 404, "message": "..." } }
        assert!(parsed.get("error").is_some());
        let error = parsed.get("error").unwrap();
        assert_eq!(error.get("code").unwrap(), 404);
        assert_eq!(error.get("message").unwrap(), "Resource not found");
        assert!(error.get("details").is_none()); // 没有 details 时不序列化
    }

    #[test]
    fn test_error_response_with_details() {
        // 测试带详情的错误响应 / Test error response with details
        let error_resp = ErrorResponse::tool_not_found("my_tool");

        let json = serde_json::to_string(&error_resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let error = parsed.get("error").unwrap();
        assert_eq!(error.get("code").unwrap(), error_codes::TOOL_NOT_FOUND);
        assert!(error
            .get("message")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("my_tool"));
        assert!(error.get("details").is_some());
        assert_eq!(
            error.get("details").unwrap().get("tool_name").unwrap(),
            "my_tool"
        );
    }

    #[test]
    fn test_error_response_convenience_constructors() {
        // 测试便捷构造方法 / Test convenience constructors
        assert_eq!(ErrorResponse::bad_request("test").error.code, 400);
        assert_eq!(ErrorResponse::unauthorized("test").error.code, 401);
        assert_eq!(ErrorResponse::forbidden("test").error.code, 403);
        assert_eq!(ErrorResponse::not_found("test").error.code, 404);
        assert_eq!(ErrorResponse::timeout("test").error.code, 408);
        assert_eq!(ErrorResponse::internal_error("test").error.code, 500);
        assert_eq!(ErrorResponse::tool_not_found("t").error.code, 4001);
        assert_eq!(ErrorResponse::tool_execution_failed("t").error.code, 4003);
        assert_eq!(ErrorResponse::tool_timeout(30).error.code, 4004);
        assert_eq!(ErrorResponse::room_full("office1").error.code, 4101);
        assert_eq!(ErrorResponse::not_in_room().error.code, 4103);
    }

    #[test]
    fn test_error_response_roundtrip() {
        // 测试序列化和反序列化往返 / Test serialization roundtrip
        let original = ErrorResponse::new(500, "Internal error")
            .with_detail("trace_id", serde_json::Value::String("abc123".to_string()));

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ErrorResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(original.error.code, deserialized.error.code);
        assert_eq!(original.error.message, deserialized.error.message);
        assert_eq!(
            original.error.details.as_ref().unwrap().get("trace_id"),
            deserialized.error.details.as_ref().unwrap().get("trace_id")
        );
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
        // 便捷构造器映射到正确的语义码
        assert_eq!(ErrorResponse::tool_not_found("t").error.code, 4001);
        assert_eq!(ErrorResponse::tool_execution_failed("t").error.code, 4003);
    }
}
