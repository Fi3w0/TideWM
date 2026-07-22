//! AccessKit/AT-SPI representation of TideWM's compositor-owned UI.
//!
//! The Unix adapter invokes its handlers on another thread. Consequently
//! those handlers only ever read this small, owned snapshot; they never hold
//! a `Smallvil` reference or call into Smithay. The compositor refreshes the
//! snapshot on a short coalescing timer after it marks a frame dirty, so
//! high-rate pointer motion cannot create matching cross-thread work.

use std::sync::{Arc, Mutex};

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Live, Node, NodeId, Role,
    Tree, TreeId, TreeUpdate,
};
use accesskit_unix::Adapter;

const ROOT: NodeId = NodeId(0);
const WORKSPACE_STATUS: NodeId = NodeId(1);
const TOAST: NodeId = NodeId(2);
const CONFIG_ERROR: NodeId = NodeId(3);
const OVERVIEW: NodeId = NodeId(10);
const OVERVIEW_ITEM_BASE: u64 = 1 << 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UiSnapshot {
    pub workspace: String,
    pub locked: bool,
    pub toast: Option<(String, bool)>,
    pub config_error: Option<String>,
    pub overview_workspaces: Vec<(u32, String)>,
    pub groups: Vec<GroupSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupSnapshot {
    pub id: u64,
    pub tabs: Vec<(u64, String)>,
    pub active: usize,
}

pub(crate) struct AccessibilityTree {
    adapter: Adapter,
    shared: Arc<Mutex<UiSnapshot>>,
    last: UiSnapshot,
}

impl AccessibilityTree {
    pub(crate) fn new(initial: UiSnapshot) -> Self {
        let shared = Arc::new(Mutex::new(initial.clone()));
        let mut adapter = Adapter::new(
            SnapshotActivation(shared.clone()),
            IgnoreActions,
            IgnoreDeactivation,
        );
        // A compositor is the focused desktop host whenever it is running;
        // individual client focus is represented by the status/tab nodes.
        adapter.update_window_focus_state(true);
        Self {
            adapter,
            shared,
            last: initial,
        }
    }

    pub(crate) fn update(&mut self, snapshot: UiSnapshot) {
        if snapshot == self.last {
            return;
        }
        self.last = snapshot.clone();
        *self.shared.lock().unwrap() = snapshot.clone();
        self.adapter.update_if_active(|| build_update(&snapshot));
    }
}

struct SnapshotActivation(Arc<Mutex<UiSnapshot>>);

impl ActivationHandler for SnapshotActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        Some(build_update(&self.0.lock().unwrap()))
    }
}

struct IgnoreActions;

impl ActionHandler for IgnoreActions {
    fn do_action(&mut self, _request: ActionRequest) {}
}

struct IgnoreDeactivation;

impl DeactivationHandler for IgnoreDeactivation {
    fn deactivate_accessibility(&mut self) {}
}

fn labelled(role: Role, label: impl Into<String>) -> Node {
    let mut node = Node::new(role);
    node.set_label(label.into());
    node
}

fn build_update(snapshot: &UiSnapshot) -> TreeUpdate {
    let mut nodes = Vec::new();
    let mut root_children = vec![WORKSPACE_STATUS];

    let workspace_label = if snapshot.locked {
        "Session locked".to_owned()
    } else if snapshot.workspace.is_empty() {
        "No active workspace".to_owned()
    } else {
        format!("Workspace {}", snapshot.workspace)
    };
    let mut workspace = labelled(Role::Status, workspace_label);
    workspace.set_live(Live::Polite);
    nodes.push((WORKSPACE_STATUS, workspace));

    // Never expose client titles or compositor notifications through the
    // accessibility bus while the session is locked.
    if !snapshot.locked {
        if let Some((message, is_error)) = &snapshot.toast {
            root_children.push(TOAST);
            let mut toast = labelled(Role::Alert, message.clone());
            toast.set_live(if *is_error {
                Live::Assertive
            } else {
                Live::Polite
            });
            nodes.push((TOAST, toast));
        }
        if let Some(message) = &snapshot.config_error {
            root_children.push(CONFIG_ERROR);
            let mut error = labelled(Role::Alert, message.clone());
            error.set_live(Live::Assertive);
            nodes.push((CONFIG_ERROR, error));
        }

        if !snapshot.overview_workspaces.is_empty() {
            root_children.push(OVERVIEW);
            let mut children = Vec::new();
            for (workspace, label) in &snapshot.overview_workspaces {
                let id = NodeId(OVERVIEW_ITEM_BASE + u64::from(*workspace));
                children.push(id);
                nodes.push((id, labelled(Role::ListItem, label.clone())));
            }
            let mut overview = labelled(Role::List, "Workspace overview");
            overview.set_children(children);
            nodes.push((OVERVIEW, overview));
        }

        for (group_index, group) in snapshot.groups.iter().enumerate() {
            let group_id = NodeId(group.id);
            root_children.push(group_id);
            let mut tab_ids = Vec::new();
            for (tab_index, (surface_id, title)) in group.tabs.iter().enumerate() {
                let id = NodeId(*surface_id);
                tab_ids.push(id);
                let mut tab = labelled(Role::Tab, title.clone());
                if tab_index == group.active {
                    tab.set_selected(true);
                }
                nodes.push((id, tab));
            }
            let mut tab_list = labelled(Role::TabList, format!("Window group {}", group_index + 1));
            tab_list.set_children(tab_ids);
            nodes.push((group_id, tab_list));
        }
    }

    let mut root = labelled(Role::Window, "TideWM desktop");
    root.set_children(root_children);
    nodes.push((ROOT, root));

    let mut tree = Tree::new(ROOT);
    tree.toolkit_name = Some("TideWM".to_owned());
    TreeUpdate {
        nodes,
        tree: Some(tree),
        tree_id: TreeId::ROOT,
        focus: ROOT,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn locked_tree_does_not_expose_private_ui() {
        let update = build_update(&UiSnapshot {
            workspace: "2".into(),
            locked: true,
            toast: Some(("secret".into(), true)),
            config_error: Some("secret config path".into()),
            overview_workspaces: vec![(1, "work".into())],
            groups: vec![GroupSnapshot {
                id: 10,
                tabs: vec![(11, "private title".into())],
                active: 0,
            }],
        });
        assert_eq!(update.nodes.len(), 2);
        assert!(!format!("{update:?}").contains("secret"));
        assert!(!format!("{update:?}").contains("private title"));
    }

    #[test]
    fn large_groups_never_collide_with_following_group_ids() {
        let tabs = (0..128)
            .map(|index| (2_000 + index, format!("tab {index}")))
            .collect();
        let update = build_update(&UiSnapshot {
            groups: vec![
                GroupSnapshot {
                    id: 1_000,
                    tabs,
                    active: 0,
                },
                GroupSnapshot {
                    id: 1_001,
                    tabs: vec![(3_000, "next group".into())],
                    active: 0,
                },
            ],
            ..UiSnapshot::default()
        });
        let ids: HashSet<_> = update.nodes.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), update.nodes.len());
    }
}
