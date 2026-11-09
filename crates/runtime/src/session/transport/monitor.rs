//! 远端监控：采集脚本、输出解析与差值计算。
//!
//! 数据面是 SSH 连接上的一条独立 exec 通道（与 shell、SFTP 同一条连接）。
//! 脚本只输出原始计数器——CPU jiffies 与网络字节——占用率和速率全部在本地
//! 按两次采样的差值计算：远端 shell/awk 的版本差异再多也不会影响口径。
//!
//! 输出契约（每行一类，一个周期四行，disk 是末行）：
//! ```text
//! cpu <user> <nice> <system> <idle> <iowait> <irq> <softirq> <steal>
//! net <rx_bytes> <tx_bytes>          （聚合除 lo 外的全部接口）
//! mem <MemTotal_kib> <MemAvailable_kib>
//! disk <used_kib> <total_kib>        （根文件系统；df 不可用时该行缺失）
//! ```
//! 磁盘行到达即结算样本；下一周期 cpu 行到达时补结算缺了磁盘行的周期。

use std::io::Read;
use std::time::Instant;

use crate::session::{MonitorEvent, MonitorSample};

/// 采集脚本。POSIX sh + awk，只需 /proc 与 df 存在；不满足时脚本立即退出，
/// 通道随之 EOF（pump 据此报"监控不可用"），终端会话不受影响。
/// 采样节奏 sleep 2 秒；速率分母用的是本地到达时间差，不依赖脚本精确计时。
pub(super) const MONITOR_SCRIPT: &str = r#"while :
do
  read -r _ u n s i w q q2 st _ < /proc/stat || break
  printf 'cpu %s %s %s %s %s %s %s %s\n' "$u" "$n" "$s" "$i" "$w" "$q" "$q2" "$st"
  awk 'NR>2 && $1!="lo:" {r+=$2; t+=$10} END {printf "net %s %s\n", r+0, t+0}' /proc/net/dev
  awk '/^MemTotal:/ {t=$2} /^MemAvailable:/ {a=$2} END {printf "mem %s %s\n", t+0, a+0}' /proc/meminfo
  df -kP / 2>/dev/null | awk 'NR==2 {printf "disk %s %s\n", $3, $2}'
  sleep 2
done"#;

/// /proc/stat 一行的 8 个 jiffies 计数（user..steal，guest 字段并入 user 不重复计）。
#[derive(Clone, Copy)]
struct CpuCounters([u64; 8]);

/// 一个采样周期内已收到的各行。`at` 是 cpu 行（周期首行）的到达时刻，
/// 也是速率差值的分母锚点——补结算晚一个周期发生时不能用结算时刻。
struct Cycle {
    cpu: CpuCounters,
    net: Option<(u64, u64)>,
    mem: Option<(u64, u64)>,
    disk: Option<(u64, u64)>,
    /// 样本已产出：disk 行触发过，或被下一周期的 cpu 行补结算过
    flushed: bool,
    at: Instant,
}

/// 上一个周期的差值基准。首个周期只做校准，不产出样本。
struct PrevCycle {
    cpu: CpuCounters,
    net: Option<(u64, u64)>,
    at: Instant,
}

/// 监控输出的行解析状态机。
#[derive(Default)]
pub(super) struct MonitorAccum {
    cycle: Option<Cycle>,
    prev: Option<PrevCycle>,
}

impl MonitorAccum {
    /// 喂一行脚本输出；周期结算完成时返回一个样本。
    pub(super) fn feed_line(&mut self, line: &str, now: Instant) -> Option<MonitorSample> {
        let line = line.trim_end_matches('\r');

        if let Some(rest) = line.strip_prefix("cpu ") {
            // 新周期开始：上一周期若缺了磁盘行还没结算，现在补上
            let late = self.finish_cycle();
            self.cycle = parse_cpu(rest).map(|cpu| Cycle {
                cpu,
                net: None,
                mem: None,
                disk: None,
                flushed: false,
                at: now,
            });
            return late;
        }

        let pair = |rest: &str| -> Option<(u64, u64)> {
            let mut fields = rest.split_whitespace();
            let first = fields.next()?.parse().ok()?;
            let second = fields.next()?.parse().ok()?;
            (fields.next().is_none()).then_some((first, second))
        };

        match line.split_once(' ') {
            Some(("net", rest)) => {
                if let Some(cycle) = self.cycle.as_mut() {
                    cycle.net = pair(rest);
                }
            }
            Some(("mem", rest)) => {
                if let Some(cycle) = self.cycle.as_mut() {
                    // avail 为 0 说明远端内核太老没有 MemAvailable：宁可显示 —，不谎报全满
                    cycle.mem = pair(rest).and_then(|(total, avail)| {
                        (total > 0 && avail > 0).then(|| {
                            // 脚本输出单位是 KiB，样本契约是字节
                            let total_bytes = total.saturating_mul(1024);
                            (
                                total_bytes.saturating_sub(avail.saturating_mul(1024)),
                                total_bytes,
                            )
                        })
                    });
                }
            }
            Some(("disk", rest)) => {
                let value = pair(rest).map(|(used, total)| {
                    // 同上：KiB -> 字节
                    (used.saturating_mul(1024), total.saturating_mul(1024))
                });
                if let Some(cycle) = self.cycle.as_mut() {
                    cycle.disk = value;
                }
                // 磁盘行是脚本每个周期的末行：正常路径在这里结算
                return self.finish_cycle();
            }
            _ => {}
        }
        None
    }

