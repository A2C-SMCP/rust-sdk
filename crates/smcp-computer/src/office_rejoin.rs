//! Computer 自动回房的有界退避策略（#234）。
//!
//! 与 `crates/smcp-agent/src/office.rs` 同源：静默断线后服务端尚未回收旧会话时，重连回放
//! `server:join_office` 撞 `4101` 属**瞬态**冲突；协议只允许客户端在**传输层重连后的恢复
//! 路径**做有界退避重试（见 room-model §静默断线与会话回收、error-handling §建议的重试策略）。
//!
//! 本 crate 不能依赖 `smcp-agent`（其策略函数为 `pub(crate)`），故在此镜像同一曲线与白名单；
//! 修改退避曲线或可重试码时须同步两侧。

use std::time::Duration;

use crate::errors::ComputerError;

/// `4101 Room Full`：本 role 席位被占（protocol#66 泛化后 Computer 席同码）。
/// 只有该码在重连恢复路径上可重试。
pub(crate) const TRANSIENT_ROOM_CONFLICT_CODE: i64 = smcp::error_codes::ROOM_FULL as i64;

/// 首档退避（第 1 次失败后等待 1s）。
pub(crate) const OFFICE_REJOIN_BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// 退避上限（1→2→4→8s，此后封顶）。
pub(crate) const OFFICE_REJOIN_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// 第 `attempt` 次尝试失败后的退避间隔（`attempt` 自 1 起）：1→2→4→8s，封顶 8s。
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(3);
    OFFICE_REJOIN_BACKOFF_INITIAL
        .saturating_mul(1u32 << shift)
        .min(OFFICE_REJOIN_BACKOFF_MAX)
}

/// 预算约束「下一次尝试的起始时刻」：退避后仍落在预算内才允许再发一次。
///
/// 与 Agent 侧 `retry_fits_budget` 同义；使用饱和加法避免极端配置溢出。
pub(crate) fn retry_fits_budget(elapsed: Duration, delay: Duration, budget: Duration) -> bool {
    elapsed.saturating_add(delay) <= budget
}

/// 仅 `4101` 是重连恢复路径上的瞬态冲突。
///
/// `4105` 已转预留码（收到即协议违规）、`4106` 是客户端状态错误、其余码与传输错误
/// 重试不改变结果——一律不可重试。
pub(crate) fn is_transient_room_conflict(error: &ComputerError) -> bool {
    matches!(
        error,
        ComputerError::ProtocolRejection { code, .. } if *code == TRANSIENT_ROOM_CONFLICT_CODE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejection(code: i64) -> ComputerError {
        ComputerError::ProtocolRejection {
            code,
            message: "rejected".to_string(),
            details: None,
        }
    }

    #[test]
    fn backoff_curve_doubles_then_caps() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(100), Duration::from_secs(8));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(8));
    }

    #[test]
    fn retry_budget_constrains_next_attempt_start() {
        // 0 预算 = 单次尝试：任何退避都放不下。
        assert!(!retry_fits_budget(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::ZERO
        ));
        // 边界：elapsed + delay == budget 仍算放得下。
        assert!(retry_fits_budget(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(1)
        ));
        assert!(!retry_fits_budget(
            Duration::from_millis(1),
            Duration::from_secs(1),
            Duration::from_secs(1)
        ));
        // 默认预算下的最后一档：52s + 8s 放得下，53s + 8s 放不下。
        assert!(retry_fits_budget(
            Duration::from_secs(52),
            Duration::from_secs(8),
            Duration::from_secs(60)
        ));
        assert!(!retry_fits_budget(
            Duration::from_secs(53),
            Duration::from_secs(8),
            Duration::from_secs(60)
        ));
        // 饱和运算：不 panic。
        assert!(retry_fits_budget(
            Duration::MAX,
            Duration::MAX,
            Duration::MAX
        ));
    }

    #[test]
    fn only_4101_is_transient() {
        assert!(is_transient_room_conflict(&rejection(
            smcp::error_codes::ROOM_FULL as i64
        )));
        for code in [400, 4008, 4102, 4103, 4104, 4105, 4106, 4019, 9999] {
            assert!(
                !is_transient_room_conflict(&rejection(code)),
                "code {code} must not be retried"
            );
        }
        assert!(!is_transient_room_conflict(
            &ComputerError::ConnectionError("reset".to_string())
        ));
        assert!(!is_transient_room_conflict(&ComputerError::SocketIoError(
            "closed".to_string()
        )));
        assert!(!is_transient_room_conflict(&ComputerError::TimeoutError(
            "timeout".to_string()
        )));
        assert!(!is_transient_room_conflict(&ComputerError::ProtocolError(
            "unrecognized ack".to_string()
        )));
    }
}
