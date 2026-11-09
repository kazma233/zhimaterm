//! 自动重连的裁决单点（ADR-013）：资格 × 退避 × 上限 × 取消 × 看门狗。
//!
//! 纯状态机、零 gpui 依赖（重连计划，见 GLOSSARY）：事件与时钟由视图喂入，
//! 视图只消费决定——发起重连、倒计时渲染与文案措辞。资格（认证成功过 ×
//! 可重连的目标 × 意外断线）与策略同居这里，全部转移可无头测试。

use std::time::{Duration, Instant};

use crate::session::ExitReason;

/// 连接看门狗：传输层两段预算（45s×2）+ TCP 余量之后，界面必须给出明确失败。
const CONNECT_WATCHDOG: Duration = Duration::from_secs(120);

/// 退避基数：第 n 次失败后等 1s << n（1/2/4/8/16 秒）。
const BASE_DELAY: Duration = Duration::from_secs(1);

/// 每秒喂给看门狗的连接期事实：视图原样转述自己的状态，不做判断——
/// 「本地与信任弹窗期间不判」是这里的裁决语义，不是调用方的纪律。
pub struct ConnectFacts {
    /// 还在等首帧快照（连接进行中）
    pub connecting: bool,
    /// 本地格：没有传输预算，启动失败由 PTY/Shell 确定性报出
    pub local: bool,
    /// 主机确认弹窗开着：连接是"挂着"，不是死了
    pub trust_pending: bool,
    /// 本次连接的开始时刻
    pub started_at: Instant,
}

/// 排下一次自动重连的结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScheduleOutcome {
    /// 已排定：到点由 [`ReconnectPlanner::tick`] 发起
    Planned,
    /// 没资格：从未认证成功（防爆破）、不可重连的目标或非断线退出
    Ineligible,
    /// 次数用尽：停在结束态，仍可手动 R（满额重来）
    Exhausted,
}

/// 每秒一次的裁决。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TickOutcome {
    /// 什么都不用做
    Idle,
    /// 到点：发起下一次自动重连（尝试计数已自增）
    Fire,
    /// 远端连接超时，视同断线并已排计划（或已用尽/没资格）；调用方置结束态
    WatchdogFired(ScheduleOutcome),
}

/// 底栏倒计时数据（渲染措辞与折算在视图）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AutoReconnectState {
    /// 距下次自动重连的剩余秒数
    pub seconds_left: u64,
    /// 即将进行的是第几次尝试（1 起）
    pub attempt: u32,
    pub max_attempts: u32,
}

/// 自动重连的裁决单点：持有资格事实与尝试状态，接口事件进、决定出，
/// 时间全部由调用方喂入（无头可测）。
pub struct ReconnectPlanner {
    /// 认证成功过（收到过 Started）：断线自动重连的资格（ADR-013）——
    /// 认证失败/连接失败不重试，错误口令反复撞服务器会触发防爆破封锁
    ever_authenticated: bool,
    /// 只有 SSH 会话可原地重连（本地格没有可重连的目标）
    reconnectable: bool,
    /// 已完成的自动尝试次数；成功连接即归零
    attempts: u32,
    /// 下次自动重连的时刻；None = 没有进行中的自动重连
    plan: Option<Instant>,
}

impl ReconnectPlanner {
    pub const MAX_ATTEMPTS: u32 = 5;

    /// `reconnectable`：会话是否有可原地重连的目标（SSH 有、本地无）。
    pub fn new(reconnectable: bool) -> Self {
        Self {
            ever_authenticated: false,
            reconnectable,
            attempts: 0,
            plan: None,
        }
    }

    /// 认证成功（会话 Started）：这是断线自动重连的资格；同时给新一轮满额尝试。
    pub fn started(&mut self) {
        self.ever_authenticated = true;
        self.attempts = 0;
        self.plan = None;
    }

    /// 会话结束。意外断线按退避排下一次；主动结束（exit / 关闭）停在结束态，
    /// R 重连与 Enter 关闭由用户选（ADR-005/013）。
    pub fn exited(&mut self, reason: ExitReason, now: Instant) -> ScheduleOutcome {
        if reason.is_disconnect() {
            self.schedule(now)
        } else {
            self.plan = None;
            ScheduleOutcome::Ineligible
        }
    }