    fn finish_cycle(&mut self) -> Option<MonitorSample> {
        let cycle = self.cycle.as_mut()?;
        if cycle.flushed {
            return None;
        }
        cycle.flushed = true;
        let (cpu, net, mem, disk, at) = (cycle.cpu, cycle.net, cycle.mem, cycle.disk, cycle.at);
        // 基准推进到本周期；首个周期只校准不出样本
        let prev = self.prev.replace(PrevCycle { cpu, net, at })?;

        // 速率分母用两个周期 cpu 行的到达时刻差：即使样本被下一周期补结算，
        // 分母仍是真实的采样间隔
        let dt = at.saturating_duration_since(prev.at).as_secs_f64();
        Some(MonitorSample {
            cpu_percent: cpu_percent(prev.cpu, cpu),
            mem,
            disk,
            rx_bytes_per_sec: byte_rate(net.map(|v| v.0), prev.net.map(|v| v.0), dt),
            tx_bytes_per_sec: byte_rate(net.map(|v| v.1), prev.net.map(|v| v.1), dt),
        })
    }
}

/// 监控通道的工作状态：跨读次的行缓冲与跨周期的计数器。通道搬运（非阻塞
/// 读、行切分、样本外发、终态上报）也住在这里——pump 每轮只调一次
/// [`MonitorChannelState::pump_round`]，与 sftp_task.rs 同一模式：pump 只调度，
/// 特征的实现在特征自己的模块。
pub(super) struct MonitorChannelState {
    channel: ssh2::Channel,
    pending: Vec<u8>,
    accum: MonitorAccum,
}

impl MonitorChannelState {
    pub(super) fn new(channel: ssh2::Channel) -> Self {
        Self {
            channel,
            pending: Vec::new(),
            accum: MonitorAccum::default(),
        }
    }

