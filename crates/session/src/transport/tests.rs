//! 传输层跨模块用例：唤醒管道与 poll 的配合。
//!
//! `wake` 是叶子模块、`poll` 保持自包含（探针按 `#[path]` 复用），两者的
//! 集成只能在这里验证。

use std::time::Duration;

use super::poll::{Events, PollFd, poll_sockets};
use zhimaterm_base::wake::wakeup_pair;

/// poke 后读端立刻就绪；排空清掉待唤醒，写端仍活着。
#[test]
fn wakeup_poke_wakes_and_drain_keeps_the_pair_alive() {
  let (handle, mut read) = wakeup_pair().unwrap();
  handle.poke();
  // 多次 poke 合并成一次待唤醒
  handle.poke();

  let mut fds = [PollFd::new(read.raw_sock(), Events::READ)];
  let ready = poll_sockets(&mut fds, Some(Duration::from_millis(100))).unwrap();
  assert!(ready >= 1);
  assert!(fds[0].ready());
  assert!(!fds[0].disconnected());
  assert!(read.drain());

  // 排空后没有遗留事件：短暂等待不应再就绪
  let mut fds = [PollFd::new(read.raw_sock(), Events::READ)];
  let ready = poll_sockets(&mut fds, Some(Duration::from_millis(20))).unwrap();
  assert_eq!(ready, 0);
  assert!(!fds[0].ready());
}
