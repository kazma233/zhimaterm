//! 本地探针（ADR-028）：本地 Shell 会话的进程树与监听端口采样。
//!
//! 三平台三实现（`target_os` 级门控，见 `linux`/`macos`/`windows`）：平台层交
//! 「全量进程表 + LISTEN socket 表 + 结束进程」，树构建、树内过滤与端口合并在
//! 这一处装配——口径只在一处维护。WSL 会话不在覆盖范围（V1 决策，见 ADR-028）。
//!
//! 生命周期是「开即采、关即停」：采样线程由 [`ProbeHandle::start`] 按运行段创建、
//! 收起即退出，空闲本地格不挂常驻轮询源（worker 的零唤醒原则不被破坏）。失败语义
//! 与远端监控同款：连续多次采集失败才判终态，单次失败重试，终端会话不受影响。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// 采样周期：与远端监控通道同节奏。快于此的进程闪现本来就观察不到，
/// 慢于此列表会发钝。
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// 睡眠分片：收起面板后采样线程在一个分片内退出，不等到下一个整周期。
const PROBE_SLEEP_SLICE: Duration = Duration::from_millis(100);
/// 连续采集失败到这个次数才判终态：lsof 这类外部命令允许偶发失败。
const PROBE_FAILURE_LIMIT: u32 = 3;

/// 一次采样结果。进程以树形交付（UI 按缩进渲染），端口按「端口 + 进程」合并。
pub struct ProbeSample {
    /// shell 根（含全部存活后代）；根已回收时整次采样报错而非给空树
    pub root: ProcessNode,
    /// LISTEN 端口（TCP），按端口号升序
    pub ports: Vec<ProbePort>,
    /// 端口区不可用的原因（macOS 缺 lsof 之类）：进程区照常展示，端口区降级
    pub ports_unavailable: Option<String>,
}

/// 树上的一个进程。
pub struct ProcessNode {
    pub process: ProbeProcess,
    pub children: Vec<ProcessNode>,
}