    /// 非阻塞搬运一轮：读通道、切行、喂解析器，样本 try_send 外发。
    /// 返回结束时给人看的原因（调用方据此上报 `Failed` 并弃掉本状态）；
    /// `None` 表示本轮没遇到终态。
    pub(super) fn pump_round(
        &mut self,
        buffer: &mut [u8],
        events: &smol::channel::Sender<MonitorEvent>,
    ) -> Option<&'static str> {
        loop {
            match self.channel.read(buffer) {
                Ok(0) => return Some("远端监控已结束（系统不受支持或脚本被终止）"),
                Ok(read) => {
                    self.pending.extend_from_slice(&buffer[..read]);
                    while let Some(newline) = self.pending.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = self.pending.drain(..=newline).collect();
                        let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                        if let Some(sample) = self.accum.feed_line(&line, Instant::now()) {
                            // 样本是瞬态数据：通道满就丢，下一个采样周期自然补上
                            let _ = events.try_send(MonitorEvent::Sample(sample));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return None,
                Err(_) => return Some("远端监控通道已断开"),
            }
        }
    }
}

fn parse_cpu(rest: &str) -> Option<CpuCounters> {
    let mut counters = [0u64; 8];
    let mut fields = rest.split_whitespace();
    for slot in &mut counters {
        *slot = fields.next()?.parse().ok()?;
    }
    Some(CpuCounters(counters))
}

/// 两次 jiffies 差值算占用率；idle 与 iowait 都算空闲。计数器回绕时本窗口放弃。
fn cpu_percent(prev: CpuCounters, current: CpuCounters) -> f32 {
    let mut total = 0u64;
    let mut idle = 0u64;
    for index in 0..8 {
        let Some(delta) = current.0[index].checked_sub(prev.0[index]) else {
            return 0.;
        };
        total += delta;
        if index == 3 || index == 4 {
            idle += delta;
        }
    }
    if total == 0 {
        return 0.;
    }
    ((total - idle) as f32 / total as f32 * 100.).clamp(0., 100.)
}

fn byte_rate(current: Option<u64>, prev: Option<u64>, dt: f64) -> f64 {
    let (Some(current), Some(prev)) = (current, prev) else {
        return 0.;
    };
    if dt <= 0. {
        return 0.;
    }
    current
        .checked_sub(prev)
        .map(|delta| delta as f64 / dt)
        .unwrap_or(0.)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::MonitorAccum;

    /// 喂完一个周期的行，返回其中产出的样本（至多一个）。
    fn feed_cycle(
        accum: &mut MonitorAccum,
        lines: &[&str],
        now: Instant,
    ) -> Option<crate::session::MonitorSample> {
        lines
            .iter()
            .filter_map(|line| accum.feed_line(line, now))
            .next()
    }

    /// 首周期只校准；第二周期起产出带差值的样本。
    #[test]
    fn first_cycle_calibrates_and_second_produces_rates() {
        let mut accum = MonitorAccum::default();
        let t0 = Instant::now();
        assert!(
            feed_cycle(
                &mut accum,
                &[
                    "cpu 100 1 100 7000 100 10 10 10",
                    "net 5000 2500",
                    "mem 8000000 4000000",
                    "disk 3000000 10000000",
                ],
                t0
            )
            .is_none(),
            "首个周期没有差值基准，不应产出样本"
        );

        let t1 = t0 + Duration::from_secs(2);
        let sample = feed_cycle(
            &mut accum,
            &[
                "cpu 150 1 150 7100 100 20 10 10",
                "net 7000 2600",
                "mem 8000000 3000000",
                "disk 3500000 10000000",
            ],
            t1,
        )
        .expect("第二周期应产出样本");

        // Δtotal=210（user50+nice0+sys50+idle100+iowait0+irq10+sirq0+steal0），空闲=100
        assert!((sample.cpu_percent - 110. / 210. * 100.).abs() < 0.01);
        assert_eq!(sample.rx_bytes_per_sec, 1000.);
        assert_eq!(sample.tx_bytes_per_sec, 50.);
        // 脚本单位是 KiB，样本契约是字节
        assert_eq!(sample.mem, Some((5_000_000 * 1024, 8_000_000 * 1024)));
        assert_eq!(sample.disk, Some((3_500_000 * 1024, 10_000_000 * 1024)));
    }

    /// 磁盘行缺失（df 不可用）时，下一周期的 cpu 行补结算；样本里磁盘为 None。
    #[test]
    fn missing_disk_line_settles_on_next_cycle() {
        let mut accum = MonitorAccum::default();
        let t0 = Instant::now();
        feed_cycle(
            &mut accum,
            &[
                "cpu 100 1 100 7000 100 10 10 10",
                "net 0 0",
                "mem 8000000 4000000",
                "disk 1 2",
            ],
            t0,
        );

        // 第二周期没有 disk 行：当周期不产出
        let t1 = t0 + Duration::from_secs(2);
        assert!(
            feed_cycle(
                &mut accum,
                &[
                    "cpu 150 1 150 7100 100 20 10 10",
                    "net 100 100",
                    "mem 8000000 3000000"
                ],
                t1
            )
            .is_none()
        );

        // 第三周期的 cpu 行补结算第二周期：磁盘 None，其余指标正常
        let t2 = t1 + Duration::from_secs(2);
        let sample = feed_cycle(&mut accum, &["cpu 200 1 200 7200 100 30 10 10"], t2)
            .expect("缺磁盘行的周期应由下一周期补结算");
        assert_eq!(sample.disk, None);
        assert_eq!(sample.mem, Some((5_000_000 * 1024, 8_000_000 * 1024)));
        assert_eq!(sample.rx_bytes_per_sec, 50.);
    }

    /// 计数器回绕（新值小于旧值）时比率归零，而不是 panic 或产出离谱值。
    #[test]
    fn counter_wraparound_yields_zero_rates() {
        let mut accum = MonitorAccum::default();
        let t0 = Instant::now();
        feed_cycle(
            &mut accum,
            &[
                "cpu 100 1 100 7000 100 10 10 10",
                &format!("net {} {}", u64::MAX - 100, 0),
                "mem 100 50",
                "disk 1 2",
            ],
            t0,
        );
        let sample = feed_cycle(
            &mut accum,
            &[
                "cpu 90 1 90 6900 100 10 10 10",
                "net 50 10",
                "mem 100 40",
                "disk 1 2",
            ],
            t0 + Duration::from_secs(2),
        )
        .expect("回绕周期仍应产出样本");
        assert_eq!(sample.cpu_percent, 0.);
        assert_eq!(sample.rx_bytes_per_sec, 0.);
        assert_eq!(sample.tx_bytes_per_sec, 10. / 2.);
    }

    /// 格式不对的行整行忽略（字段数不对即拒绝），不破坏后续周期。
    #[test]
    fn malformed_lines_are_ignored() {
        let mut accum = MonitorAccum::default();
        let t0 = Instant::now();
        feed_cycle(
            &mut accum,
            &[
                "cpu 1 1 1 1 1 1 1 1",
                "garbage",
                "net 0 0",
                "mem 100 50",
                "disk 1 2",
            ],
            t0,
        );
        // 第二周期：net 缺字段、disk 多字段——两者都应整行拒绝（值为 None），
        // 但 disk 行的到达仍是周期末行，照常触发结算
        let t1 = t0 + Duration::from_secs(2);
        let sample = feed_cycle(
            &mut accum,
            &["cpu 2 1 1 1 1 1 1 1", "net", "mem 100 40", "disk 1 2 3"],
            t1,
        )
        .expect("disk 行到达即结算");
        assert_eq!(sample.mem, Some((60 * 1024, 100 * 1024)));
        assert_eq!(sample.disk, None, "disk 行解析失败时磁盘为 None");
        assert_eq!(sample.rx_bytes_per_sec, 0., "net 行解析失败时速率为 0");
    }
}