    /// 连接/重连尝试失败：首次失败不自动重试（防爆破），重连途中的失败继续
    /// 按退避推进——资格（认证成功过）天然区分这两者。
    pub fn connect_failed(&mut self, now: Instant) -> ScheduleOutcome {
        self.schedule(now)
    }

    /// 每秒一次。到点发起下一次尝试；远端握手停滞超过看门狗预算视同断线。
    pub fn tick(&mut self, now: Instant, facts: ConnectFacts) -> TickOutcome {
        // 到点优先：排了计划的会话已退出，与看门狗（连接中）不会并发
        if let Some(next_at) = self.plan
            && now >= next_at
        {
            self.plan = None;
            self.attempts += 1;
            return TickOutcome::Fire;
        }
        // 看门狗只管远端握手：本地没有传输预算；信任弹窗挂起期间连接没有死。
        // 误判会把活着的会话标成结束态，Enter 会关掉它
        if facts.connecting
            && !facts.local
            && !facts.trust_pending
            && now.duration_since(facts.started_at) > CONNECT_WATCHDOG
        {
            return TickOutcome::WatchdogFired(self.schedule(now));
        }
        TickOutcome::Idle
    }

    /// 用户按 Esc：取消自动重连计划（手动 R 与关闭不受影响）。返回是否确有
    /// 计划被取消——取消要有反馈。
    pub fn user_cancel(&mut self) -> bool {
        self.plan.take().is_some()
    }

    /// 用户按 R：取消自动计划并把尝试计数归零，给全新的满额额度；
    /// 本次发起由调用方立即执行。
    pub fn user_manual(&mut self) {
        self.attempts = 0;
        self.plan = None;
    }

    /// 底栏倒计时数据：没有进行中的计划为 None。
    pub fn display(&self, now: Instant) -> Option<AutoReconnectState> {
        let next_at = self.plan?;
        Some(AutoReconnectState {
            seconds_left: next_at.checked_duration_since(now)?.as_secs(),
            attempt: self.attempts + 1,
            max_attempts: Self::MAX_ATTEMPTS,
        })
    }

    /// 是否有进行中的自动重连计划（时钟该不该继续走的依据之一）。
    pub fn has_plan(&self) -> bool {
        self.plan.is_some()
    }

    fn schedule(&mut self, now: Instant) -> ScheduleOutcome {
        if !self.ever_authenticated || !self.reconnectable {
            self.plan = None;
            return ScheduleOutcome::Ineligible;
        }
        if self.attempts >= Self::MAX_ATTEMPTS {
            self.plan = None;
            return ScheduleOutcome::Exhausted;
        }
        // 1/2/4/8/16 秒：尝试次数越多退避越长
        let delay = BASE_DELAY * (1u32 << self.attempts);
        self.plan = Some(now + delay);
        ScheduleOutcome::Planned
    }
}

#[cfg(test)]
mod tests {
    use super::{CONNECT_WATCHDOG, ConnectFacts, ReconnectPlanner, ScheduleOutcome, TickOutcome};
    use crate::session::ExitReason;
    use std::time::{Duration, Instant};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn idle(started_at: Instant) -> ConnectFacts {
        ConnectFacts {
            connecting: false,
            local: false,
            trust_pending: false,
            started_at,
        }
    }

    fn connecting(local: bool, trust_pending: bool, started_at: Instant) -> ConnectFacts {
        ConnectFacts {
            connecting: true,
            local,
            trust_pending,
            started_at,
        }
    }

    fn planned(planner: &mut ReconnectPlanner, base: Instant) {
        assert_eq!(
            planner.exited(ExitReason::ConnectionLost, base),
            ScheduleOutcome::Planned
        );
    }

