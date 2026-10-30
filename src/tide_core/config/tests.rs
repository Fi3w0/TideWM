use super::*;

#[test]
fn scroll_config_and_engine_actions_are_explicit_and_finite() {
    let mut raw = RawConfig {
        spatial_engine: "scroll".into(),
        ..Default::default()
    };
    apply_scroll_block(
        &mut raw.scroll,
        &[
            waves::Entry::Assign("workspaces".into(), "sections".into()),
            waves::Entry::Assign("orientation".into(), "vertical".into()),
            waves::Entry::Assign("tile_proportion".into(), "0.6".into()),
            waves::Entry::Assign("gesture_distance".into(), "900".into()),
        ],
    );
    apply_scroll_block(
        &mut raw.scroll,
        &[
            waves::Entry::Assign("tile_proportion".into(), "NaN".into()),
            waves::Entry::Assign("gesture_distance".into(), "0".into()),
        ],
    );
    let (config, _) = Config::from_raw(raw);
    assert_eq!(config.spatial_engine, SpatialEngine::Scroll);
    assert_eq!(
        config.scroll.workspaces,
        crate::scroll::WorkspaceMode::Sections
    );
    assert_eq!(
        config.scroll.orientation,
        crate::scroll::Orientation::Vertical
    );
    assert_eq!(config.scroll.tile_proportion, 0.6);
    let center_entries = wave_entries(
            "scroll {\n center_focused = on-overflow\n always_center_single = true\n gesture_invert = true\n preset_widths = [0.75, 0.25, 0.5]\n }\n",
        );
    let centered = Config::from_raw(lower_entries(&center_entries)).0;
    assert_eq!(
        centered.scroll.center_focused,
        crate::scroll::CenterMode::OnOverflow
    );
    assert!(centered.scroll.always_center_single);
    assert!(centered.scroll.gesture_invert);
    assert!(!config.scroll.gesture_invert);
    assert_eq!(centered.scroll.preset_widths.as_slice(), &[0.25, 0.5, 0.75]);
    let center_entries =
        wave_entries("scroll {\n center_focused = true\n preset_widths = [0.05]\n }\n");
    let centered = Config::from_raw(lower_entries(&center_entries)).0;
    assert_eq!(
        centered.scroll.center_focused,
        crate::scroll::CenterMode::Always
    );
    assert_eq!(
        centered.scroll.preset_widths.as_slice().len(),
        3,
        "invalid presets keep the defaults"
    );
    assert_eq!(config.scroll.gesture_distance, 900.0);
    let entries = wave_entries("engine = scroll\nscroll {\n workspaces = sections\n orientation = vertical\n tile_proportion = 0.6\n}\n");
    let (parsed, warnings) = Config::from_raw(lower_entries(&entries));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(parsed.spatial_engine, SpatialEngine::Scroll);
    assert_eq!(
        parsed.scroll.workspaces,
        crate::scroll::WorkspaceMode::Sections
    );
    assert!(matches!(
        parse_action("engine:scroll"),
        Some(Action::SetEngine(SpatialEngine::Scroll))
    ));
    assert!(parse_action("engine:invalid").is_none());
    assert!(matches!(
        parse_action("scroll-left"),
        Some(Action::ScrollView(Direction::Left))
    ));
}

/// Shorthand for the common `resolve_window_rules` test call: only
/// `app_id` set, everything else defaulted. Keeps call sites short
/// enough for rustfmt to lay out on one line.
fn facts_for(app_id: &str) -> WindowMatchFacts<'_> {
    WindowMatchFacts {
        app_id: Some(app_id),
        ..Default::default()
    }
}

#[test]
fn switch_events_parse_valid_actions_and_drop_invalid_ones() {
    // Same string syntax binds use, including the colon-prefixed
    // forms. An unrecognized action string matches the keybind path:
    // logged as a warning, dropped to `None`, never panics the whole
    // config -- a typo in one switch event must not silently disable
    // the other three.
    let parsed = SwitchEventsConfig::from_raw(SwitchEventsRaw {
        lid_close: Some("spawn:systemctl suspend".to_string()),
        lid_open: Some("close-window".to_string()),
        tablet_mode_on: Some("workspace:2".to_string()),
        tablet_mode_off: Some("not-a-real-action".to_string()),
    });

    assert!(matches!(parsed.lid_close, Some(Action::Spawn(_))));
    assert!(matches!(parsed.lid_open, Some(Action::CloseWindow)));
    assert!(matches!(
        parsed.tablet_mode_on,
        Some(Action::SwitchWorkspace(WorkspaceRef::Number(2)))
    ));
    assert!(parsed.tablet_mode_off.is_none());
}

#[test]
fn workspace_step_parses_short_and_niri_names() {
    assert!(matches!(
        parse_action("workspace-down"),
        Some(Action::WorkspaceStep(1))
    ));
    assert!(matches!(
        parse_action("workspace-up"),
        Some(Action::WorkspaceStep(-1))
    ));
    assert!(matches!(
        parse_action("focus-workspace-down"),
        Some(Action::WorkspaceStep(1))
    ));
    assert!(matches!(
        parse_action("focus-workspace-up"),
        Some(Action::WorkspaceStep(-1))
    ));
}

#[test]
fn scroll_workspace_scroll_parses_and_keeps_defaults_otherwise() {
    let mut cfg = crate::scroll::ScrollConfig::default();
    apply_scroll_block(
        &mut cfg,
        &wave_entries("workspace_scroll = true\nworkspace_scroll_duration = 300ms"),
    );
    assert!(cfg.workspace_scroll);
    assert_eq!(cfg.workspace_scroll_duration_ms, 300);
    assert_eq!(
        crate::scroll::ScrollConfig::default().workspace_scroll_duration_ms,
        220
    );
    assert!(crate::scroll::ScrollConfig::default().workspace_scroll);
    assert_eq!(
        crate::scroll::ScrollConfig::default().stack,
        crate::scroll::StackMode::Hybrid
    );
    assert_eq!(crate::scroll::ScrollConfig::default().stack_height, 0.5);
    assert_eq!(
        crate::scroll::ScrollConfig::default().stack_min_height,
        0.33
    );
}

#[test]
fn scroll_stack_modes_and_heights_parse() {
    let mut cfg = crate::scroll::ScrollConfig::default();
    apply_scroll_block(
        &mut cfg,
        &wave_entries("stack = scroll\nstack_height = 0.6\nstack_min_height = 0.4"),
    );
    assert_eq!(cfg.stack, crate::scroll::StackMode::Scroll);
    assert_eq!(cfg.stack_height, 0.6);
    assert_eq!(cfg.stack_min_height, 0.4);
    apply_scroll_block(&mut cfg, &wave_entries("stack = fit"));
    assert_eq!(cfg.stack, crate::scroll::StackMode::Fit);
}

#[test]
fn workspace_on_parses_output_and_ref() {
    assert!(matches!(
        parse_action("workspace-on:DP-4:3"),
        Some(Action::SwitchWorkspaceOn(ref o, WorkspaceRef::Number(3))) if o == "DP-4"
    ));
    assert!(matches!(
        parse_action("workspace-on:HDMI-A-2:web"),
        Some(Action::SwitchWorkspaceOn(ref o, WorkspaceRef::Name(ref n))) if o == "HDMI-A-2" && n == "web"
    ));
    assert!(parse_action("workspace-on:DP-4").is_none());
    assert!(parse_action("workspace-on::3").is_none());
    assert!(parse_action("workspace-on:DP-4:").is_none());
}

#[test]
fn switch_layout_parses_next_prev_index_and_names() {
    assert!(matches!(
        parse_action("switch-layout"),
        Some(Action::SwitchLayout(LayoutSwitch::Next))
    ));
    assert!(matches!(
        parse_action("switch-layout:next"),
        Some(Action::SwitchLayout(LayoutSwitch::Next))
    ));
    assert!(matches!(
        parse_action("switch-layout:prev"),
        Some(Action::SwitchLayout(LayoutSwitch::Previous))
    ));
    assert!(matches!(
        parse_action("switch-layout:previous"),
        Some(Action::SwitchLayout(LayoutSwitch::Previous))
    ));
    assert!(matches!(
        parse_action("switch-layout:2"),
        Some(Action::SwitchLayout(LayoutSwitch::Index(2)))
    ));
    assert!(matches!(
        parse_action("switch-layout:us"),
        Some(Action::SwitchLayout(LayoutSwitch::Name(ref code))) if code == "us"
    ));
    assert!(matches!(
        parse_action("switch-layout:Russian"),
        Some(Action::SwitchLayout(LayoutSwitch::Name(ref name))) if name == "Russian"
    ));
    assert!(parse_action("switch-layout:").is_none());
}

#[test]
fn scratchpad_actions_parse_bare_and_named_forms() {
    // Bare form stays the classic unnamed scratchpad; a `:name` suffix
    // selects a named one; a bare trailing colon degrades to unnamed
    // rather than creating a scratchpad named "".
    assert!(matches!(
        parse_action("toggle-scratchpad"),
        Some(Action::ToggleScratchpad(None))
    ));
    assert!(matches!(
        parse_action("toggle-scratchpad:music"),
        Some(Action::ToggleScratchpad(Some(ref n))) if n == "music"
    ));
    assert!(matches!(
        parse_action("move-to-scratchpad:music"),
        Some(Action::MoveToScratchpad(Some(ref n))) if n == "music"
    ));
    assert!(matches!(
        parse_action("toggle-scratchpad:"),
        Some(Action::ToggleScratchpad(None))
    ));
}

#[test]
fn tag_actions_parse_and_require_a_name() {
    assert!(matches!(
        parse_action("tag-window:work"),
        Some(Action::TagWindow(ref n)) if n == "work"
    ));
    assert!(matches!(
        parse_action("untag-window:work"),
        Some(Action::UntagWindow(ref n)) if n == "work"
    ));
    assert!(matches!(
        parse_action("toggle-tag:work"),
        Some(Action::ToggleTag(ref n)) if n == "work"
    ));
    // Unlike scratchpad's bare/named dual form, there's no sensible
    // default tag to fall back to -- a missing name drops the bind
    // entirely, same as any other malformed action string.
    assert!(parse_action("tag-window:").is_none());
    assert!(parse_action("untag-window:").is_none());
    assert!(parse_action("toggle-tag:").is_none());
}

#[test]
fn toggle_output_pin_action_parses() {
    assert!(matches!(
        parse_action("toggle-output-pin"),
        Some(Action::ToggleOutputPin)
    ));
}

#[test]
fn move_floating_actions_parse_all_four_directions() {
    assert!(matches!(
        parse_action("move-left"),
        Some(Action::MoveFloating(Direction::Left))
    ));
    assert!(matches!(
        parse_action("move-right"),
        Some(Action::MoveFloating(Direction::Right))
    ));
    assert!(matches!(
        parse_action("move-up"),
        Some(Action::MoveFloating(Direction::Up))
    ));
    assert!(matches!(
        parse_action("move-down"),
        Some(Action::MoveFloating(Direction::Down))
    ));
}

#[test]
fn layout_floating_parses_as_a_keybind_and_a_default() {
    assert!(matches!(
        parse_action("layout:floating"),
        Some(Action::SetLayout(LayoutAlgorithm::Floating))
    ));
    assert_eq!(
        parse_layout_algorithm("floating"),
        Some(LayoutAlgorithm::Floating)
    );
}

#[test]
fn rule_output_aliases_remain_spawn_only_and_pin_output_remains_literal() {
    for alias in ["focused", "camera", "current"] {
        let rule = lower_window_rule_block(&[
            waves::Entry::Assign("output".into(), alias.into()),
            waves::Entry::Assign("pin_output".into(), "DP-1".into()),
        ]);
        assert_eq!(rule.output.as_deref(), Some(alias));
        assert_eq!(rule.pin_output.as_deref(), Some("DP-1"));
    }
}

