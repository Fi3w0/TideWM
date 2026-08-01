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
independently panning and zooming per-output cameras:

- **X** is continuous lateral travel between working regions.
- **Y** is physical window depth/position, not a row of disguised workspaces.
- Reefs provide local BSP/master/cascade organization without dividing the
  world into workspace pages.
- Reefs are optional organization, not a movement restriction: the configured
  move or resize drag detaches a tile into a freely placed world rectangle,
  while dragging empty canvas pans the camera at the current zoom. Overlapping
  floaters keep an explicit front-to-back stack and clicks raise predictably.
- Smart tiling can keep a tiled drag inside its reef for tile-to-tile swaps and
  reattach a floater released near a tile, preserving a custom attached size
  while the reef tree remains the ownership authority; the tree itself stays
  frozen for the gesture, so the dragged window's render placement is
  overridden separately to lift it out and follow the pointer (it renders as
  `PlacementKind::Floating` for the drag's duration, so a `floating_only`
  border/shadow/rounding rule applies to it only while dragging), with the
  current swap target picking up a magnet-style border highlight. Drag feel
  itself is reasoned from the render pipeline, not live-verified -- nested
  winit has no pointer-drag injection, the same gap viscosity and the other
  interactive grabs carry.
- Bookmarks provide named return points and a compatibility surface for tools
  that expect workspace-like destinations.
- An optional camera-anchored adaptive guide field provides scale and motion
  cues through empty world space; it is not a workspace boundary.
- A small viewport-center marker appears only after camera movement and fades
  away after 4.2 seconds by default, keeping orientation without permanent UI.
- Attention/buoyancy and render LOD are separate from physical Y, so focusing a
  deep window does not teleport it to the surface.

Depth Down/Up visits only reef origins and explicitly world-placed floating or
sunk windows. Local tile rows are deliberately excluded, because treating
them as navigation stops recreates vertical workspaces. Sinking, dredging, and
surfacing are explicit actions; focusing a deep window alone never changes its
world Y. The later bioluminescent compass and overview will make distant or
urgent windows discoverable.

## Shared and separate state

Both engines share the compositor core, Wayland objects, window/protocol
registry, rendering, effects, input plumbing, IPC transport, and accessibility
infrastructure. They do not share spatial ownership:

- `ClassicSpace` owns `(output, workspace)`, layout leaves, deck membership,
  and restore slots.
- `OceanSpace` now owns reef-local BSP trees, world rectangles, independent
  per-output pan/zoom cameras, floating world rectangles, entry-output hints,
  configured/runtime bookmarks, and physical depth travel/actions.

Both produce the same model-neutral placed-window render input. S2 established
that boundary in `src/tide_core/placement.rs`: rendering receives a window,
logical rectangle, fractional view transform, authoritative/preview role, and
content-size policy, plus tiled/floating and normal/fullscreen presentation
flags. Classic and Ocean now both produce this contract from their own
authoritative state.

Waves owns every keyboard binding, and pointer gestures are explicit config
(`pointer_modifier` plus Ocean's selectable/disableable canvas button).
Selecting an engine or enabling Depth never inserts hidden keyboard chords;
the generated Alt/Super/Ctrl/P layers are editable examples.
`Ctrl+Alt+Escape` is the separate temporary recovery path for a self-locked
config.

## Delivery order

1. **Done:** manual Classic Depth Deck: park, navigate, swap-recall, cancel.
2. Optional Classic auto-park policy, only after real-use validation.
3. **Done:** model-neutral placement boundary for the shared renderer.
4. **Done:** Ocean world coordinates, cameras, reefs, and bookmarks.
5. **Done:** Ocean pan/zoom canvas feel and physical sink/dredge/surface travel.
6. Ocean compass and whole-world overview.

The existing continuous workspace swim is an S0 Classic navigation bridge. It
is useful on its own, but it is not the Ocean data model.