    /// 退避全链：断线 → 1s → 失败 → 2s → 4s → 8s → 16s → 用尽。
    #[test]
    fn backoff_advances_1_2_4_8_16_then_exhausts() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        let base = Instant::now();
        let mut elapsed = 0;
        // 首轮由断线触发
        planned(&mut planner, base + secs(elapsed));
        for delay in [1, 2, 4, 8, 16] {
            assert_eq!(
                planner.tick(base + secs(elapsed + delay - 1), idle(base)),
                TickOutcome::Idle,
                "到点前一秒不发起（第 {delay} 秒轮）"
            );
            assert_eq!(
                planner.tick(base + secs(elapsed + delay), idle(base)),
                TickOutcome::Fire,
                "到点发起（等 {delay} 秒的轮）"
            );
            elapsed += delay;
            let outcome = planner.connect_failed(base + secs(elapsed));
            if delay < 16 {
                assert_eq!(outcome, ScheduleOutcome::Planned, "未用尽应继续退避");
            } else {
                assert_eq!(outcome, ScheduleOutcome::Exhausted, "第 5 次失败后用尽");
            }
        }
        assert!(!planner.has_plan());
        // 用尽后断线再排也不排了
        assert_eq!(
            planner.exited(ExitReason::ConnectionLost, base + secs(elapsed)),
            ScheduleOutcome::Exhausted
        );
    }

    /// 首次连接失败不自动重试（防爆破）：资格守门，从未认证成功就没有计划。
    #[test]
    fn first_failure_without_authentication_never_schedules() {
        let mut planner = ReconnectPlanner::new(true);
        let now = Instant::now();
        assert_eq!(
            planner.connect_failed(now),
            ScheduleOutcome::Ineligible,
            "首次连接失败不排计划"
        );
        assert_eq!(
            planner.exited(ExitReason::ConnectionLost, now),
            ScheduleOutcome::Ineligible,
            "从未认证成功，断线同样没资格"
        );
        assert!(!planner.has_plan());
    }

    /// 主动结束（exit / 关闭）停在结束态，不自动爬起来（ADR-013 修订过的
    /// 真 bug：用户 exit 也自动重连）。
    #[test]
    fn voluntary_exits_stay_ended() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        let now = Instant::now();
        assert_eq!(
            planner.exited(ExitReason::ShellExit, now),
            ScheduleOutcome::Ineligible
        );
        assert_eq!(
            planner.exited(ExitReason::Closed, now),
            ScheduleOutcome::Ineligible
        );
        assert!(!planner.has_plan());
    }

    /// 本地格没有可重连的目标：任何事件都不排计划。
    #[test]
    fn unreconnectable_sessions_never_schedule() {
        let mut planner = ReconnectPlanner::new(false);
        planner.started();
        assert_eq!(
            planner.exited(ExitReason::ConnectionLost, Instant::now()),
            ScheduleOutcome::Ineligible
        );
        assert!(!planner.has_plan());
    }

    /// 重连成功（Started）：计数归零，新一轮从 1 秒起满额退避。
    #[test]
    fn successful_reconnect_restores_full_budget() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        let base = Instant::now();
        // 消耗两次尝试（等过 1s 与 2s）
        planned(&mut planner, base);
        assert_eq!(planner.tick(base + secs(1), idle(base)), TickOutcome::Fire);
        assert_eq!(
            planner.connect_failed(base + secs(1)),
            ScheduleOutcome::Planned
        );
        assert_eq!(planner.tick(base + secs(3), idle(base)), TickOutcome::Fire);
        // 重连成功：新一轮应从 1 秒重新起步，而不是接着 4 秒
        planner.started();
        planned(&mut planner, base + secs(3));
        assert_eq!(planner.tick(base + secs(3), idle(base)), TickOutcome::Idle);
        assert_eq!(planner.tick(base + secs(4), idle(base)), TickOutcome::Fire);
    }

    /// Esc 取消：清掉进行中的计划并反馈发生过；没有计划时不误报。
    #[test]
    fn user_cancel_clears_the_plan_once() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        planned(&mut planner, Instant::now());
        assert!(planner.user_cancel(), "有计划时应反馈已取消");
        assert!(!planner.has_plan());
        assert!(!planner.user_cancel(), "没有计划就没有取消这回事");
    }

    /// R 手动：清计数、清计划，下一次失败从 1 秒满额起步。
    #[test]
    fn user_manual_resets_budget_for_immediate_retry() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        let base = Instant::now();
        planned(&mut planner, base);
        assert_eq!(planner.tick(base + secs(1), idle(base)), TickOutcome::Fire);
        planner.user_manual();
        assert!(!planner.has_plan());
        assert_eq!(
            planner.connect_failed(base + secs(1)),
            ScheduleOutcome::Planned
        );
        // 计数已归零：还是 1 秒的轮，不是 2 秒
        assert_eq!(planner.tick(base + secs(1), idle(base)), TickOutcome::Idle);
        assert_eq!(planner.tick(base + secs(2), idle(base)), TickOutcome::Fire);
    }

    /// 看门狗三个边界：远端停滞超预算才触发；本地不判；信任弹窗期间不判。
    /// 恰好等于预算不算——要严格超过。
    #[test]
    fn watchdog_fires_only_for_stalled_remote_handshake() {
        let base = Instant::now();
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        assert_eq!(
            planner.tick(base + CONNECT_WATCHDOG, connecting(false, false, base)),
            TickOutcome::Idle,
            "恰好等于预算还不算，严格大于才触发"
        );
        match planner.tick(
            base + CONNECT_WATCHDOG + secs(1),
            connecting(false, false, base),
        ) {
            TickOutcome::WatchdogFired(ScheduleOutcome::Planned) => {}
            other => panic!("远端停滞应触发看门狗并排计划：{other:?}"),
        }
        assert!(planner.has_plan(), "看门狗触发后应排重连计划");
        // 清掉计划再做边界断言：到点的计划优先于一切，会抢先返回 Fire
        planner.user_cancel();
        // 本地会话不判：连接再久也不触发
        assert_eq!(
            planner.tick(base + CONNECT_WATCHDOG * 3, connecting(true, false, base)),
            TickOutcome::Idle
        );
        // 信任弹窗期间不判：连接是挂着，不是死了
        assert_eq!(
            planner.tick(base + CONNECT_WATCHDOG * 3, connecting(false, true, base)),
            TickOutcome::Idle
        );
        // 未认证成功的会话触发看门狗也排不了计划（资格守门）
        let mut never_connected = ReconnectPlanner::new(true);
        assert_eq!(
            never_connected.tick(base + CONNECT_WATCHDOG * 2, connecting(false, false, base)),
            TickOutcome::WatchdogFired(ScheduleOutcome::Ineligible)
        );
        assert!(!never_connected.has_plan());
    }

    /// 到点边界的精度：毫秒级早一点不发起，到秒发起。
    #[test]
    fn tick_fires_only_at_or_after_the_planned_instant() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        let base = Instant::now();
        planned(&mut planner, base);
        assert_eq!(
            planner.tick(base + Duration::from_millis(999), idle(base)),
            TickOutcome::Idle
        );
        assert_eq!(planner.tick(base + secs(1), idle(base)), TickOutcome::Fire);
    }

    /// 倒计时折算：剩余秒数、即将第几次、上限；无计划为 None。
    #[test]
    fn display_reports_seconds_attempt_and_cap() {
        let mut planner = ReconnectPlanner::new(true);
        planner.started();
        assert_eq!(planner.display(Instant::now()), None, "没有计划没有倒计时");
        let base = Instant::now();
        planned(&mut planner, base);
        assert_eq!(
            planner.display(base),
            Some(super::AutoReconnectState {
                seconds_left: 1,
                attempt: 1,
                max_attempts: 5,
            })
        );
        // 第一次失败后：等 2 秒、即将第 2 次
        assert_eq!(planner.tick(base + secs(1), idle(base)), TickOutcome::Fire);
        assert_eq!(
            planner.connect_failed(base + secs(1)),
            ScheduleOutcome::Planned
        );
        assert_eq!(
            planner.display(base + secs(1)),
            Some(super::AutoReconnectState {
                seconds_left: 2,
                attempt: 2,
                max_attempts: 5,
            })
        );
    }
}
