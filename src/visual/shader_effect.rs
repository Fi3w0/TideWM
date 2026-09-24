//! Custom `.frag` effects: the fixed fragment contract, the host wrapper and
//! bounded file loading (Phase 7, see `SHADER_DESIGN.md`).
//!
//! A user file defines `vec4 tide_effect(vec2 uv)` plus optional helpers and
//! returns premultiplied RGBA. The host owns everything else: `#version`,
//! precision, Smithay's `//_DEFINES_` variants, every uniform declaration,
//! `main`, opacity and rounded clipping. The contract check below is API
//! hygiene over a small GLSL token stream, not an execution sandbox; custom
//! shaders are trusted native GPU programs.

use std::{collections::HashSet, fs, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

/// Largest accepted `.frag` source.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Largest total of wrapped sources one configuration may load.
pub const MAX_CANDIDATE_BYTES: usize = 1024 * 1024;
pub const MAX_DEFINITIONS: usize = 32;
pub const MAX_STAGES: usize = 4;
pub const MAX_PARAMS_PER_STAGE: usize = 8;
pub const MAX_NAME_BYTES: usize = 64;

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
}
