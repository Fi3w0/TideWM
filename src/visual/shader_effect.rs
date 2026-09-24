//! Custom `.frag` effects: the fixed fragment contract, the host wrapper and
//! bounded file loading (Phase 7, see `SHADER_DESIGN.md`).
//!
//! A user file defines `vec4 tide_effect(vec2 uv)` plus optional helpers and
//! returns premultiplied RGBA. The host owns everything else: `#version`,
//! precision, Smithay's `//_DEFINES_` variants, every uniform declaration,
//! `main`, opacity and rounded clipping. The contract check below is API
//! hygiene over a small GLSL token stream, not an execution sandbox; custom
//! shaders are trusted native GPU programs.

use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
    time::Instant,
};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            ffi, GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
            UniformName, UniformType, UniformValue,
        },
        utils::CommitCounter,
        ContextId, Renderer, Texture,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale, Transform},
};

/// Largest accepted `.frag` source.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Largest total of wrapped sources one configuration may load.
pub const MAX_CANDIDATE_BYTES: usize = 1024 * 1024;
pub const MAX_DEFINITIONS: usize = 32;
pub const MAX_STAGES: usize = 4;
pub const MAX_PARAMS_PER_STAGE: usize = 8;
pub const MAX_NAME_BYTES: usize = 64;
/// Windows that may draw a custom effect at once.
pub const MAX_ACTIVE_INSTANCES: usize = 128;
/// Aggregate RGBA8-equivalent payload of every custom effect capture.
pub const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// `ceil(w / divisor) * ceil(h / divisor) * 4`, saturating.
pub fn capture_payload_bytes(width: i32, height: i32, divisor: i32) -> u64 {
    let divisor = i64::from(divisor.max(1));
    let side = |length: i32| {
        u64::try_from((i64::from(length.max(0)) + divisor - 1) / divisor).unwrap_or(0)
    };
    side(width).saturating_mul(side(height)).saturating_mul(4)
}

/// The only function a user file must define.
pub const ENTRY_POINT: &str = "tide_effect";

/// Host-declared uniforms every effect can read, in draw order.
pub const CONTRACT_UNIFORMS: &[(&str, &str)] = &[
    ("u_size", "vec2"),
    ("u_texture_size", "vec2"),
    ("u_content_rect", "vec4"),
    ("u_texel", "vec2"),
    ("u_time", "float"),
    ("u_delta", "float"),
    ("u_corner_radii", "vec4"),
    ("u_rounding_power", "float"),
    ("u_antialias", "float"),
];

/// Names that belong to Smithay's texture program or the host output path.
/// They may not appear in user source at all.
const FORBIDDEN_IDENTIFIERS: &[&str] = &[
    "main",
    "alpha",
    "tint",
    "v_coords",
    "matrix",
    "tex_matrix",
    "vert",
    "vert_position",
    "gl_FragColor",
    "gl_FragData",
];

/// The host emits every declaration and the precision statement.
const FORBIDDEN_KEYWORDS: &[&str] = &["uniform", "attribute", "varying", "precision", "invariant"];

const ALLOWED_DIRECTIVES: &[&str] = &[
    "define", "undef", "if", "ifdef", "ifndef", "else", "elif", "endif", "error",
];

const BUILTIN_TYPES: &[&str] = &[
    "void",
    "bool",
    "int",
    "float",
    "vec2",
    "vec3",
    "vec4",
    "bvec2",
    "bvec3",
    "bvec4",
    "ivec2",
    "ivec3",
    "ivec4",
    "mat2",
    "mat3",
    "mat4",
    "sampler2D",
    "samplerCube",
];

/// GLSL ES 1.00 keywords and reserved words a parameter name may not use.
const GLSL_KEYWORDS: &[&str] = &[
    "attribute",
    "const",
    "uniform",
    "varying",
    "break",
    "continue",
    "do",
    "for",
    "while",
    "if",
    "else",
    "in",
    "out",
    "inout",
    "true",
    "false",
    "lowp",
    "mediump",
    "highp",
    "precision",
    "invariant",
    "discard",
    "return",
    "struct",
    "asm",
    "class",
    "union",
    "enum",
    "typedef",
    "template",
    "this",
    "packed",
    "goto",
    "switch",
    "default",
    "inline",
    "noinline",
    "volatile",
    "public",
    "static",
    "extern",
    "external",
    "interface",
    "flat",
    "long",
    "short",
    "double",
    "half",
    "fixed",
    "unsigned",
    "superp",
    "input",
    "output",
    "hvec2",
    "hvec3",
    "hvec4",
    "dvec2",
    "dvec3",
    "dvec4",
    "fvec2",
    "fvec3",
    "fvec4",
    "sampler1D",
    "sampler3D",
    "sampler1DShadow",
    "sampler2DShadow",
    "sampler2DRect",
    "sampler3DRect",
    "sampler2DRectShadow",
    "sizeof",
    "cast",
    "namespace",
    "using",
];

/// One user parameter, lowered to a GLSL uniform type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShaderParam {
    Float(f32),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    Vec4([f32; 4]),
}

impl ShaderParam {
    pub fn glsl_type(&self) -> &'static str {
        match self {
            Self::Float(_) => "float",
            Self::Vec2(_) => "vec2",
            Self::Vec3(_) => "vec3",
            Self::Vec4(_) => "vec4",
        }
    }
}

impl ShaderParam {
    fn uniform_type(&self) -> UniformType {
        match self {
            Self::Float(_) => UniformType::_1f,
            Self::Vec2(_) => UniformType::_2f,
            Self::Vec3(_) => UniformType::_3f,
            Self::Vec4(_) => UniformType::_4f,
        }
    }

    fn uniform_value(&self) -> UniformValue {
        match *self {
            Self::Float(value) => value.into(),
            Self::Vec2(value) => value.into(),
            Self::Vec3(value) => value.into(),
            Self::Vec4(value) => value.into(),
        }
    }

    fn hash_bits(&self, hash: &mut impl Hasher) {
        let values: &[f32] = match self {
            Self::Float(value) => std::slice::from_ref(value),
            Self::Vec2(value) => value,
            Self::Vec3(value) => value,
            Self::Vec4(value) => value,
        };
        for value in values {
            value.to_bits().hash(hash);
        }
    }
}

