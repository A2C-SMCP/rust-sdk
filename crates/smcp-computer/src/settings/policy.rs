/*!
* 文件名: policy.rs
* 作者: JQQ
* 创建日期: 2026/06/04
* 最后修改日期: 2026/06/04
* 版权: 2023 JQQ. All rights reserved.
* 依赖: serde_json, settings::scope::merge_read；target.macos=plist, target.windows=winreg
* 描述: 治理层 settings policy（企业策略）层 —— 四子源 first-source-wins。
*       Governance-layer settings policy (enterprise) layer — four sub-sources, first-source-wins.
*/

//! policy scope 四子源 first-source-wins。
//!
//! 对标 Python 治理层资产 `a2c_smcp/computer/settings/policy.py`（v0.2.1）。
//! SDK 设计 / Design: python-sdk `docs/design-0.2.1-cli-marketplace-ux.md` §5.2 / §5.4。
//!
//! policy 是 settings 链**最高优先级**层（企业 / 运维强制）。其内部由四个子源构成，**不合并**——按优先级取
//! **第一个有内容**者整体叠加进主链（first-source-wins，照 Claude Code：remote > OS-MDM >
//! managed-settings.json[+.d] > HKCU）。
//!
//! | 优先级 | 平台 | 位置 | v0.2.1 |
//! |---|---|---|---|
//! | 1（最高）| 全平台 | Remote managed endpoint | **stub**（不联网）|
//! | 2 | macOS | `/Library/Managed Preferences/com.a2c.computer.plist` | ✓ |
//! | 2 | Windows | `HKLM\SOFTWARE\Policies\A2CComputer\Settings` | ✓（best-effort）|
//! | 3 | 全平台 | `managed-settings.json` (+ `managed-settings.d/*.json`) | ✓ |
//! | 4（最低）| Windows | `HKCU\SOFTWARE\Policies\A2CComputer\Settings` | ✓（best-effort）|
//!
//! **v0.2.1 范围**：实现层级 2/3/4 的本地文件 / 注册表读取；Remote 留 stub、**不发起网络请求**。
//! 本模块只做 **first-source-wins 选取**，返回原始 dict；类型 / scope 校验由
//! [`resolve_settings`](crate::settings::scope::resolve_settings) 按 POLICY scope 统一执行（policy-only 字段
//! `allowedMcpServers` / `deniedMcpServers` 只在该层生效）。
//!
//! 条件编译 / Conditional compilation：macOS plist（[`plist`] crate）与 Windows 注册表（`winreg` crate）读取器
//! 按 `#[cfg(target_os = ...)]` 编译真实实现，其它平台为 `None` stub —— 跨平台构建不破坏，平台依赖经
//! `[target.'cfg(...)'.dependencies]` off-platform 零成本。Windows 路径 best-effort、**无 CI lane 验证**（与
//! Python `policy.py` win32 同姿态）。

use std::path::Path;

use serde_json::{Map, Value};

use crate::settings::scope::{merge_read, EnvMap};

// ===========================================================================
// 常量 / Constants
// ===========================================================================
/// Remote managed endpoint base 环境变量（v0.2.2+ 实现）/ Remote managed endpoint base env (v0.2.2+)。
pub const REMOTE_POLICY_BASE_ENV: &str = "A2C_BASE_API_URL";
/// Remote managed endpoint 路径（v0.2.2+）/ Remote managed endpoint path (v0.2.2+)。
pub const REMOTE_POLICY_PATH: &str = "/api/computer/settings";

/// Windows 注册表子键（HKLM / HKCU 共用）/ Windows registry subkey (shared by HKLM / HKCU)。
pub const WINDOWS_POLICY_SUBKEY: &str = r"SOFTWARE\Policies\A2CComputer\Settings";
/// 注册表内承载 settings JSON 的值名（best-effort）/ Registry value name carrying the settings JSON。
pub const WINDOWS_POLICY_VALUE_NAME: &str = "Settings";

