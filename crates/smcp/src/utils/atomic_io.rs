//! 原子文件写入工具：唯一临时文件 + `fsync` + 原子 `rename`。
//! Atomic file-write helpers: unique tmp + `fsync` + atomic `rename`。
//!
//! 治理层（SKILL staging / settings store）共用原语：写临时文件 → `flush` + `sync_all`（落盘）→
//! `rename` 原子替换。任意失败（含 panic 前的早退）清理半截临时文件，目标文件保持原子完好
//! （要么旧内容、要么新内容，绝无半截）。
//! Shared primitive for the governance layer: write tmp → flush + sync_all → atomic rename;
//! on failure the half-written tmp is removed and the target stays intact.
//!
//! 对标 Python 参考实现 / Mirrors the Python reference: `a2c_smcp/utils/atomic_io.py`。

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// 生成进程唯一的临时文件名 `<filename>.<pid>.<uuid8>.tmp` / Build a process-unique tmp filename。
///
/// 若多个进程共享目录且并发写同一目标，固定 `<name>.tmp` 会让两次写共用一个临时文件——第二次
/// `rename` 源已被前者搬走 → 失败。进程号 + 随机后缀彻底规避撞名。
///
/// A fixed `<name>.tmp` would collide under concurrent writers; the pid + random suffix avoids this.
pub fn unique_tmp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let rand8: String = Uuid::new_v4().simple().to_string()[..8].to_string();
    let mut name: OsString = path.file_name().map(ToOwned::to_owned).unwrap_or_default();
    name.push(format!(".{pid}.{rand8}.tmp"));
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// 原子写字节：父目录 `create_dir_all` + 唯一临时文件 + `flush` + `sync_all` + `rename`。
/// Atomic bytes write (unique tmp + `fsync` + atomic rename).
///
/// `rename` 前 `sync_all` 保证数据已落盘；任意失败清理半截临时文件后返回错误，目标文件保持原子完好。
pub fn atomic_write_bytes(path: &Path, payload: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = unique_tmp_path(path);

    // 写入 + 落盘；任何一步失败都清理临时文件。
    let write_result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(payload)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // 原子替换；失败同样清理临时文件，绝不留垃圾。
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 原子写文本：按 UTF-8 编码后走 [`atomic_write_bytes`]（二进制写，不做平台换行翻译）。
/// Atomic text write: UTF-8 encode then delegate to [`atomic_write_bytes`] (binary, no newline xlate).
pub fn atomic_write_text(path: &Path, text: &str) -> io::Result<()> {
    atomic_write_bytes(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程内唯一的测试目录（避免污染 / 并发冲突）；用 pid + uuid 防撞名。
    fn temp_dir(tag: &str) -> PathBuf {
        let rand = Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("a2c-atomic-{tag}-{}-{rand}", std::process::id()))
    }

    #[test]
    fn test_unique_tmp_path_shape_and_uniqueness() {
        let target = Path::new("/some/dir/config.json");
        let a = unique_tmp_path(target);
        let b = unique_tmp_path(target);
        assert_ne!(a, b, "两次生成的临时名必须不同");
        let a_name = a.file_name().unwrap().to_str().unwrap();
        assert!(
            a_name.starts_with("config.json."),
            "保留原文件名前缀: {a_name}"
        );
        assert!(a_name.ends_with(".tmp"), "以 .tmp 结尾: {a_name}");
        assert_eq!(a.parent(), Some(Path::new("/some/dir")), "与目标同目录");
    }

    #[test]
    fn test_atomic_write_roundtrip_and_no_tmp_residue() {
        let dir = temp_dir("rt");
        let target = dir.join("nested").join("data.bin");
        let payload = b"hello atomic world";

        atomic_write_bytes(&target, payload).expect("写入应成功");
        assert_eq!(fs::read(&target).unwrap(), payload);

        // 成功写入后不残留任何 .tmp
        let residue: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "成功写入不得残留临时文件");

        // 覆盖写：目标始终为完整新内容（原子替换）
        atomic_write_text(&target, "second").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_atomic_write_failure_leaves_no_tmp() {
        // 目标父路径下存在一个**文件**与期望目录同名 → create_dir_all 失败 → 不应残留 tmp。
        let dir = temp_dir("fail");
        fs::create_dir_all(&dir).unwrap();
        let clash = dir.join("clash");
        fs::write(&clash, b"i am a file").unwrap();
        // clash 是文件，却被当作目录 → 写 clash/inside.txt 必失败
        let target = clash.join("inside.txt");
        let result = atomic_write_bytes(&target, b"x");
        assert!(result.is_err(), "父路径是文件时写入应失败");

        // dir 下除 clash 外不得有任何 .tmp 残留
        let residue: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "失败写入不得残留临时文件");

        let _ = fs::remove_dir_all(&dir);
    }
}