/// Whether `name` can become a user parameter uniform: an ASCII GLSL
/// identifier within the length cap that cannot shadow host names.
pub fn valid_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= MAX_NAME_BYTES
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.contains("__")
        && !reserved_prefix(name)
        && !FORBIDDEN_IDENTIFIERS.contains(&name)
        && !BUILTIN_TYPES.contains(&name)
        && !GLSL_KEYWORDS.contains(&name)
        && name != "tex"
        && name != "size"
}

fn reserved_prefix(name: &str) -> bool {
    name.starts_with("u_") || name.starts_with("gl_") || name.starts_with("tide_")
}

#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    Ident(&'a str),
    Punct(char),
    Other,
}

/// Replaces comments with spaces, keeping every newline so driver line
/// numbers still match the user's file. Comments are dropped rather than
/// passed through because Smithay substitutes every `//_DEFINES_` marker in
/// the whole source, including one inside a user comment.
fn strip_comments(source: &str) -> Result<String, String> {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            loop {
                if index >= bytes.len() {
                    return Err("unterminated /* comment".to_string());
                }
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    index += 2;
                    out.push(' ');
                    break;
                }
                if bytes[index] == b'\n' {
                    out.push('\n');
                }
                index += 1;
            }
        } else {
            let ch = source[index..]
                .chars()
                .next()
                .expect("index is a char boundary");
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    Ok(out)
}

fn tokenize(line: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(Token::Ident(&line[start..index]));
        } else if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit))
        {
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'.')
            {
                index += 1;
            }
            tokens.push(Token::Other);
        } else if byte.is_ascii_whitespace() {
            index += 1;
        } else {
            tokens.push(Token::Punct(byte as char));
            index += 1;
        }
    }
    tokens
}