#[test]
fn window_rule_pin_output_parses_and_resolves_last_match_wins() {
    let rule = lower_window_rule_block(&[waves::Entry::Assign("pin_output".into(), "DP-1".into())]);
    assert_eq!(rule.pin_output.as_deref(), Some("DP-1"));

    let entries = wave_entries(
        "rule {\n\
             app_id = grafana\n\
             pin_output = \"DP-1\"\n\
             }\n\
             rule {\n\
             app_id = grafana\n\
             pin_output = \"HDMI-A-1\"\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let resolved = config.resolve_window_rules(facts_for("grafana"));
    assert_eq!(resolved.pin_output.as_deref(), Some("HDMI-A-1"));
}

#[test]
fn parse_workspace_gaps_resolves_names_and_skips_malformed_entries() {
    let mut names = HashMap::new();
    names.insert("web".to_string(), 3);
    let gaps = parse_workspace_gaps(
        &[
            "1 0".to_string(),
            "web 16".to_string(), // name resolves via workspace_name
            "nope 4".to_string(), // unknown name, skipped
            "2".to_string(),      // no pixel value, skipped
            "2 lots".to_string(), // bad pixel value, skipped
        ],
        &names,
    );

    assert_eq!(gaps.len(), 2);
    assert_eq!(gaps.get(&1), Some(&0));
    assert_eq!(gaps.get(&3), Some(&16));
}

#[test]
fn builtin_wallpaper_defaults_on_and_parses_toggle() {
    // Omitted key keeps the identity backdrop on.
    let config = Config::from_raw(lower_entries(&wave_entries(""))).0;
    assert!(config.builtin_wallpaper);

    // Explicit off lowers through.
    let config = Config::from_raw(lower_entries(&wave_entries("builtin_wallpaper = false\n"))).0;
    assert!(!config.builtin_wallpaper);

    // Explicit on is honored too.
    let config = Config::from_raw(lower_entries(&wave_entries("builtin_wallpaper = true\n"))).0;
    assert!(config.builtin_wallpaper);
}

#[test]
fn parse_workspace_names_skips_malformed_entries_and_last_duplicate_wins() {
    let names = parse_workspace_names(&[
        "3 web".to_string(),
        "4 chat".to_string(),
        "nope".to_string(),           // no name, skipped
        "  ".to_string(),             // no name, skipped
        "notanumber web".to_string(), // bad number, skipped
        "5 web".to_string(),          // duplicate name, last one wins
    ]);

    assert_eq!(names.len(), 2);
    assert_eq!(names.get("web"), Some(&5));
    assert_eq!(names.get("chat"), Some(&4));
}

#[test]
fn switch_events_default_to_all_unset() {
    // No entries -> no actions, switch events become a pure no-op.
    // This is what ships out of the box and what an empty
    // `switch_events { }` block in an existing config produces.
    let parsed = SwitchEventsConfig::from_raw(SwitchEventsRaw::default());

    assert!(parsed.lid_close.is_none());
    assert!(parsed.lid_open.is_none());
    assert!(parsed.tablet_mode_on.is_none());
    assert!(parsed.tablet_mode_off.is_none());
}

#[test]
fn gpu_block_parses_render_and_exclusions() {
    let raw = lower_entries(&wave_entries(
        "gpu {\n\
                 render = vendor:amd\n\
                 exclude = [vendor:nvidia, \"/dev/dri/card3\"]\n\
             }\n",
    ));
    let (config, warnings) = Config::from_raw(raw);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(config.gpu.render, GpuSelector::Vendor(GpuVendor::Amd));
    assert_eq!(
        config.gpu.exclude,
        vec![
            GpuSelector::Vendor(GpuVendor::Nvidia),
            GpuSelector::Path(PathBuf::from("/dev/dri/card3")),
        ]
    );
}

#[test]
fn invalid_gpu_values_degrade_safely() {
    let raw = lower_entries(&wave_entries(
        "gpu {\n\
                 render = definitely-not-a-gpu\n\
                 exclude = [auto, vendor:bogus, \"/dev/dri/card2\"]\n\
             }\n",
    ));
    let (config, warnings) = Config::from_raw(raw);
    assert_eq!(config.gpu.render, GpuSelector::Auto);
    assert_eq!(
        config.gpu.exclude,
        vec![GpuSelector::Path(PathBuf::from("/dev/dri/card2"))]
    );
    assert_eq!(warnings.len(), 3);
}

#[test]
fn window_rule_matches_app_id_exactly_and_title_by_substring() {
    let by_app_id = WindowRule {
        app_id: Some("firefox".to_string()),
        ..Default::default()
    };
    assert!(by_app_id.matches(Some("firefox"), None, None, false, false));
    assert!(!by_app_id.matches(Some("firefox-nightly"), None, None, false, false));
    assert!(!by_app_id.matches(None, Some("Firefox"), None, false, false));

    let by_title = WindowRule {
        title: Some("Picture-in-Picture".to_string()),
        ..Default::default()
    };
    assert!(by_title.matches(None, Some("Video - picture-in-picture"), None, false, false));
    assert!(!by_title.matches(None, Some("Video"), None, false, false));
    assert!(!by_title.matches(None, None, None, false, false));

    let both = WindowRule {
        app_id: Some("firefox".to_string()),
        title: Some("pip".to_string()),
        ..Default::default()
    };
    assert!(!both.matches(Some("firefox"), Some("normal tab"), None, false, false));
    assert!(both.matches(Some("firefox"), Some("Video - PIP"), None, false, false));
}

#[test]
fn window_rule_with_no_criteria_never_matches() {
    // A blank `rule { }` block (or one that's only actions, no
    // app_id/title) is far more likely to be a config mistake than an
    // intentional "match every window" -- never silently apply it.
    let blank = WindowRule {
        float: true,
        ..Default::default()
    };
    assert!(!blank.matches(Some("anything"), Some("anything"), None, false, false));
    assert!(!blank.matches(None, None, None, false, false));
}

#[test]
fn window_rule_regexes_compile_once_and_invalid_patterns_are_ignored() {
    let rule = WindowRule {
        app_id_regex: Some(regex::Regex::new(r"^(org\.)?mozilla\.firefox$").unwrap()),
        title_regex: Some(regex::Regex::new("(?i)private browsing").unwrap()),
        ..Default::default()
    };
    assert!(rule.matches(
        Some("org.mozilla.firefox"),
        Some("Private Browsing"),
        None,
        false,
        false
    ));
    assert!(!rule.matches(Some("kitty"), Some("Private Browsing"), None, false, false));

    let invalid =
        lower_window_rule_block(&[waves::Entry::Assign("app_id_regex".into(), "[".into())]);
    assert!(invalid.app_id_regex.is_none());
    assert!(!invalid.matches(Some("anything"), None, None, false, false));
}

#[test]
fn window_rule_matches_by_pid_and_xwayland_alone() {
    // Neither field needs app_id/title alongside it -- `rule { pid =
    // ... }` and `rule { xwayland = ... }` are each a complete,
    // standalone identifying criterion, same as `app_id`/`title`.
    let by_pid = lower_window_rule_block(&[waves::Entry::Assign("pid".into(), "1234".into())]);
    assert!(by_pid.matches(None, None, Some(1234), false, false));
    assert!(!by_pid.matches(None, None, Some(5678), false, false));
    assert!(!by_pid.matches(None, None, None, false, false));

    let xwayland_only =
        lower_window_rule_block(&[waves::Entry::Assign("xwayland".into(), "true".into())]);
    assert!(xwayland_only.matches(None, None, None, true, false));
    assert!(!xwayland_only.matches(None, None, None, false, false));

    let native_only =
        lower_window_rule_block(&[waves::Entry::Assign("xwayland".into(), "false".into())]);
    assert!(native_only.matches(None, None, None, false, false));
    assert!(!native_only.matches(None, None, None, true, false));

    // Combines with app_id like any other criterion (AND, not OR).
    let combined = WindowRule {
        app_id: Some("firefox".to_string()),
        pid: Some(42),
        ..Default::default()
    };
    assert!(combined.matches(Some("firefox"), None, Some(42), false, false));
    assert!(!combined.matches(Some("firefox"), None, Some(99), false, false));
    assert!(!combined.matches(Some("chromium"), None, Some(42), false, false));
}

#[test]
fn window_rule_matches_by_urgent_alone_and_re_resolves_live() {
    // `rule { urgent = true }` is a complete, standalone identifying
    // criterion, same as `pid`/`xwayland` above -- and unlike those,
    // it's meant to flip after map time (see `Smallvil::mark_urgent`
    // and `reconcile_keyboard_focus`), so this only tests the pure
    // matching logic itself, not the live re-apply plumbing.
    let urgent_only =
        lower_window_rule_block(&[waves::Entry::Assign("urgent".into(), "true".into())]);
    assert!(urgent_only.matches(None, None, None, false, true));
    assert!(!urgent_only.matches(None, None, None, false, false));

    let calm_only =
        lower_window_rule_block(&[waves::Entry::Assign("urgent".into(), "false".into())]);
    assert!(calm_only.matches(None, None, None, false, false));
    assert!(!calm_only.matches(None, None, None, false, true));

    // Combines with app_id like any other criterion (AND, not OR).
    let combined = WindowRule {
        app_id: Some("firefox".to_string()),
        urgent: Some(true),
        ..Default::default()
    };
    assert!(combined.matches(Some("firefox"), None, None, false, true));
    assert!(!combined.matches(Some("firefox"), None, None, false, false));
    assert!(!combined.matches(Some("chromium"), None, None, false, true));
}

#[test]
fn window_rule_matches_initial_class_and_title_spawn_time_only() {
    // `matches` (the live-fact function) never sees these criteria --
    // a blank rule using only initial_class must still refuse to match
    // nothing, which is why the blank-rule guard lives in `matches` and
    // reads `self` directly rather than taking these as parameters.
    let by_initial_class =
        lower_window_rule_block(&[waves::Entry::Assign("initial_class".into(), "kitty".into())]);
    assert!(!by_initial_class.matches(Some("kitty"), None, None, false, false));
    assert!(by_initial_class.matches_initial(Some("kitty"), None, false));
    assert!(!by_initial_class.matches_initial(Some("alacritty"), None, false));
    assert!(!by_initial_class.matches_initial(None, None, false));

    let by_initial_title =
        lower_window_rule_block(&[waves::Entry::Assign("initial_title".into(), "pip".into())]);
    assert!(by_initial_title.matches_initial(None, Some("Video - PIP"), false));
    assert!(!by_initial_title.matches_initial(None, Some("normal tab"), false));
}

#[test]
fn window_rule_matches_at_startup_alone() {
    let startup_only =
        lower_window_rule_block(&[waves::Entry::Assign("at_startup".into(), "true".into())]);
    assert!(startup_only.matches_initial(None, None, true));
    assert!(!startup_only.matches_initial(None, None, false));

    let not_startup =
        lower_window_rule_block(&[waves::Entry::Assign("at_startup".into(), "false".into())]);
    assert!(not_startup.matches_initial(None, None, false));
    assert!(!not_startup.matches_initial(None, None, true));
}

#[test]
fn window_rule_tag_field_parses_bracket_list_and_bare_string() {
    let bracket_list = lower_window_rule_block(&[waves::Entry::Assign(
        "tag".into(),
        "[\"work\", \"scratch\"]".into(),
    )]);
    assert_eq!(
        bracket_list.tag,
        vec!["work".to_string(), "scratch".to_string()]
    );

    // A single bare tag (no bracket list) works too, same dual form
    // the top-level `spawn` key supports.
    let bare = lower_window_rule_block(&[waves::Entry::Assign("tag".into(), "work".into())]);
    assert_eq!(bare.tag, vec!["work".to_string()]);

    let empty = lower_window_rule_block(&[
        waves::Entry::Assign("tag".into(), "[]".into()),
        waves::Entry::Assign("add_tag".into(), "[]".into()),
    ]);
    assert!(empty.tag.is_empty());
    assert!(empty.add_tag.is_empty());
}

#[test]
fn window_rule_matches_tags_live_current_set() {
    // `tag` is checked against the window's CURRENT tag set, not a
    // spawn-time snapshot -- there's no `matches_initial`-style split
    // for it, so this goes through `matches_tags` directly.
    let by_tag = lower_window_rule_block(&[waves::Entry::Assign(
        "tag".into(),
        "[\"work\", \"scratch\"]".into(),
    )]);
    assert!(!by_tag.matches_tags(None));

    let mut work_tag = HashSet::new();
    work_tag.insert("work".to_string());
    assert!(by_tag.matches_tags(Some(&work_tag)));

    let mut unrelated_tag = HashSet::new();
    unrelated_tag.insert("unrelated".to_string());
    assert!(!by_tag.matches_tags(Some(&unrelated_tag)));

    // A rule with no `tag` criterion never matches through this path,
    // mirroring `matches`'s own blank-rule guard.
    let blank = WindowRule::default();
    assert!(!blank.matches_tags(Some(&work_tag)));
}

#[test]
fn window_rule_on_workspace_parses_and_matches_live_current_workspace() {
    let rule = lower_window_rule_block(&[waves::Entry::Assign("on_workspace".into(), "3".into())]);
    assert_eq!(rule.on_workspace, Some(3));
    assert!(rule.matches_on_workspace(Some(3)));
    // A different workspace, an untracked window (Ocean, or unmapped),
    // and a rule with no `on_workspace` criterion never match.
    assert!(!rule.matches_on_workspace(Some(4)));
    assert!(!rule.matches_on_workspace(None));
    assert!(!WindowRule::default().matches_on_workspace(Some(3)));

    let malformed = lower_window_rule_block(&[waves::Entry::Assign(
        "on_workspace".into(),
        "not-a-number".into(),
    )]);
    assert!(malformed.on_workspace.is_none());
}

#[test]
fn adaptive_sync_parses_global_and_per_output_with_aliases() {
    assert_eq!(parse_adaptive_sync("off"), Some(AdaptiveSync::Off));
    assert_eq!(parse_adaptive_sync("false"), Some(AdaptiveSync::Off));
    assert_eq!(parse_adaptive_sync("on"), Some(AdaptiveSync::On));
    assert_eq!(parse_adaptive_sync("true"), Some(AdaptiveSync::On));
    assert_eq!(
        parse_adaptive_sync("on-demand"),
        Some(AdaptiveSync::OnDemand)
    );
    assert_eq!(
        parse_adaptive_sync("on_demand"),
        Some(AdaptiveSync::OnDemand)
    );
    assert_eq!(parse_adaptive_sync("nonsense"), None);

    // Global key and its aliases all land in the same raw field.
    let mut raw = RawConfig::default();
    apply_top_level_assign(&mut raw, "adaptive_sync", "on");
    assert_eq!(raw.adaptive_sync, "on");
    apply_top_level_assign(&mut raw, "vrr", "on-demand");
    assert_eq!(raw.adaptive_sync, "on-demand");
    apply_top_level_assign(&mut raw, "variable_refresh_rate", "off");
    assert_eq!(raw.adaptive_sync, "off");

    // Per-output override, plus its own aliases, plus a malformed
    // value warning and leaving the field unset rather than guessing.
    let output = lower_output_block(
        "DP-1",
        &[waves::Entry::Assign("adaptive_sync".into(), "on".into())],
    );
    assert_eq!(output.adaptive_sync, Some(AdaptiveSync::On));
    let output = lower_output_block(
        "DP-1",
        &[waves::Entry::Assign("vrr".into(), "on-demand".into())],
    );
    assert_eq!(output.adaptive_sync, Some(AdaptiveSync::OnDemand));
    let malformed = lower_output_block(
        "DP-1",
        &[waves::Entry::Assign(
            "adaptive_sync".into(),
            "not-a-mode".into(),
        )],
    );
    assert!(malformed.adaptive_sync.is_none());
}

#[test]
fn shared_numeric_lowering_rejects_non_finite_values() {
    let mut required = 0.75;
    set_f64(&mut required, "scale", "NaN");
    assert_eq!(required, 0.75);
    set_f64(&mut required, "scale", "inf");
    assert_eq!(required, 0.75);
    set_f64(&mut required, "scale", "1.25");
    assert_eq!(required, 1.25);

    let output = lower_output_block(
        "DP-1",
        &[waves::Entry::Assign("scale".into(), "NaN".into())],
    );
    assert_eq!(output.scale, OutputConfig::default().scale);
}

#[test]
fn hardware_numeric_config_rejects_invalid_ranges_without_guessing() {
    assert_eq!(
        parse_mode_str("2560x1440@144"),
        Some((2560, 1440, Some(144.0)))
    );
    for mode in ["0x1080@60", "1920x-1@60", "1920x1080@0", "1920x1080@NaN"] {
        assert!(parse_mode_str(mode).is_none(), "{mode}");
    }

    let output = lower_output_block(
        "any-connector",
        &[waves::Entry::Assign("scale".into(), "-1".into())],
    );
    assert_eq!(output.scale, OutputConfig::default().scale);

    let mut touchpad = TouchpadConfig::default();
    apply_touchpad_block(
        &mut touchpad,
        &[
            waves::Entry::Assign("accel_speed".into(), "2".into()),
            waves::Entry::Assign("workspace_swipe_distance".into(), "0".into()),
        ],
    );
    assert!(touchpad.accel_speed.is_none());
    assert!(touchpad.workspace_swipe_distance.is_none());
}

#[test]
fn input_device_blocks_parse_exact_names_output_and_calibration() {
    let raw = lower_entries(&wave_entries(
        "input {\n\
               device \"ELAN Touchscreen\" {\n\
                 map_to_output = eDP-1\n\
                 calibration_matrix = [0, -1, 1, 1, 0, 0]\n\
               }\n\
             }",
    ));

    let elan = raw.input.resolve_device("ELAN Touchscreen");
    assert_eq!(elan.map_to_output.as_deref(), Some("eDP-1"));
    assert_eq!(
        elan.calibration_matrix,
        Some([0.0, -1.0, 1.0, 1.0, 0.0, 0.0])
    );
    let differently_cased = raw.input.resolve_device("elan touchscreen");
    assert!(differently_cased.map_to_output.is_none());
    assert!(differently_cased.calibration_matrix.is_none());
}

#[test]
fn input_device_blocks_fold_fields_and_reject_malformed_matrices() {
    let mut input = InputConfig::default();
    apply_input_block(
        &mut input,
        &[
            waves::Entry::Block(
                "device".into(),
                "Wacom Pen".into(),
                vec![
                    waves::Entry::Assign("map_to_output".into(), "DP-1".into()),
                    waves::Entry::Assign("calibration_matrix".into(), "[1, 0, 0, 0, 1, 0]".into()),
                ],
            ),
            waves::Entry::Block(
                "device".into(),
                "Wacom Pen".into(),
                vec![waves::Entry::Assign(
                    "map_to_output".into(),
                    "HDMI-A-1".into(),
                )],
            ),
            waves::Entry::Block(
                "device".into(),
                "Broken".into(),
                vec![waves::Entry::Assign(
                    "calibration_matrix".into(),
                    "[1, 0, NaN, 0, 1, 0]".into(),
                )],
            ),
        ],
    );

    let wacom = input.resolve_device("Wacom Pen");
    assert_eq!(wacom.map_to_output.as_deref(), Some("HDMI-A-1"));
    assert_eq!(
        wacom.calibration_matrix,
        Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0])
    );
    assert!(input.resolve_device("Broken").calibration_matrix.is_none());
}

#[test]
fn workspace_rule_lowering_covers_every_key() {
    let rule = lower_workspace_rule_block(&[
        waves::Entry::Assign("workspace".into(), "5".into()),
        waves::Entry::Assign("layout".into(), "cascade".into()),
        waves::Entry::Assign("on_created_empty".into(), "kitty".into()),
        waves::Entry::Assign("border".into(), "true".into()),
        waves::Entry::Assign("rounding".into(), "false".into()),
        waves::Entry::Assign("shadow".into(), "true".into()),
    ]);
    assert_eq!(rule.workspace, Some(5));
    assert_eq!(rule.layout, Some(LayoutAlgorithm::Cascade));
    assert_eq!(rule.on_created_empty.as_deref(), Some("kitty"));
    assert_eq!(rule.border.unwrap().enabled, Some(true));
    assert_eq!(rule.rounding.unwrap().enabled, Some(false));
    assert_eq!(rule.shadow.unwrap().enabled, Some(true));

    // Malformed workspace/layout values warn and leave the field unset
    // rather than panicking or defaulting to something plausible-looking.
    let malformed = lower_workspace_rule_block(&[
        waves::Entry::Assign("workspace".into(), "not-a-number".into()),
        waves::Entry::Assign("layout".into(), "not-a-layout".into()),
    ]);
    assert!(malformed.workspace.is_none());
    assert!(malformed.layout.is_none());
}

#[test]
fn resolve_workspace_rule_folds_last_match_wins_and_ignores_other_workspaces() {
    let mut config = parse_default_config();
    config.workspace_rules = vec![
        WorkspaceRule {
            workspace: Some(3),
            layout: Some(LayoutAlgorithm::Master),
            on_created_empty: Some("first".to_string()),
            ..Default::default()
        },
        WorkspaceRule {
            workspace: Some(3),
            on_created_empty: Some("second".to_string()),
            ..Default::default()
        },
        WorkspaceRule {
            workspace: Some(9),
            layout: Some(LayoutAlgorithm::Cascade),
            ..Default::default()
        },
    ];
    let resolved = config.resolve_workspace_rule(3);
    // Later match's on_created_empty wins; layout survives from the
    // earlier match since the later one didn't set it.
    assert_eq!(resolved.layout, Some(LayoutAlgorithm::Master));
    assert_eq!(resolved.on_created_empty.as_deref(), Some("second"));

    // An unrelated workspace_rule for a different number never leaks in.
    let unmatched = config.resolve_workspace_rule(4);
    assert!(unmatched.layout.is_none());
    assert!(unmatched.on_created_empty.is_none());

    assert_eq!(
        config.workspace_algorithm_overrides(),
        HashMap::from([(3, LayoutAlgorithm::Master), (9, LayoutAlgorithm::Cascade)])
    );
}

#[test]
fn window_rule_opacity_is_clamped_and_gesture_actions_parse() {
    let low = lower_window_rule_block(&[
        waves::Entry::Assign("app_id".into(), "dimmed".into()),
        waves::Entry::Assign("opacity".into(), "-0.5".into()),
    ]);
    let high = lower_window_rule_block(&[
        waves::Entry::Assign("app_id".into(), "bright".into()),
        waves::Entry::Assign("opacity".into(), "1.5".into()),
    ]);
    assert_eq!(low.opacity, Some(0.0));
    assert_eq!(high.opacity, Some(1.0));

    let mut touchpad = TouchpadConfig::default();
    apply_touchpad_block(
        &mut touchpad,
        &[
            waves::Entry::Assign("gesture_swipe_fingers".into(), "3".into()),
            waves::Entry::Assign("swipe_left".into(), "toggle-overview".into()),
            waves::Entry::Assign("gesture_pinch_fingers".into(), "4".into()),
            waves::Entry::Assign("pinch_out".into(), "close-window".into()),
            waves::Entry::Assign("modifier_pan_fingers".into(), "2".into()),
        ],
    );
    assert_eq!(touchpad.gesture_swipe_fingers, Some(3));
    assert!(matches!(touchpad.swipe_left, Some(Action::ToggleOverview)));
    assert_eq!(touchpad.gesture_pinch_fingers, Some(4));
    assert!(matches!(touchpad.pinch_out, Some(Action::CloseWindow)));
    assert_eq!(touchpad.modifier_pan_fingers, Some(2));

    apply_touchpad_block(
        &mut touchpad,
        &[waves::Entry::Assign(
            "modifier_pan_fingers".into(),
            "1".into(),
        )],
    );
    assert_eq!(
        touchpad.modifier_pan_fingers,
        Some(2),
        "below the 2-finger minimum is ignored, not silently accepted"
    );
}

#[test]
fn layer_blocks_capture_matches_by_namespace_substring_only_when_flagged() {
    let mut config = Config {
        layer_rules: Vec::new(),
        ..parse_default_config()
    };

    // No rule at all: never blocked.
    assert!(!config.layer_blocks_capture("waybar"));

    config.layer_rules.push(LayerRule {
        namespace: Some("bitwarden".to_string()),
        block_capture: true,
        ..Default::default()
    });
    assert!(config.layer_blocks_capture("bitwarden-quick-access"));
    assert!(!config.layer_blocks_capture("waybar"));

    // A rule with no namespace never matches, same "blank rule matches
    // nothing" precedent WindowRule sets.
    let blank = LayerRule {
        namespace: None,
        block_capture: true,
        ..Default::default()
    };
    assert!(!blank.matches("anything"));

    // A matching namespace but block_capture: false must not block --
    // the rule matching and the effect firing are separate things.
    config.layer_rules.push(LayerRule {
        namespace: Some("waybar".to_string()),
        block_capture: false,
        ..Default::default()
    });
    assert!(!config.layer_blocks_capture("waybar"));
}

#[test]
fn layer_rule_z_order_dim_around_above_lock_screen_and_blur_parse_and_resolve() {
    let z_order = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "waybar".into()),
        waves::Entry::Assign("z_order".into(), "-5".into()),
    ]);
    assert_eq!(z_order.z_order, Some(-5));

    let bad_z_order = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "waybar".into()),
        waves::Entry::Assign("z_order".into(), "not-a-number".into()),
    ]);
    assert_eq!(
        bad_z_order.z_order, None,
        "malformed z_order is ignored, not defaulted to 0"
    );

    let dim = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "rofi".into()),
        waves::Entry::Assign("dim_around".into(), "true".into()),
        waves::Entry::Assign("dim_amount".into(), "1.4".into()),
    ]);
    assert!(dim.dim_around);
    assert_eq!(
        dim.dim_amount,
        Some(1.0),
        "out-of-range dim_amount is clamped, not rejected"
    );

    let above_lock = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "osd".into()),
        waves::Entry::Assign("above_lock_screen".into(), "true".into()),
    ]);
    assert!(above_lock.above_lock_screen);

    let mut config = Config {
        layer_rules: Vec::new(),
        ..parse_default_config()
    };

    // z_order: last matching rule that actually sets it wins; a later
    // rule matching the same namespace for an unrelated effect must not
    // reset it back to the default.
    config.layer_rules.push(LayerRule {
        namespace: Some("waybar".to_string()),
        z_order: Some(10),
        ..Default::default()
    });
    config.layer_rules.push(LayerRule {
        namespace: Some("waybar".to_string()),
        block_capture: true,
        ..Default::default()
    });
    assert_eq!(config.layer_z_order("waybar"), Some(10));
    assert_eq!(config.layer_z_order("some-other-bar"), None);

    // dim_around: resolves to its own dim_amount, or the fixed default
    // when unset. No matching rule resolves to None, not 0.0.
    assert_eq!(config.layer_dim_around("rofi-launcher"), None);
    config.layer_rules.push(LayerRule {
        namespace: Some("rofi".to_string()),
        dim_around: true,
        ..Default::default()
    });
    assert_eq!(config.layer_dim_around("rofi-launcher"), Some(0.35));
    config.layer_rules.push(LayerRule {
        namespace: Some("rofi".to_string()),
        dim_around: true,
        dim_amount: Some(0.8),
        ..Default::default()
    });
    assert_eq!(config.layer_dim_around("rofi-launcher"), Some(0.8));

    // above_lock_screen: any() fold, same shape as block_capture.
    assert!(!config.layer_above_lock_screen("osd-volume"));
    config.layer_rules.push(LayerRule {
        namespace: Some("osd".to_string()),
        above_lock_screen: true,
        ..Default::default()
    });
    assert!(config.layer_above_lock_screen("osd-volume"));

    // blur: same any() fold, parses like every other bare bool key.
    let blur = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "waybar".into()),
        waves::Entry::Assign("blur".into(), "true".into()),
    ]);
    assert!(blur.blur);
    assert!(!config.layer_blur("waybar"));
    config.layer_rules.push(LayerRule {
        namespace: Some("waybar".to_string()),
        blur: true,
        ..Default::default()
    });
    assert!(config.layer_blur("waybar"));
    assert!(!config.layer_blur("some-other-bar"));

    let ignore_alpha = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "waybar".into()),
        waves::Entry::Assign("ignore_alpha".into(), "0.25".into()),
    ]);
    assert_eq!(ignore_alpha.ignore_alpha, Some(0.25));
    config.layer_rules.push(ignore_alpha);
    assert_eq!(config.layer_ignore_alpha("waybar"), Some(0.25));
    assert_eq!(config.layer_ignore_alpha("some-other-bar"), None);

    let clamped = lower_layer_rule_block(&[
        waves::Entry::Assign("namespace".into(), "waybar".into()),
        waves::Entry::Assign("ignore_alpha".into(), "2.0".into()),
    ]);
    assert_eq!(clamped.ignore_alpha, Some(1.0));
}

