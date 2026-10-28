use super::*;

/// A window or workspace rule's `shader` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaderAssignment {
    None,
    Named(String),
}

impl ShaderAssignment {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase() {
            name if name == "none" => Some(Self::None),
            name if valid_ripple_preset_name(&name) => Some(Self::Named(name)),
            _ => None,
        }
    }

    /// The effect name a window uses: its rule's assignment, else its
    /// workspace's default. `none` at the window level also opts out of the
    /// workspace default.
    pub fn resolve<'a>(rule: Option<&'a Self>, workspace: Option<&'a Self>) -> Option<&'a str> {
        match rule.or(workspace)? {
            Self::Named(name) => Some(name),
            Self::None => None,
        }
    }
}

/// One ordered stage of a custom effect (`shader "name" { stage { } }`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderStage {
    /// The `file` value exactly as written.
    pub file: String,
    /// `file` resolved against the main config directory at load time.
    pub path: PathBuf,
    pub params: Arc<[(String, crate::shader_effect::ShaderParam)]>,
    /// Positional input (`tex`). `None` reads the previous stage's output,
    /// or the backdrop for the first stage.
    pub input: Option<crate::shader_effect::StageTexture>,
    /// Name this stage's output can be read back by with `"get:name"`.
    pub save: Option<String>,
    /// Extra `sampler2D` bindings, declared by the host.
    pub textures: Arc<[(String, crate::shader_effect::StageTexture)]>,
    /// Host-wrapped GLSL once `path` passed the fragment contract. `None`
    /// before loading and after a read or contract failure.
    pub source: Option<Arc<str>>,
}

/// A named custom effect (`shader "name" { }`), selected per window with
/// `rule { shader = name }` or a workspace default: up to four ordered
/// stages over a window's captured backdrop, damage-driven. Every stage but
/// the last renders offscreen; the last draws in the window's glass slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderDefinition {
    /// Processing divisor for the effect's own capture, `1..=4`.
    pub render_scale: i32,
    /// `source = "shader:<name>"`: another definition whose output stands in
    /// for the backdrop. Its stages are spliced in front of this one's at
    /// load time, so `stages` is then the whole chain.
    pub source: Option<String>,
    pub stages: Vec<ShaderStage>,
}

impl ShaderDefinition {
    /// Whether every stage's file loaded and passed the contract.
    pub fn fully_loaded(&self) -> bool {
        self.stages.iter().all(|stage| stage.source.is_some())
    }
}

pub(super) fn apply_shaders_block(raw: &mut RawConfig, body: &[waves::Entry]) {
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) if key == "enabled" => {
                set_bool(&mut raw.shaders_enabled, key, value)
            }
            _ => raw.shader_warnings.push(
                "Only `enabled` is valid in a `shaders` block; ignoring an entry".to_string(),
            ),
        }
    }
}

/// Lowers one named custom effect. A later definition with the same name
/// replaces the earlier one whole, so `stage` order is always the written
/// order of a single block. Anything this slice can't render as written
/// rejects the definition with a panel warning.
pub(super) fn lower_shader_block(raw: &mut RawConfig, header: &str, body: &[waves::Entry]) {
    let name = header.trim().to_lowercase();
    match parse_shader_definition(&name, body) {
        Ok(definition) => {
            if !raw.shader_definitions.contains_key(&name)
                && raw.shader_definitions.len() >= crate::shader_effect::MAX_DEFINITIONS
            {
                raw.shader_warnings.push(format!(
                    "Shader {name:?} skipped: at most {} definitions are allowed",
                    crate::shader_effect::MAX_DEFINITIONS
                ));
                return;
            }
            raw.shader_definitions.insert(name, definition);
        }
        Err(reason) => {
            raw.shader_definitions.remove(&name);
            raw.shader_warnings
                .push(format!("Shader {name:?} skipped: {reason}"));
        }
    }
}

