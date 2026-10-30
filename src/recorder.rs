//! 全局记录门面（ADR-045）：会话输出日志（ADR-017）与操作记录（ADR-044）
//! 的统一埋点入口。埋点只说「记什么」，记到哪里、怎么写由装配期的注册条目
//! 决定——日志句柄不再从视图穿进 SessionConfig、再穿进 worker，历史实体
//! 不再注入 SFTP 面板。
//!
//! 三个入口面：
//! - 生命周期（UI 线程）：建格注册、格销毁注销（TerminalView 的 Drop）；
//! - worker 侧埋点：线程入口 `bind_pane` 绑定本格（会话是专用 std 线程，
//!   不跨线程漂移），之后任意深度无参调用；
//! - UI 侧查询与开关：显式带格 id。
//!
//! 未注册的格：查询 None、写入 no-op。「该格没有记录」是生命周期边界的
//! 正确语义（格已释放或本就没装配），不是吞错。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use gpui::{App, Entity, EntityId};

use crate::output_log::SessionLog;
use crate::views::history::{SessionHistory, TaskItem};

/// 一格的记录装配：记到哪里、怎么写在注册时定死（装配期配置），运行期
/// 唯一的动态是日志开关。
struct PaneRecorder {
    /// 输出日志 sink：共享锁里的 Option——worker 线程写行、UI 线程切开关，
    /// 各自就近加锁，互不穿透
    log: Arc<Mutex<Option<SessionLog>>>,
    /// 日志文件路径：第一次开启时生成并记下，暂停再开按它重开
    log_path: Option<PathBuf>,
    /// 装配期来源标签：首次开启日志时生成文件名用
    source: String,
    /// 操作记录实体：仅 SSH 格挂（写入方 SFTP 面板、读取方模态都在 SSH
    /// 会话）；注册表持有强引用，格注销时随之释放
    history: Option<Entity<SessionHistory>>,
}

fn registry() -> &'static RwLock<HashMap<EntityId, PaneRecorder>> {
    static REGISTRY: OnceLock<RwLock<HashMap<EntityId, PaneRecorder>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

// ---------- 生命周期 ----------

/// 建格注册日志条目：不开日志也注册——开关是运行期切的，条目得先在。
/// 不依赖 gpui 上下文，e2e 测试与生产走同一条注册路径。
pub fn attach_log(pane: EntityId, source: &str) {
    let existing = registry().write().unwrap().insert(
        pane,
        PaneRecorder {
            log: Arc::new(Mutex::new(None)),
            log_path: None,
            source: source.to_owned(),
            history: None,
        },
    );
    debug_assert!(
        existing.is_none(),
        "格重复注册：上一格未注销（Drop 路径丢了 unregister？）"
    );
}

/// SSH 格挂操作记录实体（UI 线程）。
pub fn attach_history(pane: EntityId, history: Entity<SessionHistory>) {
    if let Some(entry) = registry().write().unwrap().get_mut(&pane) {
        entry.history = Some(history);
    }
}

/// 格销毁注销（TerminalView 的 Drop 调）。正常时序 worker 已随会话退出；
/// 即便异常时序，worker 的线程局部缓存自己持有 sink 引用，摘表不影响它，
/// 句柄随线程结束释放。
pub fn unregister_pane(pane: EntityId) {
    registry().write().unwrap().remove(&pane);
}

/// 测试注入：整格替换注册（e2e 用只读句柄预置「写失败」的日志、用真实
/// 日志走开关全链路）。与生产 [`attach_log`] 的区别只在 sink 与路径的初值。
#[cfg(test)]
pub fn attach_pane_with_log(
    pane: EntityId,
    source: &str,
    log: Arc<Mutex<Option<SessionLog>>>,
    log_path: Option<PathBuf>,
) {
    registry().write().unwrap().insert(
        pane,
        PaneRecorder {
            log,
            log_path,
            source: source.to_owned(),
            history: None,
        },
    );
}

// ---------- worker 侧埋点（线程绑定，无参） ----------

thread_local! {
    /// 会话线程的当前格日志 sink：线程入口绑定一次，之后任意深度的埋点
    /// 无参调用，写入零查表
    static CURRENT_LOG: RefCell<Option<Arc<Mutex<Option<SessionLog>>>>> =
        const { RefCell::new(None) };
}

fn with_current_log<T>(f: impl FnOnce(&Arc<Mutex<Option<SessionLog>>>) -> T) -> Option<T> {
    CURRENT_LOG.with_borrow(|sink| sink.as_ref().map(f))
}