/// macOS Managed Preferences plist 路径（layer 2）/ macOS Managed Preferences plist path (layer 2)。
pub const MACOS_PLIST_PATH: &str = "/Library/Managed Preferences/com.a2c.computer.plist";
/// macOS managed-settings 目录（layer 3）/ macOS managed-settings dir (layer 3)。
pub const MACOS_MANAGED_DIR: &str = "/Library/Application Support/A2CComputer";
/// Windows managed-settings 目录（layer 3）/ Windows managed-settings dir (layer 3)。
pub const WINDOWS_MANAGED_DIR: &str = r"C:\Program Files\A2CComputer";
/// Linux managed-settings 目录（layer 3）/ Linux managed-settings dir (layer 3)。
pub const LINUX_MANAGED_DIR: &str = "/etc/a2c-computer";

/// managed-settings 主文件名 / managed-settings base filename。
pub const MANAGED_SETTINGS_FILENAME: &str = "managed-settings.json";
/// managed-settings drop-in 目录名 / managed-settings drop-in dir name。
pub const MANAGED_SETTINGS_DROPIN_DIRNAME: &str = "managed-settings.d";

// ===========================================================================
// PolicySource / 子源描述
// ===========================================================================
/// 一个 policy 子源 / A single policy sub-source。
///
/// `priority` 越小越高（1 = 最高）；`loader` 返回该子源内容（对象 map）或 `None`（缺席 / 不可读）。loader 内部
/// 已吞错降级为 `None`（单源读取失败不阻断启动）。
pub struct PolicySource {
    /// 子源名（诊断用）/ sub-source name (diagnostics)。
    pub name: String,
    /// 优先级（1 最高）/ priority (1 = highest)。
    pub priority: u32,
    /// 加载器：返回内容或 `None` / loader returning content or `None`。
    pub loader: Box<dyn Fn() -> Option<Map<String, Value>> + Send + Sync>,
}

// ===========================================================================
// 子源读取器 / Sub-source readers
// ===========================================================================
/// 读取单个 JSON 文件 → 对象 map（缺失 / 损坏 / 非对象 → `None`）/ Read one JSON file。
fn read_json_file(path: &Path) -> Option<Map<String, Value>> {
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "policy JSON unreadable, skipped");
            return None;
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Some(map),
        Ok(_) => {
            tracing::warn!(path = %path.display(), "policy JSON root is not an object, skipped");
            None
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "policy JSON corrupt, skipped");
            None
        }
    }
}

/// `merge_read` 的 map 包装（高优先覆盖低优先；两对象 → 对象）/ map-wrapper around [`merge_read`]。
fn merge_read_maps(low: &Map<String, Value>, high: &Map<String, Value>) -> Map<String, Value> {
    match merge_read(&Value::Object(low.clone()), &Value::Object(high.clone())) {
        Value::Object(m) => m,
        // 两对象的 merge_read 必为对象；理论不可达，保守回退 high。
        _ => high.clone(),
    }
}

/// layer 3 读取器：`managed-settings.json` + `managed-settings.d/*.json` 深合并 / layer-3 reader。
///
/// base 文件先读，随后按文件名字典序合并 `managed-settings.d/*.json`（drop-in 覆盖 base，沿用读语义
/// [`merge_read`]）。皆缺 → `None`。
pub fn load_managed_settings(managed_dir: &Path) -> Option<Map<String, Value>> {
    let mut merged: Option<Map<String, Value>> =
        read_json_file(&managed_dir.join(MANAGED_SETTINGS_FILENAME));

    let dropin_dir = managed_dir.join(MANAGED_SETTINGS_DROPIN_DIRNAME);
    if dropin_dir.is_dir() {
        let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&dropin_dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect(),
            Err(_) => Vec::new(),
        };
        entries.sort();
        for entry in entries {
            if let Some(piece) = read_json_file(&entry) {
                merged = Some(match merged {
                    None => piece,
                    Some(m) => merge_read_maps(&m, &piece),
                });
            }
        }
    }

    merged
}

