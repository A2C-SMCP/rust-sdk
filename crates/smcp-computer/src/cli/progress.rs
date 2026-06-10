/*!
* 文件名: progress.rs
* 作者: JQQ
* 创建日期: 2026/06/10
* 最后修改日期: 2026/06/10
* 版权: 2023 JQQ. All rights reserved.
* 依赖: console
* 描述: git clone/pull 进度 spinner / progress spinner for git clone/pull.
*
* 对标 Python `a2c_smcp/computer/cli/progress.py` 的 `clone_spinner`：clone/pull await 期显示 spinner；
* `--json` 或非终端（管道）→ no-op。Rust 用后台线程动画 + RAII guard（drop 即停并清行），spinner 写 stderr
* 以免污染 stdout 的 JSON 输出。
*/

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use console::Term;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 运行中的 spinner 句柄；drop 即停线程并清行 / a running spinner; drop stops the thread & clears the line。
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    fn start(description: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let term = Term::stderr();
            let mut i = 0usize;
            while !stop_for_thread.load(Ordering::Relaxed) {
                let _ = term.write_str(&format!("\r{} {description}", FRAMES[i % FRAMES.len()]));
                let _ = term.flush();
                i += 1;
                thread::sleep(Duration::from_millis(80));
            }
            let _ = term.clear_line();
        });
        Spinner {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// clone/pull 期 spinner；`enabled=false` 或非终端（`--json` / 管道）→ `None`（no-op）/ spinner, no-op when disabled。
///
/// 用法：`let _sp = clone_spinner("Cloning...", !json_output); handler().await; drop(_sp);`（guard 在域末自动停）。
#[must_use]
pub fn clone_spinner(description: &str, enabled: bool) -> Option<Spinner> {
    if !enabled || !console::user_attended_stderr() {
        return None;
    }
    Some(Spinner::start(description.to_string()))
}
