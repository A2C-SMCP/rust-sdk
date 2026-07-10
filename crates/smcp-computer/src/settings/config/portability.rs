/*!
* 文件名: portability.rs
* 作者: JQQ
* 创建日期: 2026/07/10
* 最后修改日期: 2026/07/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::config::{crud, validate}
* 描述: #107 S4（#111）—— config `import` / `export`（脱敏：不出/不入 secret 明文与 client-owned 字段）。
*       Config `import` / `export`: sanitized so secret plaintext and client-owned fields do not cross the boundary.
*
* 脱敏两管齐下（design-107 §10 订正 #1 / D1 / S1 header caveat）/ two-pronged sanitization:
*   ① **丢弃 client-owned / 机器本地面**：`*.local.json` 整层 + project mcp server 的 `envFile`（机器本地 `.env`
*      绝对路径，泄露 home 布局且换机必失效）—— 不随导出旅行、不从导入落入本机。
*   ② **脱敏 mcp secret-surface 字面值**：stdio `server_parameters.env` / sse·http `server_parameters.headers` 值 +
*      sse·http `url` 内联 userinfo（`user:pass@`）+ `password: true` 输入的 `default`。抹改采**分段**粒度——
*      逐字保留**合法闭合且可识别**的 `${input:*}` / `${env:*}` 引用（非明文），其余**每一段字面**（含畸形/未闭合
*      占位符、混入引用旁的明文）一律抹为 [`REDACTED_PLACEHOLDER`]。**欠脱敏是危险方向**：故整值只要含任何非引用
*      字面即被抹，绝不因「串里出现过 `${`」就整值放行（否则 `"secret${x"` / `"Basic xxx ${env:U}"` 会泄漏）。
*
* 边界（**best-effort，非穷尽**；覆盖上述已知面）/ boundary (best-effort; covers the known faces above):
*   - settings project 层无 secret 字面面 → 原样透传（治理标量/marketplace/enabledPlugins 皆非明文）。
*   - **不覆盖**（调用方/上层责任）：`command`/`args` 内联的密码 flag（无法泛化判定、误伤合法配置）、`url` 的敏感
*     query 串（`?token=` 之类，需领域知识才能判定）。这些不在已确认脱敏面内——文档如实声明，不作虚假绝对保证。
*
* import 同样先脱敏（防从不可信来源落入 secret）再 schema 校验（报告随返、非阻断）再落盘。
*/

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::crud::{load_project_config_doc, save_config, ConfigCrudError, ProjectConfigDoc};
use super::validate::{validate_config, ValidationReport};

/// 脱敏哨兵：抹去 secret 字面值后的占位记号（显式、非明文、提示导入方补值）/ redaction sentinel。
///
/// 形如占位符但 token 不匹配 `input:`/`env:` → 渲染层原样保留（明显「需替换」），且不含任何 secret。
/// 自身含 `${` 且不可识别 → 再次导出时被当作字面段抹回同一哨兵（幂等，见 [`redact_value_string`]）。
pub const REDACTED_PLACEHOLDER: &str = "${REDACTED}";

/// `${...}` 占位符正则（与 [`crate::inputs::render`] 渲染层同 grammar：要求闭合 `}`、token 非空）。
/// The `${...}` placeholder grammar (identical to the render layer: requires a closing `}`, non-empty token).
static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([^}]+)\}").expect("valid placeholder regex"));

// ===========================================================================
// export / import
// ===========================================================================

/// 导出**可分享**的 project 配置（脱敏 + 丢 local 层）/ export a shareable, sanitized project config。
///
/// 返回 [`ProjectConfigDoc`]（`settings_local`/`mcp_local` 恒 `None`；project 层已脱敏）。调用方负责序列化落盘/传输。
pub fn export_config(config_dir: &Path) -> Result<ProjectConfigDoc, ConfigCrudError> {
    let doc = load_project_config_doc(config_dir)?;
    Ok(sanitize_for_boundary(&doc))
}

/// 导入外来 project 配置到 `config_dir`（先脱敏、后 schema 校验、再落盘）/ import an external project config。
///
/// **防御性脱敏**：即便来源可信，也绝不把 secret 明文/`*.local.json` 落入本机（对齐 export 语义）。
/// 返回脱敏后文档的 [`ValidationReport`]（非阻断：schema 错误随报告呈现，落盘仍发生，遵循容错姿态）。
pub fn import_config(
    config_dir: &Path,
    incoming: &ProjectConfigDoc,
) -> Result<ValidationReport, ConfigCrudError> {
    let sanitized = sanitize_for_boundary(incoming);
    let report = validate_config(&sanitized);
    save_config(config_dir, &sanitized)?;
    Ok(report)
}