/// layer 2（macOS）读取器：解析 Managed Preferences plist / layer-2 macOS plist reader。
#[cfg(target_os = "macos")]
pub fn load_macos_plist(path: &Path) -> Option<Map<String, Value>> {
    if !path.exists() {
        return None;
    }
    let parsed = match plist::Value::from_file(path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "policy plist unreadable, skipped");
            return None;
        }
    };
    match plist_to_json(parsed) {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// 非 macOS stub：恒 `None`（跨平台构建占位）/ non-macOS stub。
#[cfg(not(target_os = "macos"))]
pub fn load_macos_plist(_path: &Path) -> Option<Map<String, Value>> {
    None
}

/// `plist::Value` → `serde_json::Value`（确定性转换，避免跨 crate serde 互操作脆弱性）/ plist→json converter。
#[cfg(target_os = "macos")]
fn plist_to_json(value: plist::Value) -> Value {
    use plist::Value as P;
    match value {
        P::Dictionary(d) => {
            Value::Object(d.into_iter().map(|(k, v)| (k, plist_to_json(v))).collect())
        }
        P::Array(a) => Value::Array(a.into_iter().map(plist_to_json).collect()),
        P::String(s) => Value::String(s),
        P::Boolean(b) => Value::Bool(b),
        P::Integer(i) => i
            .as_signed()
            .map(Value::from)
            .or_else(|| i.as_unsigned().map(Value::from))
            .unwrap_or(Value::Null),
        P::Real(r) => serde_json::Number::from_f64(r).map_or(Value::Null, Value::Number),
        // policy settings 不含 Date / Data / Uid；保守降级避免 panic / non-exhaustive。
        _ => Value::Null,
    }
}

/// layer 2/4（Windows）读取器：从注册表值读取承载的 settings JSON / layer-2/4 Windows registry reader。
///
/// best-effort：读取 `<hive>\<subkey>` 下名为 [`WINDOWS_POLICY_VALUE_NAME`] 的字符串值并按 JSON 解析（registry
/// 扁平、无法原生承载嵌套，故约定承载 JSON 串）。键缺失 / 非字符串 / 非法 JSON / 非对象 → `None`。
#[cfg(target_os = "windows")]
pub fn load_windows_registry(hive_name: &str, subkey: &str) -> Option<Map<String, Value>> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let hive = match hive_name {
        "HKEY_LOCAL_MACHINE" => HKEY_LOCAL_MACHINE,
        "HKEY_CURRENT_USER" => HKEY_CURRENT_USER,
        _ => return None,
    };
    let key = match RegKey::predef(hive).open_subkey(subkey) {
        Ok(k) => k,
        Err(_) => return None, // 键不存在 / key missing
    };
    let value: String = match key.get_value(WINDOWS_POLICY_VALUE_NAME) {
        Ok(v) => v,
        Err(_) => return None, // 值不存在 / 非字符串
    };
    match serde_json::from_str::<Value>(&value) {
        Ok(Value::Object(m)) => Some(m),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "windows policy registry value is not valid JSON, skipped");
            None
        }
    }
}

/// 非 Windows stub：恒 `None`（跨平台构建占位）/ non-Windows stub。
#[cfg(not(target_os = "windows"))]
pub fn load_windows_registry(_hive_name: &str, _subkey: &str) -> Option<Map<String, Value>> {
    None
}

/// layer 1（remote）读取器 —— **v0.2.1 stub，恒返回 `None`、不联网** / layer-1 remote reader (stub)。
///
/// v0.2.2+ 将实现 `${A2C_BASE_API_URL}/api/computer/settings` 的 30 min poll + OAuth/team 关联（见 §5.2）。
pub fn load_remote_policy(env: &EnvMap) -> Option<Map<String, Value>> {
    if let Some(base) = env.get(REMOTE_POLICY_BASE_ENV) {
        if !base.trim().is_empty() {
            tracing::debug!(
                endpoint = %base,
                "remote managed-policy endpoint configured but disabled in v0.2.1 (stub)"
            );
        }
    }
    None
}

// ===========================================================================
// 默认子源装配 + first-source-wins / Default sources + first-source-wins
// ===========================================================================
/// 进程环境快照 / a snapshot of the process environment。
fn process_env() -> EnvMap {
    std::env::vars().collect()
}

