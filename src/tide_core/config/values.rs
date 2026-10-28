/// Lowers a fully-merged Waves entry list into a `RawConfig`, starting
/// from its defaults and overwriting whatever was actually present.
/// Unknown keys/blocks warn and are ignored rather than failing the whole
/// config (a typo shouldn't take down a working session).
/// Parses the Wave engine's serialized list form (`["a", "b"]`) into
/// items; returns `None` for anything that is not a list.
pub(super) fn parse_list_value(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                items.push(current.trim().to_string());
                current.clear();
            }
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            _ => current.push(c),
        }
    }
    items.push(current.trim().to_string());
    let items: Vec<String> = items.into_iter().filter(|s| !s.is_empty()).collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Parses a duration value into milliseconds: a bare number of
/// milliseconds (the `_ms` keys) or a unit-suffixed literal (`600ms`, `1.5s`,
/// `90m`) as the Wave engine's duration values serialize to. Returns
/// `None` for zero/negative or unparseable values.
pub(super) fn parse_duration_ms(value: &str) -> Option<u32> {
    parse_duration_ms_including_zero(value).filter(|ms| *ms > 0)
}

/// The non-negative duration parser used by fields where zero has an
/// explicit meaning. Keep this separate from [`parse_duration_ms`] so the
/// many animation lifetimes that require progress cannot silently become
/// zero-length while values such as `workspace_motion_delay = 0ms` remain
/// valid.
pub(super) fn parse_duration_ms_including_zero(value: &str) -> Option<u32> {
    let value = value.trim();
    let (num, scale) = if let Some(num) = value.strip_suffix("ms") {
        (num, 1.0)
    } else if let Some(num) = value.strip_suffix('s') {
        (num, 1000.0)
    } else if let Some(num) = value.strip_suffix('m') {
        (num, 60_000.0)
    } else {
        (value, 1.0)
    };
    let n = num.trim().parse::<f64>().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let ms = (n * scale).round();
    if !ms.is_finite() || ms > f64::from(u32::MAX) {
        return None;
    }
    Some(ms as u32)
}

/// The items of a Wave list value (`[a, b]`), or a lone scalar as a
/// one-item list.
pub(super) fn list_items(value: &str) -> Vec<String> {
    parse_list_value(value).unwrap_or_else(|| vec![value.trim().to_string()])
}

/// Parses a list of non-negative integers. `None` when any item fails to
/// parse. Used by the caustics idle-decay tiers.
pub(super) fn parse_u32_list(value: &str) -> Option<Vec<u32>> {
    let items = list_items(value);
    let list: Vec<u32> = items
        .iter()
        .map(|token| token.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!list.is_empty()).then_some(list)
}

/// A list of duration values (`[600ms, 1s]`). Each item goes through
/// [`parse_duration_ms`].
pub(super) fn parse_duration_list(value: &str) -> Option<Vec<u64>> {
    let items = list_items(value);
    let list: Vec<u64> = items
        .iter()
        .map(|item| parse_duration_ms(item).map(u64::from))
        .collect::<Option<Vec<_>>>()?;
    (!list.is_empty()).then_some(list)
}

pub(super) fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub(super) fn decode_hex_rgb(hex: &str) -> Option<[f32; 3]> {
    let hex = match hex.len() {
        6 => hex,
        8 => &hex[..6],
        _ => return None,
    };
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

/// Shadow colors keep their alpha channel, unlike ripple/frost colors which
/// have a separate dedicated alpha knob. Accepts bare or `#` hex
/// (`RRGGBB` or `RRGGBBAA`).
pub(super) fn parse_rgba_color(value: &str) -> Option<[f32; 4]> {
    let value = value
        .trim()
        .strip_prefix('#')
        .unwrap_or_else(|| value.trim());
    decode_hex_rgba(value)
}

fn decode_hex_rgba(hex: &str) -> Option<[f32; 4]> {
    let (rgb, alpha) = match hex.len() {
        6 => (hex, 255),
        8 => (&hex[..6], u8::from_str_radix(&hex[6..8], 16).ok()?),
        _ => return None,
    };
    let color = decode_hex_rgb(rgb)?;
    Some([color[0], color[1], color[2], alpha as f32 / 255.0])
}

pub(super) fn parse_f32_clamped(value: &str, min: f32, max: f32, key: &str) -> Option<f32> {
    match value.parse::<f32>() {
        Ok(value) if value.is_finite() => Some(value.clamp(min, max)),
        _ => {
            tracing::warn!(value, key, "Expected a finite numeric value, ignoring");
            None
        }
    }
}

/// Validates the platform-independent subset accepted by Unix `setenv`.
/// Keeping this beside config lowering turns malformed Wave entries into a
/// visible diagnostic instead of letting `std::env::set_var` panic during
/// compositor startup. Values are deliberately not included in diagnostics:
/// environment variables can carry secrets.
pub(crate) fn validate_env_entry(key: &str, value: &str) -> Result<(), &'static str> {
    if key.is_empty() {
        return Err("name must not be empty");
    }
    if key.contains('=') {
        return Err("name must not contain '='");
    }
    if key.contains('\0') {
        return Err("name must not contain NUL");
    }
    if value.contains('\0') {
        return Err("value must not contain NUL");
    }
    Ok(())
}

/// Mutates `field` only on a successful parse -- a bad value logs a
/// warning and leaves whatever was already there (the default, or an
/// earlier include's value) rather than silently resetting it.
pub(super) fn set_bool(field: &mut bool, key: &str, value: &str) {
    match value {
        "true" => *field = true,
        "false" => *field = false,
        _ => tracing::warn!(key, value, "Expected `true` or `false`, ignoring"),
    }
}

pub(super) fn set_opt_bool(field: &mut Option<bool>, key: &str, value: &str) {
    match value {
        "true" => *field = Some(true),
        "false" => *field = Some(false),
        _ => tracing::warn!(key, value, "Expected `true` or `false`, ignoring"),
    }
}

pub(super) fn set_i32(field: &mut i32, key: &str, value: &str) {
    match value.parse() {
        Ok(n) => *field = n,
        Err(_) => tracing::warn!(key, value, "Expected an integer, ignoring"),
    }
}

pub(super) fn set_f64(field: &mut f64, key: &str, value: &str) {
    match value.parse::<f64>() {
        Ok(n) if n.is_finite() => *field = n,
        _ => tracing::warn!(key, value, "Expected a finite number, ignoring"),
    }
}
