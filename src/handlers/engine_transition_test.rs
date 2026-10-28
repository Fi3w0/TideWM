//! Real xdg-shell windows carried through live engine changes on two
//! simulated outputs; no native seat. Pins the transition invariants in
//! `.hermes/worklogs/spatial-invariants.md`.
use super::floating_placement_test::{string, Fixture};
use crate::{config::SpatialEngine, state::ClientState, Smallvil};
use smithay::{
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_server::{protocol::wl_surface::WlSurface, Display, Resource},
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::compositor::CompositorClientState,
};
use std::{
    collections::BTreeMap,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Where one window lives, independent of the engine that owns it.
#[derive(Debug, Clone, PartialEq)]
enum Owner {
    Tiled {
        output: String,
        workspace: u32,
    },
    Floating {
        output: String,
        workspace: u32,
        rect: Rectangle<i32, Logical>,
    },
    OceanTiled,
    OceanFloating,
    /// An inactive tab: owned by its group, not by a tree.
    Parked,
}

#[derive(Debug, Clone, PartialEq)]
struct Membership {
    owner: Owner,
    pinned: bool,
    fullscreen_output: Option<String>,
    group: Option<Vec<String>>,
}

fn owners(state: &Smallvil, surface: &WlSurface) -> Vec<Owner> {
    let mut found = Vec::new();
    if state.spatial.workspaces.layout.contains(surface) {
        found.push(Owner::Tiled {
            output: state
                .spatial
                .workspaces
                .layout
                .output_of(surface)
                .unwrap()
                .to_string(),
            workspace: state
                .spatial
                .workspaces
                .layout
                .workspace_of(surface)
                .unwrap(),
        });
    }
    if let Some(tag) = state.spatial.workspaces.floating_workspace.get(surface) {
        found.push(Owner::Floating {
            output: tag.output.clone(),
            workspace: tag.workspace,
            rect: tag.rect,
        });
    }
    if state.spatial.ocean.is_tiled(surface) {
        found.push(Owner::OceanTiled);
    }
    if state.spatial.ocean.floating_rect(surface).is_some() {
        found.push(Owner::OceanFloating);
    }
    if found.is_empty()
        && state
            .group_of(surface)
            .is_some_and(|g| state.groups[g].members[state.groups[g].active].surface != *surface)
    {
        found.push(Owner::Parked);
    }
    found
}

fn snapshot(state: &Smallvil, windows: &[(&str, WlSurface)]) -> BTreeMap<String, Membership> {
    windows
        .iter()
        .map(|(name, surface)| {
            let found = owners(state, surface);
            assert_eq!(
                found.len(),
                1,
                "{name} must have exactly one spatial owner under {:?}: {found:?}",
                state.spatial.engine
            );
            let group = state.group_of(surface).map(|g| {
                let mut members: Vec<String> = state.groups[g]
                    .members
                    .iter()
                    .filter_map(|m| {
                        windows
                            .iter()
                            .find(|(_, s)| *s == m.surface)
                            .map(|(n, _)| n.to_string())
                    })
                    .collect();
                members.sort();
                members
            });
            (
                name.to_string(),
                Membership {
                    owner: found[0].clone(),
                    pinned: state.window_is_pinned(surface),
                    fullscreen_output: state.fullscreen.get(surface).map(|e| e.output.clone()),
                    group,
                },
            )
        })
        .collect()
}

/// Re-runs `test` in a child process with private XDG directories holding
/// `config`, so no test touches the real session. Returns true in the
/// parent, after the child passed and printed `marker`.
fn run_isolated(test: &str, config: &str, marker: &str) -> bool {
    const CHILD: &str = "TIDEWM_ENGINE_TRANSITION_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("tidewm-engine-test-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(root.join("config/tidewm")).unwrap();
    std::fs::create_dir(root.join("runtime")).unwrap();
    std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.join("config/tidewm/config.wave"), config).unwrap();
    let log = std::fs::File::create(root.join("child.log")).unwrap();
    let test_name = format!("{}::{test}", module_path!());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            test_name.split_once("::").unwrap().1,
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    // A deadlock in a migration must fail instead of hanging the suite.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(root.join("child.log")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(status.is_some(), "engine migration hung: {output}");
    assert!(status.unwrap().success(), "{output}");
    assert!(output.contains("running 1 test"), "{output}");
    assert!(output.contains(marker), "{output}");
    true
}