/// 按平台装配默认 policy 子源 / Assemble default policy sub-sources per platform（§5.2）。
///
/// `platform` 接受 `sys.platform` 风格（`"darwin"` / `"win32"`）或 Rust 风格（`"macos"` / `"windows"`）；其余按
/// Linux（仅 layer 3）。各平台原生读取器在非对应 target 下为 `None` stub（见模块文档）。
pub fn default_policy_sources(platform: &str, env: &EnvMap) -> Vec<PolicySource> {
    let env_for_remote = env.clone();
    let mut sources = vec![PolicySource {
        name: "remote".to_string(),
        priority: 1,
        loader: Box::new(move || load_remote_policy(&env_for_remote)),
    }];

    match platform {
        "darwin" | "macos" => {
            sources.push(PolicySource {
                name: "macos-plist".to_string(),
                priority: 2,
                loader: Box::new(|| load_macos_plist(Path::new(MACOS_PLIST_PATH))),
            });
            sources.push(PolicySource {
                name: "managed-settings".to_string(),
                priority: 3,
                loader: Box::new(|| load_managed_settings(Path::new(MACOS_MANAGED_DIR))),
            });
        }
        "win32" | "windows" => {
            sources.push(PolicySource {
                name: "hklm-registry".to_string(),
                priority: 2,
                loader: Box::new(|| {
                    load_windows_registry("HKEY_LOCAL_MACHINE", WINDOWS_POLICY_SUBKEY)
                }),
            });
            sources.push(PolicySource {
                name: "managed-settings".to_string(),
                priority: 3,
                loader: Box::new(|| load_managed_settings(Path::new(WINDOWS_MANAGED_DIR))),
            });
            sources.push(PolicySource {
                name: "hkcu-registry".to_string(),
                priority: 4,
                loader: Box::new(|| {
                    load_windows_registry("HKEY_CURRENT_USER", WINDOWS_POLICY_SUBKEY)
                }),
            });
        }
        _ => {
            // Linux / 其余 POSIX：无 OS 原生 MDM，仅 layer 3。
            sources.push(PolicySource {
                name: "managed-settings".to_string(),
                priority: 3,
                loader: Box::new(|| load_managed_settings(Path::new(LINUX_MANAGED_DIR))),
            });
        }
    }

    sources
}

