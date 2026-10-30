//! 工作区持久化（ADR-037）：窗口几何与标签/分屏布局。
//!
//! 只记本地格（LOCAL 与 WSL）：SSH 格不持久化，重启后必须由用户显式重新
//! 连接（requirements §7）。混合标签在保存时按格裁剪——SSH 格被剔除，兄弟
//! 子树顶位，比例随之退化为单格；全 SSH 的标签整个消失。
//!
//! 与 `settings.json` 分开存：布局是高频自动写入的会话态，不是用户显式
//! 偏好；窗口几何变化防抖写，布局变化即时写。
//!
//! 下面的 `Saved*` 类型就是磁盘 schema：改字段前先对照 `WORKSPACE_VERSION`
//! 的兼容策略（新增字段配 `#[serde(default)]`，不兼容变更升版本回退空布局）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use gpui::{Bounds, Pixels, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

use crate::views::panes::{LayoutSpec, PaneId, SplitAxis};

/// 当前文件格式版本。读到更高版本视为不兼容：不猜测字段语义，回退空布局。
/// v2 起本地格带会话编号与最后工作目录（ADR-039，回滚恢复按编号落文件）。
pub const WORKSPACE_VERSION: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub version: u32,
    pub window: Option<SavedWindow>,
    pub tabs: Vec<SavedTab>,
    pub active_tab: usize,
    /// 累计会话计数：重启后继续编号，新格不会与恢复的格重号
    pub created: usize,
    /// 侧栏显示编号的累计计数：最后一个标签关闭时归零，只决定 `会话 N` 文案；
    /// 回滚文件与工作区叶仍用 `created`/叶编号，不复用旧键
    pub label_created: usize,
}

/// 窗口几何：保存侧只写窗口态的坐标（最大化/全屏在保存前已压平，见 `WindowState`）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub state: WindowState,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// 窗口状态的持久化口径：新写入的文件只含 `Windowed`，另两个变体只为读旧文件。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    Windowed,
    /// 仅为读取旧文件保留（2026-10-22 起保存侧不再产出）：恢复时按窗口态处理
    Maximized,
    /// 仅为读取旧文件保留：恢复时按窗口态处理
    Fullscreen,
}

/// 一个标签页的持久化形态：一棵以分屏节点为枝、本地格为叶的树。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub root: SavedNode,
    /// 用户在侧栏给标签起的名字（ADR-047）；None = 用自动标题。旧文件没有该字段
    #[serde(default)]
    pub name: Option<String>,
}

/// 保存的格：只有本地格有形态，SSH 格在保存前已被裁掉。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedNode {
    Local {
        /// 会话编号：回滚文件以它为键（ADR-039）；v1 文件没有，按 0 处理并重新分配
        #[serde(default)]
        number: usize,
        /// 侧栏显示编号（`会话 N`）；旧文件没有，恢复时回退到 `number`
        #[serde(default)]
        label_number: usize,
        /// 最后工作目录（ADR-039）：None = 沿用设置里的默认值
        #[serde(default)]
        cwd: Option<String>,
    },
    /// WSL 发行版。变体在所有平台都保留：跨平台读取时不因未知变体整份失败，
    /// 非 Windows 上恢复为默认本地 Shell。WSL 的工作目录在子系统里，
    /// 宿主侧采集不到，不持久化。
    Wsl {
        distro: String,
        #[serde(default)]
        number: usize,
        #[serde(default)]
        label_number: usize,
    },
    Split {
        axis: SavedAxis,
        ratio: f32,
        first: Box<SavedNode>,
        second: Box<SavedNode>,
    },
}

/// 分屏方向的持久化口径，运行态是 `SplitAxis`（经 From 互转）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedAxis {
    SideBySide,
    Stacked,
}

impl From<SplitAxis> for SavedAxis {
    fn from(axis: SplitAxis) -> Self {
        match axis {
            SplitAxis::SideBySide => Self::SideBySide,
            SplitAxis::Stacked => Self::Stacked,
        }
    }
}

impl From<SavedAxis> for SplitAxis {
    fn from(axis: SavedAxis) -> Self {
        match axis {
            SavedAxis::SideBySide => Self::SideBySide,
            SavedAxis::Stacked => Self::Stacked,
        }
    }
}