/// A compositor with outputs A and B (1200x900, side by side) and one bound
/// client.
fn two_output_fixture() -> Fixture {
    let mut event_loop = EventLoop::try_new().unwrap();
    let state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let (server, peer) = UnixStream::pair().unwrap();
    state
        .display_handle
        .clone()
        .insert_client(
            server,
            Arc::new(ClientState {
                compositor_state: CompositorClientState::default(),
                security_context: None,
                disconnect_sender: None,
            }),
        )
        .unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut f = Fixture {
        event_loop,
        state,
        peer,
        next_id: 7,
    };
    for (i, name) in ["A", "B"].into_iter().enumerate() {
        let o = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        o.change_current_state(
            Some(Mode {
                size: (1200, 900).into(),
                refresh: 60_000,
            }),
            None,
            Some(Scale::Integer(1)),
            None,
        );
        o.create_global::<Smallvil>(&f.state.display_handle);
        f.state.space.map_output(&o, (i as i32 * 1200, 0));
    }
    f.state.retile();
    f.request(1, 1, &[2], &[]);
    let globals = f.dispatch();
    for (interface, id) in [
        ("wl_compositor", 3u32),
        ("xdg_wm_base", 4),
        ("wp_single_pixel_buffer_manager_v1", 5),
        ("wp_viewporter", 6),
    ] {
        let name = globals
            .iter()
            .find_map(|(object, op, body)| {
                if (*object, *op) != (2, 0) {
                    return None;
                }
                let len = u32::from_ne_bytes(body[4..8].try_into().unwrap()) as usize;
                (std::str::from_utf8(&body[8..8 + len - 1]).unwrap() == interface)
                    .then(|| u32::from_ne_bytes(body[..4].try_into().unwrap()))
            })
            .unwrap();
        let mut tail = string(interface);
        tail.extend_from_slice(
            &(if interface == "wl_compositor" {
                4u32
            } else {
                1u32
            })
            .to_ne_bytes(),
        );
        tail.extend_from_slice(&id.to_ne_bytes());
        f.request(2, 0, &[name], &tail);
    }
    f.dispatch();
    f
}

/// Maps a toplevel and returns its surface. Hidden-workspace windows are
/// not in `Space`; the foreign-toplevel registry holds every mapped one.
fn map(f: &mut Fixture, app: &str) -> WlSurface {
    let id = f.window(app, None, (400, 300));
    f.state
        .foreign_toplevels
        .keys()
        .find(|s| s.id().protocol_id() == id)
        .cloned()
        .unwrap()
}

const ROUND_TRIP_CONFIG: &str = r#"engine = classic
welcome_hint = false
water_effects = false
gaps = 8
animations {
 enabled = false
}
rule {
 app_id = a
 output = A
}
rule {
 app_id = deep
 output = A
 workspace = 2
}
rule {
 app_id = b
 output = B
}
rule {
 app_id = float
 output = A
 float = true
 position = "100x120"
 size = "400x300"
}
rule {
 app_id = pin
 output = A
 pin = true
 position = "700x500"
 size = "300x200"
}
"#;

