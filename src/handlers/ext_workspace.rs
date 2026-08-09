//! ext-workspace-v1: list and control workspaces over a real Wayland
//! protocol, so a bar can show every workspace -- including empty ones,
//! when `workspace_count` is configured -- and click to switch, instead of
//! polling `tidectl`. Prompted directly by a user asking how to get a
//! persistent 12-workspace strip in waybar: waybar has no compositor-
//! specific TideWM module, and the honest answer at the time was "there
//! isn't a protocol for this yet." This is that protocol.
//!
//! Hand-rolled directly on `smithay::reexports::wayland_protocols`'s
//! generated bindings (the same crate that already backs
//! `wp-tearing-control-v1` elsewhere in this codebase) -- Smithay 0.7 ships
//! no convenience module for this protocol, same situation as
//! `wlr_foreign_toplevel.rs`/`wlr_output_management.rs`.
//!
//! Three-level object hierarchy: one manager per bound client, one
//! `ext_workspace_group_handle_v1` per output (shared across every bound
//! manager, mirroring `WlrOutputManagementState`'s per-output "heads"), and
//! one `ext_workspace_handle_v1` per workspace number within a group.
//! `refresh()` is the single entry point that keeps this in sync with
//! compositor state, diffed against a caller-supplied desired set the same
//! way `WlrOutputManagementState::refresh` diffs outputs -- called from
//! output add/remove, workspace switch, and config reload (see
//! `Smallvil::refresh_ext_workspaces`, the bottom of this file).
//!
//! First-pass scope, matching `wlr_foreign_toplevel.rs`'s own precedent of
//! documenting what's deliberately skipped rather than silently omitting
//! it:
//! - No `output_enter`/`output_leave`: same reason `wlr_foreign_toplevel.rs`
//!   skips them (no direct `Output -> WlOutput` accessor for a specific
//!   client without real per-client bookkeeping). Harmless for the common
//!   single-monitor case; a multi-output client can't yet tell which group
//!   belongs to which screen.
//! - No `coordinates` (optional per spec), no per-workspace `urgent` bit
//!   (would need aggregating every window's urgent flag per workspace --
//!   real feature, not wired up yet).
//! - Only `activate` is a supported request/capability; `deactivate`,
//!   `assign`, `remove`, and the group's `create_workspace` are all
//!   capability-gated off (0), so a client hides those UI affordances
//!   instead of sending requests that would just be silently ignored.
//! - `commit` is a no-op: nothing here batches multiple in-flight requests
//!   before applying them (there's only one request type to apply,
//!   `activate`, and it takes effect immediately), so there's nothing for
//!   `commit` to flush.

use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::Smallvil;

struct TrackedWorkspace {
    number: u32,
    instances: Vec<(Client, ExtWorkspaceHandleV1)>,
    active: bool,
}

struct TrackedGroup {
    output_name: String,
    instances: Vec<(Client, ExtWorkspaceGroupHandleV1)>,
    workspaces: Vec<TrackedWorkspace>,
}

/// Global state for `ext_workspace_manager_v1`. See the module doc for the
/// object hierarchy and first-pass scope.
pub struct ExtWorkspaceState {
    #[allow(dead_code)]
    global: GlobalId,
    managers: Vec<(Client, ExtWorkspaceManagerV1)>,
    groups: Vec<TrackedGroup>,
}