#[test]
fn resolve_window_rules_folds_last_scalar_wins_bools_accumulate() {
    let mut config = Config {
        loaded_entries: Vec::new(),
        source_paths: Vec::new(),
        terminal: String::new(),
        spatial_engine: SpatialEngine::Classic,
        ocean: OceanConfig::default(),
        scroll: crate::scroll::ScrollConfig::default(),
        ebb: EbbConfig::default(),
        pointer_modifier: Mods {
            logo: true,
            ..Default::default()
        },
        show_welcome_hint: false,
        show_config_reload_toast: true,
        water_effects: true,
        snap: SnapConfig::default(),
        builtin_wallpaper: true,
        viscosity: 1.0,
        backdrop_capture_scale: 1,
        vessels: VesselsConfig::default(),
        sway: SwayConfig::default(),
        float_physics: FloatPhysicsConfig::default(),
        currents: CurrentsConfig::default(),
        buoyancy: BuoyancyConfig::default(),
        swim: SwimConfig::default(),
        compass: CompassConfig::default(),
        minimap: MinimapConfig::default(),
        animations: WindowAnimationsConfig::default(),
        cascade: CascadeLiquidConfig::default(),
        workspace_transition: WorkspaceTransitionConfig::default(),
        depth: DepthConfig::default(),
        classic_depth: ClassicDepthConfig::default(),
        frost: FrostConfig::default(),
        water_glass: WaterGlassConfig::default(),
        shaders_enabled: false,
        shader_definitions: HashMap::new(),
        caustics: CausticsConfig::default(),
        shadow: ShadowConfig::default(),
        rounding: RoundingConfig::default(),
        border: BorderConfig::default(),
        popup: PopupConfig::default(),
        ripple: RippleConfig::system_default(),
        ripple_presets: HashMap::new(),
        cursor_always_visible: false,
        hardware_cursor: true,
        cursor_hide_after_ms: 0,
        workspace_auto_back_and_forth: false,
        workspace_count: None,
        workspace_names: HashMap::new(),
        workspace_gaps: HashMap::new(),
        gaps: 0,
        default_layout: LayoutAlgorithm::Bsp,
        adaptive_sync: AdaptiveSync::Off,
        master_orientation: MasterOrientation::Left,
        bsp_split_bias: SplitBias::Auto,
        pseudo_tile_scale: 0.7,
        keybinds: Vec::new(),
        rescue_keybinds: Vec::new(),
        input: InputConfig::default(),
        xwayland: XwaylandConfig::default(),
        spawn: Vec::new(),
        outputs: Vec::new(),
        gpu: GpuConfig::default(),
        switch_events: SwitchEventsConfig::default(),
        modes: HashMap::new(),
        env: HashMap::new(),
        window_rules: vec![
            WindowRule {
                app_id: Some("kitty".to_string()),
                workspace: Some(2),
                float: true,
                ..Default::default()
            },
            WindowRule {
                app_id: Some("kitty".to_string()),
                workspace: Some(5),
                pin: true,
                ..Default::default()
            },
            WindowRule {
                app_id: Some("firefox".to_string()),
                workspace: Some(9),
                ..Default::default()
            },
        ],
        layer_rules: Vec::new(),
        workspace_rules: Vec::new(),
    };
    // Only the two "kitty" rules should ever combine; the "firefox"
    // one must not leak in just because it's in the same list.
    let effective = config.resolve_window_rules(facts_for("kitty"));
    assert_eq!(effective.workspace, Some(5)); // later match overrides earlier
    assert!(effective.float); // set by the first match, not unset by the second
    assert!(effective.pin); // set by the second match

    config.window_rules.clear();
    let none_matched = config.resolve_window_rules(facts_for("kitty"));
    assert_eq!(none_matched.workspace, None);
    assert!(!none_matched.float);
}

#[test]
fn ocean_selector_reefs_bookmarks_and_actions_parse_without_resolution_defaults() {
    assert!(!OceanConfig::default().shared_canvas);
    let entries = wave_entries(
        "engine = ocean\n\
             ocean {\n\
                 shared_canvas = true\n\
                 camera_step = 720\n\
                 depth_enabled = false\n\
                 zoom_enabled = true\n\
                 modifier_zoom = false\n\
                 min_zoom = 0.4\n\
                 max_zoom = 2.5\n\
                 zoom_step = 1.3\n\
                 camera_animation_ms = 340\n\
                 camera_sway = 24\n\
                 canvas_guides = false\n\
                 canvas_grid_size = 320\n\
                 canvas_grid_alpha = 0.18\n\
                 canvas_marker = false\n\
                 canvas_marker_fade_ms = 5100\n\
                 reef main {\n\
                     x = -400\n\
                     y = 250\n\
                 }\n\
                 reef deep {\n\
                     x = 0\n\
                     y = 4000\n\
                     width = 3440\n\
                     height = 1440\n\
                 }\n\
                 bookmark code {\n\
                     x = 1234.5\n\
                     y = -80\n\
                 }\n\
             }\n\
             bind Super+Ctrl+H { ocean-pan-left }\n\
             bind Super+Ctrl+I { ocean-zoom-in }\n\
             bind Super+Ctrl+D { sink-window }\n\
             bind Super+Ctrl+Shift+D { ocean-dredge-window }\n\
             bind Super+Ctrl+Shift+U { ocean-surface-window }\n\
             bind Super+1 { ocean-bookmark:code }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    assert_eq!(config.spatial_engine, SpatialEngine::Ocean);
    assert!(config.ocean.shared_canvas);
    assert_eq!(config.ocean.camera_step, 720);
    assert!(!config.ocean.depth_enabled);
    assert!(config.ocean.zoom_enabled);
    assert!(!config.ocean.modifier_zoom);
    assert_eq!(config.ocean.min_zoom, 0.4);
    assert_eq!(config.ocean.max_zoom, 2.5);
    assert_eq!(config.ocean.zoom_step, 1.3);
    assert_eq!(config.ocean.camera_animation_ms, 340);
    assert_eq!(config.ocean.camera_sway, 24.0);
    assert!(!config.ocean.canvas_guides);
    assert_eq!(config.ocean.canvas_grid_size, 320);
    assert_eq!(config.ocean.canvas_grid_alpha, 0.18);
    assert!(!config.ocean.canvas_marker);
    assert_eq!(config.ocean.canvas_marker_fade_ms, 5100);
    assert_eq!(config.ocean.reefs.len(), 2);
    assert_eq!(config.ocean.reefs[0].width, None);
    assert_eq!(config.ocean.reefs[0].height, None);
    assert_eq!(config.ocean.reefs[1].width, Some(3440));
    assert_eq!(config.ocean.reefs[1].height, Some(1440));
    assert_eq!(config.ocean.bookmarks[0].name, "code");
    assert_eq!(config.ocean.bookmarks[0].x, 1234.5);
    assert!(config
        .keybinds
        .iter()
        .any(|bind| { matches!(bind.action, Action::OceanPan(Direction::Left)) }));
    assert!(config
        .keybinds
        .iter()
        .any(|bind| matches!(bind.action, Action::OceanZoomIn)));
    assert!(config.keybinds.iter().all(|bind| !matches!(
        bind.action,
        Action::SinkWindow | Action::OceanDredgeWindow | Action::OceanSurfaceWindow
    )));
    assert!(config
        .keybinds
        .iter()
        .any(|bind| { matches!(&bind.action, Action::OceanBookmark(name) if name == "code") }));
}

#[test]
fn parsed_keybinds_are_authoritative_and_never_merge_hidden_defaults() {
    let dir = TestDir::new("mod-frees-super");
    let main = dir.write(
        "config.wave",
        "@mod = ALT\n\
             bind $mod+Q { close-window }\n\
             bind Super+F { toggle-fullscreen }\n",
    );

    let (raw, _, _, _) = load_raw_config(&main).expect("should parse");
    // Only what the file declared exists. No default table is merged
    // underneath it, regardless of which modifier variables it uses.
    assert!(!raw.keybinds.contains_key("Super+Q"));
    assert_eq!(
        raw.keybinds.get("ALT+Q").map(String::as_str),
        Some("close-window")
    );
    // A Super+X the user wrote out themselves is a deliberate dual-bind
    // and must survive even though $mod is off Super.
    assert_eq!(
        raw.keybinds.get("Super+F").map(String::as_str),
        Some("toggle-fullscreen")
    );
    assert!(!raw.keybinds.contains_key("Super+Tab"));
}

#[test]
fn load_raw_config_accepts_wave_syntax_end_to_end() {
    let config = "@mod = SUPER\n\
                   terminal = kitty\n\
                   gaps = 8\n\
                   color = #8EDDFF\n\
                   border {\n\
                       width = 2\n\
                   }\n\
                   input {\n\
                       xkb_layout = us\n\
                   }\n\
                   bind $mod+Q { close-window }\n\
                   spawn = [waybar]\n";

    let dir = TestDir::new("wave-syntax-end-to-end");
    let (raw, warnings, _, _) =
        load_raw_config(&dir.write("config.wave", config)).expect("should evaluate");

    assert!(warnings.is_empty(), "should not warn: {warnings:?}");
    assert_eq!(raw.gaps, 8);
    assert_eq!(raw.terminal, "kitty");
    assert_eq!(raw.spawn, vec!["waybar".to_string()]);
    assert_eq!(
        raw.keybinds.get("SUPER+Q").map(String::as_str),
        Some("close-window")
    );
    assert!(!raw.keybinds.contains_key("$mod+Q"));
}

#[test]
fn load_raw_config_new_syntax_error_is_authoritative() {
    // The Wave parser's error must surface rather than `gaps = $extra`
    // being taken as a raw string.
    let dir = TestDir::new("wave-error-authoritative");
    let main = dir.write(
        "config.wave",
        "@mod = SUPER\nbind $mod+Q { close-window }\ngaps = $extra\n",
    );
    let err = load_raw_config(&main).expect_err("new-syntax errors must not fall back");
    assert!(err.contains("`$extra` is not defined"), "{err}");
}

#[test]
fn diff_entries_reports_changed_keys_binds_and_blocks() {
    use waves::Entry;
    let old = vec![
        Entry::VarDef("mod".into(), "SUPER".into()),
        Entry::Assign("gaps".into(), "8".into()),
        Entry::Assign("terminal".into(), "kitty".into()),
        Entry::Bind("SUPER+Q".into(), "close-window".into()),
        Entry::Bind("SUPER+F".into(), "toggle-fullscreen".into()),
        Entry::Block(
            "border".into(),
            "".into(),
            vec![Entry::Assign("width".into(), "2".into())],
        ),
    ];
    let new = vec![
        Entry::VarDef("mod".into(), "SUPER".into()),
        Entry::Assign("gaps".into(), "12".into()),
        Entry::Bind("SUPER+Q".into(), "close-window".into()),
        Entry::Bind("SUPER+F".into(), "toggle-floating".into()),
        Entry::Bind("SUPER+D".into(), "spawn:rofi".into()),
        Entry::Block(
            "border".into(),
            "".into(),
            vec![Entry::Assign("width".into(), "3".into())],
        ),
    ];

    let diff = diff_entries(&old, &new);
    assert_eq!(
        diff.keys_changed,
        vec![
            ("gaps".to_string(), "8".to_string(), "12".to_string()),
            ("terminal".to_string(), "kitty".to_string(), String::new()),
        ]
    );
    assert_eq!(
        diff.binds_added,
        vec![("SUPER+D".to_string(), "spawn:rofi".to_string())]
    );
    assert_eq!(diff.binds_removed, vec![]);
    assert_eq!(
        diff.binds_changed,
        vec![(
            "SUPER+F".to_string(),
            "toggle-fullscreen".to_string(),
            "toggle-floating".to_string()
        )]
    );
    assert_eq!(
        diff.blocks_changed,
        vec![("border".to_string(), String::new())]
    );
    assert!(!diff.is_empty());
    let summary = diff.summary();
    assert!(summary.contains("2 keys changed"), "{summary}");
    assert!(summary.contains("binds +1 -0 ~1"), "{summary}");
    assert!(summary.contains("blocks changed (border)"), "{summary}");
}

#[test]
fn diff_entries_is_empty_for_identical_lists() {
    use waves::Entry;
    let entries = vec![
        Entry::Assign("gaps".into(), "8".into()),
        Entry::Bind("SUPER+Q".into(), "close-window".into()),
    ];
    let diff = diff_entries(&entries, &entries);
    assert!(diff.is_empty());
    assert_eq!(diff.summary(), "nothing changed");
}

#[test]
fn reload_of_unchanged_file_diffs_empty_and_change_is_detected() {
    fn load_from(path: &Path) -> Config {
        let (raw, _, entries, _) = load_raw_config(path).expect("should parse");
        let (mut config, _) = Config::from_raw(raw);
        config.loaded_entries = entries;
        config
    }
    let dir = TestDir::new("wave-reload-diff");
    let main = dir.write("config.wave", "gaps = 8\nterminal = kitty\n");
    let first = load_from(&main);
    let second = load_from(&main);
    assert!(diff_entries(&first.loaded_entries, &second.loaded_entries).is_empty());

    dir.write("config.wave", "gaps = 12\nterminal = kitty\n");
    let third = load_from(&main);
    let diff = diff_entries(&first.loaded_entries, &third.loaded_entries);
    assert_eq!(
        diff.keys_changed,
        vec![("gaps".to_string(), "8".to_string(), "12".to_string())]
    );
}

#[test]
fn duration_values_parse_to_ms_everywhere() {
    assert_eq!(parse_duration_ms("600"), Some(600));
    assert_eq!(parse_duration_ms("600ms"), Some(600));
    assert_eq!(parse_duration_ms("1.5s"), Some(1500));
    assert_eq!(parse_duration_ms("2m"), Some(120_000));
    assert_eq!(parse_duration_ms("0"), None);
    assert_eq!(parse_duration_ms_including_zero("0ms"), Some(0));
    assert_eq!(parse_duration_ms_including_zero("-0.1ms"), None);
    assert_eq!(parse_duration_ms_including_zero("NaNms"), None);
    assert_eq!(parse_duration_ms("nope"), None);

    let dir = TestDir::new("wave-durations");
    let main = dir.write(
        "config.wave",
        "cursor_hide_after = 2s\n\
             transition {\n\
                 duration = 600ms\n\
                 workspace_motion_delay = 0ms\n\
             }\n",
    );
    let (raw, _, _, _) = load_raw_config(&main).expect("should parse");
    assert_eq!(raw.cursor_hide_after_ms, 2000);
    let transition = &raw.workspace_transition;
    assert_eq!(transition.duration_ms, 600);
    assert_eq!(transition.workspace_motion_delay_ms, 0);
}

#[test]
fn spawn_list_lowers_to_multiple_entries() {
    let dir = TestDir::new("wave-spawn-list");
    let main = dir.write(
        "config.wave",
        "spawn = [waybar, \"swaybg -i ~/wallpaper.png -m fill\"]\n",
    );
    let (raw, _, _, _) = load_raw_config(&main).expect("should parse");
    assert_eq!(
        raw.spawn,
        vec![
            "waybar".to_string(),
            "swaybg -i ~/wallpaper.png -m fill".to_string()
        ]
    );
    // a lone scalar is a one-item list
    let dir = TestDir::new("wave-spawn-scalar");
    let main = dir.write("config.wave", "spawn = waybar\n");
    let (raw, _, _, _) = load_raw_config(&main).expect("should parse");
    assert_eq!(raw.spawn, vec!["waybar".to_string()]);
}

#[test]
fn wave_palette_lowers_end_to_end() {
    let dir = TestDir::new("wave-palette");
    let main = dir.write(
        "config.wave",
        "theme {\n\
                 primary = #8EDDFF\n\
                 deep = primary.darken(0.35)\n\
                 highlight = primary.lighten(0.15)\n\
             }\n\
             border {\n\
                 active_from = theme.primary\n\
                 active_to = theme.deep\n\
             }\n",
    );
    let (raw, warnings, _, _) = load_raw_config(&main).expect("should parse");
    assert!(warnings.is_empty(), "{warnings:?}");
    let border = &raw.border;
    assert_eq!(border.active_from[0], 0x8E as f32 / 255.0);
    assert_eq!(border.active_from[1], 0xDD as f32 / 255.0);
    assert_eq!(border.active_from[2], 0xFF as f32 / 255.0);
    // deep = 8EDDFF * 0.65 -> 5C90A6
    assert_eq!(border.active_to[0], 0x5C as f32 / 255.0);
    assert_eq!(border.active_to[1], 0x90 as f32 / 255.0);
    assert_eq!(border.active_to[2], 0xA6 as f32 / 255.0);
}

#[test]
fn hardware_conditionals_see_the_tide_table() {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .unwrap();
    let config = "@mod = SUPER\n\
                      if tide.backend == \"udev\" and tide.gpu.vendor == \"nvidia\" then\n\
                          udev {\n\
                              disable_overlay_planes = true\n\
                          }\n\
                      end\n\
                      gaps = 8\n";
    let dir = TestDir::new("wave-tide");
    let main = dir.write("config.wave", config);

    // Nvidia on the real backend: the workaround applies.
    let tide = wave::TideInfo {
        backend: "udev",
        gpu_vendor: "nvidia",
        ..Default::default()
    };
    let (raw, warnings, _, _) = load_raw_config_in(&lua, &tide, &main).expect("should parse");
    assert!(warnings.is_empty(), "{warnings:?}");

    // AMD: the conditional is false, so no udev block and no warning.
    let tide = wave::TideInfo {
        backend: "udev",
        gpu_vendor: "amd",
        ..Default::default()
    };
    let (raw_amd, _, _, _) = load_raw_config_in(&lua, &tide, &main).expect("should parse");
    assert_eq!(raw_amd.gaps, raw.gaps);
}

#[test]
fn config_globals_persist_on_the_session_lua_for_eval() {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::MATH | mlua::StdLib::STRING | mlua::StdLib::TABLE,
        mlua::LuaOptions::default(),
    )
    .unwrap();
    let tide = wave::TideInfo {
        backend: "winit",
        ..Default::default()
    };
    let dir = TestDir::new("wave-eval-globals");
    let main = dir.write(
        "config.wave",
        "@mod = SUPER\nterminal = wave(sh)\ntheme {\n    primary = #8EDDFF\n}\n",
    );
    load_raw_config_in(&lua, &tide, &main).expect("should parse");
    // After the load, the session Lua still answers queries:
    // @mod is a global, theme is a section table, wave() resolved.
    assert_eq!(
        lua.load("mod").eval::<String>().expect("mod global"),
        "SUPER"
    );
    // Section tables and colors survive as values on the session Lua.
    let primary = lua.load("theme.primary").eval::<mlua::Value>().unwrap();
    assert_eq!(
        wave::lua_value_to_json(primary).expect("color as JSON"),
        serde_json::json!("8EDDFF")
    );
    // The tide table carries the loader's facts.
    assert_eq!(lua.load("tide.backend").eval::<String>().unwrap(), "winit");
}