impl ProcessNode {
    /// 先根序平铺自身与全部后代，携带谱系深度（根为 0）。树构建保证子节点按
    /// pid 升序，所以这个序列同时是 UI 的稳定行序；汇总统计与树内 pid 收集
    /// 也共用这一个遍历出口。
    pub fn walk(&self) -> Vec<(&ProcessNode, usize)> {
        fn visit<'a>(node: &'a ProcessNode, depth: usize, out: &mut Vec<(&'a ProcessNode, usize)>) {
            out.push((node, depth));
            for child in &node.children {
                visit(child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        visit(self, 0, &mut out);
        out
    }

    /// 按 pid 单点查找。详情弹层每帧取数、退场检测都只要一个节点，走 walk
    /// 会为此平铺整棵树。
    pub fn find(&self, pid: u32) -> Option<&ProcessNode> {
        if self.process.pid == pid {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(pid))
    }
}

/// 进程条目：字段口径跨平台统一，内存是驻留内存（RSS / WorkingSet / resident_size）。
#[derive(Clone, Debug)]
pub struct ProbeProcess {
    pub pid: u32,
    pub ppid: u32,
    /// 短名（comm / exe 名 / 路径末段；解释器脚本见 [`display_name`]）
    pub name: String,
    /// 完整命令行，尽力而为：平台取不到时为 None
    pub command: Option<String>,
    /// 完整可执行路径，尽力而为：详情视图用，命令行缺失时的兜底事实
    pub exe_path: Option<String>,
    /// 驻留内存，字节；取不到时为 None
    pub rss_bytes: Option<u64>,
}

/// 一条 LISTEN socket 的原始采集记录，合并前形态。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListeningSocket {
    pub addr: std::net::IpAddr,
    pub port: u16,
    pub pid: u32,
}

/// 常见解释器的 argv[0] 短名：这些进程在内核里的可执行文件是解释器（跑 `pi`
/// 这类 shebang 脚本时进程名是 node），用户启动的对象是脚本，展示名要从 argv
/// 恢复。Windows 的 argv[0] 带可执行后缀（node.exe），匹配前剥掉。
const INTERPRETER_ARGV0: &[&str] = &[
    "node",
    "deno",
    "bun",
    "python",
    "python3",
    "python2",
    "ruby",
    "perl",
    "php",
    "bash",
    "sh",
    "zsh",
    "fish",
    "pwsh",
    "osascript",
];

/// 进程展示名：argv[0] 是常见解释器且 argv[1] 是脚本时取脚本名末段，否则沿用
/// 平台内核名（comm / exe 末段）。argv[1] 以 `-` 开头是解释器的选项（`java -jar`、
/// `zsh -c`），不是脚本，回退内核名。
fn display_name(exec_name: &str, argv: &[String]) -> String {
    fn basename(path: &str) -> &str {
        // Windows 进程的 argv 可能两种分隔符混用（pwsh 也会收 POSIX 风格参数）
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }
    let interpreted = argv.first().is_some_and(|argv0| {
        let base = basename(argv0);
        let stem = base.strip_suffix(".exe").unwrap_or(base);
        INTERPRETER_ARGV0.contains(&stem)
    });
    match argv
        .get(1)
        .filter(|script| interpreted && !script.is_empty() && !script.starts_with('-'))
    {
        Some(script) => basename(script).to_owned(),
        None => exec_name.to_owned(),
    }
}

/// 合并后的端口行：同一「端口 + 进程」的多个绑定地址收进一行（v4/v6 各绑一个、
/// SO_REUSEPORT 的多 socket 同端口不同进程仍是独立行）。
#[derive(Clone, Debug)]
pub struct ProbePort {
    pub port: u16,
    pub pid: u32,
    /// 去重后的绑定地址，按「v4 在前、同族按地址」排序
    pub addrs: Vec<std::net::IpAddr>,
}

/// 从全量进程表构建以 `root_pid` 为根的后代树。
///
/// 防环与断链：Windows 的 PID 重用可能让 ppid 链成环或指向已死进程，构建时只收
/// 「从根出发可达且未访问过」的节点，环与孤儿天然落在外面。重复 pid（采集竞态）
/// 只保留首个。子节点按 pid 升序，同一批进程在 2 秒刷新间行序稳定（ADR-028）。
fn build_tree(processes: Vec<ProbeProcess>, root_pid: u32) -> Option<ProcessNode> {
    use std::collections::{HashMap, HashSet};

    let mut by_pid: HashMap<u32, ProbeProcess> = HashMap::with_capacity(processes.len());
    for process in processes {
        by_pid.entry(process.pid).or_insert(process);
    }
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::with_capacity(by_pid.len());
    for process in by_pid.values() {
        children_of
            .entry(process.ppid)
            .or_default()
            .push(process.pid);
    }
    for pids in children_of.values_mut() {
        pids.sort_unstable();
    }

    by_pid.get(&root_pid)?;
    let mut visited = HashSet::from([root_pid]);
    Some(build_subtree(root_pid, &children_of, &by_pid, &mut visited))
}

fn build_subtree(
    pid: u32,
    children_of: &std::collections::HashMap<u32, Vec<u32>>,
    by_pid: &std::collections::HashMap<u32, ProbeProcess>,
    visited: &mut std::collections::HashSet<u32>,
) -> ProcessNode {
    let mut children = Vec::new();
    if let Some(pids) = children_of.get(&pid) {
        for child in pids {
            // 环上的节点第二次到达时不再进入：保留首次挂载的位置
            if visited.insert(*child) && by_pid.contains_key(child) {
                children.push(build_subtree(*child, children_of, by_pid, visited));
            }
        }
    }
    ProcessNode {
        process: by_pid[&pid].clone(),
        children,
    }
}

/// 按端口 + 进程合并 LISTEN socket：地址去重排序，行按（端口，PID）升序。
fn merge_ports(mut sockets: Vec<ListeningSocket>) -> Vec<ProbePort> {
    sockets.sort_by_key(|socket| (socket.port, socket.pid));
    let mut ports: Vec<ProbePort> = Vec::new();
    for socket in sockets {
        match ports.last_mut() {
            Some(last) if last.port == socket.port && last.pid == socket.pid => {
                if !last.addrs.contains(&socket.addr) {
                    last.addrs.push(socket.addr);
                }
            }
            _ => ports.push(ProbePort {
                port: socket.port,
                pid: socket.pid,
                addrs: vec![socket.addr],
            }),
        }
    }
    ports
}

/// pump -> UI 的探针事件。通道按运行段创建：收起面板即关闭。
pub enum ProbeEvent {
    Sample(ProbeSample),
    Failed(String),
}

/// UI 侧事件流读端（与 MonitorEvents 同构）。
pub struct ProbeEvents(smol::channel::Receiver<ProbeEvent>);

impl ProbeEvents {
    pub async fn recv(&self) -> Option<ProbeEvent> {
        self.0.recv().await.ok()
    }
}

/// 探针句柄：面板展开时 `start` 起采样线程，收起时 `stop`。多次 start 幂等
/// （先停旧段再起新段），Drop 时停掉在跑的段——会话关闭后线程不应残留。
pub struct ProbeHandle {
    root_pid: u32,
    /// 当前运行段的继续旗标：置 false 请求线程退出；None = 没有线程在跑
    running: Mutex<Option<Arc<AtomicBool>>>,
}

impl ProbeHandle {
    pub(crate) fn new(root_pid: u32) -> Self {
        Self {
            root_pid,
            running: Mutex::new(None),
        }
    }

    /// 起一个采样运行段，返回这段的事件流。旧段（若有）就地停止。
    pub fn start(&self) -> ProbeEvents {
        self.stop();
        let running = Arc::new(AtomicBool::new(true));
        *self.running.lock().unwrap() = Some(running.clone());
        let (sender, receiver) = smol::channel::bounded::<ProbeEvent>(2);
        let root_pid = self.root_pid;
        thread::Builder::new()
            .name("zhimaterm-probe".into())
            .spawn(move || run_segment(root_pid, running, sender))
            .expect("创建本地探针线程失败");
        ProbeEvents(receiver)
    }

    /// 停当前运行段（若有）：线程在一个睡眠分片内自行退出。
    pub fn stop(&self) {
        if let Some(running) = self.running.lock().unwrap().take() {
            running.store(false, Ordering::Relaxed);
        }
    }

    /// 结束一个树内进程。根 shell 拒绝：杀根等于杀会话本体，探针面板随之
    /// 报终态。`expect_exe` 是采样期看到的可执行路径：发信号前一刻按平台
    /// 现行值复核身份，不一致即 PID 已被复用，拒杀（见各平台 `kill_process`）。
    pub fn kill(&self, pid: u32, expect_exe: Option<&str>) -> Result<()> {
        if pid == self.root_pid {
            anyhow::bail!("不能结束会话 shell 自身");
        }
        platform::kill_process(pid, expect_exe)
    }
}

/// Unix 发 SIGTERM：允许目标自行收尾，被忽略时由下一拍采样暴露「仍在」，
/// 用户可重发。ESRCH/EPERM 翻译成可读原因，其余错误原样透出。
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn sigterm(pid: u32) -> Result<()> {
    // SAFETY: kill 只收标量参数，返回 -1 即失败由 errno 区分。
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => anyhow::bail!("进程 {pid} 已退出"),
            Some(libc::EPERM) => anyhow::bail!("无权限结束进程 {pid}"),
            _ => return Err(error.into()),
        }
    }
    Ok(())
}

