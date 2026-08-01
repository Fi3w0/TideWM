# TideWM spatial model

TideWM is one compositor with two selectable spatial engines. The engine is a
startup choice because it changes window ownership and navigation; water,
glass, animations, and other visual features remain independent toggles.

## Classic

Classic keeps TideWM's current per-output numbered workspaces and tiling
algorithms. Depth is a per-workspace **Depth Deck**, not another workspace:

- **Surface:** the ordinary visible tiled/floating scene.
- **Shallows:** automatic attention cooling from the existing visual depth
  system; it does not change layout membership.
- **Deep Deck:** windows explicitly parked out of the active layout while the
  client stays alive and remains owned by that workspace.

The opt-in `classic_depth { enabled = true }` block enables the feature; it is
off by default and independent of automatic visual depth. The primary
interaction is fast and reversible: park the focused tiled window,
dive into the current workspace's deck, select a window, and recall it. When a
surface tile is focused, recall swaps the selected deep window into that exact
tile and parks the displaced window in the same deck slot. Empty-surface recall
restores the selected window as a normal tile. Automatic structural parking is
off by default and will not be considered until the manual workflow proves
useful.

The fast path is direct rather than modal: Depth Down/Up rotates the focused
tile through the workspace's deck in opposite directions, with a vertical
pressure wave showing direction. The full deck remains the overview for random
access.

## Ocean

Ocean has no real numbered workspaces. It owns one continuous 2D world with
per-output cameras:

- **X** is continuous lateral travel between working regions.
- **Y** is physical window depth/position, not a row of disguised workspaces.
- Reefs provide local BSP/master/cascade organization without dividing the
  world into workspace pages.
- Bookmarks provide named return points and a compatibility surface for tools
  that expect workspace-like destinations.
- Attention/buoyancy and render LOD are separate from physical Y, so focusing a
  deep window does not teleport it to the surface.

The bioluminescent compass and overview make distant or urgent windows
discoverable. Sinking, dredging, and surfacing are explicit spatial actions.

## Shared and separate state

Both engines share the compositor core, Wayland objects, window/protocol
registry, rendering, effects, input plumbing, IPC transport, and accessibility
infrastructure. They do not share spatial ownership:

- `ClassicSpace` owns `(output, workspace)`, layout leaves, deck membership,
  and restore slots.
- `OceanSpace` now owns reef-local BSP trees, world rectangles, independent
  per-output camera origins, floating world rectangles, entry-output hints,
  and configured/runtime bookmarks. Physical depth actions remain S4.

Both produce the same model-neutral placed-window render input. S2 established
that boundary in `src/tide_core/placement.rs`: rendering receives a window,
logical rectangle, fractional view transform, authoritative/preview role, and
content-size policy, plus tiled/floating and normal/fullscreen presentation
flags. Classic and Ocean now both produce this contract from their own
authoritative state.

## Delivery order

1. **Done:** manual Classic Depth Deck: park, navigate, swap-recall, cancel.
2. Optional Classic auto-park policy, only after real-use validation.
3. **Done:** model-neutral placement boundary for the shared renderer.
4. **Done:** Ocean world coordinates, cameras, reefs, and bookmarks.
5. Ocean physical-depth actions, compass, and overview.

The existing continuous workspace swim is an S0 Classic navigation bridge. It
is useful on its own, but it is not the Ocean data model.