impl ExtWorkspaceState {
    /// Registers the `ext_workspace_manager_v1` global at version 1 (the
    /// only version the protocol defines).
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Smallvil, ExtWorkspaceManagerV1, ()>(1, ());
        Self {
            global,
            managers: Vec::new(),
            groups: Vec::new(),
        }
    }

    /// Creates one client's group-handle resource and sends the
    /// `workspace_group`/`capabilities` events that always accompany it.
    /// Shared by `bind` (replaying existing groups to a new client) and
    /// `refresh` (telling existing clients about a new group), so the two
    /// paths can't drift on what a client is told.
    fn create_group(
        dh: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
    ) -> Option<ExtWorkspaceGroupHandleV1> {
        let group_handle = client
            .create_resource::<ExtWorkspaceGroupHandleV1, _, Smallvil>(dh, manager.version(), ())
            .ok()?;
        manager.workspace_group(&group_handle);
        group_handle.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
        Some(group_handle)
    }

    /// Creates one client's workspace-handle resource and sends the
    /// `workspace`/`name`/`state`/`capabilities`/`workspace_enter` events
    /// that always accompany it. Shared the same way `create_group` is.
    #[allow(clippy::too_many_arguments)]
    fn create_workspace(
        dh: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
        group_handle: &ExtWorkspaceGroupHandleV1,
        output_name: &str,
        number: u32,
        active: bool,
    ) -> Option<ExtWorkspaceHandleV1> {
        let ws_handle = client
            .create_resource::<ExtWorkspaceHandleV1, _, Smallvil>(
                dh,
                manager.version(),
                (output_name.to_string(), number),
            )
            .ok()?;
        manager.workspace(&ws_handle);
        ws_handle.name(number.to_string());
        ws_handle.state(if active {
            ext_workspace_handle_v1::State::Active
        } else {
            ext_workspace_handle_v1::State::empty()
        });
        ws_handle.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
        group_handle.workspace_enter(&ws_handle);
        Some(ws_handle)
    }

    /// Diffs tracked groups/workspaces against `desired` -- one entry per
    /// live output, each carrying its workspace numbers and which one is
    /// active -- and pushes exactly the changes to every bound manager,
    /// finishing with one `done()` per manager if anything changed at all.
    /// Mirrors `WlrOutputManagementState::refresh`'s per-output diff shape,
    /// generalized one level for workspaces-within-a-group. Called from
    /// output add/remove, workspace switch, and config reload (see
    /// `Smallvil::refresh_ext_workspaces` below).
    pub fn refresh(&mut self, dh: &DisplayHandle, desired: &[(String, Vec<(u32, bool)>)]) {
        let mut changed = false;

        // Remove groups for outputs no longer live.
        let mut i = 0;
        while i < self.groups.len() {
            if desired
                .iter()
                .any(|(name, _)| *name == self.groups[i].output_name)
            {
                i += 1;
                continue;
            }
            changed = true;
            let group = self.groups.remove(i);
            for workspace in &group.workspaces {
                for (_, instance) in &workspace.instances {
                    instance.removed();
                }
            }
            for (_, instance) in &group.instances {
                instance.removed();
            }
        }

        // Add groups for newly live outputs; their workspaces are filled
        // in by the per-group loop below, which runs for every group
        // whether just-added or pre-existing.
        for (output_name, _) in desired {
            if self.groups.iter().any(|g| &g.output_name == output_name) {
                continue;
            }
            changed = true;
            let mut group = TrackedGroup {
                output_name: output_name.clone(),
                instances: Vec::new(),
                workspaces: Vec::new(),
            };
            for (client, manager) in &self.managers {
                if let Some(group_handle) = Self::create_group(dh, client, manager) {
                    group.instances.push((client.clone(), group_handle));
                }
            }
            self.groups.push(group);
        }

        // Diff workspaces within each surviving/new group.
        for (output_name, workspaces) in desired {
            let Some(group_idx) = self
                .groups
                .iter()
                .position(|g| &g.output_name == output_name)
            else {
                continue;
            };

            // Remove workspaces no longer present.
            let mut wi = 0;
            while wi < self.groups[group_idx].workspaces.len() {
                let number = self.groups[group_idx].workspaces[wi].number;
                if workspaces.iter().any(|(n, _)| *n == number) {
                    wi += 1;
                    continue;
                }
                changed = true;
                let tracked = self.groups[group_idx].workspaces.remove(wi);
                for (client, instance) in &tracked.instances {
                    if let Some((_, group_instance)) = self.groups[group_idx]
                        .instances
                        .iter()
                        .find(|(c, _)| c == client)
                    {
                        group_instance.workspace_leave(instance);
                    }
                    instance.removed();
                }
            }

            // Add workspaces newly present.
            for (number, active) in workspaces {
                if self.groups[group_idx]
                    .workspaces
                    .iter()
                    .any(|w| w.number == *number)
                {
                    continue;
                }
                changed = true;
                let mut tracked = TrackedWorkspace {
                    number: *number,
                    instances: Vec::new(),
                    active: *active,
                };
                let group_instances = self.groups[group_idx].instances.clone();
                for (client, group_instance) in &group_instances {
                    let Some((_, manager)) = self.managers.iter().find(|(c, _)| c == client) else {
                        continue;
                    };
                    if let Some(ws_handle) = Self::create_workspace(
                        dh,
                        client,
                        manager,
                        group_instance,
                        output_name,
                        *number,
                        *active,
                    ) {
                        tracked.instances.push((client.clone(), ws_handle));
                    }
                }
                self.groups[group_idx].workspaces.push(tracked);
            }

            // Update the active bit on survivors whose state changed.
            for (number, active) in workspaces {
                let Some(tracked) = self.groups[group_idx]
                    .workspaces
                    .iter_mut()
                    .find(|w| w.number == *number)
                else {
                    continue;
                };
                if tracked.active == *active {
                    continue;
                }
                changed = true;
                tracked.active = *active;
                for (_, instance) in &tracked.instances {
                    instance.state(if *active {
                        ext_workspace_handle_v1::State::Active
                    } else {
                        ext_workspace_handle_v1::State::empty()
                    });
                }
            }
        }

        if changed {
            for (_, manager) in &self.managers {
                manager.done();
            }
        }
    }
}