/// 锁 sink：锁内不会 panic，万一毒化也取回内层继续——日志数据没有需要
/// 保护的不变量，拿回 Option 比让会话线程 panic 更符合「日志绝不拖垮
/// 会话」（ADR-017）。
fn lock_sink(sink: &Arc<Mutex<Option<SessionLog>>>) -> MutexGuard<'_, Option<SessionLog>> {
    sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 会话线程入口：绑定本格（从注册表取 sink 缓存到线程局部）。格不在表里
/// （测试未注册）绑成 None，全部埋点 no-op。
pub fn bind_pane(pane: EntityId) {
    let sink = registry()
        .read()
        .unwrap()
        .get(&pane)
        .map(|entry| entry.log.clone());
    CURRENT_LOG.with_borrow_mut(|current| *current = sink);
}

/// 一次会话尝试的左边界：连不上的尝试也留下一对标记（ADR-017）。
pub fn begin_session(label: &str) {
    with_current_log(|sink| {
        if let Some(log) = lock_sink(sink).as_mut() {
            log.begin_session(label);
        }
    });
}

/// 当前是否在记：worker 用它检测运行时开关的恢复边沿（恢复时丢弃暂停
/// 期间攒下的行）。
pub fn is_logging() -> bool {
    with_current_log(|sink| lock_sink(sink).is_some()).unwrap_or(false)
}

/// 记一行渲染后的输出。写失败（磁盘满等）在 sink 内停写不致命（ADR-017
/// 的旁路契约），失败原因经 [`take_log_failure`] 交出、只报一次。
pub fn output_line(line: &str) {
    with_current_log(|sink| {
        if let Some(log) = lock_sink(sink).as_mut() {
            log.write_line(line);
        }
    });
}

/// 取走首次写失败的原因（take 语义：同一失败只交出一次）。
pub fn take_log_failure() -> Option<String> {
    with_current_log(|sink| lock_sink(sink).as_mut().and_then(|log| log.take_failure())).flatten()
}

/// 会话线程的收尾出口：写右边界并交出累积的写失败。主循环没跑过就提前
/// 失败的写错误（如首写失败且连接随即失败）只有这里能报。
pub fn finish_session() -> Option<String> {
    with_current_log(|sink| {
        let mut guard = lock_sink(sink);
        match guard.as_mut() {
            Some(log) => {
                log.end_session();
                log.take_failure()
            }
            None => None,
        }
    })
    .flatten()
}

// ---------- UI 侧：日志开关与查询（显式带格） ----------

/// 运行时开关：开 = 有文件按原文件追加重开、没有则新建；关 = 停写并落
/// 「记录暂停」标记。返回是否成功（失败由调用方亮警示）。不写进任何设置
/// 或连接存储，只作用于本格本会话（ADR-017）。
pub fn set_logging(pane: EntityId, enabled: bool) -> bool {
    let read = registry().read().unwrap();
    let Some(entry) = read.get(&pane) else {
        return false;
    };
    let sink = entry.log.clone();
    let path = entry.log_path.clone();
    let source = entry.source.clone();
    drop(read);

    let mut log = lock_sink(&sink);
    if enabled == log.is_some() {
        return true;
    }
    if enabled {
        // 打开动作先算好（借用在赋值前结束）：有文件按原文件追加重开，
        // 没有则新建
        let opened = match &path {
            Some(path) => SessionLog::open_at(path),
            None => crate::output_log::create(&source),
        };
        match opened {
            Ok(mut session_log) => {
                if path.is_none()
                    && let Some(entry) = registry().write().unwrap().get_mut(&pane)
                {
                    entry.log_path = Some(session_log.path().to_path_buf());
                }
                session_log.mark("记录开启");
                *log = Some(session_log);
                true
            }
            Err(_) => false,
        }
    } else if let Some(mut session_log) = log.take() {
        session_log.mark("记录暂停");
        true
    } else {
        true
    }
}

/// 本格是否正在记录。
pub fn logging_enabled(pane: EntityId) -> bool {
    registry()
        .read()
        .unwrap()
        .get(&pane)
        .is_some_and(|entry| lock_sink(&entry.log).is_some())
}

/// 本格日志文件的路径：从未开过日志为 None。
pub fn log_path(pane: EntityId) -> Option<PathBuf> {
    registry()
        .read()
        .unwrap()
        .get(&pane)
        .and_then(|entry| entry.log_path.clone())
}

// ---------- 操作记录门面（UI 侧，显式带格） ----------

/// 记一条已结束的操作：内部 update 实体并 notify，打开的模态与面板传输
/// 区一起重画。
pub fn record_operation(pane: EntityId, task: TaskItem, cx: &mut App) {
    if let Some(entity) = history(pane) {
        entity.update(cx, |operations, cx| {
            operations.record(task);
            cx.notify();
        });
    }
}