#[test]
fn staged_reload_preserves_old_lua_on_failure_and_drops_removed_script_globals() {
    let tide = wave::TideInfo {
        backend: "winit",
        ..Default::default()
    };
    let dir = TestDir::new("wave-transactional-reload");
    let main = dir.write(
        "config.wave",
        "terminal = kitty\nscript {\n    script_state = { value = \"old\" }\n}\n",
    );

    let (active_lua, active, _) =
        Config::reload_staged_from(&main, &tide).expect("initial config should stage");
    assert_eq!(active.terminal, "kitty");
    assert_eq!(
        active_lua
            .load("script_state.value")
            .eval::<String>()
            .unwrap(),
        "old"
    );

    fs::write(&main, "script {\n    this is not valid lua\n}\n").unwrap();
    assert!(
        Config::reload_staged_from(&main, &tide).is_err(),
        "a broken candidate must fail before the active runtime is swapped"
    );
    assert_eq!(
        active_lua
            .load("script_state.value")
            .eval::<String>()
            .expect("the old runtime must remain intact"),
        "old"
    );

    fs::write(&main, "terminal = sh\n").unwrap();
    let (replacement_lua, replacement, _) = Config::reload_staged_from(&main, &tide)
        .expect("corrected config should stage in a fresh runtime");
    assert_eq!(replacement.terminal, "sh");
    assert!(matches!(
        replacement_lua.globals().get::<mlua::Value>("script_state"),
        Ok(mlua::Value::Nil)
    ));
}

#[test]
fn canonical_names_parse_and_old_spellings_are_ignored() {
    let dir = TestDir::new("wave-renames");
    let new = dir.write(
        "config.wave",
        "engine = classic\n\
             drag_modifier = SUPER\n\
             welcome_hint = true\n\
             reload_toast = true\n\
             auto_back_and_forth = true\n\
             layout = master\n\
             master_side = top\n\
             split_bias = horizontal\n\
             transition {\n\
                 duration = 600ms\n\
             }\n\
             vessels {\n\
                 enabled = true\n\
             }\n\
             glass {\n\
                 tint_alpha = 0.1\n\
             }\n\
             physics {\n\
                 tier = light\n\
             }\n\
             mode nav {\n\
                 bind h { focus-left }\n\
             }\n",
    );
    let (raw_new, warnings, _, _) = load_raw_config(&new).expect("new names should parse");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(raw_new.spatial_engine, "classic");
    assert_eq!(raw_new.pointer_modifier, "SUPER");
    assert!(raw_new.show_welcome_hint);
    assert!(raw_new.show_config_reload_toast);
    assert!(raw_new.workspace_auto_back_and_forth);
    assert_eq!(raw_new.default_layout, "master");
    assert_eq!(raw_new.master_orientation, "top");
    assert_eq!(raw_new.bsp_split_bias, "horizontal");
    assert!(raw_new.modes.contains_key("nav"));

    // Old spellings are never applied: each one warns as an unknown
    // key and the default stays.
    let dir = TestDir::new("wave-renames-rejected");
    let old = dir.write(
        "config.wave",
        "spatial_engine = ocean\n\
             wm_mode = ocean\n\
             pointer_modifier = ALT\n\
             mouse_modifier = ALT\n\
             default_layout = master\n",
    );
    let (raw, _, _, _) = load_raw_config(&old).expect("config still loads");
    assert!(
        raw.default_layout.is_empty(),
        "default_layout must be ignored"
    );
    assert_eq!(
        raw.spatial_engine, "classic",
        "the default engine must stay"
    );
    assert_eq!(
        raw.pointer_modifier, "super",
        "the default modifier must stay"
    );
}

#[test]
fn mode_actions_parse() {
    let parsed = |s: &str| parse_action(s).expect("should parse");
    assert!(matches!(parsed("mode:nav"), Action::EnterMode(n) if n == "nav"));
    assert!(matches!(parsed("exit-mode"), Action::ExitMode));
}

#[test]
fn wave_function_resolves_to_the_first_real_command() {
    // /bin/sh always exists on any system that can even run this test
    // suite; "definitely-not-a-real-binary" never will. First match
    // wins over an earlier miss, not just the first candidate overall.
    let dir = TestDir::new("wave-fallback");
    let main = dir.write(
        "config.wave",
        "terminal = wave(\"definitely-not-a-real-binary\", \"/bin/sh\", \"kitty\")\n",
    );

    let (raw, _, _, _) = load_raw_config(&main).expect("should parse");
    assert_eq!(raw.terminal, "/bin/sh");
}

/// Sets up an isolated directory under the system temp dir for a single
/// test, cleaned up on drop -- these tests exercise real file I/O
/// (`load_raw_config` resolving `include` paths relative to the file
/// doing the including), which in-memory AST construction can't cover.
/// Evaluates a Wave config to its entry list.
fn wave_entries(s: &str) -> Vec<waves::Entry> {
    wave::evaluate(s, Path::new("test.wave")).expect("config should evaluate")
}

struct TestDir(PathBuf);
impl TestDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("tidewm-config-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fallback_config_directory_is_owned_private_and_not_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = TestDir::new("private-config-fallback");
    let uid = fs::metadata("/proc/self").unwrap().uid();
    let private = root.0.join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o777)).unwrap();

    ensure_private_owned_dir(&private, uid).unwrap();
    let metadata = fs::symlink_metadata(&private).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    assert_eq!(metadata.uid(), uid);

    let link = root.0.join("link");
    symlink(&private, &link).unwrap();
    assert!(ensure_private_owned_dir(&link, uid).is_err());
}

#[test]
fn load_raw_config_resolves_relative_includes_with_including_files_own_keys_winning() {
    let dir = TestDir::new("includes");
    dir.write("keybinds.wave", "bind Super+Q { close-window }\n");
    let main = dir.write(
        "config.wave",
        "include \"keybinds.wave\"\nterminal = kitty\nbind Super+F { toggle-fullscreen }\n",
    );

    let (raw, _, _, _) = load_raw_config(&main).expect("include chain should resolve");

    assert_eq!(raw.terminal, "kitty");
    // Included file's bind survives...
    assert_eq!(
        raw.keybinds.get("Super+Q").map(String::as_str),
        Some("close-window")
    );
    // ...and the including file's own bind is present too, not
    // clobbered by the include (merge order: include first, own
    // entries folded on top).
    assert_eq!(
        raw.keybinds.get("Super+F").map(String::as_str),
        Some("toggle-fullscreen")
    );
}

#[test]
fn load_raw_config_later_include_overlays_earlier_one() {
    let dir = TestDir::new("include-order");
    dir.write("a.wave", "gaps = 4\n");
    dir.write("b.wave", "gaps = 12\n");
    let main = dir.write("config.wave", "include \"a.wave\"\ninclude \"b.wave\"\n");

    let (raw, _, _, _) = load_raw_config(&main).unwrap();
    assert_eq!(raw.gaps, 12);
}

#[test]
fn load_raw_config_skips_a_missing_include_instead_of_failing_the_whole_load() {
    let dir = TestDir::new("missing-include");
    let main = dir.write(
        "config.wave",
        "include \"does-not-exist.wave\"\nterminal = kitty\n",
    );

    let (raw, warnings, _, _) =
        load_raw_config(&main).expect("a bad include must not fail the top-level file");
    assert_eq!(raw.terminal, "kitty");
    // The failure must reach the caller, not just the log -- this is
    // what the compositor's warning panel reads to know something's
    // wrong (see `Config::load_with_error`/`Config::reload`).
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("does-not-exist.wave"));
}

#[test]
fn wave_source_graph_keeps_external_missing_and_symlink_paths() {
    let config_dir = TestDir::new("source-graph-config");
    let external_dir = TestDir::new("source-graph-external");
    let target = external_dir.write("target.wave", "terminal = foot\n");
    let link = external_dir.0.join("linked.wave");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let missing = external_dir.0.join("missing.wave");
    let main = config_dir.write(
        "config.wave",
        &format!(
            "include \"{}\"\ninclude \"{}\"\n",
            link.display(),
            missing.display()
        ),
    );

    let (raw, warnings, _, sources) = load_raw_config(&main).unwrap();
    assert_eq!(raw.terminal, "foot");
    assert_eq!(warnings.len(), 1);
    assert!(sources.contains(&main));
    assert!(sources.contains(&link));
    assert!(sources.contains(&target.canonicalize().unwrap()));
    assert!(sources.contains(&missing));
}

#[test]
fn watch_relevance_covers_file_removal_and_missing_ancestor_creation() {
    let root = TestDir::new("watch-relevance");
    let tracked = root.0.join("new/deep/include.wave");
    let paths = HashSet::from([tracked.clone()]);

    assert!(event_affects_paths(&tracked, &paths));
    assert!(event_affects_paths(&root.0.join("new"), &paths));
    assert!(!event_affects_paths(&root.0.join("unrelated.wave"), &paths));
    assert!(watch_directories(&paths).contains(&root.0));
}

#[test]
fn load_raw_config_detects_include_cycles_without_infinite_recursion() {
    // A cycle is caught deep in the include graph (b -> a, while a is
    // still an in-progress ancestor), the same place any other broken
    // include would be caught -- so it takes the same "log a warning,
    // skip just that include, keep going" path as a missing file,
    // rather than failing the whole top-level load. What actually
    // matters here is that this terminates at all instead of
    // recursing forever; if it didn't, this test would hang, not fail.
    let dir = TestDir::new("cycle");
    dir.write("b.wave", "include \"a.wave\"\n");
    let a = dir.write("a.wave", "include \"b.wave\"\nterminal = kitty\n");

    let (raw, warnings, _, _) =
        load_raw_config(&a).expect("a cycle is skipped with a warning, not a hard failure");
    assert_eq!(raw.terminal, "kitty");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("include cycle"));
}

/// Writes both `DEFAULT_CONFIG_WAVE` and `DEFAULT_KEYBINDS_WAVE` to a
/// real temp directory and loads them through `load_raw_config`, the
/// exact code path a real first boot uses (see `load_with_error`) --
/// needed for real disk I/O now that the shipped default genuinely
/// `include`s a second file, unlike the old single-file default that
/// `waves::parse` alone could handle in memory.
fn parse_default_config() -> Config {
    // Every caller shares one PID, so a fixed name would collide across
    // the many test functions that call this concurrently -- thread id
    // is stable within one call but unique across the parallel test
    // threads that actually race on it.
    let dir = TestDir::new(&format!("default-config-{:?}", std::thread::current().id()));
    dir.write("keybinds.wave", DEFAULT_KEYBINDS_WAVE);
    dir.write("ebb.wave", DEFAULT_EBB_WAVE);
    let main = dir.write("config.wave", DEFAULT_CONFIG_WAVE);
    let (raw, _, _, _) =
        load_raw_config(&main).expect("the shipped default must parse and resolve");
    Config::from_raw(raw).0
}

#[test]
fn pointer_modifier_fallback_and_generated_default_both_use_super() {
    assert_eq!(
        Config::from_raw(RawConfig::default()).0.pointer_modifier,
        Mods {
            logo: true,
            ..Default::default()
        }
    );
    assert_eq!(
        parse_default_config().pointer_modifier,
        Mods {
            logo: true,
            ..Default::default()
        }
    );

    let entries = wave_entries("@mod = ALT\ndrag_modifier = mod .. \"+SHIFT\"\n");
    let raw = lower_entries(&entries);
    assert_eq!(
        Config::from_raw(raw).0.pointer_modifier,
        Mods {
            alt: true,
            shift: true,
            ..Default::default()
        }
    );
}

#[test]
fn ocean_direct_manipulation_and_reload_card_are_user_configurable() {
    let entries = wave_entries(
        "reload_toast = false\n\
             ocean {\n\
                 freeform_windows = false\n\
                 smart_tiling = true\n\
                 smart_tiling_snap_distance = 96\n\
                 smart_tiling_preserve_size = false\n\
                 canvas_pan_button = middle\n\
                 canvas_pan_requires_modifier = true\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(!config.show_config_reload_toast);
    assert!(!config.ocean.freeform_windows);
    assert!(config.ocean.smart_tiling);
    assert_eq!(config.ocean.smart_tiling_snap_distance, 96);
    assert!(!config.ocean.smart_tiling_preserve_size);
    assert_eq!(config.ocean.canvas_pan_button, OceanPanButton::Middle);
    assert!(config.ocean.canvas_pan_requires_modifier);

    let defaults = parse_default_config();
    assert!(defaults.show_config_reload_toast);
    assert!(defaults.ocean.freeform_windows);
    assert!(defaults.ocean.smart_tiling);
    assert_eq!(defaults.ocean.smart_tiling_snap_distance, 64);
    assert!(defaults.ocean.smart_tiling_preserve_size);
    assert_eq!(defaults.ocean.canvas_pan_button, OceanPanButton::Left);
    assert!(!defaults.ocean.canvas_pan_requires_modifier);
}

#[test]
fn viscosity_parses_clamps_and_uses_the_last_matching_rule() {
    let entries = wave_entries(
        "viscosity = 1.75\n\
             rule {\n\
             app_id = kitty\n\
             viscosity = 0.4\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             viscosity = 9\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.viscosity, 1.75);
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).viscosity,
        Some(4.0)
    );
    assert_eq!(
        config.resolve_window_rules(facts_for("foot")).viscosity,
        None
    );
    assert_eq!(parse_default_config().viscosity, 1.0);
}