#[test]
fn engine_round_trip_keeps_membership_focus_and_floating_geometry() {
    if run_isolated(
        "engine_round_trip_keeps_membership_focus_and_floating_geometry",
        ROUND_TRIP_CONFIG,
        "engine transitions checked: 6",
    ) {
        return;
    }
    let mut f = two_output_fixture();
    let mut windows = Vec::new();
    for name in ["a", "a", "deep", "b", "float", "pin"] {
        windows.push((name, map(&mut f, name)));
    }
    let windows: Vec<(&str, WlSurface)> = windows
        .into_iter()
        .enumerate()
        .map(|(i, (name, surface))| {
            (
                match (name, i) {
                    ("a", 0) => "a1",
                    ("a", _) => "a2",
                    (other, _) => other,
                },
                surface,
            )
        })
        .collect();
    let surface = |name: &str| windows.iter().find(|(n, _)| *n == name).unwrap().1.clone();

    // a1 + a2 share one tab group; b is fullscreen on B.
    f.state
        .focus_window(Some(surface("a2")), SERIAL_COUNTER.next_serial());
    f.state.run_action(crate::config::Action::GroupDirection(
        crate::config::Direction::Left,
    ));
    f.dispatch();
    assert!(f.state.group_of(&surface("a1")).is_some());
    f.state
        .focus_window(Some(surface("b")), SERIAL_COUNTER.next_serial());
    f.state.run_action(crate::config::Action::ToggleFullscreen);
    f.dispatch();
    assert!(f.state.fullscreen.contains_key(&surface("b")));
    f.state
        .focus_window(Some(surface("float")), SERIAL_COUNTER.next_serial());
    f.dispatch();

    let initial = snapshot(&f.state, &windows);
    assert_eq!(
        initial["float"].owner,
        Owner::Floating {
            output: "A".into(),
            workspace: 1,
            rect: Rectangle::new((100, 120).into(), (400, 300).into()),
        }
    );
    assert!(initial["pin"].pinned);
    assert_eq!(initial["b"].fullscreen_output.as_deref(), Some("B"));
    assert_eq!(
        initial["deep"].owner,
        Owner::Tiled {
            output: "A".into(),
            workspace: 2
        }
    );

    let mut current = SpatialEngine::Classic;
    let mut checked = 0;
    for target in [
        SpatialEngine::Ocean,
        SpatialEngine::Scroll,
        SpatialEngine::Classic,
        SpatialEngine::Scroll,
        SpatialEngine::Ocean,
        SpatialEngine::Classic,
    ] {
        assert!(
            f.state.migrate_spatial_engine(target),
            "{current:?} -> {target:?}"
        );
        f.dispatch();
        assert_eq!(f.state.spatial.engine, target);
        let now = snapshot(&f.state, &windows);
        let step = format!("{current:?} -> {target:?}");
        assert_eq!(
            f.state.focused_window_surface(),
            Some(surface("float")),
            "focus after {step}"
        );
        for (name, before) in &initial {
            let after = &now[name];
            assert_eq!(after.pinned, before.pinned, "{name} pin after {step}");
            assert_eq!(
                after.fullscreen_output, before.fullscreen_output,
                "{name} fullscreen after {step}"
            );
            assert_eq!(after.group, before.group, "{name} group after {step}");
            let floating =
                |owner: &Owner| matches!(owner, Owner::Floating { .. } | Owner::OceanFloating);
            assert_eq!(
                floating(&after.owner),
                floating(&before.owner),
                "{name} floating after {step}: {:?}",
                after.owner
            );
            if target == SpatialEngine::Ocean {
                assert!(
                    !matches!(after.owner, Owner::Tiled { .. } | Owner::Floating { .. }),
                    "{name} kept workspace ownership under Ocean: {:?}",
                    after.owner
                );
            } else if current != SpatialEngine::Ocean {
                // Classic <-> Scroll share durable workspace membership.
                assert_eq!(after, before, "{name} after {step}");
            }
        }
        if target != SpatialEngine::Ocean {
            // Workspace membership after any round trip, including through
            // Ocean, comes back exactly as it started.
            assert_eq!(now, initial, "membership after {step}");
        }
        current = target;
        checked += 1;
    }
    println!("engine transitions checked: {checked}");
}

const SCRATCHPAD_CONFIG: &str = r#"engine = classic
welcome_hint = false
water_effects = false
gaps = 8
animations {
 enabled = false
}
rule {
 app_id = f
 output = A
 float = true
 position = "100x100"
 size = "500x400"
}
"#;

