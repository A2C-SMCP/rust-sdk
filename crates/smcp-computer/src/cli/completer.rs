/*!
* 文件名: completer.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: rustyline
* 描述: REPL Tab 补全器 / REPL Tab completer.
*
* 对标 Python `a2c_smcp/computer/cli/completer.py`：按上下文补全——行首 namespace / 顶层命令词；namespace
* 后子命令词；命令后动态名（marketplace 名取 `known_marketplaces.json`、plugin id 取 `installed_plugins.json`、
* skill 名取活跃 registry 快照、settings key 取静态字段集）；`--flag` 位置补 flag 集。分类法取自 [`super::help`]。
*
* Rust 与 Python 的差异：`get_skills` 在 Rust 是 async，而 rustyline 补全是 sync——故 skill 名经
* `Arc<Mutex<Vec<String>>>` 快照注入（REPL 每轮刷新），marketplace/plugin 名走同步磁盘读。
*/

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustyline::completion::{Completer, Pair};
use rustyline::{Context, Helper, Highlighter, Hinter, Validator};

use super::help::{flags_for, subcommands, ROOT_WORDS, SETTINGS_KEYS};
use crate::settings::store::{load_installed_plugins, load_known_marketplaces};

/// 活跃 skill 名快照句柄（REPL 每轮经 `Computer::get_skills` 刷新）/ shared snapshot of active skill names。
pub type SkillNameSnapshot = Arc<Mutex<Vec<String>>>;

#[derive(Helper, Highlighter, Hinter, Validator)]
pub struct A2CCompleter {
    home: PathBuf,
    skill_names: SkillNameSnapshot,
}

impl A2CCompleter {
    pub fn new(home: PathBuf, skill_names: SkillNameSnapshot) -> Self {
        Self { home, skill_names }
    }

    /// 命令后位置参数的动态名集（失败不打断补全 → 空）/ dynamic positional names (never break completion)。
    fn dynamic_names(&self, ns: &str, sub: &str) -> Vec<String> {
        match (ns, sub) {
            ("marketplace", "info" | "remove" | "refresh" | "set") => {
                load_known_marketplaces(Some(&self.home), None)
                    .account
                    .marketplaces
                    .keys()
                    .cloned()
                    .collect()
            }
            ("skill", "info") => self
                .skill_names
                .lock()
                .map(|v| v.clone())
                .unwrap_or_default(),
            ("plugin", "uninstall" | "enable" | "disable" | "info") => {
                load_installed_plugins(Some(&self.home), None)
                    .account
                    .plugins
                    .keys()
                    .cloned()
                    .collect()
            }
            ("settings", "get" | "set") => SETTINGS_KEYS.iter().map(|s| s.to_string()).collect(),
            _ => Vec::new(),
        }
    }
}

impl Completer for A2CCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let text = &line[..pos];
        let trailing = text.is_empty() || text.ends_with(' ');
        let words: Vec<&str> = text.split_whitespace().collect();
        let completing = if trailing {
            ""
        } else {
            words.last().copied().unwrap_or("")
        };
        let prev: &[&str] = if trailing {
            &words[..]
        } else {
            &words[..words.len().saturating_sub(1)]
        };
        let n = prev.len();

        let candidates: Vec<String> = if n == 0 {
            // 行首：namespace + 顶层命令。
            ROOT_WORDS.iter().map(|s| s.to_string()).collect()
        } else {
            let ns = prev[0].to_lowercase();
            if n == 1 {
                // namespace 后：子命令词。
                subcommands(&ns).iter().map(|s| s.to_string()).collect()
            } else {
                let sub = prev[1].to_lowercase();
                if completing.starts_with("--") {
                    flags_for(&ns, &sub).iter().map(|s| s.to_string()).collect()
                } else {
                    self.dynamic_names(&ns, &sub)
                }
            }
        };

        let start = pos - completing.len();
        let pairs: Vec<Pair> = candidates
            .into_iter()
            .filter(|cand| cand.starts_with(completing))
            .map(|cand| Pair {
                display: cand.clone(),
                replacement: cand,
            })
            .collect();
        Ok((start, pairs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completer() -> A2CCompleter {
        A2CCompleter::new(
            std::env::temp_dir(),
            Arc::new(Mutex::new(vec!["my-skill".to_string()])),
        )
    }

    fn complete_at(line: &str) -> Vec<String> {
        let helper = completer();
        let history = rustyline::history::MemHistory::new();
        let ctx = Context::new(&history);
        let (_start, pairs) = helper.complete(line, line.len(), &ctx).unwrap();
        pairs.into_iter().map(|p| p.replacement).collect()
    }

    #[test]
    fn root_words_completion() {
        let out = complete_at("mark");
        assert!(out.contains(&"marketplace".to_string()));
    }

    #[test]
    fn subcommand_completion() {
        let out = complete_at("marketplace ");
        assert!(out.contains(&"add".to_string()));
        assert!(out.contains(&"refresh".to_string()));
    }

    #[test]
    fn flag_completion() {
        let out = complete_at("marketplace add --");
        assert!(out.contains(&"--trust".to_string()));
        assert!(out.contains(&"--no-clone".to_string()));
    }

    #[test]
    fn settings_key_and_skill_name_completion() {
        // settings get <key> → 静态字段集。
        let out = complete_at("settings get enab");
        assert!(out.contains(&"enabledPlugins".to_string()));
        // skill info <name> → 快照名。
        let out = complete_at("skill info my");
        assert!(out.contains(&"my-skill".to_string()));
    }
}