impl SavedNode {
    /// 从布局投影裁剪出持久化节点：`leaf` 返回 None 的格被剔除，兄弟子树
    /// 顶到父节点位置；整棵树都不剩返回 None（该标签不保存）。
    pub fn from_layout(
        spec: &LayoutSpec,
        leaf: &mut impl FnMut(PaneId) -> Option<SavedNode>,
    ) -> Option<Self> {
        match spec {
            LayoutSpec::Leaf(pane) => leaf(*pane),
            LayoutSpec::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = Self::from_layout(first, leaf);
                let second = Self::from_layout(second, leaf);
                match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis: (*axis).into(),
                        ratio: *ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    // 一侧被裁掉：另一侧顶位，比例失去意义
                    (Some(single), None) | (None, Some(single)) => Some(single),
                    (None, None) => None,
                }
            }
        }
    }

    /// 叶的最后工作目录（ADR-039）；WSL 不记录。
    pub fn leaf_cwd(&self) -> Option<&str> {
        match self {
            Self::Local { cwd, .. } => cwd.as_deref(),
            _ => None,
        }
    }

    pub fn leaf_number(&self) -> usize {
        match self {
            Self::Local { number, .. } | Self::Wsl { number, .. } => *number,
            Self::Split { .. } => 0,
        }
    }

    /// 侧栏显示编号；旧文件没有该字段时为 0，由恢复方回退到持久化编号。
    pub fn leaf_label_number(&self) -> usize {
        match self {
            Self::Local { label_number, .. } | Self::Wsl { label_number, .. } => *label_number,
            Self::Split { .. } => 0,
        }
    }

    /// 先序收集全部保存叶：恢复时与 `to_layout` 重建的树序一一配对。
    pub fn leaves(&self) -> Vec<&Self> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    fn collect_leaves<'a>(&'a self, leaves: &mut Vec<&'a Self>) {
        match self {
            Self::Split { first, second, .. } => {
                first.collect_leaves(leaves);
                second.collect_leaves(leaves);
            }
            leaf => leaves.push(leaf),
        }
    }

    /// 重建布局投影，按树序给格分配标识（从 1 起）。
    pub fn to_layout(&self, next_pane: &mut PaneId) -> LayoutSpec {
        match self {
            Self::Local { .. } | Self::Wsl { .. } => {
                let pane = *next_pane;
                *next_pane += 1;
                LayoutSpec::Leaf(pane)
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutSpec::Split {
                axis: (*axis).into(),
                ratio: *ratio,
                first: Box::new(first.to_layout(next_pane)),
                second: Box::new(second.to_layout(next_pane)),
            },
        }
    }
}

impl Workspace {
    /// 恢复窗口几何（2026-10-22 修订：不再恢复最大化/全屏——Aero Snap 拖顶边、
    /// 双击标题栏都容易无意识最大化，退出后固化进文件，重开表现为「随机全屏」。
    /// 一律以窗口态打开；保存侧同理只记窗口态）。窗口必须与至少一个显示器
    /// 相交，否则回退默认（外接屏拔掉后不能把窗口恢复到看不见的地方）。
    pub fn window_bounds(&self, displays: &[Bounds<Pixels>]) -> Option<WindowBounds> {
        let window = self.window.as_ref()?;
        let bounds = Bounds::new(
            point(px(window.x), px(window.y)),
            size(px(window.width), px(window.height)),
        );
        displays
            .iter()
            .any(|display| display.intersects(&bounds))
            .then_some(WindowBounds::Windowed(bounds))
    }