impl Smallvil {
    /// Rebuilds the desired-state snapshot from live compositor state and
    /// hands it to `ExtWorkspaceState::refresh`. Called from output
    /// add/remove (both backends, next to the existing
    /// `wlr_output_management_state.refresh` calls), `apply_workspace_switch`,
    /// and `reload_config` (a live `workspace_count` change should show up
    /// immediately, same as everything else hot-reloadable).
    pub(crate) fn refresh_ext_workspaces(&mut self) {
        let desired: Vec<(String, Vec<(u32, bool)>)> = self
            .space
            .outputs()
            .map(|output| output.name())
            .map(|name| {
                let active = self.layout.active_workspace(&name);
                let workspaces = self
                    .advertised_workspaces(&name)
                    .into_iter()
                    .map(|number| (number, number == active))
                    .collect();
                (name, workspaces)
            })
            .collect();
        let dh = self.display_handle.clone();
        self.ext_workspace_state.refresh(&dh, &desired);
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for Smallvil {
    fn can_view(client: Client, _data: &()) -> bool {
        crate::state::trusted_client(&client)
    }

    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        for group in &mut state.ext_workspace_state.groups {
            let Some(group_handle) = ExtWorkspaceState::create_group(dh, client, &manager) else {
                continue;
            };
            for workspace in &mut group.workspaces {
                if let Some(ws_handle) = ExtWorkspaceState::create_workspace(
                    dh,
                    client,
                    &manager,
                    &group_handle,
                    &group.output_name,
                    workspace.number,
                    workspace.active,
                ) {
                    workspace.instances.push((client.clone(), ws_handle));
                }
            }
            group.instances.push((client.clone(), group_handle));
        }
        manager.done();
        state
            .ext_workspace_state
            .managers
            .push((client.clone(), manager));
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .ext_workspace_state
                    .managers
                    .retain(|(_, m)| m != manager);
            }
            // Nothing here batches multiple in-flight requests before
            // applying them -- `activate` takes effect immediately -- so
            // there's nothing for `commit` to flush. See the module doc.
            ext_workspace_manager_v1::Request::Commit => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        state
            .ext_workspace_state
            .managers
            .retain(|(_, m)| m != resource);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Smallvil {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // `create_workspace` is capability-gated off (0); a
            // well-behaved client won't send this, and the spec says to
            // just ignore it if one does anyway.
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceGroupHandleV1,
        _data: &(),
    ) {
        for group in &mut state.ext_workspace_state.groups {
            group.instances.retain(|(_, r)| r != resource);
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, (String, u32)> for Smallvil {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &(String, u32),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Deactivate/assign/remove are capability-gated off (0) the same
        // way the group's create_workspace is; only activate is wired up.
        if let ext_workspace_handle_v1::Request::Activate = request {
            let (output_name, workspace) = data;
            let output = state
                .space
                .outputs()
                .find(|o| &o.name() == output_name)
                .cloned();
            if let Some(output) = output {
                state.switch_workspace(&output, *workspace);
            }
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceHandleV1,
        _data: &(String, u32),
    ) {
        for group in &mut state.ext_workspace_state.groups {
            for workspace in &mut group.workspaces {
                workspace.instances.retain(|(_, r)| r != resource);
            }
        }
    }
}
