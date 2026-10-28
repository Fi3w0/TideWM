pub(super) const DEFAULT_EBB_WAVE: &str = include_str!("../../../share/defaults/ebb.wave");

pub(super) const DEFAULT_CONFIG_WAVE: &str = r#"# TideWM configuration.
# Full reference, every action string, and the whole protocol matrix:
# DOCUMENTATION.md in the TideWM repo.
#
# Locked out by a bad edit? Ctrl+Alt+Escape turns on a small always-safe
# keybind set (terminal, close, quit, floating, fullscreen) until the next
# reload or restart.

# ~~~~~~~~~~~~~~~~~ the core ~~~~~~~~~~~~~~~~~

include "keybinds.wave"
include "ebb.wave"

@mod = SUPER                    # one modifier, everything hangs off it

# The first-boot hint card on the empty desktop. Set false or delete this
# line entirely to keep it from coming back.
welcome_hint = true

terminal = wave(kitty, alacritty, foot, xterm)
drag_modifier = mod              # left-drag moves a floating window; right-drag resizes
engine = classic                 # classic (workspaces) or ocean (infinite canvas), live-switchable
reload_toast = true

# ~~~~~~~~~~~~~~~~~ the water ~~~~~~~~~~~~~~~~~

# Ripples, wave transitions, glass, viscosity, sway, and everything else
# that makes this TideWM instead of any other tiling WM. One toggle, plus
# a lot of dials that already have good defaults. The full catalog
# (frost/shadow/rounding/border/ripple/depth/ocean/...) is in
# DOCUMENTATION.md, not repeated here.
water_effects = true
viscosity = 1.0                  # 0 turns off drag/resize settling, higher settles slower
# backdrop_capture_scale = 2     # 1 (default, full detail) to 4; lower the frost/water-glass
                                  # capture texture's resolution to cut VRAM on many glass windows

# Automatic attention depth: unfocused windows fade into a blue wash after
# 30 s and turn into title cards after 60 s. Off by default; set true (and
# tune sink_after_ms / cool_color in DOCUMENTATION.md's depth { }) to opt in.
depth {
    enabled = false
}

# ~~~~~~~~~~~~~~~~~ the layout ~~~~~~~~~~~~~~~~~

gaps = 8
layout = bsp                     # bsp, master, cascade, or floating

snap {
    enabled = true
    preset = quarters            # halves removes corner targets
    distance = 24                # logical pixels from the output edge
    gap = workspace              # inherit this workspace/output's gap
    preview = true
    preview_color = #2EC7FF
    preview_opacity = 0.22
    fullscreen = true            # drop dead-center on the top edge to fill the screen
}

# ~~~~~~~~~~~~~~~~~ input ~~~~~~~~~~~~~~~~~

input {
    repeat_delay = 200
    repeat_rate = 25
    focus_follows_mouse = true

    # xkb_layout = us
    # xkb_options = grp:alt_shift_toggle

    touchpad {
        # tap_to_click = true
        # natural_scroll = true
    }
}

# ~~~~~~~~~~~~~~~~~ x11 apps ~~~~~~~~~~~~~~~~~

xwayland {
    enabled = true
    path = xwayland-satellite
}

# ~~~~~~~~~~~~~~~~~ monitors ~~~~~~~~~~~~~~~~~

# Select the GPU TideWM uses for its own rendering and scanout. Secondary
# non-excluded GPUs remain available for client PRIME/offload rendering.
gpu {
    render = auto                  # auto, /dev/dri/cardN, or vendor:...
    exclude = []                   # device paths and/or vendor tags
}

# TideWM picks a sensible mode/scale per monitor on its own; set this only
# to override one.
# output eDP-1 {
#     mode = 1920x1080@60
#     position = 0x0
#     scale = 1.0
# }

# ~~~~~~~~~~~~~~~~~ autostart & environment ~~~~~~~~~~~~~~~~~

# spawn = [waybar, "swaybg -i ~/wallpaper.png"]

# env {
#     XCURSOR_THEME = Adwaita
# }

# switch_events {
#     lid_close = spawn:systemctl suspend      # systemd; use "loginctl suspend" under
#                                               # elogind, or your distro's own suspend
#                                               # command (e.g. "zzz") on OpenRC without it
# }

# ~~~~~~~~~~~~~~~~~ window rules ~~~~~~~~~~~~~~~~~

# Per-app placement and appearance, matched on app_id and/or title (regex
# allowed). Every field a rule can set is in DOCUMENTATION.md.
# rule {
#     app_id = pavucontrol
#     float = true
# }
"#;

/// The keybind half of the shipped default, `include`d by
/// `DEFAULT_CONFIG_WAVE` -- a real split (not just an example in a
/// comment) so a fresh install looks like the two-file layout this
/// project's own docs already recommend. Mirrors `RawConfig::default()`'s
/// hardcoded fallback bind-for-bind (see the `default_layout_keybinds_...`
/// and `default_mode_...` tests below, which assert both agree) --
/// `RawConfig::default()` is the ultimate hard fallback used when no
/// config exists at all or parsing fails outright, so the two must never
/// silently diverge.
pub(super) const DEFAULT_KEYBINDS_WAVE: &str = r#"# Keybinds. Everything below hangs off @mod (config.wave), so rebinding
# your primary modifier is a one-line change, not a find-and-replace.

# ~~~~~~~~~~~~~~~~~ apps ~~~~~~~~~~~~~~~~~

