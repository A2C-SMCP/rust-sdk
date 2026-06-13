//! 惰性切片的边界规划（EOF-probe / 范围越界 / clamp 的单一真相源）/ lazy-slice boundary planner。
//!
//! 治理层多个「惰性切片视图」（blob `ResolvedBlob` / skill `SkillResourceView`）共享同一边界语义：
//! `offset > total` 越界、`offset == total` 为 EOF probe（返回空、**非**错误）、`length` 超剩余截断。
//! 把该判定收敛到纯函数 [`plan_slice`]，避免各视图各写一份导致语义漂移。
//!
//! EOF probe（`offset == total_size` 返回空、`>` 才越界）与 `client:get_blob` 严格 `>` 范围守卫一致，
//! 保留 HTTP-Range 风格「探测末尾」客户端行为。

/// 切片规划结果 / a slice plan。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlicePlan {
    /// `offset > total_size`：范围越界（调用方映射各自的 `range` / `InvalidInput` 错误）。
    OutOfRange,
    /// 空结果（`length == 0` 或 `offset == total_size` 的 EOF probe），无需 I/O。
    Empty,
    /// 需读取 `[offset, offset+length)`（`length` 已 clamp 到剩余）/ a clamped read range。
    Read {
        /// 起始偏移 / start offset。
        offset: u64,
        /// 已 clamp 的读取长度 / clamped read length。
        length: u64,
    },
}

/// 规划一次惰性切片 / Plan one lazy slice。
///
/// 边界语义：`offset > total_size` → [`SlicePlan::OutOfRange`]；`length == 0` 或
/// `offset == total_size`（EOF probe）→ [`SlicePlan::Empty`]；否则 [`SlicePlan::Read`]，`length`
/// 截断为 `total_size - offset`。
pub fn plan_slice(offset: u64, length: u64, total_size: u64) -> SlicePlan {
    if offset > total_size {
        SlicePlan::OutOfRange
    } else if length == 0 || offset == total_size {
        SlicePlan::Empty
    } else {
        SlicePlan::Read {
            offset,
            length: length.min(total_size - offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_slice_boundary_matrix() {
        // 正常读 / normal read。
        assert_eq!(
            plan_slice(0, 4, 10),
            SlicePlan::Read {
                offset: 0,
                length: 4
            }
        );
        // 超长 → 截断到剩余 / over-length truncates。
        assert_eq!(
            plan_slice(8, 100, 10),
            SlicePlan::Read {
                offset: 8,
                length: 2
            }
        );
        // length == 0 → 空 / empty。
        assert_eq!(plan_slice(2, 0, 10), SlicePlan::Empty);
        // offset == total（EOF probe）→ 空 / empty。
        assert_eq!(plan_slice(10, 5, 10), SlicePlan::Empty);
        // offset > total → 越界 / out of range。
        assert_eq!(plan_slice(11, 1, 10), SlicePlan::OutOfRange);
        // 空资源 / empty resource。
        assert_eq!(plan_slice(0, 0, 0), SlicePlan::Empty);
        assert_eq!(plan_slice(0, 5, 0), SlicePlan::Empty);
        assert_eq!(plan_slice(1, 5, 0), SlicePlan::OutOfRange);
    }
}
