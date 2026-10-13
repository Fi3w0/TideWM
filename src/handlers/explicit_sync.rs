//! Hold unused transaction release points until Smithay adopts their buffer.
//! Like KWin's SyncObjReleasePoint, cancellation releases a point that never
//! reached rendering. Applied buffers keep Smithay's renderer/KMS lifetime.
use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, RendererSurfaceStateUserData},
    reexports::wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle},
    wayland::{
        compositor::{
            with_states, with_surface_tree_upward, BufferAssignment, Cacheable, SurfaceAttributes,
            TraversalAction,
        },
        drm_syncobj::{DrmSyncPoint, DrmSyncobjCachedState},
    },
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

#[derive(Debug)]
struct PendingRelease {
    point: DrmSyncPoint,
    finished: AtomicBool,
}
impl PendingRelease {
    fn release(&self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            if let Err(error) = self.point.signal() {
                tracing::warn!(%error, "Failed to release an unused explicit-sync buffer");
            }
        }
    }
    fn adopted(&self) {
        self.finished.store(true, Ordering::Release);
    }
}
impl Drop for PendingRelease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Default)]
struct PendingReleaseState(Option<Arc<PendingRelease>>);
impl Cacheable for PendingReleaseState {
    fn commit(&mut self, _: &DisplayHandle) -> Self {
        Self(self.0.take())
    }
    fn merge_into(self, into: &mut Self, _: &DisplayHandle) {
        if self.0.is_some() {
            into.0 = self.0;
        }
    }
}
#[derive(Default)]
struct PendingReleases(Mutex<Vec<Weak<PendingRelease>>>);

pub(super) fn track_pending_release(surface: &WlSurface, limit: usize) -> bool {
    with_states(surface, |states| {
        let point = states
            .cached_state
            .get::<DrmSyncobjCachedState>()
            .pending()
            .release_point
            .clone();
        let Some(point) = point else {
            return true;
        };
        states
            .data_map
            .insert_if_missing_threadsafe(PendingReleases::default);
        let mut pending = states
            .data_map
            .get::<PendingReleases>()
            .unwrap()
            .0
            .lock()
            .unwrap();
        pending.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|p| !p.finished.load(Ordering::Acquire))
        });
        if pending.len() >= limit {
            return false;
        }
        let release = Arc::new(PendingRelease {
            point,
            finished: AtomicBool::new(false),
        });
        pending.push(Arc::downgrade(&release));
        states.cached_state.get::<PendingReleaseState>().pending().0 = Some(release);
        true
    })
}

pub(super) fn release_cancelled(surface: &WlSurface) {
    with_states(surface, |states| {
        if let Some(pending) = states.data_map.get::<PendingReleases>() {
            for release in pending
                .0
                .lock()
                .unwrap()
                .drain(..)
                .filter_map(|p| p.upgrade())
            {
                release.release();
            }
        }
    });
}

pub(super) fn commit_buffers<D: 'static>(surface: &WlSurface) {
    if smithay::wayland::compositor::is_sync_subsurface(surface) {
        return;
    }
    // The pinned renderer keeps its old Buffer wrapper when the identical
    // wl_buffer is reattached. Explicit points describe each attachment;
    // use the public removal path to retire that wrapper before adopting
    // the new points, without unmapping the compositor's window/layout.
    let mut reattached = Vec::new();
    with_surface_tree_upward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |surface, states, _| {
            if states.data_map.get::<PendingReleases>().is_none() {
                return;
            }
            let explicit = states
                .cached_state
                .get::<DrmSyncobjCachedState>()
                .current()
                .acquire_point
                .is_some();
            if !explicit {
                return;
            }
            let buffer = states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .as_ref()
                .and_then(|assignment| match assignment {
                    BufferAssignment::NewBuffer(buffer) => Some(buffer.clone()),
                    _ => None,
                });
            if let Some(buffer) = buffer {
                let same = states
                    .data_map
                    .get::<RendererSurfaceStateUserData>()
                    .is_some_and(|renderer| {
                        renderer
                            .lock()
                            .unwrap()
                            .buffer()
                            .is_some_and(|current| current == buffer)
                    });
                if same {
                    states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer = Some(BufferAssignment::Removed);
                    reattached.push((surface.clone(), buffer));
                }
            }
        },
        |_, _, _| true,
    );
    on_commit_buffer_handler::<D>(surface);
    if !reattached.is_empty() {
        for (surface, buffer) in reattached {
            with_states(&surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer = Some(BufferAssignment::NewBuffer(buffer))
            });
        }
        on_commit_buffer_handler::<D>(surface);
    }
    with_surface_tree_upward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            if states.data_map.get::<PendingReleases>().is_none() {
                return;
            }
            if let Some(release) = states
                .cached_state
                .get::<PendingReleaseState>()
                .current()
                .0
                .take()
            {
                let adopted = states
                    .data_map
                    .get::<RendererSurfaceStateUserData>()
                    .is_some_and(|renderer| renderer.lock().unwrap().buffer().is_some());
                if adopted {
                    release.adopted();
                }
            }
        },
        |_, _, _| true,
    );
}