// ===========================================================================
// 脱敏 / Sanitization
// ===========================================================================

/// 跨 import/export 边界的脱敏：丢 local 层 + 脱敏 project mcp secret 面 / sanitize across the boundary。
fn sanitize_for_boundary(doc: &ProjectConfigDoc) -> ProjectConfigDoc {
    ProjectConfigDoc {
        // settings project 层无 secret 字面面 → 原样透传。
        settings: doc.settings.clone(),
        // ① 丢弃 client-owned / 机器本地层。
        settings_local: None,
        // ② 脱敏 mcp secret-surface 字面值。
        mcp: doc.mcp.as_ref().map(sanitize_mcp_map),
        mcp_local: None,
    }
}

/// 脱敏单个 mcp map：抹 env/headers/url-userinfo 字面 secret + password 输入的 default + 丢 envFile。
fn sanitize_mcp_map(mcp: &Map<String, Value>) -> Map<String, Value> {
    let mut out = mcp.clone();
    if let Some(Value::Object(servers)) = out.get_mut("servers") {
        for sdef in servers.values_mut() {
            if let Value::Object(sobj) = sdef {
                // ① 丢 client-owned envFile（机器本地 `.env` 路径；`env_file` 为 snake alias）。
                sobj.remove("envFile");
                sobj.remove("env_file");
                if let Some(Value::Object(params)) = sobj.get_mut("server_parameters") {
                    redact_string_map_values(params, "env"); // stdio
                    redact_string_map_values(params, "headers"); // sse / http
                    redact_url_userinfo(params); // sse / http：url 内联 user:pass@
                }
            }
        }
    }
    if let Some(Value::Array(inputs)) = out.get_mut("inputs") {
        for idef in inputs.iter_mut() {
            if let Value::Object(iobj) = idef {
                // password:true 输入的 default 可能是明文密码 → 抹去（保留其余定义字段）。
                if iobj.get("password").and_then(Value::as_bool) == Some(true) {
                    iobj.remove("default");
                }
            }
        }
    }
    out
}

/// 对 `params[field]`（string→string map）的每个字符串值做**分段脱敏** / segment-redact each string value。
fn redact_string_map_values(params: &mut Map<String, Value>, field: &str) {
    if let Some(Value::Object(m)) = params.get_mut(field) {
        for v in m.values_mut() {
            if let Value::String(s) = v {
                *v = Value::String(redact_value_string(s));
            }
        }
    }
}

/// sse·http `url` 内联 userinfo 脱敏：`scheme://user:pass@host/…` → `scheme://<sentinel>@host/…`。
///
/// 只动 authority 段的 userinfo（`@` 前、authority 内）——host/path/query 不动（query token 属声明外未覆盖面）。
fn redact_url_userinfo(params: &mut Map<String, Value>) {
    if let Some(Value::String(url)) = params.get_mut("url") {
        if let Some(redacted) = url_with_redacted_userinfo(url) {
            *url = redacted;
        }
    }
}

/// 若 `url` 的 authority 段含非空 userinfo → 返回抹去 userinfo 的新串；否则 `None`（不改）。
fn url_with_redacted_userinfo(url: &str) -> Option<String> {
    let after_scheme = url.find("://")? + 3;
    // authority 终止于首个 '/'（path 起点）或串尾。
    let authority_end = url[after_scheme..]
        .find('/')
        .map(|i| after_scheme + i)
        .unwrap_or(url.len());
    let at_rel = url[after_scheme..authority_end].find('@')?;
    let at = after_scheme + at_rel;
    if at <= after_scheme {
        return None; // userinfo 为空（`scheme://@host`）→ 无 secret。
    }
    Some(format!(
        "{}{}{}",
        &url[..after_scheme],
        REDACTED_PLACEHOLDER,
        &url[at..]
    ))
}

/// **分段脱敏**一个字符串：逐字保留合法闭合且可识别（`input:`/`env:`）的占位符引用，其余**每段字面**抹为哨兵。
/// Segment-redact a string: keep well-formed recognizable `${input:*}`/`${env:*}` references verbatim; every
/// other (literal) segment — including malformed/unclosed placeholders and plaintext beside a reference —
/// collapses to the sentinel. 空串保持空（空非 secret）。相邻哨兵折叠为一（幂等 + 洁净）。
fn redact_value_string(s: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for caps in PLACEHOLDER_RE.captures_iter(s) {
        let whole = caps.get(0).unwrap();
        let token = caps.get(1).unwrap().as_str();
        if whole.start() > last {
            push_sentinel(&mut out); // 占位符前的字面段。
        }
        if is_reference_token(token) {
            out.push_str(whole.as_str()); // 引用逐字保留。
        } else {
            push_sentinel(&mut out); // 未识别/畸形占位符视作字面。
        }
        last = whole.end();
    }
    if last < s.len() {
        push_sentinel(&mut out); // 尾随字面段（含整串「无闭合 ${」的情形）。
    }
    out
}