/// Checks a user `.frag` against the fixed contract: no NUL, ASCII outside
/// comments, only conditional/define directives, no host-owned declarations
/// or identifiers, and exactly one `vec4 tide_effect(vec2 uv)` definition.
pub fn validate_fragment_contract(source: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "source is {} bytes, the limit is {MAX_SOURCE_BYTES}",
            source.len()
        ));
    }
    if source.contains('\0') {
        return Err("source contains a NUL byte".to_string());
    }
    let code = strip_comments(source)?;
    let mut tokens = Vec::new();
    for (line_no, line) in code.lines().enumerate() {
        let line_no = line_no + 1;
        if let Some(bad) = line.chars().find(|c| !c.is_ascii()) {
            return Err(format!(
                "line {line_no}: non-ASCII character {bad:?} outside a comment"
            ));
        }
        let trimmed = line.trim_start();
        if let Some(directive) = trimmed.strip_prefix('#') {
            let name = directive
                .trim_start()
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !ALLOWED_DIRECTIVES.contains(&name) {
                return Err(format!(
                    "line {line_no}: #{name} is not allowed, the host owns the GLSL version, extensions and line mapping"
                ));
            }
            if directive.contains("##") {
                return Err(format!("line {line_no}: token pasting (##) is not allowed"));
            }
        }
        for token in tokenize(line) {
            if let Token::Ident(ident) = token {
                if FORBIDDEN_IDENTIFIERS.contains(&ident) {
                    return Err(format!(
                        "line {line_no}: `{ident}` is reserved, the host writes the output and applies opacity"
                    ));
                }
                if FORBIDDEN_KEYWORDS.contains(&ident) {
                    return Err(format!(
                        "line {line_no}: `{ident}` is not allowed, the host emits every declaration and the precision"
                    ));
                }
            }
            tokens.push((line_no, token));
        }
    }

    let mut types: HashSet<&str> = BUILTIN_TYPES.iter().copied().collect();
    let mut entry_definitions = 0usize;
    let mut depth = 0i32;
    let mut index = 0;
    while index < tokens.len() {
        let (line_no, token) = &tokens[index];
        match token {
            Token::Punct('{') => depth += 1,
            Token::Punct('}') => depth -= 1,
            Token::Ident("struct") => {
                if let Some((_, Token::Ident(name))) = tokens.get(index + 1) {
                    check_declared_name(name, *line_no)?;
                    types.insert(name);
                }
            }
            Token::Ident(ty) if types.contains(ty) => {
                if let Some((decl_line, Token::Ident(name))) = tokens.get(index + 1) {
                    if *name == ENTRY_POINT {
                        let signature = tokens.get(index + 2..index + 6).map(|window| {
                            window
                                .iter()
                                .map(|(_, token)| token.clone())
                                .collect::<Vec<_>>()
                        });
                        let is_definition = *ty == "vec4"
                            && depth == 0
                            && matches!(
                                signature.as_deref(),
                                Some([
                                    Token::Punct('('),
                                    Token::Ident("vec2"),
                                    Token::Ident(_),
                                    Token::Punct(')')
                                ])
                            );
                        if !is_definition {
                            return Err(format!(
                                "line {decl_line}: `{ENTRY_POINT}` must be declared as `vec4 {ENTRY_POINT}(vec2 uv)`"
                            ));
                        }
                        if matches!(tokens.get(index + 6), Some((_, Token::Punct('{')))) {
                            entry_definitions += 1;
                        }
                    } else {
                        check_declared_name(name, *decl_line)?;
                        check_declarator_list(&tokens, index + 2)?;
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
    match entry_definitions {
        0 => Err(format!("no `vec4 {ENTRY_POINT}(vec2 uv)` definition")),
        1 => Ok(()),
        _ => Err(format!("`{ENTRY_POINT}` is defined more than once")),
    }
}

/// Checks the trailing declarators of `float a = f(x, y), b;`. Function
/// parameters need no special case: each has its own type token.
fn check_declarator_list(tokens: &[(usize, Token<'_>)], start: usize) -> Result<(), String> {
    let mut nesting = 0i32;
    let mut index = start;
    while let Some((line_no, token)) = tokens.get(index) {
        match token {
            Token::Punct('(') | Token::Punct('[') => nesting += 1,
            Token::Punct(')') | Token::Punct(']') if nesting > 0 => nesting -= 1,
            Token::Punct(')') | Token::Punct(';') | Token::Punct('{') if nesting == 0 => {
                return Ok(());
            }
            Token::Punct(',') if nesting == 0 => {
                if let Some((_, Token::Ident(name))) = tokens.get(index + 1) {
                    check_declared_name(name, *line_no)?;
                }
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

fn check_declared_name(name: &str, line_no: usize) -> Result<(), String> {
    if name == "tex" || name == "size" || reserved_prefix(name) {
        return Err(format!(
            "line {line_no}: `{name}` is reserved for host uniforms and may not be declared"
        ));
    }
    Ok(())
}

/// Wraps a validated user source into a Smithay texture-shader program:
/// host prelude (source string 0), the user file from line 1 (string 1),
/// then the host epilogue (string 2) so driver logs name the right file.
pub fn wrap_texture_fragment(user_source: &str, params: &[(String, ShaderParam)]) -> String {
    let code = strip_comments(user_source).unwrap_or_else(|_| user_source.to_string());
    let mut out = String::with_capacity(code.len() + 2048);
    out.push_str(
        "#version 100\n\
         //_DEFINES_\n\
         #if defined(EXTERNAL)\n\
         #extension GL_OES_EGL_image_external : require\n\
         #endif\n\
         precision highp float;\n\
         #if defined(EXTERNAL)\n\
         uniform samplerExternalOES tex;\n\
         #else\n\
         uniform sampler2D tex;\n\
         #endif\n\
         uniform float alpha;\n\
         #if defined(DEBUG_FLAGS)\n\
         uniform float tint;\n\
         #endif\n\
         varying vec2 v_coords;\n",
    );
    for (name, ty) in CONTRACT_UNIFORMS {
        out.push_str(&format!("uniform {ty} {name};\n"));
    }
    for (name, value) in params {
        out.push_str(&format!("uniform {} {name};\n", value.glsl_type()));
    }
    out.push_str("#line 1 1\n");
    out.push_str(&code);
    if !code.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(
        "#line 1 2\n\
         float tide_rounded_mask() {\n\
         vec2 point = v_coords * u_size;\n\
         vec2 center;\n\
         float radius;\n\
         if (point.x < u_corner_radii.x && point.y < u_corner_radii.x) {\n\
         radius = u_corner_radii.x;\n\
         center = vec2(radius);\n\
         } else if (point.x > u_size.x - u_corner_radii.y && point.y < u_corner_radii.y) {\n\
         radius = u_corner_radii.y;\n\
         center = vec2(u_size.x - radius, radius);\n\
         } else if (point.x > u_size.x - u_corner_radii.z && point.y > u_size.y - u_corner_radii.z) {\n\
         radius = u_corner_radii.z;\n\
         center = vec2(u_size.x - radius, u_size.y - radius);\n\
         } else if (point.x < u_corner_radii.w && point.y > u_size.y - u_corner_radii.w) {\n\
         radius = u_corner_radii.w;\n\
         center = vec2(radius, u_size.y - radius);\n\
         } else {\n\
         return 1.0;\n\
         }\n\
         radius = min(radius, min(u_size.x, u_size.y) * 0.5);\n\
         if (radius < 0.01)\n\
         return 1.0;\n\
         vec2 q = abs(point - center) / radius;\n\
         float distance = pow(pow(q.x, u_rounding_power) + pow(q.y, u_rounding_power), 1.0 / u_rounding_power);\n\
         float antialias = u_antialias / radius;\n\
         return 1.0 - smoothstep(1.0 - antialias, 1.0 + antialias, distance);\n\
         }\n\
         void main() {\n\
         vec4 color = tide_effect(v_coords);\n\
         color.a = clamp(color.a, 0.0, 1.0);\n\
         color.rgb = clamp(color.rgb, vec3(0.0), vec3(color.a));\n\
         color *= tide_rounded_mask() * alpha;\n\
         #if defined(DEBUG_FLAGS)\n\
         if (tint == 1.0)\n\
         color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;\n\
         #endif\n\
         gl_FragColor = color;\n\
         }\n",
    );
    out
}

/// Reads one `.frag` file without following it into a FIFO or device and
/// without trusting metadata length: at most `MAX_SOURCE_BYTES + 1` bytes are
/// read, then the result must be UTF-8.
pub fn read_fragment_file(path: &Path) -> Result<String, String> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("frag") {
        return Err("shader files must use the .frag extension".to_string());
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|err| err.to_string())?;
    let metadata = file.metadata().map_err(|err| err.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".to_string());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(format!("larger than the {MAX_SOURCE_BYTES}-byte limit"));
    }
    String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())
}

fn contract_uniform_type(glsl_type: &str) -> UniformType {
    match glsl_type {
        "vec2" => UniformType::_2f,
        "vec4" => UniformType::_4f,
        _ => UniformType::_1f,
    }
}

fn source_hash(source: &str) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    hash.finish()
}

/// A compiled program together with the parameter schema it was built for.
/// Drawing with any other schema would bind uniforms the program lacks.
#[derive(Clone)]
pub struct ShaderProgram {
    /// Hash of the wrapped source, for damage identity.
    pub generation: u64,
    pub program: GlesTexProgram,
    pub params: Arc<[(String, ShaderParam)]>,
}

#[derive(Default)]
struct CachedProgram {
    good: Option<ShaderProgram>,
    good_source: Option<Arc<str>>,
    /// Source that failed to compile, never retried until it changes.
    failed: Option<Arc<str>>,
    /// The driver's reason for `failed`, re-reported after a reload.
    failure_reason: Option<String>,
}

/// Pointer equality first so a steady frame never rereads the source; a
/// reload hands in a fresh `Arc`, which is adopted once it compares equal.
fn same_source(cached: &mut Option<Arc<str>>, source: &Arc<str>) -> bool {
    match cached {
        Some(cached) if Arc::ptr_eq(cached, source) => true,
        Some(cached) if **cached == **source => {
            *cached = source.clone();
            true
        }
        _ => false,
    }
}

pub struct ProgramLookup {
    pub program: Option<ShaderProgram>,
    /// Set once, the first time a new source fails to compile.
    pub failure: Option<String>,
    /// A compile was needed but this frame's budget was spent.
    pub deferred: bool,
}

/// Last-good compiled program per definition name. A changed source
/// compiles once; a failure is remembered so a broken file never recompiles
/// every frame, and the previous program stays in use until the file is
/// fixed. Removing a definition drops its entry.
#[derive(Default)]
pub struct CustomShaderPrograms {
    entries: HashMap<String, CachedProgram>,
    /// Programs belong to one GL context; a replacement renderer drops them.
    context: Option<ContextId<GlesTexture>>,
    compiles: u64,
    compile_failures: u64,
}

impl CustomShaderPrograms {
    /// `(programs, compiles, compile_failures)` for `tidectl perf`.
    pub fn stats(&self) -> (usize, u64, u64) {
        let programs = self
            .entries
            .values()
            .filter(|entry| entry.good.is_some())
            .count();
        (programs, self.compiles, self.compile_failures)
    }

    /// Whether `lookup` could return a program without compiling anything
    /// known to fail: a last-good program, or a source not yet tried.
    pub fn can_draw(&self, name: &str, source: Option<&Arc<str>>) -> bool {
        let Some(entry) = self.entries.get(name) else {
            return source.is_some();
        };
        entry.good.is_some()
            || source.is_some_and(|source| {
                entry
                    .failed
                    .as_ref()
                    .is_none_or(|failed| !Arc::ptr_eq(failed, source) && **failed != **source)
            })
    }

    /// The program to draw `name` with this frame. `source` is the current
    /// wrapped source, `None` after a read or contract failure; either way a
    /// last-good program keeps drawing. At most `compile_budget` new
    /// programs compile per call site and frame.
    pub fn lookup(
        &mut self,
        renderer: &mut GlesRenderer,
        name: &str,
        source: Option<&Arc<str>>,
        params: &Arc<[(String, ShaderParam)]>,
        compile_budget: &mut usize,
    ) -> ProgramLookup {
        let context = renderer.context_id();
        if self.context.as_ref() != Some(&context) {
            self.entries.clear();
            self.context = Some(context);
        }
        if !self.entries.contains_key(name) {
            self.entries
                .insert(name.to_string(), CachedProgram::default());
        }
        let Self {
            entries,
            compiles,
            compile_failures,
            ..
        } = self;
        let entry = entries.get_mut(name).expect("entry inserted above");
        let mut lookup = ProgramLookup {
            program: None,
            failure: None,
            deferred: false,
        };
        let Some(source) = source else {
            lookup.program = entry.good.clone();
            return lookup;
        };
        if same_source(&mut entry.good_source, source) {
            if let Some(good) = &mut entry.good {
                // Same source means the same declarations, so only values moved.
                good.params = params.clone();
            }
            lookup.program = entry.good.clone();
            return lookup;
        }
        if same_source(&mut entry.failed, source) {
            lookup.program = entry.good.clone();
            return lookup;
        }
        if *compile_budget == 0 {
            lookup.deferred = true;
            lookup.program = entry.good.clone();
            return lookup;
        }
        *compile_budget -= 1;
        *compiles += 1;
        let uniforms: Vec<UniformName<'_>> = CONTRACT_UNIFORMS
            .iter()
            .map(|(name, ty)| UniformName::new(*name, contract_uniform_type(ty)))
            .chain(
                params
                    .iter()
                    .map(|(name, value)| UniformName::new(name.as_str(), value.uniform_type())),
            )
            .collect();
        match renderer.compile_custom_texture_shader(&**source, &uniforms) {
            Ok(program) => {
                entry.failed = None;
                entry.good_source = Some(source.clone());
                entry.good = Some(ShaderProgram {
                    generation: source_hash(source),
                    program,
                    params: params.clone(),
                });
                lookup.program = entry.good.clone();
            }
            Err(err) => {
                *compile_failures += 1;
                let reason = driver_compile_log(renderer, source)
                    .unwrap_or_else(|| format!("{err}; the driver log is in the journal"));
                lookup.failure = Some(compile_failure_message(name, &reason, entry.good.is_some()));
                entry.failed = Some(source.clone());
                entry.failure_reason = Some(reason);
                lookup.program = entry.good.clone();
            }
        }
        lookup
    }

    /// Drops entries for removed definitions and reports every definition
    /// whose current source is still the one that failed, so a reload
    /// doesn't clear a live compile diagnostic from the panel.
    pub fn retain_definitions(
        &mut self,
        definitions: &HashMap<String, crate::config::ShaderDefinition>,
    ) -> Vec<String> {
        self.entries
            .retain(|name, _| definitions.contains_key(name));
        let mut failures: Vec<String> = self
            .entries
            .iter_mut()
            .filter_map(|(name, entry)| {
                let source = definitions[name].single_stage_source()?;
                same_source(&mut entry.failed, source).then(|| {
                    compile_failure_message(
                        name,
                        entry.failure_reason.as_deref().unwrap_or("compile error"),
                        entry.good.is_some(),
                    )
                })
            })
            .collect();
        failures.sort_unstable();
        failures
    }
}

fn compile_failure_message(name: &str, reason: &str, has_previous: bool) -> String {
    format!(
        "Shader {name:?} did not compile: {reason}; {}",
        if has_previous {
            "keeping the previous program"
        } else {
            "rendering the window without it"
        }
    )
}

/// Longest driver diagnostic carried onto the panel.
const MAX_DRIVER_LOG_BYTES: usize = 240;

/// Smithay reports a failed compile only as `ShaderCompileError` and sends
/// the driver's text to the journal, so a failure is compiled once more
/// here, alone, to read that text for the panel. Runs only after a failure,
/// never per frame.
fn driver_compile_log(renderer: &mut GlesRenderer, source: &str) -> Option<String> {
    let source = source.replace("//_DEFINES_", "");
    let log = renderer
        .with_context(|gl| {
            // SAFETY: `with_context` made this renderer's context current;
            // the pointers stay valid for these calls and the shader object
            // is deleted before returning.
            unsafe {
                let shader = gl.CreateShader(ffi::FRAGMENT_SHADER);
                if shader == 0 {
                    return None;
                }
                let pointer = source.as_ptr() as *const ffi::types::GLchar;
                let length = source.len() as ffi::types::GLint;
                gl.ShaderSource(shader, 1, &pointer, &length);
                gl.CompileShader(shader);
                let mut log_length = 0;
                gl.GetShaderiv(shader, ffi::INFO_LOG_LENGTH, &mut log_length);
                let mut log = vec![0u8; log_length.max(0) as usize];
                let mut written = 0;
                gl.GetShaderInfoLog(
                    shader,
                    log_length,
                    &mut written,
                    log.as_mut_ptr() as *mut ffi::types::GLchar,
                );
                gl.DeleteShader(shader);
                log.truncate(written.max(0) as usize);
                Some(String::from_utf8_lossy(&log).into_owned())
            }
        })
        .ok()??;
    summarize_driver_log(&log)
}

/// First error line of a driver log, with Mesa's `1:LINE(COL)` source
/// prefix rewritten to the user's file (source string 1 in the wrapper).
fn summarize_driver_log(log: &str) -> Option<String> {
    let line = log
        .lines()
        .map(str::trim)
        .find(|line| line.to_ascii_lowercase().contains("error"))
        .or_else(|| log.lines().map(str::trim).find(|line| !line.is_empty()))?;
    let line = match line.split_once(':') {
        Some(("1", rest)) if rest.starts_with(|c: char| c.is_ascii_digit()) => {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let (number, rest) = rest.split_at(digits);
            format!("line {number}{rest}")
        }
        Some(("0" | "2", rest)) => match rest.split_once(": ") {
            Some((_, message)) => format!("host wrapper: {message}"),
            None => line.to_string(),
        },
        _ => line.to_string(),
    };
    let mut line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > MAX_DRIVER_LOG_BYTES {
        let mut end = MAX_DRIVER_LOG_BYTES;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str("...");
    }
    Some(line)
}

/// Per-window clock behind `u_time` and `u_delta`. Both are sampled only
/// when the element's content commit changes: time alone never schedules a
/// redraw under damage-driven invalidation.
pub struct ShaderInstance {
    epoch: Instant,
    last_update: Option<Instant>,
    commit: Option<CommitCounter>,
    time: f32,
    delta: f32,
    updates: u64,
}

impl ShaderInstance {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_update: None,
            commit: None,
            time: 0.0,
            delta: 0.0,
            updates: 0,
        }
    }

    /// `(u_time, u_delta)` for this update: seconds since the instance
    /// started, modulo 4096, and seconds since the previous update clamped
    /// to `0..=0.1`, zero on the first.
    pub fn sample(&mut self, commit: CommitCounter) -> (f32, f32) {
        if self.commit != Some(commit) {
            let now = Instant::now();
            self.time = now.duration_since(self.epoch).as_secs_f32() % 4096.0;
            self.delta = self
                .last_update
                .map(|last| now.duration_since(last).as_secs_f32().clamp(0.0, 0.1))
                .unwrap_or(0.0);
            self.last_update = Some(now);
            self.commit = Some(commit);
            self.updates += 1;
        }
        (self.time, self.delta)
    }

    /// Content updates this instance has drawn.
    pub fn updates(&self) -> u64 {
        self.updates
    }
}

/// Everything that changes a custom effect's pixels, except time.
#[allow(clippy::too_many_arguments)]
pub fn custom_shader_commit(
    capture_version: usize,
    generation: u64,
    size: (i32, i32),
    params: &[(String, ShaderParam)],
    corner_radii: [f32; 4],
    rounding_power: f32,
    antialias: f32,
    opacity: f32,
) -> CommitCounter {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    capture_version.hash(&mut hash);
    generation.hash(&mut hash);
    size.hash(&mut hash);
    for (name, value) in params {
        name.hash(&mut hash);
        value.hash_bits(&mut hash);
    }
    for radius in corner_radii {
        radius.to_bits().hash(&mut hash);
    }
    rounding_power.to_bits().hash(&mut hash);
    antialias.to_bits().hash(&mut hash);
    opacity.to_bits().hash(&mut hash);
    CommitCounter::from(hash.finish() as usize)
}

/// One window's custom effect over its captured backdrop, drawn in the
/// glass layer's z-slot directly behind the window's own surfaces.
pub struct CustomShaderElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    geometry: Rectangle<i32, Physical>,
    program: ShaderProgram,
    corner_radii: [f32; 4],
    rounding_power: f32,
    antialias: f32,
    opacity: f32,
    time: f32,
    delta: f32,
}

impl CustomShaderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        texture: GlesTexture,
        geometry: Rectangle<i32, Physical>,
        program: ShaderProgram,
        corner_radii: [f32; 4],
        rounding_power: f32,
        antialias: f32,
        opacity: f32,
        (time, delta): (f32, f32),
    ) -> Self {
        Self {
            id,
            commit,
            texture,
            geometry,
            program,
            corner_radii,
            rounding_power,
            antialias,
            opacity,
            time,
            delta,
        }
    }

    fn uniforms(&self) -> Vec<Uniform<'_>> {
        let size = self.texture.size();
        let (texture_w, texture_h) = (size.w.max(1) as f32, size.h.max(1) as f32);
        let mut uniforms = Vec::with_capacity(CONTRACT_UNIFORMS.len() + self.program.params.len());
        uniforms.extend([
            Uniform::new(
                "u_size",
                [
                    self.geometry.size.w.max(1) as f32,
                    self.geometry.size.h.max(1) as f32,
                ],
            ),
            Uniform::new("u_texture_size", [texture_w, texture_h]),
            Uniform::new("u_content_rect", [0.0, 0.0, texture_w, texture_h]),
            Uniform::new("u_texel", [1.0 / texture_w, 1.0 / texture_h]),
            Uniform::new("u_time", self.time),
            Uniform::new("u_delta", self.delta),
            Uniform::new("u_corner_radii", self.corner_radii),
            Uniform::new("u_rounding_power", self.rounding_power),
            Uniform::new("u_antialias", self.antialias),
        ]);
        uniforms.extend(
            self.program
                .params
                .iter()
                .map(|(name, value)| Uniform::new(name.as_str(), value.uniform_value())),
        );
        uniforms
    }
}

impl Element for CustomShaderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size().to_f64())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn alpha(&self) -> f32 {
        self.opacity
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for CustomShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha(),
            Some(&self.program.program),
            &self.uniforms(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSTHROUGH: &str = "vec4 tide_effect(vec2 uv) {\n    return texture2D(tex, uv);\n}\n";

    #[test]
    fn passthrough_satisfies_the_contract() {
        assert_eq!(validate_fragment_contract(PASSTHROUGH), Ok(()));
    }

    #[test]
    fn helpers_structs_and_contract_uniforms_are_allowed() {
        let source = "\
struct Wave { float height; };
const float PI = 3.14159;
float ripple(vec2 p, float t) { return sin(length(p) * 10.0 - t) * 0.01; }
// a comment mentioning main and uniform is fine
/* so is gl_FragColor in a block comment */
vec4 tide_effect(vec2 uv) {
    Wave wave;
    float shift = ripple(uv * u_size, u_time), other = 1.0;
    return texture2D(tex, uv + vec2(shift)) * other;
}
";
        assert_eq!(validate_fragment_contract(source), Ok(()));
    }

    #[test]
    fn host_owned_directives_are_rejected() {
        for directive in [
            "#version 100",
            "#extension GL_OES_standard_derivatives : enable",
            "#include \"x\"",
            "#line 4",
            "#pragma optimize(off)",
        ] {
            let source = format!("{directive}\n{PASSTHROUGH}");
            assert!(validate_fragment_contract(&source).is_err(), "{directive}");
        }
        let source = format!("#define X 1\n#ifdef X\n#endif\n{PASSTHROUGH}");
        assert_eq!(validate_fragment_contract(&source), Ok(()));
        let source = format!("#define CAT(a, b) a ## b\n{PASSTHROUGH}");
        assert!(validate_fragment_contract(&source).is_err());
    }

    #[test]
    fn host_owned_identifiers_and_declarations_are_rejected() {
        for bad in [
            "void main() {}\n",
            "uniform float strength;\n",
            "precision mediump float;\n",
            "varying vec2 v_other;\n",
            "float alpha = 1.0;\n",
            "float u_time = 1.0;\n",
            "float x, u_extra;\n",
            "float f(vec2 p, float u_param) { return 1.0; }\n",
            "vec4 tint_it() { return vec4(tint); }\n",
            "void f() { gl_FragColor = vec4(1.0); }\n",
            "float tide_helper() { return 1.0; }\n",
            "struct u_bad { float a; };\n",
            "sampler2D tex;\n",
        ] {
            let source = format!("{bad}{PASSTHROUGH}");
            assert!(validate_fragment_contract(&source).is_err(), "{bad}");
        }
    }

    #[test]
    fn entry_point_must_be_defined_exactly_once_with_the_fixed_signature() {
        assert!(validate_fragment_contract("float f() { return 1.0; }\n").is_err());
        assert!(
            validate_fragment_contract("vec3 tide_effect(vec2 uv) { return vec3(0.0); }\n")
                .is_err()
        );
        assert!(
            validate_fragment_contract("vec4 tide_effect(vec3 uv) { return vec4(0.0); }\n")
                .is_err()
        );
        assert!(validate_fragment_contract(&format!("{PASSTHROUGH}{PASSTHROUGH}")).is_err());
        let prototype = format!("vec4 tide_effect(vec2 uv);\n{PASSTHROUGH}");
        assert_eq!(validate_fragment_contract(&prototype), Ok(()));
    }

    #[test]
    fn nul_non_ascii_oversize_and_open_comments_are_rejected() {
        assert!(validate_fragment_contract(&format!("{PASSTHROUGH}\0")).is_err());
        assert!(validate_fragment_contract(&format!("float café = 1.0;\n{PASSTHROUGH}")).is_err());
        assert_eq!(
            validate_fragment_contract(&format!("// café\n{PASSTHROUGH}")),
            Ok(())
        );
        assert!(validate_fragment_contract(&format!("{PASSTHROUGH}/* open")).is_err());
        let huge = format!("{PASSTHROUGH}{}", " ".repeat(MAX_SOURCE_BYTES));
        assert!(validate_fragment_contract(&huge).is_err());
    }

    #[test]
    fn wrapper_keeps_smithay_markers_line_mapping_and_declares_params() {
        let params = vec![
            ("strength".to_string(), ShaderParam::Float(0.25)),
            ("tint_color".to_string(), ShaderParam::Vec4([1.0; 4])),
        ];
        let source = format!("// //_DEFINES_ in a user comment\n{PASSTHROUGH}");
        let wrapped = wrap_texture_fragment(&source, &params);
        assert!(wrapped.starts_with("#version 100\n"));
        assert_eq!(wrapped.matches("//_DEFINES_").count(), 1);
        assert!(wrapped.contains("uniform sampler2D tex;"));
        assert!(wrapped.contains("uniform float alpha;"));
        for (name, ty) in CONTRACT_UNIFORMS {
            assert!(wrapped.contains(&format!("uniform {ty} {name};")));
        }
        assert!(wrapped.contains("uniform float strength;"));
        assert!(wrapped.contains("uniform vec4 tint_color;"));
        let user_start = wrapped.find("#line 1 1\n").expect("user line mapping");
        let epilogue = wrapped.find("#line 1 2\n").expect("epilogue line mapping");
        assert!(user_start < epilogue);
        assert!(wrapped[user_start..epilogue].contains("return texture2D(tex, uv);"));
        assert!(wrapped[epilogue..].contains("tide_effect(v_coords)"));
        assert!(wrapped[epilogue..].contains("gl_FragColor = color;"));
        // Comment stripping keeps user line numbers aligned.
        assert_eq!(
            wrapped[user_start..epilogue].lines().count(),
            source.lines().count() + 1
        );
    }

    #[test]
    fn param_names_cannot_shadow_host_or_glsl_names() {
        assert!(valid_param_name("strength"));
        assert!(valid_param_name("_scale2"));
        for bad in [
            "", "2x", "u_time", "gl_x", "tide_x", "tex", "alpha", "size", "main", "float", "vec4",
            "for", "a__b", "has-dash",
        ] {
            assert!(!valid_param_name(bad), "{bad}");
        }
        assert!(!valid_param_name(&"x".repeat(MAX_NAME_BYTES + 1)));
    }

    #[test]
    fn file_reads_require_a_bounded_regular_frag_file() {
        let dir = std::env::temp_dir().join(format!("tidewm-shader-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let good = dir.join("pass.frag");
        fs::write(&good, PASSTHROUGH).unwrap();
        assert_eq!(read_fragment_file(&good).as_deref(), Ok(PASSTHROUGH));

        let wrong_extension = dir.join("pass.glsl");
        fs::write(&wrong_extension, PASSTHROUGH).unwrap();
        assert!(read_fragment_file(&wrong_extension).is_err());

        let directory = dir.join("dir.frag");
        fs::create_dir_all(&directory).unwrap();
        assert!(read_fragment_file(&directory).is_err());

        let fifo = dir.join("pipe.frag");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and plain permission bits.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert!(read_fragment_file(&fifo).is_err());

        let oversized = dir.join("big.frag");
        fs::write(&oversized, vec![b' '; MAX_SOURCE_BYTES + 1]).unwrap();
        assert!(read_fragment_file(&oversized).is_err());

        let not_utf8 = dir.join("bytes.frag");
        fs::write(&not_utf8, [0xff, 0xfe]).unwrap();
        assert!(read_fragment_file(&not_utf8).is_err());

        assert!(read_fragment_file(&dir.join("missing.frag")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_payload_rounds_each_axis_up_and_saturates() {
        assert_eq!(capture_payload_bytes(1920, 1080, 1), 1920 * 1080 * 4);
        assert_eq!(capture_payload_bytes(1921, 1081, 2), 961 * 541 * 4);
        assert_eq!(capture_payload_bytes(3, 3, 4), 4);
        assert_eq!(capture_payload_bytes(-5, 10, 1), 0);
        assert_eq!(
            capture_payload_bytes(i32::MAX, i32::MAX, 1),
            (i32::MAX as u64).pow(2) * 4
        );
        // Two native 4K captures already use most of the aggregate budget.
        assert!(2 * capture_payload_bytes(3840, 2160, 1) < MAX_PAYLOAD_BYTES);
        assert!(3 * capture_payload_bytes(3840, 2160, 1) > MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn driver_logs_are_summarized_onto_the_users_file() {
        assert_eq!(
            summarize_driver_log("1:2(12): error: `return' with wrong type float\n1:3: warning: x")
                .as_deref(),
            Some("line 2(12): error: `return' with wrong type float")
        );
        assert_eq!(
            summarize_driver_log("0:40(1): error: something in the prelude").as_deref(),
            Some("host wrapper: error: something in the prelude")
        );
        assert_eq!(
            summarize_driver_log("some vendor text\nERROR: 1(2) : bad").as_deref(),
            Some("ERROR: 1(2) : bad")
        );
        assert_eq!(summarize_driver_log("  \n"), None);
        let long = format!("1:1(1): error: {}", "é".repeat(400));
        let summary = summarize_driver_log(&long).unwrap();
        assert!(summary.len() <= MAX_DRIVER_LOG_BYTES + 3 && summary.ends_with("..."));
    }

    #[test]
    fn commit_tracks_content_but_not_time() {
        let params = [("strength".to_string(), ShaderParam::Float(0.5))];
        let commit = |version, generation, params: &[(String, ShaderParam)], opacity| {
            custom_shader_commit(
                version,
                generation,
                (100, 80),
                params,
                [4.0; 4],
                2.0,
                1.0,
                opacity,
            )
        };
        let baseline = commit(3, 7, &params, 1.0);
        assert_eq!(baseline, commit(3, 7, &params, 1.0));
        assert_ne!(baseline, commit(4, 7, &params, 1.0));
        assert_ne!(baseline, commit(3, 8, &params, 1.0));
        assert_ne!(baseline, commit(3, 7, &params, 0.5));
        let changed = [("strength".to_string(), ShaderParam::Float(0.75))];
        assert_ne!(baseline, commit(3, 7, &changed, 1.0));
    }

    #[test]
    fn instance_clock_advances_only_on_a_new_commit() {
        let mut instance = ShaderInstance::new();
        let first = CommitCounter::from(1usize);
        let (time, delta) = instance.sample(first);
        assert_eq!(delta, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(instance.sample(first), (time, delta));
        let (later, delta) = instance.sample(CommitCounter::from(2usize));
        assert!(later > time);
        assert!(delta > 0.0 && delta <= 0.1);
    }

    #[test]
    fn source_identity_adopts_an_equal_reloaded_source() {
        let original: Arc<str> = Arc::from("vec4 a;");
        let reloaded: Arc<str> = Arc::from("vec4 a;");
        let mut cached = Some(original.clone());
        assert!(same_source(&mut cached, &original));
        assert!(same_source(&mut cached, &reloaded));
        assert!(Arc::ptr_eq(cached.as_ref().unwrap(), &reloaded));
        assert!(!same_source(&mut cached, &Arc::from("vec4 b;")));
        assert!(!same_source(&mut None, &original));
    }

    /// A headless renderer on Mesa's software EGL device. Tests that need
    /// one skip where it's missing, so they prove a real GLSL ES compile and
    /// draw where they run, without making a bare CI image fail.
    fn software_renderer() -> Option<GlesRenderer> {
        use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
        let device = EGLDevice::enumerate()
            .ok()?
            .find(|device| device.is_software())?;
        // SAFETY: the display owns the device handle for the renderer's life.
        let display = unsafe { EGLDisplay::new(device) }.ok()?;
        let context = EGLContext::new(&display).ok()?;
        // SAFETY: the context is fresh and used only by this renderer.
        unsafe { GlesRenderer::new(context) }.ok()
    }

    fn compile(
        renderer: &mut GlesRenderer,
        programs: &mut CustomShaderPrograms,
        name: &str,
        user_source: &str,
        params: &[(String, ShaderParam)],
    ) -> ProgramLookup {
        validate_fragment_contract(user_source).expect("fixture passes the contract");
        let params: Arc<[(String, ShaderParam)]> = params.to_vec().into();
        let wrapped: Arc<str> = Arc::from(wrap_texture_fragment(user_source, &params));
        programs.lookup(renderer, name, Some(&wrapped), &params, &mut 1)
    }

    const SIZE: i32 = 8;

    /// RGBA with red rising left to right and green top to bottom, so any
    /// flip or transpose shows up in the readback.
    fn gradient() -> Vec<u8> {
        (0..SIZE)
            .flat_map(|y| (0..SIZE).flat_map(move |x| [x as u8 * 32, y as u8 * 32, 128, 255]))
            .collect()
    }

    fn render(
        renderer: &mut GlesRenderer,
        program: ShaderProgram,
        opacity: f32,
        corner_radii: [f32; 4],
    ) -> Vec<u8> {
        use smithay::backend::{
            allocator::Fourcc,
            renderer::{damage::OutputDamageTracker, Bind, ExportMem, ImportMem, Offscreen},
        };
        let size = smithay::utils::Size::<i32, Buffer>::from((SIZE, SIZE));
        let input = renderer
            .import_memory(&gradient(), Fourcc::Abgr8888, size, false)
            .expect("import fixture");
        let mut output: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, size)
            .expect("allocate target");
        let element = CustomShaderElement::new(
            Id::new(),
            CommitCounter::default(),
            input,
            Rectangle::from_size((SIZE, SIZE).into()),
            program,
            corner_radii,
            2.0,
            0.5,
            opacity,
            (0.0, 0.0),
        );
        let mut target = renderer.bind(&mut output).expect("bind target");
        OutputDamageTracker::new((SIZE, SIZE), 1.0, Transform::Normal)
            .render_output(renderer, &mut target, 0, &[element], [0.0, 0.0, 0.0, 0.0])
            .expect("draw");
        let mapping = renderer
            .copy_framebuffer(&target, Rectangle::from_size(size), Fourcc::Abgr8888)
            .expect("read back");
        drop(target);
        renderer
            .map_texture(&mapping)
            .expect("map readback")
            .to_vec()
    }

    fn pixel(image: &[u8], x: i32, y: i32) -> [u8; 4] {
        let index = ((y * SIZE + x) * 4) as usize;
        image[index..index + 4].try_into().unwrap()
    }

    #[test]
    fn passthrough_draws_the_input_unchanged_and_upright() {
        let Some(mut renderer) = software_renderer() else {
            eprintln!("no software EGL device; skipping the real compile and draw");
            return;
        };
        let mut programs = CustomShaderPrograms::default();
        let lookup = compile(&mut renderer, &mut programs, "pass", PASSTHROUGH, &[]);
        assert!(lookup.failure.is_none());
        let image = render(
            &mut renderer,
            lookup.program.expect("compiled"),
            1.0,
            [0.0; 4],
        );
        assert_eq!(image, gradient());

        let uv = "vec4 tide_effect(vec2 uv) {\n    return vec4(uv, 0.0, 1.0);\n}\n";
        let lookup = compile(&mut renderer, &mut programs, "uv", uv, &[]);
        let image = render(
            &mut renderer,
            lookup.program.expect("compiled"),
            1.0,
            [0.0; 4],
        );
        // uv (0, 0) is the top-left of the captured input.
        assert!(pixel(&image, 0, 0)[0] < 32 && pixel(&image, 0, 0)[1] < 32);
        assert!(pixel(&image, SIZE - 1, 0)[0] > 220 && pixel(&image, SIZE - 1, 0)[1] < 32);
        assert!(pixel(&image, 0, SIZE - 1)[1] > 220);
    }

    #[test]
    fn host_applies_params_opacity_premultiplication_and_rounding() {
        let Some(mut renderer) = software_renderer() else {
            eprintln!("no software EGL device; skipping the real compile and draw");
            return;
        };
        let mut programs = CustomShaderPrograms::default();
        let source = "vec4 tide_effect(vec2 uv) {\n    return vec4(tint_color.rgb * 4.0, tint_color.a);\n}\n";
        let params = [(
            "tint_color".to_string(),
            ShaderParam::Vec4([0.5, 0.25, 0.0, 0.5]),
        )];
        let program = compile(&mut renderer, &mut programs, "tint", source, &params)
            .program
            .expect("compiled");

        // RGB is bounded by alpha, then opacity scales the whole premultiplied color.
        let image = render(&mut renderer, program.clone(), 1.0, [0.0; 4]);
        assert_eq!(pixel(&image, 4, 4), [128, 128, 0, 128]);
        let image = render(&mut renderer, program.clone(), 0.5, [0.0; 4]);
        let faded = pixel(&image, 4, 4);
        assert!(faded
            .iter()
            .zip([64, 64, 0, 64])
            .all(|(got, want)| got.abs_diff(want) <= 1));

        // Rounding clips even though the shader never reads the radii.
        let image = render(&mut renderer, program, 1.0, [4.0; 4]);
        assert_eq!(pixel(&image, 0, 0)[3], 0);
        assert_eq!(pixel(&image, 4, 4)[3], 128);
    }

    #[test]
    fn compile_failures_are_reported_once_and_keep_the_last_good_program() {
        let Some(mut renderer) = software_renderer() else {
            eprintln!("no software EGL device; skipping the real compile and draw");
            return;
        };
        let mut programs = CustomShaderPrograms::default();
        // Passes the token contract, fails GLSL type checking.
        let broken = "vec4 tide_effect(vec2 uv) {\n    return 1.0;\n}\n";
        let first = compile(&mut renderer, &mut programs, "fx", broken, &[]);
        assert!(first.program.is_none());
        let message = first.failure.expect("first failure is reported");
        assert!(message.contains("without it"), "{message}");
        // Mesa names the user's own line and column, not wrapper-relative ones.
        assert!(message.contains(": line 2("), "{message}");
        assert!(message.contains("error"), "{message}");
        let again = compile(&mut renderer, &mut programs, "fx", broken, &[]);
        assert!(again.program.is_none() && again.failure.is_none());

        let good = compile(&mut renderer, &mut programs, "fx", PASSTHROUGH, &[]);
        let good_generation = good.program.expect("compiled").generation;
        let regressed = compile(&mut renderer, &mut programs, "fx", broken, &[]);
        assert_eq!(
            regressed.program.map(|program| program.generation),
            Some(good_generation)
        );
        assert!(regressed
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("keeping the previous program")));

        // The budget defers a compile instead of stalling one frame on several.
        let deferred = programs.lookup(
            &mut renderer,
            "other",
            Some(&Arc::from(wrap_texture_fragment(PASSTHROUGH, &[]))),
            &Arc::from(Vec::new()),
            &mut 0,
        );
        assert!(deferred.deferred && deferred.program.is_none());
    }
}