#[test]
fn window_rule_size_and_scroll_effects_parse_and_fold() {
    let entries = wave_entries(
        "rule {\n\
             app_id = kitty\n\
             persistent_size = true\n\
             min_width = 200\n\
             min_height = 150\n\
             scroll_factor = 2.5\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             max_width = 1200\n\
             max_height = 900\n\
             }\n\
             rule {\n\
             app_id = foot\n\
             min_width = -5\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let kitty = config.resolve_window_rules(facts_for("kitty"));
    assert!(kitty.persistent_size);
    assert_eq!(kitty.min_width, Some(200));
    assert_eq!(kitty.min_height, Some(150));
    // Set by a later matching rule, folded on top of the first's fields.
    assert_eq!(kitty.max_width, Some(1200));
    assert_eq!(kitty.max_height, Some(900));
    assert_eq!(kitty.scroll_factor, Some(2.5));

    // A negative min_width is rejected at parse time, not clamped.
    let foot = config.resolve_window_rules(facts_for("foot"));
    assert_eq!(foot.min_width, None);
}

#[test]
fn window_rule_add_tag_accumulates_and_tag_match_is_live_only() {
    let entries = wave_entries(
        "rule {\n\
             app_id = kitty\n\
             add_tag = [\"work\", \"terminal\"]\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             add_tag = scratch\n\
             }\n\
             rule {\n\
             tag = [\"work\", \"scratch\"]\n\
             opacity = 0.5\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let kitty = config.resolve_window_rules(facts_for("kitty"));
    // Every matching rule's add_tag list accumulates rather than
    // last-wins, since tags are additive.
    assert_eq!(
        kitty.add_tag,
        vec![
            "work".to_string(),
            "terminal".to_string(),
            "scratch".to_string(),
        ]
    );
    // `facts_for` sets no tags, so the tag-matching rule (which has no
    // app_id/title criterion at all) never applies from app_id alone --
    // `resolve_window_rules` itself is a single pass and never
    // retroactively satisfies `tag` from its own `add_tag` output.
    // `Smallvil::map_toplevel` is what re-resolves a second time after
    // writing `window_tags`, so a `tag`-keyed rule actually does see a
    // same-map `add_tag`; that chaining lives one level up from this
    // pure fold, not here.
    assert!(kitty.opacity.is_none());

    let mut current_tags = HashSet::new();
    current_tags.insert("work".to_string());
    let tagged = config.resolve_window_rules(WindowMatchFacts {
        tags: Some(&current_tags),
        ..Default::default()
    });
    assert_eq!(tagged.opacity, Some(0.5));

    let mut other_tags = HashSet::new();
    other_tags.insert("unrelated".to_string());
    let untagged = config.resolve_window_rules(WindowMatchFacts {
        tags: Some(&other_tags),
        ..Default::default()
    });
    assert!(untagged.opacity.is_none());
}

#[test]
fn window_rule_on_workspace_resolves_by_live_workspace_number_not_app_id() {
    // No app_id/title at all -- this rule matches purely on which
    // workspace the window currently lives on, the "workspace-specific
    // window-rule override" the config-parity audit asked for.
    let entries = wave_entries(
        "rule {\n\
             on_workspace = 3\n\
             opacity = 0.5\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    let on_three = config.resolve_window_rules(WindowMatchFacts {
        app_id: Some("anything"),
        on_workspace: Some(3),
        ..Default::default()
    });
    assert_eq!(on_three.opacity, Some(0.5));

    let on_four = config.resolve_window_rules(WindowMatchFacts {
        app_id: Some("anything"),
        on_workspace: Some(4),
        ..Default::default()
    });
    assert!(on_four.opacity.is_none());

    // Ocean (or an untracked window) reports no workspace at all.
    let no_workspace = config.resolve_window_rules(WindowMatchFacts {
        app_id: Some("anything"),
        on_workspace: None,
        ..Default::default()
    });
    assert!(no_workspace.opacity.is_none());
}

#[test]
fn connected_vessels_block_parses_clamps_and_matches_generated_defaults() {
    let entries = wave_entries(
        "vessels {\n\
             enabled = false\n\
             falloff = 1.7\n\
             max_splits = 7\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(!config.vessels.enabled);
    assert_eq!(config.vessels.falloff, 1.0);
    assert_eq!(config.vessels.max_splits, 7);

    let defaults = parse_default_config().vessels;
    assert!(defaults.enabled);
    assert_eq!(defaults.falloff, 0.5);
    assert_eq!(defaults.max_splits, 4);
}

#[test]
fn snap_block_actions_and_scope_overrides_parse() {
    let entries = wave_entries(
        "snap {\n\
             enabled = true\n\
             preset = halves\n\
             zones = [left, bottom-right]\n\
             distance = 999\n\
             gap = 5\n\
             preview = false\n\
             preview_color = #123456\n\
             preview_opacity = 2.0\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             snap = false\n\
             }\n\
             workspace_rule {\n\
             workspace = 3\n\
             snap = false\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.snap.enabled);
    assert_eq!(config.snap.preset, crate::snap::SnapPreset::Halves);
    assert_eq!(
        config.snap.zones,
        Some(vec![
            crate::snap::SnapZone::Left,
            crate::snap::SnapZone::BottomRight
        ])
    );
    assert_eq!(config.snap.distance, 512);
    assert_eq!(config.snap.gap, Some(5));
    assert!(!config.snap.preview);
    assert_eq!(config.snap.preview_opacity, 1.0);
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).snap,
        Some(false)
    );
    assert_eq!(config.resolve_workspace_rule(3).snap, Some(false));
    assert!(matches!(
        parse_action("snap:top-left"),
        Some(Action::Snap(crate::snap::SnapZone::TopLeft))
    ));
    assert!(matches!(
        parse_action("snap:fullscreen"),
        Some(Action::Snap(crate::snap::SnapZone::Fullscreen))
    ));
    assert!(parse_action("snap:middle").is_none());

    assert_eq!(parse_default_config().snap, SnapConfig::default());
}

#[test]
fn swim_block_parses_clamps_and_matches_generated_defaults() {
    let entries = wave_entries(
        "swim {\n\
             enabled = true\n\
             neighbors = 9\n\
             response = 12.0\n\
             snap_duration_ms = 99999\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.swim.enabled);
    assert_eq!(config.swim.neighbors, 4);
    assert_eq!(config.swim.response, 4.0);
    assert_eq!(config.swim.snap_duration_ms, 2000);

    let defaults = parse_default_config().swim;
    assert!(!defaults.enabled);
    assert_eq!(defaults.neighbors, 1);
    assert_eq!(defaults.response, 1.0);
    assert_eq!(defaults.snap_duration_ms, 220);
}

#[test]
fn sway_block_parses_clamps_rules_and_matches_generated_defaults() {
    let entries = wave_entries(
        "sway {\n\
             enabled = true\n\
             response = 2.5\n\
             max_offset = 999\n\
             frequency = 0\n\
             damping = 42\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             sway = false\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.sway.enabled);
    assert_eq!(config.sway.response, 1.0);
    assert_eq!(config.sway.max_offset, 128.0);
    assert_eq!(config.sway.frequency, 0.1);
    assert_eq!(config.sway.damping, 20.0);
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).sway,
        Some(false)
    );
    assert_eq!(config.resolve_window_rules(facts_for("foot")).sway, None);

    let defaults = parse_default_config().sway;
    assert!(!defaults.enabled);
    assert_eq!(defaults, SwayConfig::default());
}

#[test]
fn currents_block_parses_clamps_and_matches_generated_defaults() {
    let entries = wave_entries(
        "currents {\n\
             enabled = true\n\
             direction = up\n\
             strength = 999\n\
             period = 22s\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.currents.enabled);
    assert_eq!(config.currents.direction_degrees, 270.0);
    assert_eq!(config.currents.strength, 64.0);
    assert_eq!(config.currents.period_ms, 22_000);

    let numeric = wave_entries("currents {\n direction = -45\n }\n");
    let numeric = Config::from_raw(lower_entries(&numeric)).0;
    assert_eq!(numeric.currents.direction_degrees, 315.0);

    assert_eq!(parse_default_config().currents, CurrentsConfig::default());
}

#[test]
fn buoyancy_block_parses_clamps_rules_and_matches_generated_defaults() {
    let entries = wave_entries(
        "buoyancy {\n\
             enabled = true\n\
             default_weight = 2\n\
             max_sink = 999\n\
             settle = 320ms\n\
             flow_reduction = -1\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             weight = 0.8\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             weight = 0.6\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.buoyancy.enabled);
    assert_eq!(config.buoyancy.default_weight, 1.0);
    assert_eq!(config.buoyancy.max_sink, 64.0);
    assert_eq!(config.buoyancy.settle_ms, 320);
    assert_eq!(config.buoyancy.flow_reduction, 0.0);
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).weight,
        Some(0.6)
    );
    assert_eq!(config.resolve_window_rules(facts_for("foot")).weight, None);
    assert_eq!(parse_default_config().buoyancy, BuoyancyConfig::default());
}

#[test]
fn float_physics_block_parses_clamps_rules_and_matches_generated_defaults() {
    let entries = wave_entries(
        "physics {\n\
             tier = full\n\
             response = 2.5\n\
             max_offset = 999\n\
             frequency = 0\n\
             damping = 42\n\
             bob_ratio = -1\n\
             radius = 99999\n\
             falloff = false\n\
             ambient_period_s = 999\n\
             restitution = 5\n\
             bounce_off_edges = false\n\
             wave {\n\
             enabled = false\n\
             amplitude = 999\n\
             wavelength = 1\n\
             speed = 99999\n\
             }\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             float_physics = off\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.float_physics.tier, FloatPhysicsTier::Full);
    assert_eq!(config.float_physics.response, 1.0);
    assert_eq!(config.float_physics.max_offset, 128.0);
    assert_eq!(config.float_physics.frequency, 0.1);
    assert_eq!(config.float_physics.damping, 20.0);
    assert_eq!(config.float_physics.bob_ratio, 0.0);
    assert_eq!(config.float_physics.radius, 2048.0);
    assert!(!config.float_physics.falloff);
    assert_eq!(config.float_physics.ambient_period_s, 60.0);
    assert_eq!(config.float_physics.restitution, 1.0);
    assert!(!config.float_physics.bounce_off_edges);
    assert!(!config.float_physics.wave.enabled);
    assert_eq!(config.float_physics.wave.amplitude, 128.0);
    assert_eq!(config.float_physics.wave.wavelength, 10.0);
    assert_eq!(config.float_physics.wave.speed, 2000.0);
    assert_eq!(
        config
            .resolve_window_rules(facts_for("kitty"))
            .float_physics,
        Some(FloatPhysicsTier::Off)
    );
    assert_eq!(
        config.resolve_window_rules(facts_for("foot")).float_physics,
        None
    );

    // Only tier names select a tier: the old true/false switch is
    // ignored, both globally and per rule.
    let old = wave_entries(
        "physics {\n\
             enabled = true\n\
             tier = true\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             float_physics = true\n\
             }\n",
    );
    let old_config = Config::from_raw(lower_entries(&old)).0;
    assert_eq!(old_config.float_physics.tier, FloatPhysicsTier::Off);
    assert_eq!(
        old_config
            .resolve_window_rules(facts_for("kitty"))
            .float_physics,
        None
    );

    let defaults = parse_default_config().float_physics;
    assert_eq!(defaults.tier, FloatPhysicsTier::Off);
    assert_eq!(defaults, FloatPhysicsConfig::default());
}

#[test]
fn water_glass_block_parses_clamps_and_matches_generated_defaults() {
    let entries = wave_entries(
        "glass {\n\
             animation = ambient\n\
             speed = 99\n\
             amplitude = -1\n\
             settle_ms = 5\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.water_glass.animation, GlassAnimation::Ambient);
    assert_eq!(config.water_glass.speed, 8.0);
    assert_eq!(config.water_glass.amplitude, 0.0);
    assert_eq!(config.water_glass.settle_ms, 100);

    let defaults = parse_default_config().water_glass;
    assert_eq!(defaults.animation, GlassAnimation::Reactive);
    assert_eq!(defaults, WaterGlassConfig::default());
}

#[test]
fn caustics_block_parses_clamps_and_matches_generated_defaults() {
    let entries = wave_entries(
        "caustics {\n\
             enabled = true\n\
             intensity = 5\n\
             color = 8CDDFF\n\
             scale = 0\n\
             speed = -2\n\
             fps = 999\n\
             idle_fps = [20, 10]\n\
             idle_after = [600000, 1200000]\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.caustics.enabled);
    assert_eq!(config.caustics.intensity, 1.0);
    assert_eq!(config.caustics.color, parse_ripple_color("8CDDFF").unwrap());
    assert_eq!(config.caustics.scale, 0.1);
    assert_eq!(config.caustics.speed, 0.0);
    assert_eq!(config.caustics.fps, 60);
    assert_eq!(config.caustics.idle_fps, vec![20, 10]);
    assert_eq!(config.caustics.idle_after_ms, vec![600000, 1200000]);

    let defaults = parse_default_config().caustics;
    assert!(!defaults.enabled);
    assert_eq!(defaults, CausticsConfig::default());
    assert_eq!(defaults.idle_fps, vec![30, 15, 10]);
    assert_eq!(defaults.idle_after_ms, vec![300000, 600000, 1800000]);
}

#[test]
fn rule_depth_override_uses_the_last_matching_rule() {
    let entries = wave_entries(
        "rule {\n\
             app_id = kitty\n\
             depth = false\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             depth = true\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    // Two matching rules for the same app: the later one wins, same
    // fold rule every other Option<bool> rule field uses.
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).depth,
        Some(true)
    );
    assert_eq!(config.resolve_window_rules(facts_for("foot")).depth, None);
}

#[test]
fn invalid_pointer_modifier_warns_and_safely_falls_back_to_super() {
    let raw = RawConfig {
        pointer_modifier: "none".to_string(),
        ..Default::default()
    };
    let (config, warnings) = Config::from_raw(raw);
    assert_eq!(
        config.pointer_modifier,
        Mods {
            logo: true,
            ..Default::default()
        }
    );
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("Invalid drag_modifier")));
}

#[test]
fn mod_left_at_super_or_undefined_produces_no_leftover_super_lint() {
    let (_, warnings) = Config::from_raw(RawConfig::default());
    assert!(!warnings
        .iter()
        .any(|w| w.contains("$mod is set away from Super")));

    let raw = RawConfig {
        ..Default::default()
    };
    let (_, warnings) = Config::from_raw(raw);
    assert!(!warnings
        .iter()
        .any(|w| w.contains("$mod is set away from Super")));
}

#[test]
fn mod_freed_from_super_with_every_bind_rewritten_has_nothing_left_to_warn_about() {
    let mut keybinds = HashMap::new();
    keybinds.insert("ALT+Return".to_string(), "spawn:kitty".to_string());
    let raw = RawConfig {
        keybinds,
        pointer_modifier: "ALT".to_string(),
        ..Default::default()
    };
    let (_, warnings) = Config::from_raw(raw);
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("$mod is set away from Super")),
        "got: {warnings:?}"
    );
}

#[test]
fn workspace_transition_block_parses_every_tuning_knob() {
    let entries = wave_entries(
        "transition {\n\
             enabled = false\n\
             style = glow\n\
             duration = 900ms\n\
             speed = 1.75\n\
             curve = exp-out\n\
             direction = left-to-right\n\
             workspace_motion = true\n\
             workspace_motion_delay = 225ms\n\
             wave_amplitude = 72.5\n\
             wave_frequency = 4.5\n\
             edge_width = 26\n\
             color = #FF0088\n\
             wave_size = 14\n\
             wave_alpha = 0.8\n\
             glow_size = 64\n\
             glow_alpha = 0.35\n\
             water_depth = 330\n\
             water_alpha = 0.96\n\
             foam_color = EEF9FF\n\
             foam_size = 24\n\
             foam_alpha = 0.88\n\
             spray_amount = 0.6\n\
             turbulence = 1.2\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let transition = config.workspace_transition;

    assert!(!transition.enabled);
    assert_eq!(transition.style, WorkspaceTransitionStyle::Glow);
    assert_eq!(transition.duration_ms, 900);
    assert_eq!(transition.speed, 1.75);
    assert_eq!(transition.curve, RippleEase::ExpOut);
    assert_eq!(
        transition.direction,
        WorkspaceTransitionDirectionMode::LeftToRight
    );
    assert!(transition.workspace_motion);
    assert_eq!(transition.workspace_motion_delay_ms, 225);
    assert_eq!(transition.wave_amplitude, 72.5);
    assert_eq!(transition.wave_frequency, 4.5);
    assert_eq!(transition.edge_width, 26.0);
    assert_eq!(transition.color, [1.0, 0.0, 136.0 / 255.0]);
    assert_eq!(transition.wave_size, 14.0);
    assert_eq!(transition.wave_alpha, 0.8);
    assert_eq!(transition.glow_size, 64.0);
    assert_eq!(transition.glow_alpha, 0.35);
    assert_eq!(transition.water_depth, 330.0);
    assert_eq!(transition.water_alpha, 0.96);
    assert_eq!(transition.foam_color, [238.0 / 255.0, 249.0 / 255.0, 1.0]);
    assert_eq!(transition.foam_size, 24.0);
    assert_eq!(transition.foam_alpha, 0.88);
    assert_eq!(transition.spray_amount, 0.6);
    assert_eq!(transition.turbulence, 1.2);
}

#[test]
fn cascade_liquid_block_parses_presets_timing_and_motion_policy() {
    let entries = wave_entries(
        "cascade {\n\
             pour = trickle\n\
             drain = splash\n\
             replace_motion = true\n\
             pour_duration = 310ms\n\
             drain_duration = 180ms\n\
             curve = cubic-in-out\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0.cascade;
    assert_eq!(config.pour, CascadeLiquidPreset::Trickle);
    assert_eq!(config.drain, CascadeLiquidPreset::Splash);
    assert!(config.replace_motion);
    assert_eq!(config.pour_duration_ms, 310);
    assert_eq!(config.drain_duration_ms, 180);
    assert_eq!(config.curve, RippleEase::CubicInOut);
    assert_eq!(
        parse_default_config().cascade,
        CascadeLiquidConfig::default()
    );
}

#[test]
fn window_animation_blocks_parse_bezier_offsets_and_opacity() {
    let entries = wave_entries(
        "animations {\n\
             preset = wave\n\
             enabled = true\n\
             slowdown = 1.5\n\
             max_closing_snapshots = 12\n\
             close_snapshot_output_budget = 1.25\n\
             open {\n\
             duration = 240ms\n\
             curve = \"cubic-bezier(0.1,0.8,0.2,1.1)\"\n\
             opacity_duration = 410ms\n\
             opacity_curve = cubic-in-out\n\
             offset = -12x32\n\
             from_opacity = 0.2\n\
             to_opacity = 0.95\n\
             origin = nearest-edge\n\
             effect = tide\n\
             wave_amplitude = 22\n\
             wave_cycles = 1.75\n\
             wave_decay = 2.25\n\
             }\n\
             close {\n\
             enabled = false\n\
             curve = ease-out-quad\n\
             }\n\
             movement {\n\
             animate_size = false\n\
             duration = 360\n\
             curve = exp-out\n\
             }\n\
             }\n",
    );
    let animations = Config::from_raw(lower_entries(&entries)).0.animations;

    assert!(animations.enabled);
    assert_eq!(animations.slowdown, 1.5);
    assert_eq!(animations.max_closing_snapshots, 12);
    assert_eq!(animations.close_snapshot_output_budget, 1.25);
    assert_eq!(animations.open.duration_ms, 240);
    assert_eq!(
        animations.open.curve,
        WindowAnimationCurve::CubicBezier([0.1, 0.8, 0.2, 1.1])
    );
    assert_eq!(animations.open.opacity_duration_ms, Some(410));
    assert_eq!(
        animations.open.opacity_curve,
        Some(WindowAnimationCurve::CubicInOut)
    );
    assert_eq!(animations.open.offset, (-12, 32));
    assert_eq!(animations.open.from_opacity, 0.2);
    assert_eq!(animations.open.to_opacity, 0.95);
    assert_eq!(animations.open.origin, WindowAnimationOrigin::NearestEdge);
    assert_eq!(animations.open.effect, WindowAnimationEffect::Tide);
    assert_eq!(animations.open.wave_amplitude, 22.0);
    assert_eq!(animations.open.wave_cycles, 1.75);
    assert_eq!(animations.open.wave_decay, 2.25);
    assert!(!animations.close.enabled);
    assert_eq!(animations.close.curve, WindowAnimationCurve::QuadOut);
    assert_eq!(animations.close.effect, WindowAnimationEffect::Wave);
    assert_eq!(animations.movement.duration_ms, 360);
    assert_eq!(animations.movement.curve, WindowAnimationCurve::ExpOut);
    assert!(!animations.movement.animate_size);
}

#[test]
fn smooth_preset_and_explicit_workspace_controls_are_order_independent() {
    let entries = wave_entries(
        "animations {\n\
             workspace {\n\
             enabled = true\n\
             style = fade\n\
             duration = 333ms\n\
             curve = linear\n\
             travel = 0.42\n\
             }\n\
             interactive {\n\
             enabled = true\n\
             half_life = 37ms\n\
             }\n\
             preset = snappy\n\
             }\n",
    );
    let animations = Config::from_raw(lower_entries(&entries)).0.animations;
    assert_eq!(animations.open.duration_ms, 120);
    assert_eq!(animations.open.effect, WindowAnimationEffect::Glide);
    assert!(animations.workspace.enabled);
    assert_eq!(animations.workspace.style, WorkspaceAnimationStyle::Fade);
    assert_eq!(animations.workspace.duration_ms, 333);
    assert_eq!(animations.workspace.curve, WindowAnimationCurve::Linear);
    assert_eq!(animations.workspace.travel, 0.42);
    assert!(animations.interactive.enabled);
    assert_eq!(animations.interactive.half_life_ms, 37);
}

#[test]
fn spring_curves_parse_through_wave_without_changing_presets_or_disable_flags() {
    let entries = wave_entries(
        r#"
animations {
    enabled = false
    open {
        curve = "spring(1, 400, 30)"
    }
    movement {
        curve = "spring(0, 400, 30)"
    }
    workspace {
        curve = "spring(1, 400, 40)"
    }
    preset = silk
}
"#,
    );
    let config = Config::from_raw(lower_entries(&entries)).0.animations;
    let baseline = WindowAnimationsConfig::silk();
    assert!(!config.enabled);
    assert!(matches!(config.open.curve, WindowAnimationCurve::Spring(_)));
    assert!(matches!(
        config.workspace.curve,
        WindowAnimationCurve::Spring(_)
    ));
    assert_eq!(config.open.duration_ms, baseline.open.duration_ms);
    assert_eq!(config.close.curve, baseline.close.curve);
    assert_eq!(config.movement.curve, baseline.movement.curve);
    for invalid in [
        "spring(1,2)",
        "spring(1,2,3,4)",
        "spring(NaN,400,30)",
        "spring(100,0.01,0.01)",
    ] {
        assert!(parse_window_animation_curve(invalid).is_none());
    }
    // The two refusal reasons from the 2026-09-12 live pass are told apart.
    let range = spring_curve_rejection("spring(200, 400, 30)").unwrap();
    let settle = spring_curve_rejection("spring(1, 0.01, 0.01)").unwrap();
    assert!(range.contains("mass 0.01 to 100"), "{range}");
    assert!(settle.contains("does not settle"), "{settle}");
    assert!(spring_curve_rejection("spring(1,2)")
        .unwrap()
        .contains("three numbers"));
    assert_eq!(spring_curve_rejection("spring(1, 400, 30)"), None);
    assert_eq!(spring_curve_rejection("cubic-out"), None);
}

#[test]
fn non_water_motion_presets_enable_workspace_and_interactive_motion() {
    for preset in [
        WindowAnimationsConfig::silk(),
        WindowAnimationsConfig::snappy(),
        WindowAnimationsConfig::gentle(),
        WindowAnimationsConfig::cinematic(),
        WindowAnimationsConfig::minimal(),
    ] {
        assert!(preset.workspace.enabled);
        assert!(preset.interactive.enabled);
        assert_eq!(preset.open.effect, WindowAnimationEffect::Glide);
        assert_eq!(preset.close.effect, WindowAnimationEffect::Glide);
        assert_eq!(preset.movement.effect, WindowAnimationEffect::Glide);
        assert!((0.0..=1.0).contains(&preset.workspace.travel));
    }
    assert!(!WindowAnimationsConfig::tide().workspace.enabled);
    assert!(!WindowAnimationsConfig::hypr_smooth().interactive.enabled);
}

#[test]
fn shader_definitions_parse_and_default_off() {
    let entries = wave_entries(
        "shaders {\n\
             enabled = true\n\
             }\n\
             shader \"Soft-Glass\" {\n\
             scope = window\n\
             source = backdrop\n\
             invalidate = damage-box\n\
             render_scale = 3\n\
             stage {\n\
             file = \"shaders/soft.frag\"\n\
             params {\n\
             strength = 0.25\n\
             offset = [1, 2]\n\
             tint_color = #8EDDFF\n\
             exponent_like = #1E0000\n\
             quoted = \"#123456\"\n\
             }\n\
             }\n\
             }\n",
    );
    let (config, warnings) = Config::from_raw(lower_entries(&entries));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(config.shaders_enabled);
    let definition = &config.shader_definitions["soft-glass"];
    assert_eq!(definition.render_scale, 3);
    let [stage] = definition.stages.as_slice() else {
        panic!("one stage expected");
    };
    assert_eq!(stage.file, "shaders/soft.frag");
    assert_eq!(stage.source, None);
    use crate::shader_effect::ShaderParam;
    assert_eq!(
        stage.params.to_vec(),
        vec![
            ("strength".to_string(), ShaderParam::Float(0.25)),
            ("offset".to_string(), ShaderParam::Vec2([1.0, 2.0])),
            (
                "tint_color".to_string(),
                ShaderParam::Vec4([142.0 / 255.0, 221.0 / 255.0, 1.0, 1.0])
            ),
            (
                "exponent_like".to_string(),
                ShaderParam::Vec4([30.0 / 255.0, 0.0, 0.0, 1.0])
            ),
            (
                "quoted".to_string(),
                ShaderParam::Vec4([18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0, 1.0])
            ),
        ]
    );

    let defaults = parse_default_config();
    assert!(!defaults.shaders_enabled);
    assert!(defaults.shader_definitions.is_empty());
}

#[test]
fn unsupported_or_malformed_shader_definitions_are_rejected_visibly() {
    let stage = "stage {\n file = \"a.frag\"\n }\n";
    for (body, reason) in [
            (format!("scope = desktop\n{stage}"), "scope = desktop"),
            (format!("source = surface\n{stage}"), "source = surface"),
            (format!("source = \"shader:\"\n{stage}"), "does not name a definition"),
            (
                format!("invalidate = always\n{stage}"),
                "invalidate = always",
            ),
            (format!("render_scale = 5\n{stage}"), "render_scale"),
            (format!("blend = add\n{stage}"), "unknown key"),
            (String::new(), "needs a `stage"),
            (
                format!("{stage}{stage}{stage}{stage}{stage}"),
                "at most 4 stages",
            ),
            (
                "stage {\n save = x\n file = \"a.frag\"\n }\n".to_string(),
                "cannot `save`",
            ),
            (
                "stage {\n file = \"a.frag\"\n source = \"get:x\"\n }\n".to_string(),
                "no earlier stage saves",
            ),
            (
                format!(
                    "stage {{\n file = \"a.frag\"\n save = x\n }}\n\
                     stage {{\n file = \"a.frag\"\n save = x\n }}\n{stage}"
                ),
                "more than one stage",
            ),
            (
                format!("{stage}stage {{\n file = \"a.frag\"\n save = later\n }}\n"),
                "cannot `save`",
            ),
            (
                format!(
                    "stage {{\n file = \"a.frag\"\n textures {{\n m = \"get:later\"\n }}\n save = later\n }}\n{stage}"
                ),
                "no earlier stage saves",
            ),
            (
                "stage {\n file = \"a.frag\"\n source = elsewhere\n }\n".to_string(),
                "stage source must be",
            ),
            (
                "stage {\n file = \"a.frag\"\n textures {\n u_m = backdrop\n }\n }\n"
                    .to_string(),
                "cannot be a texture binding",
            ),
            (
                "stage {\n file = \"a.frag\"\n textures {\n a = backdrop\n b = backdrop\n c = backdrop\n d = backdrop\n e = backdrop\n }\n }\n"
                    .to_string(),
                "at most 4 extra textures",
            ),
            (
                "stage {\n file = \"a.frag\"\n params {\n m = 1\n }\n textures {\n m = backdrop\n }\n }\n"
                    .to_string(),
                "both a parameter and a texture",
            ),
            (
                "stage {\n params {\n x = 1\n }\n }\n".to_string(),
                "needs `file",
            ),
            (
                "stage {\n file = \"a.frag\"\n params {\n u_time = 1\n }\n }\n".to_string(),
                "cannot be a parameter",
            ),
            (
                "stage {\n file = \"a.frag\"\n params {\n x = [1, 2, 3, 4, 5]\n }\n }\n"
                    .to_string(),
                "finite number",
            ),
            (
                "stage {\n file = \"a.frag\"\n params {\n x = wide\n }\n }\n".to_string(),
                "finite number",
            ),
        ] {
            let entries = wave_entries(&format!("shader broken {{\n{body}}}\n"));
            let (config, warnings) = Config::from_raw(lower_entries(&entries));
            assert!(config.shader_definitions.is_empty(), "{body}");
            assert!(
                warnings.iter().any(|warning| warning.contains(reason)),
                "{body}: {warnings:?}"
            );
        }

    let params: String = (0..=crate::shader_effect::MAX_PARAMS_PER_STAGE)
        .map(|index| format!(" p{index} = 1\n"))
        .collect();
    let entries = wave_entries(&format!(
        "shader many {{\n stage {{\n file = \"a.frag\"\n params {{\n{params} }}\n }}\n}}\n"
    ));
    let (config, warnings) = Config::from_raw(lower_entries(&entries));
    assert!(config.shader_definitions.is_empty());
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("at most 8 parameters")));

    let entries = wave_entries(&format!("shader none {{\n{stage}}}\n"));
    assert!(Config::from_raw(lower_entries(&entries))
        .0
        .shader_definitions
        .is_empty());
}