fn parse_shader_definition(name: &str, body: &[waves::Entry]) -> Result<ShaderDefinition, String> {
    if !valid_ripple_preset_name(name) || name == "none" {
        return Err(
            "needs an alphanumeric, dash, or underscore name other than `none`".to_string(),
        );
    }
    let mut definition = ShaderDefinition {
        render_scale: 2,
        source: None,
        stages: Vec::new(),
    };
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match (key.as_str(), value.trim()) {
                ("scope", "window") | ("source", "backdrop") | ("invalidate", "damage-box") => {}
                ("source", value) if value.starts_with("shader:") => {
                    let source = value["shader:".len()..].trim().to_lowercase();
                    if !valid_ripple_preset_name(&source) {
                        return Err(format!("source = {value} does not name a definition"));
                    }
                    definition.source = Some(source);
                }
                ("scope" | "source" | "invalidate", other) => {
                    return Err(format!("{key} = {other} is not supported yet"));
                }
                ("render_scale", value) => match value.parse::<i32>() {
                    Ok(scale @ 1..=4) => definition.render_scale = scale,
                    _ => {
                        return Err(format!(
                            "render_scale must be an integer from 1 to 4, got {value}"
                        ))
                    }
                },
                (other, _) => return Err(format!("unknown key `{other}`")),
            },
            waves::Entry::Block(keyword, _, stage_body) if keyword == "stage" => {
                if definition.stages.len() == crate::shader_effect::MAX_STAGES {
                    return Err(format!(
                        "at most {} stages are allowed",
                        crate::shader_effect::MAX_STAGES
                    ));
                }
                definition.stages.push(parse_shader_stage(stage_body)?);
            }
            _ => return Err("only settings and `stage` blocks are allowed".to_string()),
        }
    }
    let Some(last) = definition.stages.len().checked_sub(1) else {
        return Err("needs a `stage { file = \"...\" }` block".to_string());
    };
    // The written order is the dependency order: a stage may only read
    // names saved by stages above it, so there is never a cycle to detect.
    let mut saved: Vec<&str> = Vec::new();
    for (index, stage) in definition.stages.iter().enumerate() {
        let reads = stage
            .input
            .iter()
            .chain(stage.textures.iter().map(|(_, texture)| texture));
        for texture in reads {
            if let crate::shader_effect::StageTexture::Saved(name) = texture {
                if !saved.contains(&name.as_str()) {
                    return Err(format!(
                        "stage {} reads \"get:{name}\", which no earlier stage saves",
                        index + 1
                    ));
                }
            }
        }
        if let Some(name) = &stage.save {
            if index == last {
                return Err("the last stage draws on screen and cannot `save`".to_string());
            }
            if saved.contains(&name.as_str()) {
                return Err(format!("\"{name}\" is saved by more than one stage"));
            }
            saved.push(name);
        }
    }
    Ok(definition)
}

/// Splices every `source = "shader:<name>"` definition's source chain in
/// front of its own stages, after files have loaded so each file is read
/// once. The source's last output is saved under a host-reserved name (save
/// names starting with `tide_` can't be written by hand), the definition's
/// first stage reads it positionally, and its explicit `backdrop` reads
/// become reads of that output. Unknown names, cycles, clashing save names
/// and chains over the stage cap reject the definition, and everything that
/// sources it, with a warning.
pub(crate) fn expand_shader_sources(
    definitions: &mut HashMap<String, ShaderDefinition>,
) -> Vec<String> {
    use crate::shader_effect::StageTexture;
    type Expanded = Result<Vec<ShaderStage>, String>;
    fn expand(
        name: &str,
        definitions: &HashMap<String, ShaderDefinition>,
        visiting: &mut Vec<String>,
        done: &mut HashMap<String, Expanded>,
    ) -> Expanded {
        if let Some(result) = done.get(name) {
            return result.clone();
        }
        if visiting.iter().any(|visited| visited == name) {
            return Err(format!("its source chain loops back to {name:?}"));
        }
        let definition = definitions
            .get(name)
            .ok_or_else(|| format!("its source {name:?} is not a loaded definition"))?;
        let Some(source) = &definition.source else {
            return Ok(definition.stages.clone());
        };
        visiting.push(name.to_string());
        let base = expand(source, definitions, visiting, done);
        visiting.pop();
        let mut stages = base?;
        let saved = format!("tide_source_{}", stages.len());
        if let Some(last) = stages.last_mut() {
            last.save = Some(saved.clone());
        }
        let redirect = |texture: &StageTexture| match texture {
            StageTexture::Backdrop => StageTexture::Saved(saved.clone()),
            other => other.clone(),
        };
        for stage in &definition.stages {
            if let Some(name) = stage.save.as_ref().filter(|name| {
                stages
                    .iter()
                    .any(|earlier| earlier.save.as_ref() == Some(*name))
            }) {
                return Err(format!("it and its source both save \"{name}\""));
            }
            let mut stage = stage.clone();
            stage.input = stage.input.as_ref().map(redirect);
            stage.textures = stage
                .textures
                .iter()
                .map(|(binding, texture)| (binding.clone(), redirect(texture)))
                .collect::<Vec<_>>()
                .into();
            stages.push(stage);
        }
        if stages.len() > crate::shader_effect::MAX_STAGES {
            return Err(format!(
                "with its source it has {} stages, more than {}",
                stages.len(),
                crate::shader_effect::MAX_STAGES
            ));
        }
        Ok(stages)
    }

    let mut names: Vec<String> = definitions
        .iter()
        .filter(|(_, definition)| definition.source.is_some())
        .map(|(name, _)| name.clone())
        .collect();
    names.sort_unstable();
    let mut done = HashMap::new();
    let mut results = Vec::new();
    for name in names {
        let result = expand(&name, definitions, &mut Vec::new(), &mut done);
        done.insert(name.clone(), result.clone());
        results.push((name, result));
    }
    let mut warnings = Vec::new();
    for (name, result) in results {
        match result {
            Ok(stages) => {
                if let Some(definition) = definitions.get_mut(&name) {
                    definition.stages = stages;
                }
            }
            Err(reason) => {
                definitions.remove(&name);
                warnings.push(format!("Shader {name:?} skipped: {reason}"));
            }
        }
    }
    warnings
}