bind $mod+Return  { spawn:kitty }

# Most setups also want a launcher and a file manager, but both assume a
# tool this config can't verify you have installed -- uncomment yours:
# bind $mod+D  { "spawn:wofi --show drun" }
# bind $mod+E  { spawn:thunar }

# ~~~~~~~~~~~~~~~~~ windows ~~~~~~~~~~~~~~~~~

bind $mod+Q  { close-window }
bind $mod+Shift+Q  { quit }

bind $mod+V  { toggle-floating }
bind $mod+F  { toggle-fullscreen }
bind $mod+M  { toggle-border-fullscreen }
bind $mod+Shift+M  { resize-to-monitor }
bind $mod+P  { toggle-pin }
bind $mod+Shift+P  { toggle-pseudo-tile }

# Focus and swap, vim-style
bind $mod+H  { focus-left }
bind $mod+L  { focus-right }
bind $mod+K  { focus-up }
bind $mod+J  { focus-down }
bind $mod+Shift+H  { swap-left }
bind $mod+Shift+L  { swap-right }
bind $mod+Shift+K  { swap-up }
bind $mod+Shift+J  { swap-down }
bind $mod+Tab  { cycle-focus }

# ~~~~~~~~~~~~~~~~~ tiling ~~~~~~~~~~~~~~~~~

bind $mod+W  { layout:bsp }
bind $mod+Shift+W  { layout:master }
bind $mod+Ctrl+Minus  { master-shrink }
bind $mod+Ctrl+Equal  { master-grow }
bind $mod+O  { toggle-overview }

# Group windows into one tabbed slot
bind $mod+Ctrl+H  { group-left }
bind $mod+Ctrl+L  { group-right }
bind $mod+Ctrl+K  { group-up }
bind $mod+Ctrl+J  { group-down }
bind $mod+Shift+G  { ungroup }
bind $mod+BracketRight  { cycle-tab-next }
bind $mod+BracketLeft  { cycle-tab-prev }

bind $mod+Minus  { toggle-scratchpad }
bind $mod+Shift+Minus  { move-to-scratchpad }

# ~~~~~~~~~~~~~~~~~ workspaces ~~~~~~~~~~~~~~~~~

bind $mod+1  { workspace:1 }
bind $mod+2  { workspace:2 }
bind $mod+3  { workspace:3 }
bind $mod+4  { workspace:4 }
bind $mod+5  { workspace:5 }
bind $mod+6  { workspace:6 }
bind $mod+7  { workspace:7 }
bind $mod+8  { workspace:8 }
bind $mod+9  { workspace:9 }
bind $mod+0  { workspace:10 }
bind $mod+Shift+1  { move-to-workspace:1 }
bind $mod+Shift+2  { move-to-workspace:2 }
bind $mod+Shift+3  { move-to-workspace:3 }
bind $mod+Shift+4  { move-to-workspace:4 }
bind $mod+Shift+5  { move-to-workspace:5 }
bind $mod+Shift+6  { move-to-workspace:6 }
bind $mod+Shift+7  { move-to-workspace:7 }
bind $mod+Shift+8  { move-to-workspace:8 }
bind $mod+Shift+9  { move-to-workspace:9 }
bind $mod+Shift+0  { move-to-workspace:10 }

# ~~~~~~~~~~~~~~~~~ modes ~~~~~~~~~~~~~~~~~

# A mode (like sway/Hyprland's): a temporary keybind layer, entered
# below, left active until its own exit -- not tied to focus.
bind $mod+N  { mode:nav }
mode nav {
    bind h  { focus-left }
    bind l { focus-right }
    bind k { focus-up }
    bind j { focus-down }
    bind Escape { exit-mode }
}

# ~~~~~~~~~~~~~~~~~ ocean's camera ~~~~~~~~~~~~~~~~~

# Off by default: these chords are ones apps use themselves (Ctrl+arrows
# is word-jump in every editor), and the actions only do anything once
# engine = ocean in config.wave anyway. Uncomment when you switch.
# bind Ctrl+Left { ocean-pan-left }
# bind Ctrl+Right { ocean-pan-right }
# bind Ctrl+Up { ocean-pan-up }
# bind Ctrl+Down { ocean-pan-down }
# bind Ctrl+I { ocean-zoom-in }
# bind Ctrl+O { ocean-zoom-out }
# bind Ctrl+0 { ocean-zoom-reset }
# bind $mod+Ctrl+D  { sink-window }
# bind $mod+Ctrl+Shift+D  { ocean-dredge-window }
# bind $mod+Ctrl+Shift+U  { ocean-surface-window }

# ~~~~~~~~~~~~~~~~~ media keys ~~~~~~~~~~~~~~~~~

# Commented out since they assume tools this config can't verify you have
# installed.
# bind Print  { "spawn:grim ~/screenshot.png" }
# bind XF86AudioRaiseVolume { spawn:pactl set-sink-volume @DEFAULT_SINK@ +5% }
# bind XF86AudioLowerVolume { spawn:pactl set-sink-volume @DEFAULT_SINK@ -5% }
# bind XF86AudioMute { spawn:pactl set-sink-mute @DEFAULT_SINK@ toggle }
# bind XF86MonBrightnessUp { spawn:brightnessctl set 10%+ }
# bind XF86MonBrightnessDown { spawn:brightnessctl set 10%- }
"#;