impl Drop for ProbeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 一次采样的装配：平台层交「全量进程表 + 全系统 LISTEN 表」，树构建、按树内
/// 进程过滤与端口合并在这一处完成——三平台的端口归属口径单点维护（ADR-028）。
fn collect_sample(root_pid: u32) -> Result<ProbeSample> {
    let processes = platform::processes()?;
    let root = build_tree(processes, root_pid)
        .ok_or_else(|| anyhow::anyhow!("Shell 进程 {root_pid} 已不存在"))?;
    let pids: HashSet<u32> = root
        .walk()
        .iter()
        .map(|(node, _)| node.process.pid)
        .collect();
    let (sockets, ports_unavailable) = platform::listening_sockets(&pids)?;
    let sockets = sockets
        .into_iter()
        .filter(|socket| pids.contains(&socket.pid))
        .collect();
    Ok(ProbeSample {
        root,
        ports: merge_ports(sockets),
        ports_unavailable,
    })
}

/// 一个采样运行段：周期采集直到运行旗标落下或通道关闭。
fn run_segment(root_pid: u32, running: Arc<AtomicBool>, sender: smol::channel::Sender<ProbeEvent>) {
    let mut failures = 0;
    loop {
        match collect_sample(root_pid) {
            Ok(sample) => {
                failures = 0;
                if sender.send_blocking(ProbeEvent::Sample(sample)).is_err() {
                    return;
                }
            }
            Err(error) => {
                failures += 1;
                if failures >= PROBE_FAILURE_LIMIT {
                    let _ = sender.send_blocking(ProbeEvent::Failed(format!("{error:#}")));
                    return;
                }
            }
        }
        let slices = PROBE_INTERVAL.as_millis() / PROBE_SLEEP_SLICE.as_millis();
        for _ in 0..slices {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(PROBE_SLEEP_SLICE);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::thread;
    use std::time::Duration;

    use super::ListeningSocket;
    use super::{ProbeHandle, ProbeProcess, build_tree, merge_ports};

    fn proc(pid: u32, ppid: u32) -> ProbeProcess {
        ProbeProcess {
            pid,
            ppid,
            name: format!("p{pid}"),
            command: None,
            exe_path: None,
            rss_bytes: Some(1024),
        }
    }

    fn walk_pids(root: &super::ProcessNode) -> Vec<u32> {
        root.walk()
            .iter()
            .map(|(node, _)| node.process.pid)
            .collect()
    }

    /// 基本树形：shell → vim、shell → make → 两 个 gcc；深度与顺序按 pid 升序。
    #[test]
    fn tree_contains_all_descendants_sorted_by_pid() {
        let procs = vec![
            proc(1, 0),
            proc(10, 1),
            proc(30, 20),
            proc(20, 1),
            proc(21, 1),
        ];
        let root = build_tree(procs, 1).expect("根存在");
        assert_eq!(root.process.pid, 1);
        let child_pids: Vec<u32> = root.children.iter().map(|c| c.process.pid).collect();
        assert_eq!(child_pids, vec![10, 20, 21], "子节点按 pid 升序");
        let make = &root.children[1];
        assert_eq!(make.process.pid, 20);
        assert_eq!(make.children[0].process.pid, 30);
        assert_eq!(walk_pids(&root), vec![1, 10, 20, 30, 21]);
        let walked = root.walk();
        assert_eq!(walked[3].0.process.pid, 30);
        assert_eq!(walked[3].1, 2, "walk 携带谱系深度（根为 0）");
    }

    /// ppid 自环（PID 重用可能造成）：环上节点不从根可达，丢弃且不死循环。
    #[test]
    fn ppid_cycle_does_not_hang_or_duplicate() {
        let procs = vec![proc(1, 0), proc(2, 1), proc(3, 3)];
        let root = build_tree(procs, 1).expect("根存在");
        assert_eq!(walk_pids(&root), vec![1, 2], "自环节点不从根可达，丢弃");
    }

    /// 根不在表里（shell 已回收的收尾窗口）：返回 None，由调用方判终态。
    #[test]
    fn missing_root_yields_none() {
        assert!(build_tree(vec![proc(2, 1)], 1).is_none());
    }

    /// 同端口多地址（v4 + v6）合并成一行；不同 pid 分行；行序按（端口, pid）。
    #[test]
    fn ports_merge_by_port_and_pid() {
        let sockets = vec![
            ListeningSocket {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 8080,
                pid: 50,
            },
            ListeningSocket {
                addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
                port: 8080,
                pid: 50,
            },
            ListeningSocket {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 8080,
                pid: 51,
            },
            ListeningSocket {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 3000,
                pid: 52,
            },
            // 重复地址去重
            ListeningSocket {
                addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
                port: 8080,
                pid: 50,
            },
        ];
        let ports = merge_ports(sockets);
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].port, 3000);
        assert_eq!(ports[0].pid, 52);
        assert_eq!(ports[1].port, 8080);
        assert_eq!(ports[1].pid, 50);
        assert_eq!(ports[1].addrs.len(), 2, "v4/v6 合并去重");
        assert_eq!(ports[2].pid, 51);
    }

    /// 结束进程的实机端到端（三平台）：spawn 一个长驻子进程后——
    /// 1. 身份复核不过（路径对不上，疑似 PID 复用）拒杀，子进程仍活着；
    /// 2. 放行的 kill 让子进程在超时内退出；
    /// 3. 已退出的进程再杀报「已退出」；
    /// 4. 杀根 shell（这里是测试进程自身）被拒绝。
    #[test]
    fn kill_checks_identity_terminates_and_rejects_root() {
        #[cfg(windows)]
        let mut command = {
            let mut command = std::process::Command::new("ping");
            command.args(["-n", "31", "127.0.0.1"]);
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：测试子进程不闪控制台窗
            command.creation_flags(0x0800_0000);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            command
        };
        let mut child = command.spawn().expect("长驻子进程应能启动");

        let handle = ProbeHandle::new(std::process::id());

        let error = handle
            .kill(child.id(), Some("/nonexistent/path"))
            .expect_err("身份复核不过应拒杀");
        assert!(error.to_string().contains("复用"), "{error}");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "拒杀后子进程应仍活着"
        );

        handle.kill(child.id(), None).expect("放行的 kill 应成功");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while child.try_wait().expect("try_wait").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "子进程应在 5 秒内退出"
            );
            thread::sleep(Duration::from_millis(50));
        }

        let error = handle
            .kill(child.id(), None)
            .expect_err("已退出的进程再杀应报错");
        assert!(error.to_string().contains("已退出"), "{error}");

        let error = handle
            .kill(std::process::id(), None)
            .expect_err("杀根 shell 应被拒绝");
        assert!(error.to_string().contains("shell"), "{error}");
    }

    /// 展示名：解释器跑脚本时取脚本名（用户启动的对象），选项形态与非解释器
    /// 沿用内核名；Windows 的完整路径、`.exe` 后缀与反斜杠同样识别。
    #[test]
    fn display_name_prefers_script_over_interpreter() {
        use super::display_name;
        let argv: &[&str] = &["node", "/usr/local/bin/pi", "--serve"];
        let owned: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        assert_eq!(
            display_name("node", &owned),
            "pi",
            "跑 pi 的 node 进程显示 pi"
        );

        let argv: Vec<String> = ["/opt/nvm/node", "server.js"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(
            display_name("node", &argv),
            "server.js",
            "argv[0] 是完整路径同样识别"
        );

        // argv[1] 是解释器选项：没有脚本信息量，回退内核名
        let argv: Vec<String> = ["java", "-jar", "app.jar"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(display_name("java", &argv), "java");
        let argv: Vec<String> = ["zsh", "-c", "pi serve"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(display_name("zsh", &argv), "zsh", "选项后的文本不是脚本");

        let argv: Vec<String> = ["/usr/local/bin/pi"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(display_name("pi", &argv), "pi", "非解释器沿用内核名");
        assert_eq!(
            display_name("kworker", &[]),
            "kworker",
            "空 argv 沿用内核名"
        );

        // Windows 形态：argv[0] 是带 .exe 的完整路径、脚本路径用反斜杠
        let argv: Vec<String> = [r"C:\Program Files\nodejs\node.exe", r"D:\tools\pi.js"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(display_name("node.exe", &argv), "pi.js");
    }

    /// 运行段启停：start 后一个周期内样本到达；stop 后线程在一个睡眠分片内
    /// 退出并关闭通道——不再有样本，recv 返回 None 而非等到超时。
    /// 根用测试进程自身：树只含自己，三平台都采得到。
    #[test]
    fn probe_handle_delivers_sample_then_stops() {
        let handle = super::ProbeHandle::new(std::process::id());
        let events = handle.start();
        let first = smol::block_on(smol::future::or(async { events.recv().await }, async {
            smol::Timer::after(Duration::from_secs(5)).await;
            None
        }));
        assert!(
            matches!(first, Some(super::ProbeEvent::Sample(_))),
            "首拍应在 5 秒内到达"
        );
        handle.stop();
        let stopped_at = std::time::Instant::now();
        let after_stop = smol::block_on(smol::future::or(async { events.recv().await }, async {
            smol::Timer::after(Duration::from_secs(3)).await;
            None
        }));
        assert!(after_stop.is_none(), "停止后不应再有样本");
        assert!(
            stopped_at.elapsed() < Duration::from_secs(2),
            "线程应在一个睡眠分片内退出并关闭通道，实际耗时 {:?}",
            stopped_at.elapsed()
        );
    }

    /// 端口归属的实机验证（ADR-028 unverified 项）：本进程监听临时端口后采样，
    /// 端口应归到自己名下——GetExtendedTcpTable 的参数序传反只会查空表、
    /// 端口/地址解码错了数字对不上，两种错在这里都会露馅。
    #[cfg(target_os = "windows")]
    #[test]
    fn own_listening_sockets_appear_in_the_sample() {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定 v4 临时端口");
        let v6 = std::net::TcpListener::bind("[::1]:0").expect("绑定 v6 临时端口");
        let sample = super::collect_sample(std::process::id()).expect("采样自身");
        for listener in [&v4, &v6] {
            let addr = listener.local_addr().expect("本地地址");
            let found = sample
                .ports
                .iter()
                .any(|port| port.pid == std::process::id() && port.port == addr.port());
            assert!(found, "采样应含自己的 LISTEN 端口 {addr}");
        }
    }
}