#[test]
fn shader_chains_parse_saves_sources_and_texture_bindings() {
    use crate::shader_effect::StageTexture;
    let entries = wave_entries(
        "shader wet {\n\
             stage {\n\
             file = \"blur.frag\"\n\
             save = \"blurred\"\n\
             }\n\
             stage {\n\
             file = \"mask.frag\"\n\
             source = backdrop\n\
             save = mask\n\
             }\n\
             stage {\n\
             file = \"composite.frag\"\n\
             source = \"get:blurred\"\n\
             textures {\n\
             mask = \"get:mask\"\n\
             original = backdrop\n\
             }\n\
             }\n\
             }\n",
    );
    let (config, warnings) = Config::from_raw(lower_entries(&entries));
    assert!(warnings.is_empty(), "{warnings:?}");
    let stages = &config.shader_definitions["wet"].stages;
    assert_eq!(stages.len(), 3);
    assert_eq!(stages[0].save.as_deref(), Some("blurred"));
    assert_eq!(stages[0].input, None);
    assert_eq!(stages[1].input, Some(StageTexture::Backdrop));
    assert_eq!(
        stages[2].input,
        Some(StageTexture::Saved("blurred".to_string()))
    );
    assert_eq!(
        stages[2].textures.to_vec(),
        vec![
            ("mask".to_string(), StageTexture::Saved("mask".to_string())),
            ("original".to_string(), StageTexture::Backdrop),
        ]
    );
}

/// The two examples in DOCUMENTATION.md's "Custom shaders" section,
/// verbatim, so the documented syntax can't drift from what parses.
#[test]
fn documented_shader_examples_parse_without_warnings() {
    let entries = wave_entries(
        r#"shaders {
    enabled = true
}

shader soft-blur {
    render_scale = 2
    stage {
        file = "shaders/blur-h.frag"
        params {
            radius = 2
        }
    }
    stage {
        file = "shaders/blur-v.frag"
        params {
            radius = 2
        }
    }
}

rule {
    app_id = kitty
    shader = soft-blur
}

shader edged {
    stage {
        file = "shaders/blur-h.frag"
        params {
            radius = 3
        }
    }
    stage {
        file = "shaders/blur-v.frag"
        params {
            radius = 3
        }
        save = "blurred"
    }
    stage {
        file = "shaders/edge-mix.frag"
        source = "get:blurred"
        params {
            margin = 0.08
        }
        textures {
            original = backdrop
        }
    }
}

shader tinted-blur {
    source = "shader:soft-blur"
    stage {
        file = "shaders/tint.frag"
        params {
            strength = 0.3
            tint_color = 88CCFF
        }
    }
}
"#,
    );
    let (mut config, mut warnings) = Config::from_raw(lower_entries(&entries));
    warnings.extend(expand_shader_sources(&mut config.shader_definitions));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(config.shaders_enabled);
    assert_eq!(config.shader_definitions["soft-blur"].stages.len(), 2);
    assert_eq!(config.shader_definitions["edged"].stages.len(), 3);
    assert_eq!(config.shader_definitions["tinted-blur"].stages.len(), 3);
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).shader,
        Some(ShaderAssignment::Named("soft-blur".to_string()))
    );
}

#[test]
fn shader_sources_splice_the_source_chain_in_front() {
    use crate::shader_effect::StageTexture;
    let entries = wave_entries(
            "shader blur {\n\
             stage {\n file = \"h.frag\"\n }\n\
             stage {\n file = \"v.frag\"\n }\n\
             }\n\
             shader wet {\n\
             source = \"shader:blur\"\n\
             stage {\n file = \"mix.frag\"\n textures {\n original = backdrop\n }\n }\n\
             }\n\
             shader looped-a {\n source = \"shader:looped-b\"\n stage {\n file = \"a.frag\"\n }\n }\n\
             shader looped-b {\n source = \"shader:looped-a\"\n stage {\n file = \"b.frag\"\n }\n }\n\
             shader orphan {\n source = \"shader:missing\"\n stage {\n file = \"a.frag\"\n }\n }\n\
             shader long {\n source = \"shader:wet\"\n stage {\n file = \"a.frag\"\n }\n stage {\n file = \"b.frag\"\n }\n }\n\
             shader clash-base {\n stage {\n file = \"a.frag\"\n save = x\n }\n stage {\n file = \"b.frag\"\n }\n }\n\
             shader clash {\n source = \"shader:clash-base\"\n stage {\n file = \"a.frag\"\n save = x\n }\n stage {\n file = \"b.frag\"\n }\n }\n",
        );
    let (mut config, warnings) = Config::from_raw(lower_entries(&entries));
    assert!(warnings.is_empty(), "{warnings:?}");
    let warnings = expand_shader_sources(&mut config.shader_definitions);
    let wet = &config.shader_definitions["wet"].stages;
    let files: Vec<&str> = wet.iter().map(|stage| stage.file.as_str()).collect();
    assert_eq!(files, ["h.frag", "v.frag", "mix.frag"]);
    assert_eq!(wet[1].save.as_deref(), Some("tide_source_2"));
    // `backdrop` in the definition means its source's output.
    assert_eq!(
        wet[2].textures[0].1,
        StageTexture::Saved("tide_source_2".to_string())
    );
    assert_eq!(wet[2].input, None);
    assert_eq!(config.shader_definitions["blur"].stages.len(), 2);
    for (name, reason) in [
        ("looped-a", "loops back"),
        ("looped-b", "loops back"),
        ("orphan", "not a loaded definition"),
        ("long", "more than 4"),
        ("clash", "both save"),
    ] {
        assert!(!config.shader_definitions.contains_key(name), "{name}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains(name) && warning.contains(reason)),
            "{name}: {warnings:?}"
        );
    }
}

