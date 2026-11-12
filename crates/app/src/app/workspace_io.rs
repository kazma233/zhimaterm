//! 工作区持久化：快照、恢复、防抖落盘与孤儿回滚文件清理。

use std::collections::HashMap;
use std::time::Duration;

use gpui::{App, Context, Window};

use crate::views::panes::PaneTree;
use crate::workspace;
use zhimaterm_session::{LocalShell, SessionTarget, session_number_of};

use super::{MAX_SESSIONS, Tab, ZhimaTermApp};

impl ZhimaTermApp {
  /// 恢复工作区布局（ADR-037）：逐标签重建分屏树并为每个本地格启动**新**会话
  /// （不恢复旧进程、历史或状态；SSH 格不存在于保存文件里）。
  /// 返回是否存在可恢复的标签。
  pub(super) fn restore_workspace(
    &mut self,
    saved: &workspace::Workspace,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> bool {
    let mut restored = 0;
    // 持久化编号接着上次的计数走，新格不会与恢复的格重号
    self.created = saved.created;
    for tab in &saved.tabs {
      if self.tabs.len() >= MAX_SESSIONS {
        break;
      }
      let mut next_pane = 1;
      let spec = tab.root.to_layout(&mut next_pane);
      let Some(tree) = PaneTree::from_spec(&spec) else {
        continue;
      };
      let leaves = tab.root.leaves();
      let panes = tree.panes();
      let mut views = HashMap::new();
      for (leaf, pane) in leaves.into_iter().zip(panes) {
        let number = restored_number(leaf.leaf_number(), self.created);
        self.created = self.created.max(number);
        let target = SessionTarget::Local {
          shell: restored_shell(
            leaf,
            self
              .settings
              .settings()
              .and_then(|store| store.local_shell_or_default()),
            self
              .settings
              .settings()
              .and_then(|store| store.local_workdir_or_default()),
          ),
        };
        let view = self.new_view(number, target, cx);
        // worker 上报真实目录之前，先把保存的值记在视图上：恢复过程中
        // 的第一次工作区落盘不能把 cwd 抹成 null（ADR-039）
        view.update(cx, |view, _| {
          view.set_remembered_cwd(leaf.leaf_cwd().map(std::path::PathBuf::from));
        });
        views.insert(pane, view);
      }
      self.tabs.push(Tab {
        tree,
        views,
        name: tab.name.clone(),
      });
      restored += 1;
    }
    if restored == 0 {
      return false;
    }
    let active = saved.active_tab.min(restored - 1);
    self.activate(active, window, cx);
    true
  }

  // 当前布局与窗口几何的快照（ADR-037）：SSH 格映射为 None，被
  /// `SavedNode::from_layout` 裁掉。
  fn workspace_snapshot(&self, cx: &App) -> workspace::Workspace {
    let tabs = self
      .tabs
      .iter()
      .filter_map(|tab| {
        let spec = tab.tree.structure()?;
        let root = workspace::SavedNode::from_layout(&spec, &mut |pane| {
          tab.views.get(&pane).and_then(|view| view.read(cx).workspace_leaf())
        })?;
        Some(workspace::SavedTab {
          root,
          name: tab.name.clone(),
        })
      })
      .collect();
    workspace::Workspace {
      version: workspace::WORKSPACE_VERSION,
      window: self.last_window,
      tabs,
      active_tab: self.active,
      created: self.created,
    }
  }

  /// 布局变更收口（ADR-037）：重画并在本轮更新末合并写一次工作区。
  /// 改标签列表、分屏树、活动标签、编号、标签名字的变更方法收尾都走这里——
  /// 连环变更（批量关格、恢复逐标签重建）合并成一次写，新的变更方法只有这一个名字可调。
  /// 侧栏右键菜单按标签下标记目标，列表一变它就可能指错行，一并收起。
  pub(super) fn layout_changed(&mut self, cx: &mut Context<Self>) {
    self.sidebar_menu = None;
    cx.notify();
    if self.layout_save_scheduled {
      return;
    }
    self.layout_save_scheduled = true;
    let app = cx.entity().downgrade();
    cx.defer(move |cx| {
      let _ = app.update(cx, |app, cx| {
        app.layout_save_scheduled = false;
        app.persist_workspace(cx);
      });
    });
  }

  /// 写入器：写失败在侧栏亮错误，不打断操作。只被 `layout_changed` 的
  /// 合并写、窗口几何防抖、退出兜底三处调用——布局变更一律走
  /// [`ZhimaTermApp::layout_changed`]，不直接调这里。
  pub(super) fn persist_workspace(&mut self, cx: &App) {
    match self.workspace_snapshot(cx).save() {
      Ok(()) => self.workspace_error = None,
      Err(error) => self.workspace_error = Some(format!("保存工作区失败：{error:#}")),
    }
  }

  // 清掉没有被任何格引用的回滚文件（ADR-039）：关掉的格不再占磁盘。
  pub(super) fn prune_scrollback_files(&self, cx: &App) {
    let Ok(dir) = zhimaterm_base::config_paths::scrollback_dir() else {
      return;
    };
    let referenced: std::collections::HashSet<usize> = self.views().map(|view| view.read(cx).number()).collect();
    let Ok(entries) = std::fs::read_dir(&dir) else {
      return;
    };
    // 命名解析归回滚持久化自己（session_number_of）：清理方不知道
    // 文件与临时文件的起名规则
    for entry in entries.flatten() {
      let name = entry.file_name();
      let Some(name) = name.to_str() else {
        continue;
      };
      let Some(number) = session_number_of(name) else {
        continue;
      };
      if !referenced.contains(&number) {
        let _ = std::fs::remove_file(entry.path());
      }
    }
  }

  /// 窗口几何防抖写（500ms）：拖拽/缩放期间只记录代际，停下来后写最后一次。
  pub(super) fn schedule_workspace_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.bounds_save_generation += 1;
    if self.bounds_save_scheduled {
      return;
    }
    self.bounds_save_scheduled = true;
    let mut observed = self.bounds_save_generation;
    cx.spawn_in(window, async move |app, cx| {
      loop {
        cx.background_executor().timer(Duration::from_millis(500)).await;
        let outcome = app.update_in(cx, |app, _, cx| {
          let changed = app.bounds_save_generation != observed;
          if !changed {
            app.bounds_save_scheduled = false;
            app.persist_workspace(cx);
          }
          (changed, app.bounds_save_generation)
        });
        let Ok((changed, generation)) = outcome else {
          return;
        };
        if !changed {
          return;
        }
        observed = generation;
      }
    })
    .detach();
  }
}