    /// 保存前把窗口状态压成文件里的形态。最大化/全屏一律记窗口态：bounds
    /// 用 gpui 给的还原矩形（最大化时 rcNormalPosition 仍保有还原尺寸），
    /// 重开大小不变、不会全屏。
    pub fn saved_window(bounds: WindowBounds) -> SavedWindow {
        let bounds = match bounds {
            WindowBounds::Windowed(bounds)
            | WindowBounds::Maximized(bounds)
            | WindowBounds::Fullscreen(bounds) => bounds,
        };
        SavedWindow {
            state: WindowState::Windowed,
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    /// 读取工作区。文件不存在返回 None（首次启动）；解析失败与版本过新
    /// 都报错，由调用方决定回退——不静默当作空文件，否则下一次保存会覆盖
    /// 用户手改的内容。
    pub fn load() -> Result<Option<Self>> {
        let path = crate::config_paths::config_dir()?.join("workspace.json");
        Self::load_at(path)
    }

    pub fn load_at(path: PathBuf) -> Result<Option<Self>> {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("读取工作区失败：{}", path.display()));
            }
        };
        let workspace: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("解析工作区失败：{}", path.display()))?;
        if workspace.version > WORKSPACE_VERSION {
            anyhow::bail!(
                "工作区文件版本 {} 高于当前支持的 {}：{}",
                workspace.version,
                WORKSPACE_VERSION,
                path.display()
            );
        }
        Ok(Some(workspace))
    }

    pub fn save(&self) -> Result<()> {
        let path = crate::config_paths::config_dir()?.join("workspace.json");
        self.save_at(path)
    }

    pub fn save_at(&self, path: PathBuf) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("序列化工作区失败")?;
        crate::config_paths::write_atomic(&path, &json)
            .with_context(|| format!("保存工作区失败：{}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::{SavedAxis, SavedNode, WORKSPACE_VERSION, WindowState, Workspace};
    use crate::views::panes::{LayoutSpec, SplitAxis};
    use gpui::{Bounds, WindowBounds, point, px, size};

    fn local(_pane: u64) -> Option<SavedNode> {
        Some(SavedNode::Local {
            number: 1,
            label_number: 1,
            cwd: None,
        })
    }

    fn leaf_specs(spec: &LayoutSpec) -> Vec<u64> {
        spec.panes()
    }

    /// 混合标签：SSH 格（映射为 None）被剔除，本地格顶位，比例消失。
    #[test]
    fn pruning_drops_ssh_cells_and_collapses_their_splits() {
        // 树：[本地1 | SSH2] 上叠 本地3
        let spec = LayoutSpec::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.4,
            first: Box::new(LayoutSpec::Leaf(1)),
            second: Box::new(LayoutSpec::Leaf(2)),
        };
        // 只有 1 是本地格：整棵树退化为单叶
        let pruned = SavedNode::from_layout(&spec, &mut |pane| {
            (pane == 1).then_some(SavedNode::Local {
                number: 1,
                label_number: 1,
                cwd: None,
            })
        })
        .expect("至少剩一个本地格");
        assert_eq!(
            pruned,
            SavedNode::Local {
                number: 1,
                label_number: 1,
                cwd: None
            }
        );

        // 两格都本地：结构保留，方向与比例不变
        let both = SavedNode::from_layout(&spec, &mut local).expect("两格都本地");
        assert!(matches!(
            both,
            SavedNode::Split {
                axis: SavedAxis::SideBySide,
                ratio,
                ..
            } if (ratio - 0.4).abs() < f32::EPSILON
        ));

        // 全 SSH：整个标签不保存
        assert!(SavedNode::from_layout(&spec, &mut |_| None).is_none());
    }

    /// 保存 → 读取 → 重建：格序与方向一致，标识重新从 1 分配。
    #[test]
    fn saved_tree_rebuilds_with_fresh_pane_ids() {
        let node = SavedNode::Split {
            axis: SavedAxis::Stacked,
            ratio: 0.25,
            first: Box::new(SavedNode::Local {
                number: 3,
                label_number: 3,
                cwd: Some("/tmp".into()),
            }),
            second: Box::new(SavedNode::Wsl {
                distro: "Ubuntu".into(),
                number: 4,
                label_number: 4,
            }),
        };
        let mut next = 1;
        let layout = node.to_layout(&mut next);
        assert_eq!(leaf_specs(&layout), vec![1, 2]);
        assert_eq!(next, 3, "下一个可用标识接在最后");
    }

    /// 叶按树序先序收集：与 `to_layout` 重建的格序一一对应，恢复才能配对。
    #[test]
    fn saved_leaves_follow_tree_order() {
        let node = SavedNode::Split {
            axis: SavedAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SavedNode::Local {
                number: 1,
                label_number: 1,
                cwd: None,
            }),
            second: Box::new(SavedNode::Split {
                axis: SavedAxis::Stacked,
                ratio: 0.5,
                first: Box::new(SavedNode::Wsl {
                    distro: "Ubuntu".into(),
                    number: 2,
                    label_number: 2,
                }),
                second: Box::new(SavedNode::Local {
                    number: 3,
                    label_number: 3,
                    cwd: None,
                }),
            }),
        };
        let numbers: Vec<usize> = node
            .leaves()
            .iter()
            .map(|leaf| leaf.leaf_number())
            .collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    /// 窗口几何：Windowed 与所有显示器都不相交时回退；相交时按原样恢复。
    #[test]
    fn window_bounds_require_an_intersecting_display() {
        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let mut workspace = Workspace {
            version: WORKSPACE_VERSION,
            window: Some(super::SavedWindow {
                state: WindowState::Windowed,
                x: 20000.,
                y: 20000.,
                width: 800.,
                height: 600.,
            }),
            ..Default::default()
        };
        assert!(
            workspace.window_bounds(&[display]).is_none(),
            "屏外窗口不应恢复"
        );

        let window = workspace.window.as_mut().unwrap();
        window.x = 100.;
        window.y = 100.;
        assert!(
            workspace.window_bounds(&[display]).is_some(),
            "屏内窗口按原位置恢复"
        );
    }

    /// 2026-10-22 语义：最大化状态不再保存、不再恢复。保存侧把 Maximized
    /// 压成窗口态（bounds 用还原矩形）；旧文件里的 maximized 也按窗口态
    /// 恢复并走同样的屏内检查。
    #[test]
    fn maximized_state_is_flattened_on_save_and_restore() {
        let restored = super::Workspace::saved_window(WindowBounds::Maximized(Bounds::new(
            point(px(10.), px(20.)),
            size(px(1080.), px(720.)),
        )));
        assert_eq!(restored.state, WindowState::Windowed);
        assert_eq!(
            (restored.x, restored.y, restored.width, restored.height),
            (10., 20., 1080., 720.),
            "最大化保存的是还原矩形，重开大小不变"
        );

        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let workspace = Workspace {
            version: WORKSPACE_VERSION,
            window: Some(super::SavedWindow {
                state: WindowState::Maximized,
                x: 100.,
                y: 100.,
                width: 1454.,
                height: 1051.,
            }),
            ..Default::default()
        };
        match workspace.window_bounds(&[display]) {
            Some(WindowBounds::Windowed(_)) => {}
            other => panic!("旧 maximized 文件应以窗口态恢复：{other:?}"),
        }
        // 压平后同样受屏内检查约束：屏外的旧 maximized 记录不恢复
        let offscreen = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let workspace = Workspace {
            window: Some(super::SavedWindow {
                state: WindowState::Maximized,
                x: 20000.,
                y: 20000.,
                width: 800.,
                height: 600.,
            }),
            ..workspace
        };
        assert!(workspace.window_bounds(&[offscreen]).is_none());
    }

    #[test]
    fn workspace_survives_a_round_trip_and_rejects_newer_versions() {
        let dir = std::env::temp_dir().join(format!("zhimaterm-workspace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.json");

        let workspace = Workspace {
            version: WORKSPACE_VERSION,
            window: Some(super::SavedWindow {
                state: WindowState::Maximized,
                x: 10.,
                y: 20.,
                width: 1080.,
                height: 720.,
            }),
            tabs: vec![
                super::SavedTab {
                    root: SavedNode::Local {
                        number: 1,
                        label_number: 1,
                        cwd: Some("/tmp".into()),
                    },
                    name: Some("生产库".into()),
                },
                super::SavedTab {
                    root: SavedNode::Local {
                        number: 2,
                        label_number: 2,
                        cwd: None,
                    },
                    name: None,
                },
            ],
            active_tab: 0,
            created: 3,
            label_created: 3,
        };
        workspace.save_at(path.clone()).expect("保存应成功");
        let reloaded = Workspace::load_at(path.clone())
            .expect("读取应成功")
            .expect("文件存在");
        assert_eq!(reloaded, workspace);

        // 版本更新：明确拒绝，不按旧语义猜测字段
        std::fs::write(
            &path,
            format!(r#"{{"version": {}}}"#, WORKSPACE_VERSION + 1),
        )
        .unwrap();
        assert!(Workspace::load_at(path).is_err(), "更高版本必须报错");
    }

    /// 标签名字是 ADR-047 追加的字段：此前写出的文件没有它，读回时标签按
    /// 「没起名」处理，整份文件照常可读。
    #[test]
    fn tabs_saved_before_names_existed_load_as_unnamed() {
        let json = format!(
            r#"{{"version": {WORKSPACE_VERSION}, "tabs": [{{"root": {{"kind": "local", "number": 1, "label_number": 1, "cwd": null}}}}]}}"#
        );
        let workspace: Workspace = serde_json::from_str(&json).expect("旧文件可读");
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].name, None);
    }
}