#[test]
fn a_later_shader_definition_replaces_the_earlier_one_whole() {
    let entries = wave_entries(
        "shader glass {\n\
             render_scale = 1\n\
             stage {\n\
             file = \"first.frag\"\n\
             }\n\
             }\n\
             shader glass {\n\
             stage {\n\
             file = \"second.frag\"\n\
             }\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let definition = &config.shader_definitions["glass"];
    assert_eq!(definition.render_scale, 2);
    assert_eq!(definition.stages.len(), 1);
    assert_eq!(definition.stages[0].file, "second.frag");

    // A broken redefinition removes the name instead of keeping the
    // earlier, different effect around under it.
    let entries = wave_entries(
        "shader glass {\n stage {\n file = \"first.frag\"\n }\n }\n\
             shader glass {\n scope = output\n stage {\n file = \"first.frag\"\n }\n }\n",
    );
    assert!(Config::from_raw(lower_entries(&entries))
        .0
        .shader_definitions
        .is_empty());
}

#[test]
fn shader_definition_count_is_capped() {
    let source: String = (0..=crate::shader_effect::MAX_DEFINITIONS)
        .map(|index| format!("shader s{index} {{\n stage {{\n file = \"a.frag\"\n }}\n}}\n"))
        .collect();
    let (config, warnings) = Config::from_raw(lower_entries(&wave_entries(&source)));
    assert_eq!(
        config.shader_definitions.len(),
        crate::shader_effect::MAX_DEFINITIONS
    );
    assert!(warnings
        .iter()
        .any(|warning| warning.contains("at most 32 definitions")));
}

#[test]
fn rule_shader_is_last_match_wins_and_none_clears() {
    let entries = wave_entries(
        "rule {\n app_id = kitty\n shader = Soft-Glass\n }\n\
             rule {\n app_id = foot\n shader = crt\n }\n\
             rule {\n app_id = foot\n shader = none\n }\n\
             rule {\n app_id = mpv\n shader = crt\n }\n\
             rule {\n app_id = mpv\n shader = \"bad name!\"\n }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).shader,
        Some(ShaderAssignment::Named("soft-glass".to_string()))
    );
    assert_eq!(
        config.resolve_window_rules(facts_for("foot")).shader,
        Some(ShaderAssignment::None)
    );
    // An invalid later value is ignored, not treated as `none`.
    assert_eq!(
        config.resolve_window_rules(facts_for("mpv")).shader,
        Some(ShaderAssignment::Named("crt".to_string()))
    );
    assert_eq!(
        config.resolve_window_rules(facts_for("alacritty")).shader,
        None
    );
}

#[test]
fn workspace_shader_is_a_default_the_window_rule_overrides() {
    let entries = wave_entries(
        "workspace_rule {\n workspace = 2\n shader = crt\n }\n\
             workspace_rule {\n workspace = 2\n shader = glow\n }\n\
             workspace_rule {\n workspace = 3\n shader = none\n }\n\
             workspace_rule {\n workspace = 3\n snap = false\n }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    let glow = ShaderAssignment::Named("glow".to_string());
    assert_eq!(config.workspace_shader(2), Some(&glow));
    assert_eq!(config.resolve_workspace_rule(2).shader, Some(glow.clone()));
    assert_eq!(config.workspace_shader(3), Some(&ShaderAssignment::None));
    assert_eq!(config.workspace_shader(4), None);

    let crt = ShaderAssignment::Named("crt".to_string());
    assert_eq!(ShaderAssignment::resolve(None, Some(&glow)), Some("glow"));
    assert_eq!(
        ShaderAssignment::resolve(Some(&crt), Some(&glow)),
        Some("crt")
    );
    assert_eq!(
        ShaderAssignment::resolve(Some(&ShaderAssignment::None), Some(&glow)),
        None
    );
    assert_eq!(
        ShaderAssignment::resolve(None, Some(&ShaderAssignment::None)),
        None
    );
    assert_eq!(ShaderAssignment::resolve(None, None), None);
}

#[test]
fn layer_shader_is_explicit_and_last_match_wins() {
    let entries = wave_entries(
        "layer_rule {\n namespace = waybar\n shader = glow\n }\n\
             layer_rule {\n namespace = waybar\n ignore_alpha = 0.1\n }\n\
             layer_rule {\n namespace = rofi\n shader = glow\n }\n\
             layer_rule {\n namespace = rofi\n shader = none\n }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    // A later rule without `shader` leaves the earlier assignment alone.
    assert_eq!(config.layer_shader("waybar"), Some("glow"));
    assert_eq!(config.layer_ignore_alpha("waybar"), Some(0.1));
    assert_eq!(config.layer_shader("rofi"), None);
    assert_eq!(config.layer_shader("mako"), None);
}

#[test]
fn shader_files_load_relative_to_the_config_and_join_the_watch_set() {
    let dir = TestDir::new("shader-load");
    fs::create_dir_all(dir.0.join("shaders")).unwrap();
    dir.write(
        "shaders/pass.frag",
        "vec4 tide_effect(vec2 uv) {\n    return texture2D(tex, uv) * strength;\n}\n",
    );
    dir.write("shaders/bad.frag", "void main() {}\n");
    let main = dir.write(
        "config.wave",
        "shader pass {\n\
             stage {\n\
             file = \"shaders/pass.frag\"\n\
             params {\n\
             strength = 0.5\n\
             }\n\
             }\n\
             }\n\
             shader bad {\n\
             stage {\n\
             file = \"shaders/bad.frag\"\n\
             }\n\
             }\n\
             shader missing {\n\
             stage {\n\
             file = \"shaders/later.frag\"\n\
             }\n\
             }\n\
             shader dimmer {\n\
             source = \"shader:pass\"\n\
             stage {\n\
             file = \"shaders/pass.frag\"\n\
             params {\n\
             strength = 0.25\n\
             }\n\
             }\n\
             }\n",
    );
    let (_, config, warnings) =
        Config::reload_staged_from(&main, &wave::TideInfo::default()).expect("config loads");

    let pass = &config.shader_definitions["pass"];
    assert_eq!(pass.stages[0].path, dir.0.join("shaders/pass.frag"));
    let source = pass.stages[0].source.as_ref().expect("pass.frag loads");
    assert!(source.contains("uniform float strength;"));
    assert!(source.contains("return texture2D(tex, uv) * strength;"));

    assert!(config.shader_definitions["bad"].stages[0]
        .source
        .as_ref()
        .is_none());
    assert!(config.shader_definitions["missing"].stages[0]
        .source
        .as_ref()
        .is_none());
    assert!(warnings
        .iter()
        .any(|w| w.contains("\"bad\" stage 1") && w.contains("`main` is reserved")));
    assert!(warnings.iter().any(|w| w.contains("\"missing\" stage 1")));
    // A sourced definition carries its source's loaded stages in front.
    let dimmer = &config.shader_definitions["dimmer"].stages;
    assert_eq!(dimmer.len(), 2);
    assert!(dimmer.iter().all(|stage| stage.source.is_some()));
    assert_eq!(
        dimmer[0].params[0].1,
        crate::shader_effect::ShaderParam::Float(0.5)
    );
    assert_eq!(
        dimmer[1].params[0].1,
        crate::shader_effect::ShaderParam::Float(0.25)
    );
    assert_eq!(dimmer[0].save.as_deref(), Some("tide_source_1"));

    for name in ["pass.frag", "bad.frag", "later.frag"] {
        assert!(
            config
                .source_paths
                .contains(&dir.0.join("shaders").join(name)),
            "{name} must be watched"
        );
    }
}

#[test]
fn frost_block_and_per_window_glass_mode_parse() {
    let entries = wave_entries(
        "frost {\n\
             enabled = false\n\
             liquid = 0.7\n\
             radius = 24\n\
             strength = 0.75\n\
             opacity = 0.8\n\
             saturation = 1.25\n\
             contrast = 0.9\n\
             brightness = 0.8\n\
             noise = 0.03\n\
             noise_scale = 2\n\
             vibrancy = 0.4\n\
             vibrancy_darkness = 0.6\n\
             tint_color = 88CCFF\n\
             tint_alpha = 0.2\n\
             corner_radius = 16\n\
             corner_softness = 1.5\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             opacity = 0.7\n\
             active_opacity = 1.0\n\
             inactive_opacity = 0.75\n\
             fullscreen_opacity = 0.95\n\
             glass = frost\n\
             frost {\n\
             radius = 32\n\
             opacity = 0.65\n\
             tint_alpha = 0.0\n\
             noise = 0.02\n\
             }\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    assert!(!config.frost.enabled);
    assert_eq!(config.frost.liquid, 0.7);
    assert_eq!(config.frost.radius, 24.0);
    assert_eq!(config.frost.strength, 0.75);
    assert_eq!(config.frost.opacity, 0.8);
    assert_eq!(config.frost.saturation, 1.25);
    assert_eq!(config.frost.contrast, 0.9);
    assert_eq!(config.frost.brightness, 0.8);
    assert_eq!(config.frost.noise, 0.03);
    assert_eq!(config.frost.noise_scale, 2.0);
    assert_eq!(config.frost.vibrancy, 0.4);
    assert_eq!(config.frost.vibrancy_darkness, 0.6);
    assert_eq!(config.frost.tint_color, [136.0 / 255.0, 204.0 / 255.0, 1.0]);
    assert_eq!(config.frost.tint_alpha, 0.2);
    assert_eq!(config.frost.corner_radius, 16.0);
    assert_eq!(config.frost.corner_softness, 1.5);
    let rule = config.resolve_window_rules(facts_for("kitty"));
    assert_eq!(rule.opacity, Some(0.7));
    assert_eq!(rule.active_opacity, Some(1.0));
    assert_eq!(rule.inactive_opacity, Some(0.75));
    assert_eq!(rule.fullscreen_opacity, Some(0.95));
    assert_eq!(rule.glass, Some(GlassMode::Frost));
    let frost = rule.frost.unwrap().apply_to(&config.frost);
    assert_eq!(frost.radius, 32.0);
    assert_eq!(frost.opacity, 0.65);
    assert_eq!(frost.tint_alpha, 0.0);
    assert_eq!(frost.noise, 0.02);
    assert_eq!(frost.contrast, 0.9);
}

#[test]
fn window_opacity_multiplies_base_and_focus_state_with_fullscreen_priority() {
    let opacity = WindowOpacity {
        base: Some(0.8),
        active: Some(1.0),
        inactive: Some(0.6),
        fullscreen: Some(0.95),
    };
    assert_eq!(opacity.alpha(true, false), 0.8);
    assert!((opacity.alpha(false, false) - 0.48).abs() < f32::EPSILON);
    assert!((opacity.alpha(false, true) - 0.76).abs() < f32::EPSILON);
}

#[test]
fn shadow_block_and_per_window_overrides_parse_and_merge() {
    let entries = wave_entries(
        "shadow {\n\
             enabled = false\n\
             range = 42\n\
             spread = -3\n\
             offset = 4x-6\n\
             scale = 0.9\n\
             render_power = 4\n\
             sharp = true\n\
             ignore_window = false\n\
             color = 11223380\n\
             color_inactive = 10203040\n\
             urgent_color = 00CCFFC0\n\
             opacity = 0.8\n\
             inactive_opacity = 0.5\n\
             urgent_opacity = 0.95\n\
             corner_radius = 18\n\
             floating_only = true\n\
             fullscreen = true\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             shadow {\n\
             enabled = true\n\
             softness = 24\n\
             offset_y = 12\n\
             draw_behind_window = false\n\
             }\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             shadow {\n\
             spread = 5\n\
             inactive_opacity = 0.7\n\
             }\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    assert!(!config.shadow.enabled);
    assert_eq!(config.shadow.softness, 42.0);
    assert_eq!(config.shadow.spread, -3.0);
    assert_eq!(config.shadow.offset, (4.0, -6.0));
    assert_eq!(config.shadow.scale, 0.9);
    assert_eq!(config.shadow.render_power, 4.0);
    assert!(config.shadow.sharp);
    assert!(config.shadow.draw_behind_window);
    assert_eq!(
        config.shadow.color,
        [17.0 / 255.0, 34.0 / 255.0, 51.0 / 255.0, 128.0 / 255.0]
    );
    assert_eq!(
        config.shadow.inactive_color,
        [16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0, 64.0 / 255.0]
    );
    assert_eq!(config.shadow.opacity, 0.8);
    assert_eq!(config.shadow.inactive_opacity, 0.5);
    assert_eq!(config.shadow.urgent_opacity, 0.95);
    assert_eq!(config.shadow.corner_radius, 18.0);
    assert!(config.shadow.floating_only);
    assert!(config.shadow.fullscreen);

    let resolved = config.resolve_window_rules(facts_for("kitty"));
    let shadow = resolved.shadow.unwrap().apply_to(&config.shadow);
    assert!(shadow.enabled);
    assert_eq!(shadow.softness, 24.0);
    assert_eq!(shadow.spread, 5.0);
    // Per-axis sparse override keeps the global X offset.
    assert_eq!(shadow.offset, (4.0, 12.0));
    assert_eq!(shadow.inactive_opacity, 0.7);
    assert!(!shadow.draw_behind_window);
    assert_eq!(shadow.render_power, 4.0);
}

#[test]
fn rounding_and_border_blocks_parse_and_merge_per_window() {
    let entries = wave_entries(
        "rounding {\n\
             radius = [18, 14, 10, 6]\n\
             power = 2.5\n\
             antialias = 1.25\n\
             clip = true\n\
             floating_only = true\n\
             }\n\
             border {\n\
             width = 3.5\n\
             placement = center\n\
             active_from = 2EC7FFEE\n\
             active_to = 5CFFD2EE\n\
             inactive_color = 10203080\n\
             urgent_color_to = 4775FFFF\n\
             angle = 120\n\
             animate = true\n\
             animate_focused = true\n\
             animate_inactive = false\n\
             animate_urgent = true\n\
             inactive_enabled = true\n\
             animation_speed = 42\n\
             pulse_amount = 0.2\n\
             pulse_speed = 1.5\n\
             radius_offset = 2\n\
             }\n\
             rule {\n\
             app_id = kitty\n\
             rounding {\n\
             top_right = 22\n\
             floating_only = false\n\
             }\n\
             border {\n\
             placement = inside\n\
             inactive_opacity = 0.45\n\
             }\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.rounding.radii, [18.0, 14.0, 10.0, 6.0]);
    assert_eq!(config.rounding.power, 2.5);
    assert_eq!(config.rounding.antialias, 1.25);
    assert!(config.rounding.clip);
    assert!(config.rounding.floating_only);
    assert_eq!(config.border.width, 3.5);
    assert_eq!(config.border.placement, BorderPlacement::Center);
    assert!(config.border.animate);
    assert!(config.border.animate_focused);
    assert!(!config.border.animate_inactive);
    assert!(config.border.animate_urgent);
    assert!(config.border.inactive_enabled);
    assert_eq!(config.border.animation_speed, 42.0);
    assert_eq!(config.border.pulse_amount, 0.2);

    let resolved = config.resolve_window_rules(facts_for("kitty"));
    let rounding = resolved.rounding.unwrap().apply_to(&config.rounding);
    let border = resolved.border.unwrap().apply_to(&config.border);
    assert_eq!(rounding.radii, [18.0, 22.0, 10.0, 6.0]);
    assert!(!rounding.floating_only);
    assert_eq!(border.placement, BorderPlacement::Inside);
    assert_eq!(border.inactive_opacity, 0.45);
    assert_eq!(border.active_to, config.border.active_to);
}

#[test]
fn popup_theme_is_auto_by_default_and_pinned_by_explicit_overrides() {
    // Auto: nothing set, so the popup theme just follows border.width
    // (clamped) and the accent gradient -- no [popup] block at all.
    let auto = Config::from_raw(RawConfig::default()).0;
    let auto_theme = crate::ui_theme::UiTheme::from_config(&auto);
    assert_eq!(auto_theme.border_width, auto.border.width.clamp(1.0, 4.0));
    assert_eq!(
        auto_theme.popup_accent(false, 0.5),
        auto_theme.accent(false, 0.5)
    );

    let entries = wave_entries(
        "popup {\n\
             border_width = 3\n\
             border_color = FF0000\n\
             radius = 20\n\
             }\n",
    );
    let pinned = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(pinned.popup.border_width, Some(3.0));
    assert_eq!(pinned.popup.radius, Some(20.0));

    let theme = crate::ui_theme::UiTheme::from_config(&pinned);
    assert_eq!(theme.border_width, 3.0);
    assert_eq!(theme.radius, 20);
    // The color pin ignores the gradient position entirely -- same
    // flat color no matter where along the border it's sampled.
    assert_eq!(theme.popup_accent(false, 0.0), [255, 0, 0]);
    assert_eq!(theme.popup_accent(false, 1.0), [255, 0, 0]);
    assert_eq!(theme.popup_accent(true, 0.5), [255, 0, 0]);
}

#[test]
fn popup_style_defaults_to_underline_and_parses_every_style() {
    let auto = Config::from_raw(RawConfig::default()).0;
    assert_eq!(auto.popup.style, ToastStyle::Underline);
    let entries = wave_entries("popup {\n style = pill\n }\n");
    assert_eq!(
        Config::from_raw(lower_entries(&entries)).0.popup.style,
        ToastStyle::Pill
    );

    let entries = wave_entries("popup {\n style = banner\n }\n");
    let banner = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(banner.popup.style, ToastStyle::Banner);

    let entries = wave_entries("popup {\n style = outline\n }\n");
    let outline = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(outline.popup.style, ToastStyle::Outline);

    let entries = wave_entries("popup {\n style = underline\n line_width = 6\n }\n");
    let underline = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(underline.popup.style, ToastStyle::Underline);
    assert_eq!(underline.popup.line_width, Some(6.0));

    let entries = wave_entries("popup {\n height = 30\n min_width = 600\n }\n");
    let sized = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(sized.popup.height, Some(30.0));
    assert_eq!(sized.popup.min_width, Some(600.0));

    let entries = wave_entries("popup {\n style = nonsense\n }\n");
    let fallback = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(fallback.popup.style, ToastStyle::default());
}

#[test]
fn popup_duration_parses_and_clamps() {
    let auto = Config::from_raw(RawConfig::default()).0;
    assert_eq!(auto.popup.duration_ms, None);
    let duration = |text: &str| {
        let entries = wave_entries(&format!("popup {{\n duration_ms = {text}\n }}\n"));
        Config::from_raw(lower_entries(&entries))
            .0
            .popup
            .duration_ms
    };
    assert_eq!(duration("1500"), Some(1500));
    assert_eq!(duration("10"), Some(300));
    assert_eq!(duration("999999"), Some(60_000));
    assert_eq!(duration("soon"), None);
}

#[test]
fn popup_animation_defaults_to_slide_and_parses_fade() {
    let auto = Config::from_raw(RawConfig::default()).0;
    assert_eq!(auto.popup.animation, PopupAnimation::Slide);
    let entries = wave_entries("popup {\n animation = fade\n }\n");
    let fade = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(fade.popup.animation, PopupAnimation::Fade);
    let entries = wave_entries("popup {\n animation = slide\n }\n");
    let slide = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(slide.popup.animation, PopupAnimation::Slide);
    assert_eq!(slide.popup.slide_ms, None);
    let entries = wave_entries("popup {\n slide_ms = 900\n }\n");
    assert_eq!(
        Config::from_raw(lower_entries(&entries)).0.popup.slide_ms,
        Some(900)
    );
}

#[test]
fn glass_mode_is_last_match_wins_and_plain_is_explicit() {
    let entries = wave_entries(
        "rule {\n app_id = kitty\n glass = frost\n }\n\
             rule {\n app_id = kitty\n glass = none\n }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    assert_eq!(
        config.resolve_window_rules(facts_for("kitty")).glass,
        Some(GlassMode::Plain)
    );
    assert_eq!(config.resolve_window_rules(facts_for("foot")).glass, None);
}

#[test]
fn workspace_transition_defaults_match_generated_config() {
    for config in [
        Config::from_raw(RawConfig::default()).0,
        parse_default_config(),
    ] {
        let transition = config.workspace_transition;
        assert!(transition.enabled);
        assert_eq!(transition.style, WorkspaceTransitionStyle::Water);
        assert_eq!(transition.duration_ms, 520);
        assert_eq!(transition.speed, 1.0);
        assert_eq!(transition.curve, RippleEase::CubicInOut);
        assert_eq!(transition.direction, WorkspaceTransitionDirectionMode::Auto);
        assert!(!transition.workspace_motion);
        assert_eq!(transition.workspace_motion_delay_ms, 150);
        assert_eq!(transition.wave_amplitude, 34.0);
        assert_eq!(transition.wave_frequency, 3.0);
        assert_eq!(transition.edge_width, 18.0);
        assert_eq!(transition.color, [142.0 / 255.0, 221.0 / 255.0, 1.0]);
        assert_eq!(transition.wave_size, 10.0);
        assert_eq!(transition.wave_alpha, 0.9);
        assert_eq!(transition.glow_size, 46.0);
        assert_eq!(transition.glow_alpha, 0.25);
        assert_eq!(transition.water_depth, 260.0);
        assert_eq!(transition.water_alpha, 0.88);
        assert_eq!(transition.foam_color, [232.0 / 255.0, 252.0 / 255.0, 1.0]);
        assert_eq!(transition.foam_size, 18.0);
        assert_eq!(transition.foam_alpha, 0.95);
        assert_eq!(transition.spray_amount, 0.7);
        assert_eq!(transition.turbulence, 0.7);
    }
}

#[test]
fn depth_artwork_paths_are_watched_and_relative_to_main_config() {
    let dir = TestDir::new("depth-artwork");
    let main = dir.write("config.wave", "depth {\n background = \"art with spaces.gif\"\n background_fit = contain\n show_title = false\n}\n");
    let (_, config, warnings) =
        Config::reload_staged_from(&main, &wave::TideInfo::default()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let path = main.parent().unwrap().join("art with spaces.gif");
    assert_eq!(config.depth.background, Some(path.clone()));
    assert_eq!(config.depth.background_fit, DepthBackgroundFit::Contain);
    assert!(!config.depth.show_title);
    assert!(config.source_paths.contains(&path));
    let entries = wave_entries("depth {\n background = none\n}\n");
    assert!(Config::from_raw(lower_entries(&entries))
        .0
        .depth
        .background
        .is_none());
}

#[test]
fn depth_block_parses_every_tuning_knob() {
    let entries = wave_entries(
        "depth {\n\
             enabled = false\n\
             sink_after_ms = 1200\n\
             tier_interval_ms = 800\n\
             max_tier = 4\n\
             tier_one_alpha = 0.61\n\
             cool_color = 123456\n\
             cool_alpha = 0.33\n\
             schematic_color = 102030\n\
             schematic_alpha = 0.75\n\
             border_color = AABBCC\n\
             urgent_color = FFEEDD\n\
             urgent_alpha = 0.84\n\
             }\n",
    );
    let depth = Config::from_raw(lower_entries(&entries)).0.depth;
    assert!(!depth.enabled);
    assert_eq!(depth.sink_after_ms, 1200);
    assert_eq!(depth.tier_interval_ms, 800);
    assert_eq!(depth.max_tier, 4);
    assert_eq!(depth.tier_one_alpha, 0.61);
    assert_eq!(depth.cool_color, [18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0]);
    assert_eq!(depth.cool_alpha, 0.33);
    assert_eq!(
        depth.schematic_color,
        [16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0]
    );
    assert_eq!(depth.schematic_alpha, 0.75);
    assert_eq!(
        depth.border_color,
        [170.0 / 255.0, 187.0 / 255.0, 204.0 / 255.0]
    );
    assert_eq!(depth.urgent_color, [1.0, 238.0 / 255.0, 221.0 / 255.0]);
    assert_eq!(depth.urgent_alpha, 0.84);
}

#[test]
fn cursor_hide_after_accepts_zero_as_disabled() {
    for (value, expected) in [("0", 0), ("0ms", 0), ("2s", 2000), ("1500ms", 1500)] {
        let mut raw = RawConfig {
            cursor_hide_after_ms: 777,
            ..RawConfig::default()
        };
        apply_top_level_assign(&mut raw, "cursor_hide_after", value);
        assert_eq!(raw.cursor_hide_after_ms, expected, "{value}");
    }
}

#[test]
fn depth_defaults_match_generated_config() {
    for config in [
        Config::from_raw(RawConfig::default()).0,
        parse_default_config(),
    ] {
        let depth = config.depth;
        assert!(!depth.enabled);
        assert_eq!(depth.sink_after_ms, 30_000);
        assert_eq!(depth.tier_interval_ms, 30_000);
        assert_eq!(depth.max_tier, 2);
        assert_eq!(depth.tier_one_alpha, 0.78);
        assert_eq!(depth.cool_alpha, 0.24);
        assert_eq!(depth.schematic_alpha, 0.9);
        assert_eq!(depth.urgent_alpha, 0.95);
    }
}

#[test]
fn classic_depth_is_independently_opt_in() {
    let entries = wave_entries(
        "depth_deck {\n\
             enabled = true\n\
             animation = false\n\
             animation_duration_ms = 610\n\
             wave_color = 123456\n\
             wave_alpha = 0.44\n\
             }\n\
             bind Super+D { depth-down }\n\
             bind Super+Shift+D { depth-up }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert!(config.classic_depth.enabled);
    assert!(!config.classic_depth.animation);
    assert_eq!(config.classic_depth.animation_duration_ms, 610);
    assert_eq!(
        config.classic_depth.wave_color,
        [18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0]
    );
    assert_eq!(config.classic_depth.wave_alpha, 0.44);
    assert!(config
        .keybinds
        .iter()
        .any(|bind| matches!(bind.action, Action::DepthDown)));
    assert!(config
        .keybinds
        .iter()
        .any(|bind| matches!(bind.action, Action::DepthUp)));
    assert!(!RawConfig::default().classic_depth.enabled);
    let default = parse_default_config();
    assert!(!default.classic_depth.enabled);
    assert!(default
        .keybinds
        .iter()
        .all(|bind| !is_depth_action(&bind.action)));
}

#[test]
fn default_mode_parses_from_both_the_in_memory_and_written_defaults() {
    // Two independently hand-maintained representations of the same
    // default (see `RawConfig::default()` and `DEFAULT_KEYBINDS_WAVE`'s
    // own doc note) -- assert both actually agree, not just that one
    // of them happens to parse.
    for config in [
        Config::from_raw(RawConfig::default()).0,
        parse_default_config(),
    ] {
        let nav = config
            .modes
            .get("nav")
            .expect("default config should ship a `nav` mode");
        let find = |key: &str| {
            nav.iter()
                .find(|b| b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE))
        };
        assert!(matches!(
            find("h").map(|b| &b.action),
            Some(Action::FocusDirection(Direction::Left))
        ));
        assert!(matches!(
            find("l").map(|b| &b.action),
            Some(Action::FocusDirection(Direction::Right))
        ));
        assert!(matches!(
            find("k").map(|b| &b.action),
            Some(Action::FocusDirection(Direction::Up))
        ));
        assert!(matches!(
            find("j").map(|b| &b.action),
            Some(Action::FocusDirection(Direction::Down))
        ));
        assert!(matches!(
            find("Escape").map(|b| &b.action),
            Some(Action::ExitMode)
        ));

        let enters_nav = config
            .keybinds
            .iter()
            .any(|b| matches!(&b.action, Action::EnterMode(name) if name == "nav"));
        assert!(
            enters_nav,
            "default keybinds should have a bind entering the `nav` mode"
        );
    }
}

#[test]
fn default_layout_keybinds_parse_from_both_the_in_memory_and_written_defaults() {
    // Same two-representations-must-agree check as the mode test
    // above, for the layout-algorithm keybinds: also catches, for free,
    // a duplicate `bind` for the same combo in DEFAULT_KEYBINDS_WAVE
    // (last one silently wins there -- worth a second look at the
    // template by eye if this test ever breaks unexpectedly).
    for (config, main) in [
        (
            Config::from_raw(RawConfig::default()).0,
            Mods {
                logo: true,
                ..Default::default()
            },
        ),
        (
            parse_default_config(),
            Mods {
                logo: true,
                ..Default::default()
            },
        ),
    ] {
        let find = |key: &str, mods: Mods| {
            config.keybinds.iter().find(|b| {
                b.keysym == xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE)
                    && b.mods == mods
            })
        };
        let main_shift = Mods {
            alt: main.alt,
            logo: main.logo,
            shift: true,
            ..Default::default()
        };
        let main_ctrl = Mods {
            alt: main.alt,
            logo: main.logo,
            ctrl: true,
            ..Default::default()
        };

        assert!(matches!(
            find("w", main).map(|b| &b.action),
            Some(Action::SetLayout(LayoutAlgorithm::Bsp))
        ));
        assert!(matches!(
            find("w", main_shift).map(|b| &b.action),
            Some(Action::SetLayout(LayoutAlgorithm::Master))
        ));
        assert!(matches!(
            find("minus", main_ctrl).map(|b| &b.action),
            Some(Action::ShrinkMaster)
        ));
        assert!(matches!(
            find("equal", main_ctrl).map(|b| &b.action),
            Some(Action::GrowMaster)
        ));
    }
}

#[test]
fn shipped_default_keybinds_never_trip_the_footgun_lint() {
    // The default config itself must stay clean, or every fresh
    // install would show the new warning panel on first boot.
    let (_, warnings) = Config::from_raw(RawConfig::default());
    assert!(
        warnings.is_empty(),
        "RawConfig::default() produced diagnostics: {warnings:?}"
    );
}

#[test]
fn shipped_default_config_files_never_trip_the_footgun_lint() {
    // Same check as above, but through the real written text
    // (`DEFAULT_CONFIG_WAVE` + `DEFAULT_KEYBINDS_WAVE`, resolved via
    // `load_raw_config` exactly like a real first boot) rather than
    // the separate Rust-literal `RawConfig::default()` -- catches a
    // problem in the shipped *text* specifically, like a genuinely
    // missing include, that the Rust-side default can't.
    let dir = TestDir::new(&format!(
        "default-config-files-{:?}",
        std::thread::current().id()
    ));
    dir.write("keybinds.wave", DEFAULT_KEYBINDS_WAVE);
    dir.write("ebb.wave", DEFAULT_EBB_WAVE);
    let main = dir.write("config.wave", DEFAULT_CONFIG_WAVE);
    let (raw, include_warnings, _, _) =
        load_raw_config(&main).expect("the shipped default must parse and resolve");
    let (_, mut warnings) = Config::from_raw(raw);
    warnings.extend(include_warnings);
    assert!(
        warnings.is_empty(),
        "the shipped default config produced diagnostics: {warnings:?}"
    );
}

#[test]
fn bare_typing_key_in_base_keybinds_is_allowed_without_policy_warning() {
    let mut raw = RawConfig::default();
    raw.keybinds
        .insert("Return".to_string(), "spawn:kitty".to_string());
    let (config, warnings) = Config::from_raw(raw);

    // Waves describes exactly what the user requested, even when that
    // deliberately captures a typing key globally.
    let bound = config.keybinds.iter().any(|b| {
        b.keysym == xkb::keysym_from_name("Return", xkb::KEYSYM_CASE_INSENSITIVE)
            && b.mods == Mods::default()
    });
    assert!(bound, "the bare bind should still be applied");
    assert!(
        warnings.is_empty(),
        "unexpected bare-key policy: {warnings:?}"
    );
}

#[test]
fn bare_typing_key_in_a_mode_is_not_flagged() {
    // Modes rely on bare keys by design (the default `nav` mode's
    // own Escape = exit-mode) -- only the always-active base table
    // can silently steal a key from every other window.
    let mut raw = RawConfig::default();
    let mut mode = HashMap::new();
    mode.insert("a".to_string(), "close-window".to_string());
    raw.modes.insert("test".to_string(), mode);
    let (_, warnings) = Config::from_raw(raw);
    assert!(
        !warnings.iter().any(|w| w.contains('a')),
        "mode binds must not trigger the base-table footgun lint: {warnings:?}"
    );
}

#[test]
fn ordinary_keys_can_be_held_as_user_defined_helpers() {
    let entries = wave_entries(
        "@sub = P\n\
             bind $sub+Ctrl+H { focus-left }\n\
             bind F { toggle-fullscreen }\n",
    );
    let raw = lower_entries(&entries);
    let (config, warnings) = Config::from_raw(raw);
    assert!(warnings.is_empty());

    let helper = config
        .keybinds
        .iter()
        .find(|bind| matches!(bind.action, Action::FocusDirection(Direction::Left)))
        .unwrap();
    assert_eq!(
        helper.keysym,
        xkb::keysym_from_name("h", xkb::KEYSYM_CASE_INSENSITIVE)
    );
    assert_eq!(
        helper.held_keysyms,
        vec![xkb::keysym_from_name("p", xkb::KEYSYM_CASE_INSENSITIVE)]
    );
    assert!(helper.mods.ctrl);
    assert!(config.keybinds.iter().any(|bind| {
        bind.keysym == xkb::keysym_from_name("f", xkb::KEYSYM_CASE_INSENSITIVE)
            && bind.held_keysyms.is_empty()
            && bind.mods == Mods::default()
            && matches!(bind.action, Action::ToggleFullscreen)
    }));
}

#[test]
fn unknown_keybind_action_is_dropped_and_reported() {
    let mut raw = RawConfig::default();
    raw.keybinds
        .insert("Super+Z".to_string(), "not-a-real-action".to_string());
    let (config, warnings) = Config::from_raw(raw);
    assert!(!config
        .keybinds
        .iter()
        .any(|b| b.keysym == xkb::keysym_from_name("z", xkb::KEYSYM_CASE_INSENSITIVE)));
    assert!(
        warnings.iter().any(|w| w.contains("not-a-real-action")),
        "expected the dropped action to be reported: {warnings:?}"
    );
}

#[test]
fn ripple_color_accepts_hash_and_bare_forms() {
    let parse = parse_ripple_color;
    assert_eq!(parse("00FFFF"), Some([0.0, 1.0, 1.0]));
    assert_eq!(parse("#00FFFF"), Some([0.0, 1.0, 1.0]));
    assert_eq!(parse("00FFFF80"), Some([0.0, 1.0, 1.0]));
    // only hex: no rgb()/rgba() text forms
    assert_eq!(parse("rgb(0,255,255)"), None);
    assert_eq!(parse("rgba(00FFFF, 128)"), None);
}

#[test]
fn ripple_presets_parse_with_trigger_specific_styles_and_tuning() {
    assert_eq!(
        parse_ripple_preset_selection("water-drop"),
        Some(RipplePresetSelection::BuiltIn(RipplePreset::WaterDrop))
    );
    assert_eq!(
        parse_ripple_preset_selection("jiggle"),
        Some(RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
    );
    assert_eq!(
        parse_ripple_preset_selection("giggle"),
        Some(RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
    );

    let entries = wave_entries(
        "ripple {\n\
             preset = tide\n\
             map_preset = splash\n\
             focus_preset = jelly\n\
             urgent_preset = bubble\n\
             focus_on_map = true\n\
             secondary_color = CBA6F7\n\
             glow = 1.2\n\
             wobble = 0.9\n\
             detail = 1.1\n\
             triggers = [map, focus, urgent]\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(
        config.ripple.preset,
        Some(RipplePresetSelection::BuiltIn(RipplePreset::Tide))
    );
    assert_eq!(
        config.ripple.preset_for(RippleTrigger::Map),
        Some(&RipplePresetSelection::BuiltIn(RipplePreset::Splash))
    );
    assert_eq!(
        config.ripple.preset_for(RippleTrigger::Focus),
        Some(&RipplePresetSelection::BuiltIn(RipplePreset::Jelly))
    );
    assert_eq!(
        config.ripple.preset_for(RippleTrigger::Urgent),
        Some(&RipplePresetSelection::BuiltIn(RipplePreset::Bubble))
    );
    assert_eq!(config.ripple.focus_on_map, Some(true));
    assert_eq!(config.ripple.secondary_color, parse_ripple_color("CBA6F7"));
    assert_eq!(config.ripple.glow, Some(1.2));
    assert_eq!(config.ripple.wobble, Some(0.9));
    assert_eq!(config.ripple.detail, Some(1.1));
    assert_eq!(
        config.ripple.triggers,
        vec![
            RippleTrigger::Map,
            RippleTrigger::Focus,
            RippleTrigger::Urgent
        ]
    );
}

#[test]
fn urgent_repeat_parses_and_merges_with_clamped_interval() {
    let entries = wave_entries(
        "ripple {\n\
             urgent_repeat = false\n\
             urgent_repeat_interval_ms = 10\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.ripple.urgent_repeat, Some(false));
    // 10ms is below the 100ms floor; clamped at parse time.
    assert_eq!(config.ripple.urgent_repeat_interval_ms, Some(100));

    let resolved = config.resolve_ripple_config(None, RippleTrigger::Urgent);
    assert_eq!(resolved.urgent_repeat, Some(false));
    assert_eq!(resolved.urgent_repeat_interval_ms, Some(100));

    // Unset inherits the system default (repeat on, 1500ms).
    let default = RippleConfig::system_default();
    assert_eq!(default.urgent_repeat, Some(true));
    assert_eq!(default.urgent_repeat_interval_ms, Some(1500));
}

#[test]
fn assigning_legacy_shapes_selects_the_compatibility_preset() {
    let entries = wave_entries("ripple {\nshapes = [ring, square]\n}\n");
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(
        config.ripple.preset,
        Some(RipplePresetSelection::BuiltIn(RipplePreset::Legacy))
    );
    assert_eq!(
        config.ripple.shapes,
        vec![RippleShape::Ring, RippleShape::Square]
    );
}

#[test]
fn named_ripple_presets_inherit_and_keep_local_adjustments() {
    let entries = wave_entries(
        "ripple_preset ocean-base {\n\
             preset = tide\n\
             color = 89B4FA\n\
             size_mode = window\n\
             size_scale = 0.8\n\
             min_radius = 90\n\
             max_radius = 900\n\
             anchor = right\n\
             edge_position = 0.25\n\
             edge_offset = 12\n\
             }\n\
             ripple_preset ocean-wide {\n\
             preset = ocean-base\n\
             detail = 1.4\n\
             }\n\
             ripple_preset ocean-jelly {\n\
             preset = jelly\n\
             wobble = 1.4\n\
             }\n\
             ripple {\n\
             map_preset = ocean-wide\n\
             focus_preset = ocean-jelly\n\
             color = CBA6F7\n\
             triggers = [map, focus]\n\
             }\n",
    );
    let config = Config::from_raw(lower_entries(&entries)).0;

    let map = config.resolve_ripple_config(None, RippleTrigger::Map);
    assert_eq!(map.built_in_preset(), RipplePreset::Tide);
    assert_eq!(map.color, parse_ripple_color("CBA6F7"));
    assert_eq!(map.size_mode, Some(RippleSizeMode::Window));
    assert_eq!(map.anchor, Some(RippleAnchor::Right));
    assert_eq!(map.edge_position, Some(0.25));
    assert_eq!(map.edge_offset, Some(12.0));
    assert_eq!(map.detail, Some(1.4));
    assert!((map.radius_for_window(600.0, 800.0) - 400.0).abs() < 0.01);

    let focus = config.resolve_ripple_config(None, RippleTrigger::Focus);
    assert_eq!(focus.built_in_preset(), RipplePreset::Jelly);
    assert_eq!(focus.wobble, Some(1.4));
}

#[test]
fn ripple_window_size_modes_and_radius_clamps_are_deterministic() {
    let base = RippleConfig {
        size_scale: Some(1.0),
        min_radius: Some(40.0),
        max_radius: Some(500.0),
        ..RippleConfig::system_default()
    };
    for (mode, expected) in [
        (RippleSizeMode::Window, 500.0),
        (RippleSizeMode::Width, 300.0),
        (RippleSizeMode::Height, 400.0),
        (RippleSizeMode::MinDimension, 300.0),
        (RippleSizeMode::MaxDimension, 400.0),
    ] {
        let cfg = RippleConfig {
            size_mode: Some(mode),
            ..base.clone()
        };
        assert!((cfg.radius_for_window(600.0, 800.0) - expected).abs() < 0.01);
    }
}

#[test]
fn shadow_color_keeps_alpha_across_hex_forms() {
    let parse = parse_rgba_color;
    assert_eq!(parse("00FFFF"), Some([0.0, 1.0, 1.0, 1.0]));
    assert_eq!(parse("#00FFFF"), Some([0.0, 1.0, 1.0, 1.0]));
    assert_eq!(parse("00FFFF80"), Some([0.0, 1.0, 1.0, 0.5019608]));
    // only hex: no 0xAARRGGBB, no rgb()/rgba() text forms
    assert_eq!(parse("0x8000FFFF"), None);
    assert_eq!(parse("rgb(0,255,255)"), None);
}

#[test]
fn environment_validation_matches_setenv_preconditions() {
    assert!(validate_env_entry("XCURSOR_THEME", "Tide").is_ok());
    assert!(validate_env_entry("", "value").is_err());
    assert!(validate_env_entry("BAD=NAME", "value").is_err());
    assert!(validate_env_entry("BAD\0NAME", "value").is_err());
    assert!(validate_env_entry("GOOD_NAME", "bad\0value").is_err());
}

#[test]
fn malformed_environment_entries_become_redacted_warnings() {
    let mut raw = RawConfig::default();
    raw.env.insert("VALID_NAME".into(), "visible".into());
    raw.env.insert("BAD=NAME".into(), "top-secret".into());
    raw.env
        .insert("NUL_VALUE".into(), "top-secret\0tail".into());

    let (config, warnings) = Config::from_raw(raw);

    assert_eq!(
        config.env.get("VALID_NAME").map(String::as_str),
        Some("visible")
    );
    assert!(!config.env.contains_key("BAD=NAME"));
    assert!(!config.env.contains_key("NUL_VALUE"));
    assert_eq!(warnings.len(), 2);
    assert!(warnings
        .iter()
        .all(|warning| !warning.contains("top-secret")));
}
#[test]
fn destination_workspace_rules_define_snap_limits_before_ownership_changes() {
    let config = Config::from_raw(RawConfig {
        window_rules: vec![WindowRule {
            on_workspace: Some(3),
            max_width: Some(240),
            ..Default::default()
        }],
        ..Default::default()
    })
    .0;
    let area = smithay::utils::Rectangle::new((1500, 0).into(), (1000, 700).into());
    for (workspace, width) in [(2, 500), (3, 240)] {
        let rule = config.resolve_window_rules(WindowMatchFacts {
            on_workspace: Some(workspace),
            ..Default::default()
        });
        let bounds = crate::window_size::FloatingSizeConstraints::from_hints(
            (0, 0).into(),
            (0, 0).into(),
            &rule,
        );
        let rect =
            crate::snap::constrained_target_rect(area, crate::snap::SnapZone::Right, 0, bounds);
        assert_eq!(rect.size.w, width);
        assert_eq!(rect.loc.x + rect.size.w, 2500);
    }
}

#[test]
fn reload_config_is_a_plain_bindable_action() {
    assert!(matches!(
        parse_action("reload-config"),
        Some(Action::ReloadConfig)
    ));
    assert!(parse_action("reload-config:x").is_none());
}

#[test]
fn move_column_to_index_parses_one_based_places_only() {
    assert!(matches!(
        parse_action("move-column-to-index:3"),
        Some(Action::Column(ColumnAction::MoveToIndex(3)))
    ));
    assert!(parse_action("move-column-to-index:0").is_none());
    assert!(parse_action("move-column-to-index:x").is_none());
    assert!(parse_action("move-column-to-index:").is_none());
}

#[test]
fn center_visible_columns_is_a_plain_column_action() {
    assert!(matches!(
        parse_action("center-visible-columns"),
        Some(Action::Column(ColumnAction::CenterVisible))
    ));
}

#[test]
fn window_height_actions_and_preset_heights_parse() {
    assert!(matches!(
        parse_action("set-window-height:50%"),
        Some(Action::Column(ColumnAction::SetHeight(_)))
    ));
    assert!(matches!(
        parse_action("set-window-height:+10%"),
        Some(Action::Column(ColumnAction::SetHeight(_)))
    ));
    assert!(parse_action("set-window-height:0").is_none());
    assert!(parse_action("set-window-height:").is_none());
    assert!(matches!(
        parse_action("reset-window-height"),
        Some(Action::Column(ColumnAction::ResetHeight))
    ));
    assert!(matches!(
        parse_action("switch-preset-height-back"),
        Some(Action::Column(ColumnAction::SwitchPresetHeightBack))
    ));
    let entries = wave_entries("scroll {\n preset_heights = [0.75, 0.25]\n }\n");
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.scroll.preset_heights.as_slice(), &[0.25, 0.75]);
    assert_eq!(config.scroll.preset_widths.as_slice().len(), 3);
    let entries = wave_entries("scroll {\n preset_heights = [5]\n }\n");
    let config = Config::from_raw(lower_entries(&entries)).0;
    assert_eq!(config.scroll.preset_heights.as_slice().len(), 3);
}

#[test]
fn floating_tiling_focus_actions_parse() {
    assert!(matches!(
        parse_action("focus-floating"),
        Some(Action::FocusFloating)
    ));
    assert!(matches!(
        parse_action("focus-tiling"),
        Some(Action::FocusTiling)
    ));
    assert!(matches!(
        parse_action("switch-focus-between-floating-and-tiling"),
        Some(Action::SwitchFocusFloatingTiling)
    ));
}

#[test]
fn shortcut_inhibit_and_touchpad_toggles_parse() {
    assert!(matches!(
        parse_action("toggle-keyboard-shortcuts-inhibit"),
        Some(Action::ToggleKeyboardShortcutsInhibit)
    ));
    assert!(matches!(
        parse_action("toggle-touchpad"),
        Some(Action::ToggleTouchpad)
    ));
}

#[test]
fn load_config_file_needs_a_path() {
    assert!(matches!(
        parse_action("load-config-file:/tmp/other.wave"),
        Some(Action::LoadConfigFile(ref path)) if path == "/tmp/other.wave"
    ));
    assert!(parse_action("load-config-file:").is_none());
    assert!(parse_action("load-config-file:   ").is_none());
}

#[test]
fn session_config_path_keeps_symlinks() {
    let dir = std::env::temp_dir().join(format!("tidewm-session-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dots")).unwrap();
    std::fs::write(dir.join("dots/config.wave"), "gaps = 4\n").unwrap();
    std::os::unix::fs::symlink(dir.join("dots/config.wave"), dir.join("config.wave")).unwrap();
    let link = dir.join("config.wave");
    assert_eq!(
        crate::config::session_config_path(link.to_str().unwrap()),
        Some(link.clone())
    );
    assert_eq!(
        crate::config::session_config_path(dir.to_str().unwrap()),
        None
    );
    assert_eq!(
        crate::config::session_config_path(dir.join("missing.wave").to_str().unwrap()),
        None
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