/// 追加哨兵，若尾部已是哨兵则跳过（折叠相邻哨兵）/ append sentinel, collapsing an adjacent one。
fn push_sentinel(out: &mut String) {
    if !out.ends_with(REDACTED_PLACEHOLDER) {
        out.push_str(REDACTED_PLACEHOLDER);
    }
}

/// token 是否为**引用**（`${input:...}`/`${env:...}`，运行期解析、非明文）/ whether the token is a reference。
///
/// 预定义变量 / `${REDACTED}` / 任何其它 token 一律**不**视作引用（→ 当字面抹去，安全方向）。
fn is_reference_token(token: &str) -> bool {
    token.starts_with("input:") || token.starts_with("env:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::mcp_config::{workdir_mcp_config_path, workdir_mcp_local_config_path};
    use crate::settings::scope::{workdir_local_settings_path, workdir_project_settings_path};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    struct Fx {
        _tmp: TempDir,
        wd: PathBuf,
    }
    impl Fx {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let wd = tmp.path().join("wd");
            Self { _tmp: tmp, wd }
        }
    }

    // ---- export: 脱敏 secret 面 ----

    #[test]
    fn export_redacts_stdio_env_literals_keeps_placeholders() {
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"db": {"type": "stdio", "server_parameters": {
                "command": "run",
                "env": {"API_KEY": "sk-real-secret", "REF": "${input:tok}", "MODE": "prod"}
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let env = &exported.mcp.unwrap()["servers"]["db"]["server_parameters"]["env"];
        // 纯字面 secret 抹去；占位符引用保留。
        assert_eq!(env["API_KEY"], json!(REDACTED_PLACEHOLDER));
        assert_eq!(env["MODE"], json!(REDACTED_PLACEHOLDER));
        assert_eq!(env["REF"], json!("${input:tok}"));
    }

    #[test]
    fn export_redacts_http_header_literals() {
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"api": {"type": "http", "server_parameters": {
                "url": "https://x", "headers": {"Authorization": "Bearer sk-xyz", "X-Ref": "${env:TOK}"}
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let headers = &exported.mcp.unwrap()["servers"]["api"]["server_parameters"]["headers"];
        assert_eq!(headers["Authorization"], json!(REDACTED_PLACEHOLDER));
        assert_eq!(headers["X-Ref"], json!("${env:TOK}")); // env 引用保留
    }

    #[test]
    fn export_redacts_literal_secret_containing_unclosed_dollar_brace() {
        // 🔴 回归（场景 A）：值含 `${` 但**无闭合** → 运行期是纯明文 → 必须抹（旧 contains("${") 会漏放行）。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"s": {"type": "stdio", "server_parameters": {
                "command": "run", "env": {"PW": "secret${x", "TPL": "pre${bogus}post"}
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let env = &exported.mcp.unwrap()["servers"]["s"]["server_parameters"]["env"];
        assert_eq!(
            env["PW"],
            json!(REDACTED_PLACEHOLDER),
            "无闭合 ${{ 的字面必须抹"
        );
        // 未识别占位符 `${bogus}` + 两侧字面 → 全抹、折叠为单一哨兵。
        assert_eq!(env["TPL"], json!(REDACTED_PLACEHOLDER));
    }

    #[test]
    fn export_redacts_mixed_literal_and_reference_value() {
        // 🔴 回归（场景 B）：字面 secret + 合法引用混在一个值 → 抹字面段、留引用段（不整值放行）。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"api": {"type": "http", "server_parameters": {
                "url": "https://h",
                "headers": {"Authorization": "Basic dXNlcjpsZWFrZWQ= ${env:UNUSED}"},
                "extra_env_like": {}
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let auth = exported.mcp.unwrap()["servers"]["api"]["server_parameters"]["headers"]
            ["Authorization"]
            .clone();
        // base64 明文段被抹、`${env:UNUSED}` 引用保留。
        assert_eq!(
            auth,
            json!(format!("{REDACTED_PLACEHOLDER}${{env:UNUSED}}"))
        );
        assert!(
            !auth.as_str().unwrap().contains("dXNlcjpsZWFrZWQ="),
            "明文段不得残留"
        );
    }

    #[test]
    fn export_keeps_embedded_references_redacts_literals_in_url_like_value() {
        // 值内嵌多个引用 + 字面（如 DSN）→ 引用逐字保留、每段字面抹为哨兵。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"db": {"type": "stdio", "server_parameters": {
                "command": "run", "env": {"DSN": "postgres://${input:user}:${input:pw}@host/db"}
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let dsn = exported.mcp.unwrap()["servers"]["db"]["server_parameters"]["env"]["DSN"].clone();
        let dsn = dsn.as_str().unwrap();
        assert!(
            dsn.contains("${input:user}") && dsn.contains("${input:pw}"),
            "引用须保留"
        );
        assert!(
            !dsn.contains("postgres") && !dsn.contains("host/db"),
            "字面段须抹"
        );
    }

    #[test]
    fn export_redacts_sse_url_inline_userinfo() {
        // 🟡：sse·http url 内联 user:pass@ 是明文 secret 面 → 抹 userinfo，host/path 保留。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"api": {"type": "sse", "server_parameters": {
                "url": "https://alice:s3cr3t@host.example/mcp"
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let url = exported.mcp.unwrap()["servers"]["api"]["server_parameters"]["url"].clone();
        assert_eq!(
            url,
            json!(format!("https://{REDACTED_PLACEHOLDER}@host.example/mcp"))
        );
        assert!(
            !url.as_str().unwrap().contains("s3cr3t"),
            "userinfo 明文不得残留"
        );
    }

    #[test]
    fn export_redacts_ipv6_url_userinfo_keeps_host() {
        // 🟢 回归护栏：IPv6 authority **带** userinfo → 抹 userinfo、保 `[::1]:8080` host。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"api": {"type": "http", "server_parameters": {
                "url": "https://user:tok3n@[::1]:8080/mcp"
            }}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let url = exported.mcp.unwrap()["servers"]["api"]["server_parameters"]["url"].clone();
        assert_eq!(
            url,
            json!(format!("https://{REDACTED_PLACEHOLDER}@[::1]:8080/mcp"))
        );
        assert!(!url.as_str().unwrap().contains("tok3n"));
    }

    #[test]
    fn export_url_without_userinfo_is_unchanged() {
        // 无 userinfo（含 IPv6 authority `[::1]` 无 `@`、含端口、query 里的 `@`）→ 不误抹、不 panic。
        for url in [
            "https://host.example:8080/mcp",
            "https://[::1]:8080/mcp",
            "http://plain/path?token=keep-me", // query 属未覆盖面 → 保持原样（文档已声明）
            "https://host/p?email=a@b.com",    // path/query 里的 `@` 不得触发 userinfo 抹除
            "not-a-url",
            "://malformed",
        ] {
            let fx = Fx::new();
            write(
                &workdir_mcp_config_path(&fx.wd),
                &format!(
                    r#"{{"servers": {{"api": {{"type": "sse", "server_parameters": {{"url": {url:?}}}}}}}}}"#
                ),
            );
            let exported = export_config(&fx.wd).unwrap();
            assert_eq!(
                exported.mcp.unwrap()["servers"]["api"]["server_parameters"]["url"],
                json!(url),
                "无 userinfo 的 url 不应被改动: {url}"
            );
        }
    }

    #[test]
    fn export_drops_machine_local_envfile() {
        // 🟡：envFile 是机器本地 `.env` 绝对路径（client-owned，泄露 home 布局）→ 导出时丢弃。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"s": {"type": "stdio", "envFile": "/Users/alice/.secrets/.env",
                "server_parameters": {"command": "run"}}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let srv = exported.mcp.unwrap()["servers"]["s"].clone();
        assert!(srv.get("envFile").is_none(), "envFile 须被丢弃");
        assert!(srv.get("env_file").is_none());
        // 其余定义保留。
        assert_eq!(srv["server_parameters"]["command"], json!("run"));
    }

    #[test]
    fn export_redaction_is_idempotent_on_reexport() {
        // 已抹哨兵值再导出 → 稳定（哨兵含 ${{ 但不可识别 → 当字面折叠回同一哨兵）。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"s": {"type": "stdio", "server_parameters": {
                "command": "run", "env": {"K": "literal"}
            }}}}"#,
        );
        let once = export_config(&fx.wd).unwrap();
        let dst = fx._tmp.path().join("dst");
        import_config(&dst, &once).unwrap();
        let twice = export_config(&dst).unwrap();
        assert_eq!(once.mcp, twice.mcp);
        assert_eq!(
            twice.mcp.unwrap()["servers"]["s"]["server_parameters"]["env"]["K"],
            json!(REDACTED_PLACEHOLDER)
        );
    }

    #[test]
    fn export_drops_password_input_default_keeps_definition() {
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"inputs": [
                {"type": "PromptString", "id": "pw", "description": "secret", "password": true, "default": "hunter2"},
                {"type": "PromptString", "id": "name", "description": "user", "default": "alice"}
            ]}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        let inputs = exported.mcp.unwrap()["inputs"].clone();
        // password 输入：定义保留、default 抹去。
        assert_eq!(inputs[0]["id"], json!("pw"));
        assert!(
            inputs[0].get("default").is_none(),
            "password default 应抹去"
        );
        // 非 password 输入：default 保留（非 secret 面）。
        assert_eq!(inputs[1]["default"], json!("alice"));
    }

    #[test]
    fn export_drops_local_layer() {
        let fx = Fx::new();
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"strictKnownMarketplaces": true}"#,
        );
        write(
            &workdir_local_settings_path(&fx.wd),
            r#"{"trustedMarketplaces": ["local-only"]}"#,
        );
        write(
            &workdir_mcp_local_config_path(&fx.wd),
            r#"{"servers": {"local-srv": {"type": "stdio", "server_parameters": {"command": "x"}}}}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        // project 层在、local 层被丢。
        assert!(exported.settings.is_some());
        assert!(exported.settings_local.is_none(), "local settings 不导出");
        assert!(exported.mcp_local.is_none(), "local mcp 不导出");
    }

    #[test]
    fn export_passes_through_settings_project_layer() {
        let fx = Fx::new();
        write(
            &workdir_project_settings_path(&fx.wd),
            r#"{"enabledPlugins": {"p@mp": true}, "trustedMarketplaces": ["mp"]}"#,
        );
        let exported = export_config(&fx.wd).unwrap();
        assert_eq!(
            exported.settings.unwrap()["enabledPlugins"],
            json!({"p@mp": true})
        );
    }

    // ---- import: 防御脱敏 + 落盘 + 报告 ----

    #[test]
    fn import_sanitizes_before_persist_and_drops_local() {
        let fx = Fx::new();
        let incoming = ProjectConfigDoc {
            settings: Some(obj(json!({"strictKnownMarketplaces": true}))),
            // 来源里的 local 层 + secret 字面：都不得落入本机。
            settings_local: Some(obj(json!({"trustedMarketplaces": ["theirs"]}))),
            mcp: Some(obj(
                json!({"servers": {"s": {"type": "stdio", "server_parameters": {
                    "command": "x", "env": {"SECRET": "leaked"}
                }}}}),
            )),
            ..Default::default()
        };
        let report = import_config(&fx.wd, &incoming).unwrap();
        assert!(report.is_valid());
        // secret 抹去后落盘。
        assert_eq!(
            read_json(&workdir_mcp_config_path(&fx.wd))["servers"]["s"]["server_parameters"]["env"]
                ["SECRET"],
            json!(REDACTED_PLACEHOLDER)
        );
        // 来源 local 层未落入本机。
        assert!(
            !workdir_local_settings_path(&fx.wd).exists(),
            "来源 local 层不得落盘"
        );
        // project settings 落盘。
        assert_eq!(
            read_json(&workdir_project_settings_path(&fx.wd)),
            json!({"strictKnownMarketplaces": true})
        );
    }

    #[test]
    fn import_reports_schema_errors_nonblocking() {
        let fx = Fx::new();
        let incoming = ProjectConfigDoc {
            mcp: Some(obj(
                json!({"servers": {"bad": {"type": "not-a-transport"}}}),
            )),
            ..Default::default()
        };
        let report = import_config(&fx.wd, &incoming).unwrap();
        assert!(!report.is_valid(), "非法 server type → 报告非空");
        // 非阻断：仍落盘。
        assert!(workdir_mcp_config_path(&fx.wd).exists());
    }

    #[test]
    fn export_import_roundtrip_is_stable() {
        // 已脱敏的导出物再导入再导出 → 内容稳定（幂等，无二次抹改）。
        let fx = Fx::new();
        write(
            &workdir_mcp_config_path(&fx.wd),
            r#"{"servers": {"s": {"type": "stdio", "server_parameters": {"command": "x", "env": {"K": "v"}}}}}"#,
        );
        let first = export_config(&fx.wd).unwrap();
        let dst = fx._tmp.path().join("dst");
        import_config(&dst, &first).unwrap();
        let second = export_config(&dst).unwrap();
        assert_eq!(first.mcp, second.mcp);
        assert_eq!(first.settings, second.settings);
    }
}