/// 操作记录实体（读取方：历史模态、SFTP 面板的最近条目）；本地格为 None。
pub fn history(pane: EntityId) -> Option<Entity<SessionHistory>> {
    registry()
        .read()
        .unwrap()
        .get(&pane)
        .and_then(|entry| entry.history.clone())
}

/// 测试自造的格 id：**全进程唯一**的自增源（recorder 单测与 e2e 共用同一
/// 计数器——两套独立计数器会跨模块撞 id，注入 insert 静默覆盖别人的条目）。
#[cfg(test)]
pub fn test_pane() -> EntityId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    EntityId::from(NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    fn temp_log(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zhimaterm-recorder-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("session.log");
        std::fs::write(&path, b"").expect("建空日志文件");
        path
    }

    /// 开关的完整行为（ADR-017 口径）：已开启再开是幂等 no-op；暂停再开按
    /// 同一路径重开、各自落标记。预置 log_path 走 open_at 路径——不经
    /// output_log::create（它读全局配置目录 env，会与并行用例互扰），「新建
    /// 文件」分支由 output_log 的既有测试守护。
    #[gpui::test]
    fn logging_toggle_round_trips_one_file(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::theme::init(None, cx));
        let log_file = temp_log("toggle");
        let pane = test_pane();

        cx.update(|cx| {
            let history = cx.new(|_| SessionHistory::new());
            attach_log(pane, "deploy@web-1:22");
            attach_history(pane, history);
            registry()
                .write()
                .unwrap()
                .get_mut(&pane)
                .expect("刚注册的格")
                .log_path = Some(log_file.clone());

            assert!(!logging_enabled(pane), "注册后未开启");
            assert!(set_logging(pane, true), "按既有路径开启");
            assert!(logging_enabled(pane));
            assert!(set_logging(pane, true), "已开启再开是幂等成功");
            assert!(set_logging(pane, false), "暂停");
            assert!(!logging_enabled(pane));
            assert!(set_logging(pane, true), "恢复：按同一路径重开");
        });

        let content = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(
            content.matches("==== 记录开启").count(),
            2,
            "两次开启各落一次标记：{content}"
        );
        assert_eq!(
            content.matches("==== 记录暂停").count(),
            1,
            "暂停落一次标记：{content}"
        );
        let _ = std::fs::remove_dir_all(log_file.parent().unwrap());
    }

    /// worker 侧无参埋点走线程绑定：绑定后 begin/output/finish 生效并落盘；
    /// 未绑定的线程（或未注册的格）全部 no-op。测试线程即模拟会话线程。
    #[gpui::test]
    fn worker_bare_calls_route_via_thread_binding(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::theme::init(None, cx));
        let log_file = temp_log("binding");
        let pane = test_pane();
        cx.update(|cx| {
            let history = cx.new(|_| SessionHistory::new());
            attach_log(pane, "deploy@web-1:22");
            attach_history(pane, history);
            registry()
                .write()
                .unwrap()
                .get_mut(&pane)
                .expect("刚注册的格")
                .log_path = Some(log_file.clone());
            assert!(set_logging(pane, true));
        });

        // 模拟会话线程入口：绑定后任意深度的埋点无参可用
        bind_pane(pane);
        assert!(is_logging());
        begin_session("deploy@web-1:22");
        output_line("probe 中文");
        assert_eq!(finish_session(), None, "写成功不应报失败");

        let content = std::fs::read_to_string(&log_file).unwrap();
        assert!(
            content.contains("==== 会话开始 ") && content.contains("probe 中文"),
            "线程绑定的写点要落盘：{content}"
        );
        assert!(
            content.contains("==== 会话结束 "),
            "收尾出口要写右边界：{content}"
        );

        // 注销后：查询为否、开关失败、操作记录 no-op——生命周期边界的旁路语义
        unregister_pane(pane);
        cx.update(|cx| {
            assert!(!logging_enabled(pane));
            assert!(!set_logging(pane, true), "格已注销，开关失败");
            assert!(log_path(pane).is_none());
            assert!(history(pane).is_none());
            record_operation(pane, sample_task(), cx);
        });
        let _ = std::fs::remove_dir_all(log_file.parent().unwrap());
    }

    fn sample_task() -> TaskItem {
        TaskItem {
            id: 0,
            label: "上传 a → b".into(),
            unit: crate::views::history::ProgressUnit::Bytes,
            done: 1,
            total: Some(1),
            current: None,
            state: crate::views::history::TaskState::Completed("完成".into()),
            finished_at: None,
        }
    }
}
