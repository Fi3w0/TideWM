pub mod move_grab;
pub use move_grab::MoveSurfaceGrab;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// One-shot proof that a move grab reached a user-confirmed end rather than
/// being torn down by grab replacement, locking, surface loss, or backend
/// cancellation. Smithay deliberately gives `PointerGrab::unset` no reason,
/// so transactional drop effects must carry their own completion state.
#[derive(Clone, Debug, Default)]
pub(crate) struct GrabCompletion(Arc<AtomicBool>);

impl GrabCompletion {
    pub(crate) fn mark_complete(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take_complete(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

pub mod ocean_pan_grab;
pub use ocean_pan_grab::OceanPanGrab;

pub mod ocean_tile_move_grab;
pub use ocean_tile_move_grab::OceanTileMoveGrab;

pub mod resize_grab;
pub use resize_grab::ResizeSurfaceGrab;

pub mod tile_resize_grab;
pub use tile_resize_grab::TileResizeGrab;

pub mod tile_move_grab;
pub use tile_move_grab::TileMoveGrab;

pub mod tile_window_resize_grab;
pub use tile_window_resize_grab::TileWindowResizeGrab;

pub mod cascade_resize_grab;
pub use cascade_resize_grab::CascadeResizeGrab;

#[cfg(test)]
mod tests {
    use super::GrabCompletion;

    #[test]
    fn grab_completion_is_explicit_and_one_shot() {
        let completion = GrabCompletion::default();
        let observer = completion.clone();
        assert!(!completion.take_complete());
        observer.mark_complete();
        assert!(completion.take_complete());
        assert!(!observer.take_complete());
    }
}