/// first-source-wins 选取 policy 内容 / Select policy content by first-source-wins（§5.2 / §5.4）。
///
/// 按 `priority` 升序（1 最高）逐源尝试，返回**第一个非空对象**整体；皆空 → `{}`。子源**不合并**（照 CC：只取
/// 最高且有内容者）。loader 内部已吞错降级为 `None`，单源失败不阻断。
///
/// - `env`：环境映射，`None` → 进程环境。
/// - `platform`：平台串，`None` → [`std::env::consts::OS`]（`"macos"` / `"windows"` / `"linux"`）。
/// - `sources`：自定义子源（便于测试注入）；`None` → 按 [`default_policy_sources`] 装配。
pub fn resolve_policy_settings(
    env: Option<&EnvMap>,
    platform: Option<&str>,
    sources: Option<Vec<PolicySource>>,
) -> Map<String, Value> {
    let owned_env;
    let env_ref: &EnvMap = match env {
        Some(e) => e,
        None => {
            owned_env = process_env();
            &owned_env
        }
    };
    let plat = platform.unwrap_or(std::env::consts::OS);
    let mut srcs = sources.unwrap_or_else(|| default_policy_sources(plat, env_ref));
    srcs.sort_by_key(|s| s.priority);

    for source in &srcs {
        if let Some(loaded) = (source.loader)() {
            if !loaded.is_empty() {
                tracing::debug!(
                    source = %source.name,
                    priority = source.priority,
                    "policy resolved from sub-source"
                );
                return loaded;
            }
        }
    }

    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// 构造一个返回固定内容的注入子源 / a source returning a fixed map。
    fn fixed_source(name: &str, priority: u32, content: Option<Value>) -> PolicySource {
        let map = content.and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        });
        PolicySource {
            name: name.to_string(),
            priority,
            loader: Box::new(move || map.clone()),
        }
    }

    // ---- first-source-wins --------------------------------------------------
    #[test]
    fn resolve_first_non_empty_by_priority_wins_wholesale() {
        let srcs = vec![
            fixed_source("low", 3, Some(json!({"k": "low"}))),
            fixed_source("high", 1, Some(json!({"k": "high", "only": 1}))),
            fixed_source("mid", 2, Some(json!({"k": "mid"}))),
        ];
        let got = resolve_policy_settings(None, Some("linux"), Some(srcs));
        // 最高优先级（1）整体胜出，不与低优先合并 / highest wins wholesale, no merge
        assert_eq!(got.get("k"), Some(&json!("high")));
        assert_eq!(got.get("only"), Some(&json!(1)));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn resolve_skips_empty_and_none_sources() {
        let srcs = vec![
            fixed_source("p1-none", 1, None),
            fixed_source("p2-empty", 2, Some(json!({}))),
            fixed_source("p3-content", 3, Some(json!({"k": "v"}))),
        ];
        let got = resolve_policy_settings(None, Some("linux"), Some(srcs));
        assert_eq!(got.get("k"), Some(&json!("v")));
    }

    #[test]
    fn resolve_all_empty_returns_empty() {
        let srcs = vec![
            fixed_source("a", 1, None),
            fixed_source("b", 2, Some(json!({}))),
        ];
        assert!(resolve_policy_settings(None, Some("linux"), Some(srcs)).is_empty());
    }

    #[test]
    fn resolve_sorts_unordered_sources_by_priority() {
        // 乱序传入，仍按 priority 选 / out-of-order input, still priority-ordered
        let srcs = vec![
            fixed_source("p4", 4, Some(json!({"k": "p4"}))),
            fixed_source("p2", 2, Some(json!({"k": "p2"}))),
        ];
        let got = resolve_policy_settings(None, Some("linux"), Some(srcs));
        assert_eq!(got.get("k"), Some(&json!("p2")));
    }

    // ---- layer 3 managed-settings -------------------------------------------
    #[test]
    fn managed_settings_base_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(MANAGED_SETTINGS_FILENAME),
            r#"{"defaultMcpApproval": "deny"}"#,
        )
        .unwrap();
        let got = load_managed_settings(dir.path()).unwrap();
        assert_eq!(got.get("defaultMcpApproval"), Some(&json!("deny")));
    }

    #[test]
    fn managed_settings_dropin_overrides_base_in_lexical_order() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(MANAGED_SETTINGS_FILENAME),
            r#"{"a": 1, "b": "base"}"#,
        )
        .unwrap();
        let dropin = dir.path().join(MANAGED_SETTINGS_DROPIN_DIRNAME);
        std::fs::create_dir_all(&dropin).unwrap();
        // 字典序 10 在 20 前；20 最后写、覆盖 b / lexical 10 before 20; 20 wins on b
        std::fs::write(dropin.join("10-x.json"), r#"{"b": "ten", "c": 3}"#).unwrap();
        std::fs::write(dropin.join("20-y.json"), r#"{"b": "twenty"}"#).unwrap();

        let got = load_managed_settings(dir.path()).unwrap();
        assert_eq!(got.get("a"), Some(&json!(1))); // base 保留 / base kept
        assert_eq!(got.get("c"), Some(&json!(3))); // drop-in 新增 / drop-in adds
        assert_eq!(got.get("b"), Some(&json!("twenty"))); // 末位 drop-in 覆盖 / last drop-in wins
    }

    #[test]
    fn managed_settings_missing_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(load_managed_settings(&dir.path().join("nonexistent")).is_none());
    }

    #[test]
    fn read_json_file_corrupt_or_nonobject_is_none() {
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("c.json");
        std::fs::write(&corrupt, "{ not json").unwrap();
        assert!(read_json_file(&corrupt).is_none());
        let arr = dir.path().join("a.json");
        std::fs::write(&arr, "[1,2,3]").unwrap();
        assert!(read_json_file(&arr).is_none());
    }

    // ---- 默认子源装配 / default sources per platform -------------------------
    #[test]
    fn default_sources_per_platform_shape() {
        let env = EnvMap::new();
        let names = |p: &str| -> Vec<(String, u32)> {
            default_policy_sources(p, &env)
                .into_iter()
                .map(|s| (s.name, s.priority))
                .collect()
        };
        assert_eq!(
            names("darwin"),
            vec![
                ("remote".into(), 1),
                ("macos-plist".into(), 2),
                ("managed-settings".into(), 3)
            ]
        );
        assert_eq!(
            names("win32"),
            vec![
                ("remote".into(), 1),
                ("hklm-registry".into(), 2),
                ("managed-settings".into(), 3),
                ("hkcu-registry".into(), 4),
            ]
        );
        assert_eq!(
            names("linux"),
            vec![("remote".into(), 1), ("managed-settings".into(), 3)]
        );
    }

    // ---- remote stub --------------------------------------------------------
    #[test]
    fn remote_policy_is_stub_none_even_when_configured() {
        let mut env = EnvMap::new();
        env.insert(
            REMOTE_POLICY_BASE_ENV.to_string(),
            "https://api.example.com".to_string(),
        );
        assert!(load_remote_policy(&env).is_none());
    }

    // ---- 非 macOS plist stub（本机非 Windows，验证 registry stub）------------
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn windows_registry_is_stub_off_platform() {
        assert!(load_windows_registry("HKEY_LOCAL_MACHINE", WINDOWS_POLICY_SUBKEY).is_none());
    }

    // ---- macOS plist 真实路径（本机 darwin 编译并运行）----------------------
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_plist_parses_managed_preferences() {
        let dir = TempDir::new().unwrap();
        let plist_path = dir.path().join("com.a2c.computer.plist");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>defaultMcpApproval</key>
  <string>allow</string>
  <key>strictKnownMarketplaces</key>
  <true/>
  <key>allowedMcpServers</key>
  <array><string>srv-a</string><string>srv-b</string></array>
</dict>
</plist>"#;
        std::fs::write(&plist_path, xml).unwrap();

        let got = load_macos_plist(&plist_path).unwrap();
        assert_eq!(got.get("defaultMcpApproval"), Some(&json!("allow")));
        assert_eq!(got.get("strictKnownMarketplaces"), Some(&json!(true)));
        assert_eq!(
            got.get("allowedMcpServers"),
            Some(&json!(["srv-a", "srv-b"]))
        );

        // 缺失文件 → None / missing → None
        assert!(load_macos_plist(&dir.path().join("nope.plist")).is_none());
    }

    // ---- 集成：policy → resolve_settings（policy-only 字段生效）--------------
    #[test]
    fn policy_feeds_resolve_settings_and_policy_only_survives() {
        use crate::settings::scope::{resolve_settings, ResolveSettingsArgs, XDG_CONFIG_HOME_ENV};

        // hermetic：user config dir 指向空临时目录（避免读真实用户 settings）
        let tmp = TempDir::new().unwrap();
        let mut env = EnvMap::new();
        env.insert(
            XDG_CONFIG_HOME_ENV.to_string(),
            tmp.path().display().to_string(),
        );

        // policy 子源带 policy-only 字段 allowedMcpServers
        let srcs = vec![fixed_source(
            "managed",
            3,
            Some(json!({"strictKnownMarketplaces": true, "allowedMcpServers": ["srv"]})),
        )];
        let policy = resolve_policy_settings(None, Some("linux"), Some(srcs));

        let resolved = resolve_settings(ResolveSettingsArgs {
            env: Some(&env),
            policy_settings: Some(&policy),
            ..Default::default()
        });
        // policy 最高优先生效 + policy-only 字段在 policy scope 合法保留
        assert_eq!(
            resolved.settings.get("strictKnownMarketplaces"),
            Some(&json!(true))
        );
        assert_eq!(
            resolved.settings.get("allowedMcpServers"),
            Some(&json!(["srv"]))
        );
    }
}