#[test]
fn scratchpads_and_floating_stacking_survive_engine_changes() {
    if run_isolated(
        "scratchpads_and_floating_stacking_survive_engine_changes",
        SCRATCHPAD_CONFIG,
        "scratchpad transitions checked: 4",
    ) {
        return;
    }
    let mut f = two_output_fixture();
    let tile = map(&mut f, "tile");
    let scratch = map(&mut f, "scratch");
    f.state
        .focus_window(Some(scratch.clone()), SERIAL_COUNTER.next_serial());
    f.state
        .run_action(crate::config::Action::MoveToScratchpad(None));
    let notes = map(&mut f, "notes");
    f.state
        .focus_window(Some(notes.clone()), SERIAL_COUNTER.next_serial());
    f.state
        .run_action(crate::config::Action::MoveToScratchpad(Some(
            "notes".into(),
        )));
    // Six overlapping floaters; each new one maps on top.
    let floaters: Vec<WlSurface> = (0..6).map(|_| map(&mut f, "f")).collect();
    f.dispatch();
    let a = f.state.output_by_name("A").unwrap();
    let b = f.state.output_by_name("B").unwrap();
    let stack = |f: &mut Fixture| -> Vec<usize> {
        f.state.refresh_space();
        f.state
            .render_placements(&a)
            .unwrap()
            .iter()
            .filter_map(|p| floaters.iter().position(|s| Some(s) == p.surface()))
            .collect()
    };
    let classic_stack = stack(&mut f);
    assert_eq!(classic_stack, vec![5, 4, 3, 2, 1, 0]);
    let initial: Vec<Vec<Owner>> = [&tile, &scratch, &notes]
        .iter()
        .map(|s| owners(&f.state, s))
        .collect();
    let notes_ws = match initial[2][0] {
        Owner::Tiled { workspace, .. } => workspace,
        ref other => panic!("named scratchpad window not tiled: {other:?}"),
    };
    assert_eq!(
        initial,
        vec![
            vec![Owner::Tiled {
                output: "A".into(),
                workspace: 1
            }],
            vec![Owner::Tiled {
                output: "A".into(),
                workspace: 0
            }],
            vec![Owner::Tiled {
                output: "A".into(),
                workspace: notes_ws
            }],
        ]
    );

    let mut checked = 0;
    for target in [
        SpatialEngine::Ocean,
        SpatialEngine::Classic,
        SpatialEngine::Scroll,
        SpatialEngine::Classic,
    ] {
        assert!(f.state.migrate_spatial_engine(target));
        f.dispatch();
        assert_eq!(
            stack(&mut f),
            classic_stack,
            "floating stack after {target:?}"
        );
        let now: Vec<Vec<Owner>> = [&tile, &scratch, &notes]
            .iter()
            .map(|s| owners(&f.state, s))
            .collect();
        if target == SpatialEngine::Ocean {
            // Scratchpads stay parked in workspace storage, out of the world.
            assert_eq!(now[0], vec![Owner::OceanTiled]);
            assert_eq!(now[1..], initial[1..]);
            for output in [&a, &b] {
                let shown = f.state.render_placements(output).unwrap();
                assert!(!shown
                    .iter()
                    .any(|p| p.surface() == Some(&scratch) || p.surface() == Some(&notes)));
            }
            let reefs = f.state.spatial.ocean.reef_rects_on_output("A");
            assert_eq!(reefs.len(), 1, "only the real workspace becomes a reef");
        } else {
            assert_eq!(now, initial, "membership after {target:?}");
        }
        checked += 1;
    }
    println!("scratchpad transitions checked: {checked}");
}

#[test]
fn refused_engine_switch_keeps_the_request_and_retries_on_reload() {
    if run_isolated(
        "refused_engine_switch_keeps_the_request_and_retries_on_reload",
        "engine = classic\nwelcome_hint = false\nwater_effects = false\n",
        "refused switch retried",
    ) {
        return;
    }
    let mut event_loop: EventLoop<'static, Smallvil> = EventLoop::try_new().unwrap();
    let mut state = Smallvil::new(&mut event_loop, Display::new().unwrap());
    let config = std::path::PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap())
        .join("tidewm/config.wave");
    std::fs::write(
        &config,
        "engine = ocean\nwelcome_hint = false\nwater_effects = false\n",
    )
    .unwrap();
    // No output is mapped, so the migration is refused.
    state.reload_config();
    assert_eq!(state.spatial.engine, SpatialEngine::Classic);
    assert_eq!(state.config.spatial_engine, SpatialEngine::Ocean);
    let output = Output::new(
        "A".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
            serial_number: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1200, 900).into(),
            refresh: 60_000,
        }),
        None,
        Some(Scale::Integer(1)),
        None,
    );
    state.space.map_output(&output, (0, 0));
    // Same file: the request still differs from what runs, so it retries.
    state.reload_config();
    assert_eq!(state.spatial.engine, SpatialEngine::Ocean);
    assert_eq!(state.config.spatial_engine, SpatialEngine::Ocean);
    println!("refused switch retried");
}