fn parse_shader_stage(body: &[waves::Entry]) -> Result<ShaderStage, String> {
    use crate::shader_effect::StageTexture;
    let mut file = None;
    let mut params = Vec::new();
    let mut input = None;
    let mut save = None;
    let mut textures: Vec<(String, StageTexture)> = Vec::new();
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) if key == "file" => file = Some(value.clone()),
            waves::Entry::Assign(key, value) if key == "source" => {
                input = Some(StageTexture::parse(value).ok_or_else(|| {
                    format!("stage source must be backdrop or \"get:name\", got {value}")
                })?);
            }
            waves::Entry::Assign(key, value) if key == "save" => {
                let name = value.trim();
                if !crate::shader_effect::valid_param_name(name) {
                    return Err(format!("`{name}` cannot be a saved texture name"));
                }
                save = Some(name.to_string());
            }
            waves::Entry::Block(keyword, _, texture_body) if keyword == "textures" => {
                for binding in texture_body {
                    let waves::Entry::Assign(name, value) = binding else {
                        return Err(
                            "`textures` may only contain `name = source` entries".to_string()
                        );
                    };
                    if !crate::shader_effect::valid_param_name(name) {
                        return Err(format!("`{name}` cannot be a texture binding name"));
                    }
                    let texture = StageTexture::parse(value).ok_or_else(|| {
                        format!("texture `{name}` must be backdrop or \"get:name\", got {value}")
                    })?;
                    if let Some(existing) =
                        textures.iter_mut().find(|(existing, _)| existing == name)
                    {
                        existing.1 = texture;
                    } else {
                        textures.push((name.clone(), texture));
                    }
                }
                if textures.len() > crate::shader_effect::MAX_TEXTURES_PER_STAGE {
                    return Err(format!(
                        "at most {} extra textures per stage are allowed",
                        crate::shader_effect::MAX_TEXTURES_PER_STAGE
                    ));
                }
            }
            waves::Entry::Block(keyword, _, param_body) if keyword == "params" => {
                for param in param_body {
                    let waves::Entry::Assign(name, value) = param else {
                        return Err("`params` may only contain `name = value` entries".to_string());
                    };
                    if !crate::shader_effect::valid_param_name(name) {
                        return Err(format!("`{name}` cannot be a parameter name"));
                    }
                    let value = parse_shader_param(value).ok_or_else(|| {
                        format!("parameter `{name}` needs a finite number, a list of 2 to 4 numbers, or a color")
                    })?;
                    if let Some(existing) = params.iter_mut().find(|(existing, _)| existing == name)
                    {
                        existing.1 = value;
                    } else {
                        params.push((name.clone(), value));
                    }
                }
                if params.len() > crate::shader_effect::MAX_PARAMS_PER_STAGE {
                    return Err(format!(
                        "at most {} parameters per stage are allowed",
                        crate::shader_effect::MAX_PARAMS_PER_STAGE
                    ));
                }
            }
            waves::Entry::Assign(key, _) | waves::Entry::Block(key, _, _) => {
                return Err(format!("`{key}` is not supported in a stage yet"));
            }
            _ => return Err("unexpected entry in a stage".to_string()),
        }
    }
    let file = file
        .filter(|file| !file.trim().is_empty())
        .ok_or_else(|| "a stage needs `file = \"path.frag\"`".to_string())?;
    if let Some((name, _)) = textures
        .iter()
        .find(|(name, _)| params.iter().any(|(param, _)| param == name))
    {
        return Err(format!("`{name}` is both a parameter and a texture"));
    }
    Ok(ShaderStage {
        file,
        path: PathBuf::new(),
        params: params.into(),
        input,
        save,
        textures: textures.into(),
        source: None,
    })
}

/// A color, a number, or a list of 2 to 4 numbers. Wave serializes colors
/// without the `#`, so six or eight hex digits with a letter among them read
/// as a color (even `1E0000`, which would also parse as a float), and a
/// color whose digits are all decimal has to be quoted with its `#`.
fn parse_shader_param(value: &str) -> Option<crate::shader_effect::ShaderParam> {
    use crate::shader_effect::ShaderParam;
    let bare = value.trim();
    if matches!(bare.len(), 6 | 8)
        && bare.chars().all(|c| c.is_ascii_hexdigit())
        && bare.chars().any(|c| c.is_ascii_alphabetic())
    {
        return parse_rgba_color(bare).map(ShaderParam::Vec4);
    }
    let finite = |item: &str| item.trim().parse::<f32>().ok().filter(|v| v.is_finite());
    if let Some(number) = finite(value) {
        return Some(ShaderParam::Float(number));
    }
    if let Some(items) = parse_list_value(value) {
        let items: Option<Vec<f32>> = items.iter().map(|item| finite(item)).collect();
        return match items?.as_slice() {
            [x, y] => Some(ShaderParam::Vec2([*x, *y])),
            [x, y, z] => Some(ShaderParam::Vec3([*x, *y, *z])),
            [x, y, z, w] => Some(ShaderParam::Vec4([*x, *y, *z, *w])),
            _ => None,
        };
    }
    parse_rgba_color(value).map(ShaderParam::Vec4)
}