/// 按树序收集保存节点里的叶（与 `SavedNode::to_layout` 的格编号顺序一致）。
/// 恢复叶的编号分配（ADR-037）：v2 文件直取保存值；v1 文件缺编号（0）时按
/// 当前计数分配（回滚文件以编号为键，不能重号）。调用方据此把计数推进到 max。
fn restored_number(saved_number: usize, created: usize) -> usize {
  match saved_number {
    0 => created + 1,
    saved => saved,
  }
}
/// 恢复的格用什么 Shell：WSL 格按原发行版恢复（WSL 只在 Windows 编译，
/// 其余平台回落普通本地 Shell）；普通格的 cwd 优先上次值（ADR-039，目录
/// 已删除时由 PTY 启动失败显式报错，不静默换），否则用设置的默认目录；
/// 程序用设置的默认 Shell（ADR-036），没有则平台默认。普通参数不碰实体，
/// 决策链可无头测试——它是 session × settings × workspace 三域的桥接决策，
/// 归壳层（挪进 workspace 会给它加上层依赖）。
fn restored_shell(node: &workspace::SavedNode, shell_pref: Option<&str>, workdir_pref: Option<&str>) -> LocalShell {
  if let workspace::SavedNode::Wsl { distro, .. } = node {
    #[cfg(windows)]
    {
      return LocalShell::Wsl { distro: distro.clone() };
    }
    #[cfg(not(windows))]
    {
      let _ = distro;
    }
  }
  let cwd = node
    .leaf_cwd()
    .map(std::path::PathBuf::from)
    .or_else(|| workdir_pref.map(std::path::PathBuf::from));
  match shell_pref.map(std::path::PathBuf::from) {
    Some(program) => LocalShell::Program { program, cwd },
    None => LocalShell::Default { cwd },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// v1 文件没有叶编号（serde 默认 0）：按当前计数分配新编号。
  #[test]
  fn v1_files_without_numbers_allocate_from_the_counter() {
    assert_eq!(restored_number(0, 7), 8);
  }

  /// v2 文件的编号原样恢复：回滚文件以编号为键，重启后不能换号。
  #[test]
  fn v2_numbers_restore_directly() {
    assert_eq!(restored_number(12, 12), 12);
  }

  // 恢复的格用什么 Shell（ADR-036/039）：cwd 三级链（上次值 → 设置默认 →
  /// None）、程序用设置默认。WSL 分支在 Windows 上编译，非 Windows 平台
  /// 走回落路径（Windows 实机 unverified）。
  #[test]
  fn restored_shell_prefers_last_cwd_then_settings() {
    use crate::workspace::SavedNode;
    let plain = |cwd: Option<&str>| SavedNode::Local {
      number: 3,
      cwd: cwd.map(str::to_owned),
    };
    // 没有任何偏好：平台默认 Shell、无目录
    assert!(matches!(
      restored_shell(&plain(None), None, None),
      LocalShell::Default { cwd: None }
    ));
    // 上次目录优先于设置默认（目录被删由 PTY 启动显式报错，不静默换）
    assert!(matches!(
        restored_shell(&plain(Some("/last/dir")), None, Some("/pref/dir")),
        LocalShell::Default { ref cwd }
            if cwd.as_deref() == Some(std::path::Path::new("/last/dir"))
    ));
    // 没有上次目录才用设置默认
    assert!(matches!(
        restored_shell(&plain(None), None, Some("/pref/dir")),
        LocalShell::Default { ref cwd }
            if cwd.as_deref() == Some(std::path::Path::new("/pref/dir"))
    ));
    // 设置指定了 Shell：Program 形态
    assert!(matches!(
        restored_shell(&plain(None), Some("/bin/zsh"), None),
        LocalShell::Program { ref program, cwd: None }
            if program == &std::path::PathBuf::from("/bin/zsh")
    ));
  }

  /// WSL 格的恢复决策：Windows 按原发行版，其余平台回落普通本地 Shell
  /// （SavedNode::Wsl 的注释契约）。非 Windows 侧在此验收。
  #[test]
  fn restored_shell_maps_wsl_nodes_per_platform() {
    use crate::workspace::SavedNode;
    let wsl = SavedNode::Wsl {
      distro: "Ubuntu".to_owned(),
      number: 5,
    };
    #[cfg(windows)]
    assert!(matches!(
        restored_shell(&wsl, Some("/bin/zsh"), Some("/pref/dir")),
        LocalShell::Wsl { ref distro } if distro == "Ubuntu"
    ));
    #[cfg(not(windows))]
    assert!(
      matches!(restored_shell(&wsl, None, None), LocalShell::Default { cwd: None }),
      "非 Windows 回落普通本地 Shell，忽略 WSL 记录"
    );
  }
}
